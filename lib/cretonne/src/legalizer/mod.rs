//! Legalize instructions.
//!
//! A legal instruction is one that can be mapped directly to a machine code instruction for the
//! target ISA. The `legalize_function()` function takes as input any function and transforms it
//! into an equivalent function using only legal instructions.
//!
//! The characteristics of legal instructions depend on the target ISA, so any given instruction
//! can be legal for one ISA and illegal for another.
//!
//! Besides transforming instructions, the legalizer also fills out the `function.encodings` map
//! which provides a legal encoding recipe for every instruction.
//!
//! The legalizer does not deal with register allocation constraints. These constraints are derived
//! from the encoding recipes, and solved later by the register allocator.

use cursor::{Cursor, FuncCursor};
use flowgraph::ControlFlowGraph;
use ir::{self, InstBuilder};
use isa::TargetIsa;
use bitset::BitSet;
use timing;

mod boundary;
mod globalvar;
mod heap;
mod libcall;
mod split;

use self::globalvar::expand_global_addr;
use self::heap::expand_heap_addr;
use self::libcall::expand_as_libcall;

/// Legalize `func` for `isa`.
///
/// - Transform any instructions that don't have a legal representation in `isa`.
/// - Fill out `func.encodings`.
///
pub fn legalize_function(func: &mut ir::Function, cfg: &mut ControlFlowGraph, isa: &TargetIsa) {
    let _tt = timing::legalize();
    debug_assert!(cfg.is_valid());

    boundary::legalize_signatures(func, isa);

    func.encodings.resize(func.dfg.num_insts());

    initialize_static_heap_bases(func, isa);

    let mut pos = FuncCursor::new(func);

    // Process EBBs in layout order. Some legalization actions may split the current EBB or append
    // new ones to the end. We need to make sure we visit those new EBBs too.
    while let Some(_ebb) = pos.next_ebb() {
        // Keep track of the cursor position before the instruction being processed, so we can
        // double back when replacing instructions.
        let mut prev_pos = pos.position();

        while let Some(inst) = pos.next_inst() {
            let opcode = pos.func.dfg[inst].opcode();

            // Check for ABI boundaries that need to be converted to the legalized signature.
            if opcode.is_call() {
                if boundary::handle_call_abi(inst, pos.func, cfg) {
                    // Go back and legalize the inserted argument conversion instructions.
                    pos.set_position(prev_pos);
                    continue;
                }
            } else if opcode.is_return() {
                if boundary::handle_return_abi(inst, pos.func, cfg) {
                    // Go back and legalize the inserted return value conversion instructions.
                    pos.set_position(prev_pos);
                    continue;
                }
            } else if opcode.is_branch() {
                split::simplify_branch_arguments(&mut pos.func.dfg, inst);
            }

            match isa.encode(
                &pos.func.dfg,
                &pos.func.dfg[inst],
                pos.func.dfg.ctrl_typevar(inst),
            ) {
                Ok(encoding) => pos.func.encodings[inst] = encoding,
                Err(action) => {
                    // We should transform the instruction into legal equivalents.
                    let changed = action(inst, pos.func, cfg, isa);
                    // If the current instruction was replaced, we need to double back and revisit
                    // the expanded sequence. This is both to assign encodings and possible to
                    // expand further.
                    // There's a risk of infinite looping here if the legalization patterns are
                    // unsound. Should we attempt to detect that?
                    if changed {
                        pos.set_position(prev_pos);
                        continue;
                    }

                    // We don't have any pattern expansion for this instruction either.
                    // Try converting it to a library call as a last resort.
                    if expand_as_libcall(inst, pos.func) {
                        pos.set_position(prev_pos);
                        continue;
                    }
                }
            }

            // Remember this position in case we need to double back.
            prev_pos = pos.position();
        }
    }
}

fn initialize_static_heap_bases(func: &mut ir::Function, isa: &TargetIsa) {
    let addr_ty = if isa.flags().is_64bit() {
        ir::types::I64
    } else {
        ir::types::I32
    };

    if let Some(entry_block) = func.layout.entry_block() {
        let mut pos = FuncCursor::new(func).at_first_insertion_point(entry_block);

        for heap in pos.func.heaps.keys() {
            if let ir::HeapStyle::Static { .. } = pos.func.heaps[heap].style {
                pos.func.static_heap_bases[heap] = match pos.func.heaps[heap].base {
                    ir::HeapBase::ReservedReg => unimplemented!(),
                    ir::HeapBase::GlobalVar(base_gv) => {
                        let base_addr = pos.ins().global_addr(addr_ty, base_gv);
                        let mut mflags = ir::MemFlags::new();
                        mflags.set_aligned();
                        mflags.set_notrap();
                        pos.ins().load(addr_ty, mflags, base_addr, 0)
                    }
                }.into();
            }
        }
    }
}

// Include legalization patterns that were generated by `gen_legalizer.py` from the `XForms` in
// `lib/cretonne/meta/base/legalize.py`.
//
// Concretely, this defines private functions `narrow()`, and `expand()`.
include!(concat!(env!("OUT_DIR"), "/legalizer.rs"));

/// Custom expansion for conditional trap instructions.
/// TODO: Add CFG support to the Python patterns so we won't have to do this.
fn expand_cond_trap(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &TargetIsa,
) {
    // Parse the instruction.
    let trapz;
    let (arg, code) = match func.dfg[inst] {
        ir::InstructionData::CondTrap { opcode, arg, code } => {
            // We want to branch *over* an unconditional trap.
            trapz = match opcode {
                ir::Opcode::Trapz => true,
                ir::Opcode::Trapnz => false,
                _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
            };
            (arg, code)
        }
        _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
    };

    // Split the EBB after `inst`:
    //
    //     trapnz arg
    //
    // Becomes:
    //
    //     brz arg, new_ebb
    //     trap
    //   new_ebb:
    //
    let old_ebb = func.layout.pp_ebb(inst);
    let new_ebb = func.dfg.make_ebb();
    if trapz {
        func.dfg.replace(inst).brnz(arg, new_ebb, &[]);
    } else {
        func.dfg.replace(inst).brz(arg, new_ebb, &[]);
    }

    let mut pos = FuncCursor::new(func).after_inst(inst);
    pos.use_srcloc(inst);
    pos.ins().trap(code);
    pos.insert_ebb(new_ebb);

    // Finally update the CFG.
    cfg.recompute_ebb(pos.func, old_ebb);
    cfg.recompute_ebb(pos.func, new_ebb);
}

/// Jump tables.
fn expand_br_table(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &TargetIsa,
) {
    use ir::condcodes::IntCC;

    let (arg, table) = match func.dfg[inst] {
        ir::InstructionData::BranchTable {
            opcode: ir::Opcode::BrTable,
            arg,
            table,
        } => (arg, table),
        _ => panic!("Expected br_table: {}", func.dfg.display_inst(inst, None)),
    };

    // This is a poor man's jump table using just a sequence of conditional branches.
    // TODO: Lower into a jump table load and indirect branch.
    let table_size = func.jump_tables[table].len();
    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    for i in 0..table_size {
        if let Some(dest) = pos.func.jump_tables[table].get_entry(i) {
            let t = pos.ins().icmp_imm(IntCC::Equal, arg, i as i64);
            pos.ins().brnz(t, dest, &[]);
        }
    }

    // `br_table` falls through when nothing matches.
    let ebb = pos.current_ebb().unwrap();
    pos.remove_inst();
    cfg.recompute_ebb(pos.func, ebb);
}

/// Expand the select instruction.
///
/// Conditional moves are available in some ISAs for some register classes. The remaining selects
/// are handled by a branch.
fn expand_select(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &TargetIsa,
) {
    let (ctrl, tval, fval) = match func.dfg[inst] {
        ir::InstructionData::Ternary {
            opcode: ir::Opcode::Select,
            args,
        } => (args[0], args[1], args[2]),
        _ => panic!("Expected select: {}", func.dfg.display_inst(inst, None)),
    };

    // Replace `result = select ctrl, tval, fval` with:
    //
    //   brnz ctrl, new_ebb(tval)
    //   jump new_ebb(fval)
    // new_ebb(result):
    let old_ebb = func.layout.pp_ebb(inst);
    let result = func.dfg.first_result(inst);
    func.dfg.clear_results(inst);
    let new_ebb = func.dfg.make_ebb();
    func.dfg.attach_ebb_param(new_ebb, result);

    func.dfg.replace(inst).brnz(ctrl, new_ebb, &[tval]);
    let mut pos = FuncCursor::new(func).after_inst(inst);
    pos.use_srcloc(inst);
    pos.ins().jump(new_ebb, &[fval]);
    pos.insert_ebb(new_ebb);

    cfg.recompute_ebb(pos.func, new_ebb);
    cfg.recompute_ebb(pos.func, old_ebb);
}


/// Expand illegal `f32const` and `f64const` instructions.
fn expand_fconst(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    _isa: &TargetIsa,
) {
    let ty = func.dfg.value_type(func.dfg.first_result(inst));
    debug_assert!(!ty.is_vector(), "Only scalar fconst supported: {}", ty);

    // In the future, we may want to generate constant pool entries for these constants, but for
    // now use an `iconst` and a bit cast.
    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);
    let ival = match pos.func.dfg[inst] {
        ir::InstructionData::UnaryIeee32 {
            opcode: ir::Opcode::F32const,
            imm,
        } => pos.ins().iconst(ir::types::I32, i64::from(imm.bits())),
        ir::InstructionData::UnaryIeee64 {
            opcode: ir::Opcode::F64const,
            imm,
        } => pos.ins().iconst(ir::types::I64, imm.bits() as i64),
        _ => panic!("Expected fconst: {}", pos.func.dfg.display_inst(inst, None)),
    };
    pos.func.dfg.replace(inst).bitcast(ty, ival);
}

/// Expand the stack check instruction.
pub fn expand_stack_check(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &TargetIsa,
) {
    use ir::condcodes::IntCC;

    let gv = match func.dfg[inst] {
        ir::InstructionData::UnaryGlobalVar { global_var, .. } => global_var,
        _ => panic!("Want stack_check: {}", func.dfg.display_inst(inst, isa)),
    };
    let ptr_ty = if isa.flags().is_64bit() {
        ir::types::I64
    } else {
        ir::types::I32
    };

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let limit_addr = pos.ins().global_addr(ptr_ty, gv);

    let mut mflags = ir::MemFlags::new();
    mflags.set_aligned();
    mflags.set_notrap();
    let limit = pos.ins().load(ptr_ty, mflags, limit_addr, 0);
    let cflags = pos.ins().ifcmp_sp(limit);
    pos.func.dfg.replace(inst).trapif(
        IntCC::UnsignedGreaterThanOrEqual,
        cflags,
        ir::TrapCode::StackOverflow,
    );
}
