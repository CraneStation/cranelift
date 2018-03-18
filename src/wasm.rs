//! CLI tool to use the functions provided by the [cretonne-wasm](../cton_wasm/index.html) crate.
//!
//! Reads Wasm binary files, translates the functions' code to Cretonne IL.

use cton_wasm::{translate_module, DummyEnvironment, ModuleEnvironment};
use std::path::PathBuf;
use cretonne::Context;
use cretonne::settings::FlagsOrIsa;
use cretonne::print_errors::{pretty_error, pretty_verifier_error};
use std::error::Error;
use std::path::Path;
use term;
use utils::{parse_sets_and_isa, read_to_end};
use wabt;

macro_rules! vprintln {
    ($x: expr, $($tts:tt)*) => {
        if $x {
            println!($($tts)*);
        }
    }
}

macro_rules! vprint {
    ($x: expr, $($tts:tt)*) => {
        if $x {
            print!($($tts)*);
        }
    }
}

pub fn run(
    files: Vec<String>,
    flag_verbose: bool,
    flag_just_decode: bool,
    flag_check_translation: bool,
    flag_print: bool,
    flag_set: Vec<String>,
    flag_isa: String,
    flag_print_size: bool,
) -> Result<(), String> {
    let parsed = parse_sets_and_isa(flag_set, flag_isa)?;

    for filename in files {
        let path = Path::new(&filename);
        let name = String::from(path.as_os_str().to_string_lossy());
        handle_module(
            flag_verbose,
            flag_just_decode,
            flag_check_translation,
            flag_print,
            flag_print_size,
            path.to_path_buf(),
            name,
            parsed.as_fisa(),
        )?;
    }
    Ok(())
}

fn handle_module(
    flag_verbose: bool,
    flag_just_decode: bool,
    flag_check_translation: bool,
    flag_print: bool,
    flag_print_size: bool,
    path: PathBuf,
    name: String,
    fisa: FlagsOrIsa,
) -> Result<(), String> {
    let mut terminal = term::stdout().unwrap();
    terminal.fg(term::color::YELLOW).unwrap();
    vprint!(flag_verbose, "Handling: ");
    terminal.reset().unwrap();
    vprintln!(flag_verbose, "\"{}\"", name);
    terminal.fg(term::color::MAGENTA).unwrap();
    vprint!(flag_verbose, "Translating... ");
    terminal.reset().unwrap();

    let mut data = read_to_end(path.clone()).map_err(|err| {
        String::from(err.description())
    })?;

    // If `data` doesn't contain wasm magic file then assume it
    // is a '.wat' file and try to convert it to wasm by `wat2wasm`.
    if !data.starts_with(&[b'\0', b'a', b's', b'm']) {
        data = wabt::wat2wasm(data).map_err(
            |err| format!("wat2wasm: {:?}", err),
        )?;
    }

    let mut dummy_environ = DummyEnvironment::with_flags(fisa.flags.clone());
    translate_module(&data, &mut dummy_environ)?;

    terminal.fg(term::color::GREEN).unwrap();
    vprintln!(flag_verbose, "ok");
    terminal.reset().unwrap();

    if flag_just_decode {
        if flag_print {
            let num_func_imports = dummy_environ.get_num_func_imports();
            for (def_index, func) in dummy_environ.info.function_bodies.iter().enumerate() {
                let func_index = num_func_imports + def_index;
                let mut context = Context::new();
                context.func = func.clone();
                if let Some(start_func) = dummy_environ.info.start_func {
                    if func_index == start_func {
                        println!("; Selected as wasm start function");
                    }
                }
                vprintln!(flag_verbose, "");
                for export_name in &dummy_environ.info.functions[func_index].export_names {
                    println!("; Exported as \"{}\"", export_name);
                }
                println!("{}", context.func.display(None));
                vprintln!(flag_verbose, "");
            }
            terminal.reset().unwrap();
        }
        return Ok(());
    }

    terminal.fg(term::color::MAGENTA).unwrap();
    if flag_check_translation {
        vprint!(flag_verbose, "Checking... ");
    } else {
        vprint!(flag_verbose, "Compiling... ");
    }
    terminal.reset().unwrap();

    if flag_print_size {
        vprintln!(flag_verbose, "");
    }

    let num_func_imports = dummy_environ.get_num_func_imports();
    let mut total_module_code_size = 0;
    for (def_index, func) in dummy_environ.info.function_bodies.iter().enumerate() {
        let func_index = num_func_imports + def_index;
        let mut context = Context::new();
        context.func = func.clone();
        if flag_check_translation {
            context.verify(fisa).map_err(|err| {
                pretty_verifier_error(&context.func, fisa.isa, err)
            })?;
        } else {
            if let Some(isa) = fisa.isa {
                let compiled_size = context.compile(isa).map_err(|err| {
                    pretty_error(&context.func, fisa.isa, err)
                })?;
                if flag_print_size {
                    println!(
                        "Function #{} code size: {} bytes",
                        func_index,
                        compiled_size
                    );
                    total_module_code_size += compiled_size;
                    println!(
                        "Function #{} bytecode size: {} bytes",
                        func_index,
                        dummy_environ.func_bytecode_sizes[func_index]
                    );
                }
            } else {
                return Err(String::from("compilation requires a target isa"));
            }
        }
        if flag_print {
            vprintln!(flag_verbose, "");
            if let Some(start_func) = dummy_environ.info.start_func {
                if func_index == start_func {
                    println!("; Selected as wasm start function");
                }
            }
            for export_name in &dummy_environ.info.functions[func_index].export_names {
                println!("; Exported as \"{}\"", export_name);
            }
            println!("{}", context.func.display(fisa.isa));
            vprintln!(flag_verbose, "");
        }
    }

    if !flag_check_translation && flag_print_size {
        println!("Total module code size: {} bytes", total_module_code_size);
        let total_bytecode_size = dummy_environ.func_bytecode_sizes.iter().fold(
            0,
            |sum, x| sum + x,
        );
        println!("Total module bytecode size: {} bytes", total_bytecode_size);
    }

    terminal.fg(term::color::GREEN).unwrap();
    vprintln!(flag_verbose, "ok");
    terminal.reset().unwrap();
    Ok(())
}
