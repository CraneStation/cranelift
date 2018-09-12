//! Test runner.
//!
//! This module implements the `TestRunner` struct which manages executing tests as well as
//! scanning directories for tests.

use concurrent::{ConcurrentRunner, Reply};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{self, Display};
use std::path::{Path, PathBuf};
use std::time;
use {runone, TestResult};

/// Timeout in seconds when we're not making progress.
const TIMEOUT_PANIC: usize = 10;

/// Timeout for reporting slow tests without panicking.
const TIMEOUT_SLOW: usize = 3;

struct QueueEntry {
    path: PathBuf,
    state: State,
}

#[derive(PartialEq, Eq, Debug)]
enum State {
    New,
    Queued,
    Running,
    Done(TestResult),
}

impl QueueEntry {
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
}

impl Display for QueueEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let p = self.path.to_string_lossy();
        match self.state {
            State::Done(Ok(dur)) => write!(
                f,
                "{}.{:03} {}",
                dur.as_secs(),
                dur.subsec_nanos() / 1_000_000,
                p
            ),
            State::Done(Err(ref e)) => write!(f, "FAIL {}: {}", p, e),
            _ => write!(f, "{}", p),
        }
    }
}

pub struct TestRunner {
    verbose: bool,

    // Directories that have not yet been scanned.
    dir_stack: Vec<PathBuf>,

    // Filenames of tests to run.
    tests: Vec<QueueEntry>,

    // Pointer into `tests` where the `New` entries begin.
    new_tests: usize,

    // Number of contiguous reported tests at the front of `tests`.
    reported_tests: usize,

    // Number of errors seen so far.
    errors: usize,

    // Number of ticks received since we saw any progress.
    ticks_since_progress: usize,

    threads: Option<ConcurrentRunner>,
}

impl TestRunner {
    /// Create a new blank TrstRunner.
    pub fn new(verbose: bool) -> Self {
        Self {
            verbose,
            dir_stack: Vec::new(),
            tests: Vec::new(),
            new_tests: 0,
            reported_tests: 0,
            errors: 0,
            ticks_since_progress: 0,
            threads: None,
        }
    }

    /// Add a directory path to be scanned later.
    ///
    /// If `dir` turns out to be a regular file, it is silently ignored.
    /// Otherwise, any problems reading the directory are reported.
    pub fn push_dir<P: Into<PathBuf>>(&mut self, dir: P) {
        self.dir_stack.push(dir.into());
    }

    /// Add a test to be executed later.
    ///
    /// Any problems reading `file` as a test case file will be reported as a test failure.
    pub fn push_test<P: Into<PathBuf>>(&mut self, file: P) {
        self.tests.push(QueueEntry {
            path: file.into(),
            state: State::New,
        });
    }

    /// Begin running tests concurrently.
    pub fn start_threads(&mut self) {
        assert!(self.threads.is_none());
        self.threads = Some(ConcurrentRunner::new());
    }

    /// Scan any directories pushed so far.
    ///
    /// Call `on_finished_scanning_dir` after each directory's entries have been
    /// enumerated to push any potential test cases found.
    pub fn scan_dirs<F>(&mut self, mut on_finished_scanning_dir: F)
    where
        F: FnMut(&mut Self),
    {
        // This recursive search tries to minimize statting in a directory hierarchy containing
        // mostly test cases.
        //
        // - Directory entries with a "clif" extension are presumed to be test case files.
        // - Directory entries with no extension are presumed to be subdirectories.
        // - Anything else is ignored.
        //
        while let Some(dir) = self.dir_stack.pop() {
            match dir.read_dir() {
                Err(err) => {
                    // Fail silently if `dir` was actually a regular file.
                    // This lets us skip spurious extensionless files without statting everything
                    // needlessly.
                    if !dir.is_file() {
                        self.path_error(&dir, &err);
                    }
                }
                Ok(entries) => {
                    // Read all directory entries. Avoid statting.
                    for entry_result in entries {
                        match entry_result {
                            Err(err) => {
                                // Not sure why this would happen. `read_dir` succeeds, but there's
                                // a problem with an entry. I/O error during a getdirentries
                                // syscall seems to be the reason. The implementation in
                                // libstd/sys/unix/fs.rs seems to suggest that breaking now would
                                // be a good idea, or the iterator could keep returning the same
                                // error forever.
                                self.path_error(&dir, &err);
                                break;
                            }
                            Ok(entry) => {
                                let path = entry.path();
                                // Recognize directories and tests by extension.
                                // Yes, this means we ignore directories with '.' in their name.
                                match path.extension().and_then(OsStr::to_str) {
                                    Some("clif") => self.push_test(path),
                                    Some(_) => {}
                                    None => self.push_dir(path),
                                }
                            }
                        }
                    }
                }
            }
            // Get the new jobs running before moving on to the next directory.
            on_finished_scanning_dir(self);
        }
    }

    /// Report an error related to a path.
    fn path_error<E: Error>(&mut self, path: &PathBuf, err: &E) {
        self.errors += 1;
        println!("{}: {}", path.to_string_lossy(), err);
    }

    /// Report on the next in-order job, if it's done.
    fn report_job(&self) -> bool {
        let jobid = self.reported_tests;
        if let Some(&QueueEntry {
            state: State::Done(ref result),
            ..
        }) = self.tests.get(jobid)
        {
            if self.verbose || result.is_err() {
                println!("{}", self.tests[jobid]);
            }
            true
        } else {
            false
        }
    }

    /// Schedule any new jobs to run.
    fn schedule_jobs(&mut self) {
        for jobid in self.new_tests..self.tests.len() {
            assert_eq!(self.tests[jobid].state, State::New);
            if let Some(ref mut conc) = self.threads {
                // Queue test for concurrent execution.
                self.tests[jobid].state = State::Queued;
                conc.put(jobid, self.tests[jobid].path());
            } else {
                // Run test synchronously.
                self.tests[jobid].state = State::Running;
                let result = runone::run(self.tests[jobid].path(), None, None);
                self.finish_job(jobid, result);
            }
            self.new_tests = jobid + 1;
        }

        // Check for any asynchronous replies without blocking.
        while let Some(reply) = self.threads.as_mut().and_then(ConcurrentRunner::try_get) {
            self.handle_reply(reply);
        }
    }

    /// Schedule any new job to run for the pass command.
    fn schedule_pass_job(&mut self, passes: &[String], target: &str) {
        self.tests[0].state = State::Running;
        let result: Result<time::Duration, String>;

        let specified_target = match target {
            "" => None,
            targ => Some(targ),
        };

        result = runone::run(self.tests[0].path(), Some(passes), specified_target);
        self.finish_job(0, result);
    }

    /// Report the end of a job.
    fn finish_job(&mut self, jobid: usize, result: TestResult) {
        assert_eq!(self.tests[jobid].state, State::Running);
        if result.is_err() {
            self.errors += 1;
        }
        self.tests[jobid].state = State::Done(result);

        // Reports jobs in order.
        while self.report_job() {
            self.reported_tests += 1;
        }
    }

    /// Handle a reply from the async threads.
    fn handle_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Starting { jobid, .. } => {
                assert_eq!(self.tests[jobid].state, State::Queued);
                self.tests[jobid].state = State::Running;
            }
            Reply::Done { jobid, result } => {
                self.ticks_since_progress = 0;
                self.finish_job(jobid, result)
            }
            Reply::Tick => {
                self.ticks_since_progress += 1;
                if self.ticks_since_progress == TIMEOUT_SLOW {
                    println!(
                        "STALLED for {} seconds with {}/{} tests finished",
                        self.ticks_since_progress,
                        self.reported_tests,
                        self.tests.len()
                    );
                    for jobid in self.reported_tests..self.tests.len() {
                        if self.tests[jobid].state == State::Running {
                            println!("slow: {}", self.tests[jobid]);
                        }
                    }
                }
                if self.ticks_since_progress >= TIMEOUT_PANIC {
                    panic!(
                        "worker threads stalled for {} seconds.",
                        self.ticks_since_progress
                    );
                }
            }
        }
    }

    /// Drain the async jobs and shut down the threads.
    fn drain_threads(&mut self) {
        if let Some(mut conc) = self.threads.take() {
            conc.shutdown();
            while self.reported_tests < self.tests.len() {
                match conc.get() {
                    Some(reply) => self.handle_reply(reply),
                    None => break,
                }
            }
            conc.join();
        }
    }

    /// Print out a report of slow tests.
    fn report_slow_tests(&self) {
        // Collect runtimes of succeeded tests.
        let mut times = self
            .tests
            .iter()
            .filter_map(|entry| match *entry {
                QueueEntry {
                    state: State::Done(Ok(dur)),
                    ..
                } => Some(dur),
                _ => None,
            })
            .collect::<Vec<_>>();

        // Get me some real data, kid.
        let len = times.len();
        if len < 4 {
            return;
        }

        // Compute quartiles.
        times.sort();
        let qlen = len / 4;
        let q1 = times[qlen];
        let q3 = times[len - 1 - qlen];
        // Inter-quartile range.
        let iqr = q3 - q1;

        // Cut-off for what we consider a 'slow' test: 3 IQR from the 75% quartile.
        //
        // Q3 + 1.5 IQR are the data points that would be plotted as outliers outside a box plot,
        // but we have a wider distribution of test times, so double it to 3 IQR.
        let cut = q3 + iqr * 3;
        if cut > *times.last().unwrap() {
            return;
        }

        for t in self.tests.iter().filter(|entry| match **entry {
            QueueEntry {
                state: State::Done(Ok(dur)),
                ..
            } => dur > cut,
            _ => false,
        }) {
            println!("slow: {}", t)
        }
    }

    /// Scan pushed directories for tests and run them.
    pub fn run(&mut self) -> TestResult {
        let started = time::Instant::now();
        self.scan_dirs(|me| me.schedule_jobs());
        self.schedule_jobs();
        self.report_slow_tests();
        self.drain_threads();

        println!("{} tests", self.tests.len());
        match self.errors {
            0 => Ok(started.elapsed()),
            1 => Err("1 failure".to_string()),
            n => Err(format!("{} failures", n)),
        }
    }

    /// Scan pushed directories for tests and run specified passes from commandline on them.
    pub fn run_passes(&mut self, passes: &[String], target: &str) -> TestResult {
        let started = time::Instant::now();
        self.scan_dirs(|me| me.schedule_pass_job(passes, target));
        self.schedule_pass_job(passes, target);
        self.report_slow_tests();

        println!("{} tests", self.tests.len());
        match self.errors {
            0 => Ok(started.elapsed()),
            1 => Err("1 failure".to_string()),
            n => Err(format!("{} failures", n)),
        }
    }
}
