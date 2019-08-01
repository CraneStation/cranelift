//! The `CFGPrinter` utility.

use core::fmt::{Display, Formatter, Result, Write};
use std::vec::Vec;

use crate::entity::SecondaryMap;
use crate::flowgraph::{BasicBlock, ControlFlowGraph};
use crate::ir::Function;
use crate::write::{FuncWriter, PlainWriter};

/// A utility for pretty-printing the CFG of a `Function`.
pub struct CFGPrinter<'a> {
    func: &'a Function,
    cfg: ControlFlowGraph,
}

/// A utility for pretty-printing the CFG of a `Function`.
impl<'a> CFGPrinter<'a> {
    /// Create a new CFGPrinter.
    pub fn new(func: &'a Function) -> Self {
        Self {
            func,
            cfg: ControlFlowGraph::with_function(func),
        }
    }

    /// Write the CFG for this function to `w`.
    pub fn write(&self, w: &mut dyn Write) -> Result {
        self.header(w)?;
        self.ebb_nodes(w)?;
        self.cfg_connections(w)?;
        writeln!(w, "}}")
    }

    fn header(&self, w: &mut dyn Write) -> Result {
        writeln!(w, "digraph \"{}\" {{", self.func.name)?;
        if let Some(entry) = self.func.layout.entry_block() {
            writeln!(w, "    {{rank=min; {}}}", entry)?;
        }
        Ok(())
    }

    fn ebb_nodes(&self, w: &mut dyn Write) -> Result {
        let mut aliases = SecondaryMap::<_, Vec<_>>::new();
        for v in self.func.dfg.values() {
            // VADFS returns the immediate target of an alias
            if let Some(k) = self.func.dfg.value_alias_dest_for_serialization(v) {
                aliases[k].push(v);
            }
        }

        for ebb in &self.func.layout {
            write!(w, "    {} [shape=record, label=\"{{", ebb)?;
            crate::write::write_ebb_header(w, self.func, None, ebb, 4)?;
            // Add all outgoing branch instructions to the label.
            for inst in self.func.layout.ebb_insts(ebb) {
                write!(w, " | <{}>", inst)?;
                PlainWriter.write_instruction(w, self.func, &aliases, None, inst, 0)?;
            }
            writeln!(w, "}}\"]")?
        }
        Ok(())
    }

    fn cfg_connections(&self, w: &mut dyn Write) -> Result {
        for ebb in &self.func.layout {
            for BasicBlock { ebb: parent, inst } in self.cfg.pred_iter(ebb) {
                writeln!(w, "    {}:{} -> {}", parent, inst, ebb)?;
            }
        }
        Ok(())
    }
}

impl<'a> Display for CFGPrinter<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result {
        self.write(f)
    }
}
