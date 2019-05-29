//! CLI tool to read Cranelift IR files and compile them into native code.

use crate::disasm::{PrintRelocs, PrintTraps};
use crate::utils::{parse_sets_and_triple, read_to_string};
use cranelift_codegen::ir::{Ebb, Function, Inst, InstBuilder, TrapCode};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::FlagsOrIsa;
use cranelift_codegen::timing;
use cranelift_codegen::Context;
use cranelift_reader::parse_test;
use std::path::Path;
use std::path::PathBuf;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub fn run(filename: &str, flag_set: &[String], flag_isa: &str) -> Result<(), String> {
    let parsed = parse_sets_and_triple(flag_set, flag_isa)?;

    let path = Path::new(&filename);
    let name = String::from(path.as_os_str().to_string_lossy());
    handle_module(&path.to_path_buf(), &name, parsed.as_fisa())
}

fn handle_module(path: &PathBuf, name: &str, fisa: FlagsOrIsa) -> Result<(), String> {
    let buffer = read_to_string(&path).map_err(|e| format!("{}: {}", name, e))?;
    let test_file = parse_test(&buffer, None, None).map_err(|e| format!("{}: {}", name, e))?;

    // If we have an isa from the command-line, use that. Otherwise if the
    // file contains a unique isa, use that.
    let isa = if let Some(isa) = fisa.isa {
        isa
    } else if let Some(isa) = test_file.isa_spec.unique_isa() {
        isa
    } else {
        return Err(String::from("compilation requires a target isa"));
    };

    std::env::set_var("RUST_BACKTRACE", "0"); // Disable backtraces to reduce verbosity

    for (func, _) in test_file.functions {
        reduce(isa, func);
    }

    //print!("{}", timing::take_current());

    Ok(())
}

/// This stores the current thing to reduce
enum Phase {
    Started,

    /// Try to remove this instruction
    RemoveInst(Ebb, Inst),
    /// Try to replace inst with iconst
    ReplaceInstWithIconst(Ebb, Inst),
    /// Try to replace inst with trap
    ReplaceInstWithTrap(Ebb, Inst),
    /// Try to remove an ebb
    RemoveEbb(Ebb),
}

enum PhaseStepResult {
    Shrinked(String),
    Replaced(String),
    NoChange,
    NextPhase(&'static str, usize),
    Finished,
}

impl Phase {
    fn step(&mut self, func: &mut Function) -> PhaseStepResult {
        let first_ebb = func.layout.entry_block().unwrap();
        match self {
            Phase::Started => {
                *self = Phase::RemoveInst(first_ebb, func.layout.first_inst(first_ebb).unwrap());
                PhaseStepResult::NextPhase("remove inst", inst_count(func))
            }
            Phase::RemoveInst(ref mut ebb, ref mut inst) => {
                if let Some((prev_ebb, prev_inst)) = next_inst_ret_prev(func, ebb, inst) {
                    func.layout.remove_inst(prev_inst);
                    if func.layout.ebb_insts(prev_ebb).next().is_none() {
                        // Make sure empty ebbs are removed, as `next_inst_ret_prev` depends on non empty ebbs
                        func.layout.remove_ebb(prev_ebb);
                        PhaseStepResult::Shrinked(format!("Remove inst {} and empty ebb {}", prev_inst, prev_ebb))
                    } else {
                        PhaseStepResult::Shrinked(format!("Remove inst {}", prev_inst))
                    }
                } else {
                    *self = Phase::ReplaceInstWithIconst(
                        first_ebb,
                        func.layout.first_inst(first_ebb).unwrap(),
                    );
                    PhaseStepResult::NextPhase("replace inst with iconst", inst_count(func))
                }
            }
            Phase::ReplaceInstWithIconst(ref mut ebb, ref mut inst) => {
                if let Some((_prev_ebb, prev_inst)) = next_inst_ret_prev(func, ebb, inst) {
                    let results = func.dfg.inst_results(prev_inst);
                    if results.len() == 1 {
                        let ty = func.dfg.value_type(results[0]);
                        func.dfg.replace(prev_inst).iconst(ty, 0);
                        PhaseStepResult::Replaced(format!("Replace inst {} with iconst.{}", prev_inst, ty))
                    } else {
                        PhaseStepResult::NoChange
                    }
                } else {
                    *self = Phase::ReplaceInstWithTrap(
                        first_ebb,
                        func.layout.first_inst(first_ebb).unwrap(),
                    );
                    PhaseStepResult::NextPhase("replace inst with trap", inst_count(func))
                }
            }
            Phase::ReplaceInstWithTrap(ref mut ebb, ref mut inst) => {
                if let Some((_prev_ebb, prev_inst)) = next_inst_ret_prev(func, ebb, inst) {
                    func.dfg.replace(prev_inst).trap(TrapCode::User(0));
                    PhaseStepResult::Replaced(format!("Replace inst {} with trap", prev_inst))
                } else {
                    *self = Phase::RemoveEbb(first_ebb);
                    PhaseStepResult::NextPhase("remove ebb", ebb_count(func))
                }
            }
            Phase::RemoveEbb(ref mut ebb) => {
                let prev_ebb = *ebb;
                if let Some(next_ebb) = func.layout.next_ebb(*ebb) {
                    *ebb = next_ebb;
                    func.layout.remove_ebb(*ebb);
                    PhaseStepResult::Shrinked(format!("Remove ebb {}", prev_ebb))
                } else {
                    PhaseStepResult::Finished
                }
            }
        }
    }
}

fn next_inst_ret_prev(func: &Function, ebb: &mut Ebb, inst: &mut Inst) -> Option<(Ebb, Inst)> {
    let prev = (*ebb, *inst);
    if let Some(next_inst) = func.layout.next_inst(*inst) {
        *inst = next_inst;
        return Some(prev);
    } else if let Some(next_ebb) = func.layout.next_ebb(*ebb) {
        *ebb = next_ebb;
        *inst = func.layout.first_inst(*ebb).expect("no inst");
        return Some(prev);
    } else {
        return None;
    }
}

fn ebb_count(func: &Function) -> usize {
    func.layout.ebbs().count()
}

fn inst_count(func: &Function) -> usize {
    func.layout.ebbs().map(|ebb| func.layout.ebb_insts(ebb).count()).sum()
}

fn reduce(isa: &TargetIsa, mut func: Function) {
    let (orig_ebb_count, orig_inst_count) = (ebb_count(&func), inst_count(&func));
    'outer_loop: for pass_idx in 0..100 {
        let mut was_reduced = false;
        let mut phase = Phase::Started;

        let mut progress = ProgressBar::hidden();

        'inner_loop: for _ in 0..10000 {
            progress.inc(1);
            let mut func2 = func.clone();

            let (msg, shrinked) = match phase.step(&mut func2) {
                PhaseStepResult::Shrinked(msg) => (msg, true),
                PhaseStepResult::Replaced(msg) => (msg, false),
                PhaseStepResult::NoChange => continue 'inner_loop,
                PhaseStepResult::NextPhase(msg, count) => {
                    progress.set_message("done");
                    progress.finish();
                    progress = ProgressBar::with_draw_target(count as u64, ProgressDrawTarget::stdout());
                    progress.set_style(ProgressStyle::default_bar().template("{bar:80} {prefix:30} {pos}/{len} {msg}"));
                    progress.set_prefix(&format!("pass {} phase {}", pass_idx, msg));
                    continue 'inner_loop;
                }
                PhaseStepResult::Finished => {
                    progress.finish();
                    break 'inner_loop;
                }
            };

            progress.set_message(&msg);

            match check_for_crash(isa, &func2) {
                Res::Succeed => {
                    // Shrinking didn't hit the problem anymore, discard changes.
                    //progress.println("succeeded");
                    continue;
                }
                Res::Verifier(err) => {
                    // Shrinking produced invalid clif, discard changes.
                    //progress.println(format!("verifier error {}", err));
                    continue;
                }
                Res::Panic => {
                    // Panic remained while shrinking, make changes definitive.
                    func = func2;
                    if shrinked {
                        was_reduced = true;
                        progress.println(format!("{}: shrink", msg));
                    } else {
                        progress.println(format!("{}: replace", msg));
                    }
                }
            }
        }

        if !was_reduced {
            // No new shrinking opportunities have been found this pass. This means none will ever
            // be found. Skip the rest of the passes over the function.
            break 'outer_loop;
        }
    }

    println!("{}", func);

    println!("{} ebbs {} insts -> {} ebbs {} insts", orig_ebb_count, orig_inst_count, ebb_count(&func), inst_count(&func));
}

enum Res {
    Succeed,
    Verifier(String),
    Panic,
}

fn check_for_crash(isa: &TargetIsa, func: &Function) -> Res {
    let mut context = Context::new();
    context.func = func.clone();

    let mut relocs = PrintRelocs::new(false);
    let mut traps = PrintTraps::new(false);
    let mut mem = vec![];

    use std::io::Write;
    std::io::stdout().flush().unwrap(); // Flush stdout to sync with panic messages on stderr

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Err(err) = cranelift_codegen::verifier::verify_function(&func, isa) {
            Some(err)
        } else {
            None
        }
    })) {
        Ok(Some(err)) => return Res::Verifier(err.to_string()),
        Ok(None) => {}
        Err(err) => {
            // FIXME prevent verifier panic on removing ebb1
            return Res::Verifier(format!("verifier panicked: {:?}", err.downcast::<&'static str>()));
        }
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Err(verifier_err) = context.compile_and_emit(isa, &mut mem, &mut relocs, &mut traps)
        {
            Res::Verifier(verifier_err.to_string())
        } else {
            Res::Succeed
        }
    })) {
        Ok(res) => res,
        Err(_panic) => Res::Panic,
    }
}
