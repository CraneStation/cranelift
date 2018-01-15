//! Debug tracing macros.
//!
//! This module defines the `dbg!` macro which works like `println!` except it writes to the
//! Cretonne tracing output file if enabled.
//!
//! Tracing can be enabled by setting the `CRETONNE_DBG` environment variable to something
/// other than `0`.
///
/// The output will appear in files named `cretonne.dbg.*`, where the suffix is named after the
/// thread doing the logging.

#[cfg(not(feature = "no_std"))]
use std::cell::RefCell;
#[cfg(not(feature = "no_std"))]
use std::env;
#[cfg(not(feature = "no_std"))]
use std::ffi::OsStr;
use std::fmt;
#[cfg(not(feature = "no_std"))]
use std::fs::File;
#[cfg(not(feature = "no_std"))]
use std::io::{self, Write};
use std::sync::atomic;
#[cfg(not(feature = "no_std"))]
use std::thread;

static STATE: atomic::AtomicIsize = atomic::ATOMIC_ISIZE_INIT;

/// Is debug tracing enabled?
///
/// Debug tracing can be enabled by setting the `CRETONNE_DBG` environment variable to something
/// other than `0`.
///
/// This inline function turns into a constant `false` when debug assertions are disabled.
#[cfg(not(feature = "no_std"))]
#[inline]
pub fn enabled() -> bool {
    if cfg!(debug_assertions) {
        match STATE.load(atomic::Ordering::Relaxed) {
            0 => initialize(),
            s => s > 0,
        }
    } else {
        false
    }
}

/// Does nothing
#[cfg(feature = "no_std")]
#[inline]
pub fn enabled() -> bool {
    false
}

/// Initialize `STATE` from the environment variable.
#[cfg(not(feature = "no_std"))]
fn initialize() -> bool {
    let enable = match env::var_os("CRETONNE_DBG") {
        Some(s) => s != OsStr::new("0"),
        None => false,
    };

    if enable {
        STATE.store(1, atomic::Ordering::Relaxed);
    } else {
        STATE.store(-1, atomic::Ordering::Relaxed);
    }

    enable
}

#[cfg(not(feature = "no_std"))]
thread_local! {
    static WRITER : RefCell<io::BufWriter<File>> = RefCell::new(open_file());
}

/// Write a line with the given format arguments.
///
/// This is for use by the `dbg!` macro.
#[cfg(not(feature = "no_std"))]
pub fn writeln_with_format_args(args: fmt::Arguments) -> io::Result<()> {
    WRITER.with(|rc| {
        let mut w = rc.borrow_mut();
        writeln!(*w, "{}", args)?;
        w.flush()
    })
}

/// Open the tracing file for the current thread.
#[cfg(not(feature = "no_std"))]
fn open_file() -> io::BufWriter<File> {
    let curthread = thread::current();
    let tmpstr;
    let mut path = "cretonne.dbg.".to_owned();
    path.extend(
        match curthread.name() {
            Some(name) => name.chars(),
            // The thread is unnamed, so use the thread ID instead.
            None => {
                tmpstr = format!("{:?}", curthread.id());
                tmpstr.chars()
            }
        }.filter(|ch| ch.is_alphanumeric() || *ch == '-' || *ch == '_'),
    );
    let file = File::create(path).expect("Can't open tracing file");
    io::BufWriter::new(file)
}

/// Write a line to the debug trace file if tracing is enabled.
///
/// Arguments are the same as for `printf!`.
#[macro_export]
macro_rules! dbg {
    ($($arg:tt)+) => {
        if $crate::dbg::enabled() {
            // Drop the error result so we don't get compiler errors for ignoring it.
            // What are you going to do, log the error?
            #[cfg(not(feature = "no_std"))]
            $crate::dbg::writeln_with_format_args(format_args!($($arg)+)).ok();
        }
    }
}

/// Helper for printing lists.
pub struct DisplayList<'a, T>(pub &'a [T])
where
    T: 'a + fmt::Display;

impl<'a, T> fmt::Display for DisplayList<'a, T>
where
    T: 'a + fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0.split_first() {
            None => write!(f, "[]"),
            Some((first, rest)) => {
                write!(f, "[{}", first)?;
                for x in rest {
                    write!(f, ", {}", x)?;
                }
                write!(f, "]")
            }
        }
    }
}
