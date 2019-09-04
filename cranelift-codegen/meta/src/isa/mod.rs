use crate::cdsl::isa::TargetIsa;
use crate::shared::Definitions as SharedDefinitions;
use std::fmt;

mod arm32;
mod arm64;
mod riscv;
mod x86;

/// Represents known ISA target.
#[derive(Copy, Clone)]
pub enum Isa {
    Riscv,
    X86,
    Arm32,
    Arm64,
}

impl Isa {
    /// Creates isa target using name.
    pub fn from_name(name: &str) -> Option<Self> {
        Isa::all()
            .iter()
            .cloned()
            .filter(|isa| isa.to_string() == name)
            .next()
    }

    /// Creates isa target from arch.
    pub fn from_arch(arch: &str) -> Option<Self> {
        match arch {
            "riscv" => Some(Isa::Riscv),
            "aarch64" => Some(Isa::Arm64),
            x if ["x86_64", "i386", "i586", "i686"].contains(&x) => Some(Isa::X86),
            x if x.starts_with("arm") || arch.starts_with("thumb") => Some(Isa::Arm32),
            _ => None,
        }
    }

    /// Returns all supported isa targets.
    pub fn all() -> [Isa; 4] {
        [Isa::Riscv, Isa::X86, Isa::Arm32, Isa::Arm64]
    }
}

impl fmt::Display for Isa {
    // These names should be kept in sync with the crate features.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Isa::Riscv => write!(f, "riscv"),
            Isa::X86 => write!(f, "x86"),
            Isa::Arm32 => write!(f, "arm32"),
            Isa::Arm64 => write!(f, "arm64"),
        }
    }
}

pub(crate) fn define(isas: &Vec<Isa>, shared_defs: &mut SharedDefinitions) -> Vec<TargetIsa> {
    isas.iter()
        .map(|isa| match isa {
            Isa::Riscv => riscv::define(shared_defs),
            Isa::X86 => x86::define(shared_defs),
            Isa::Arm32 => arm32::define(shared_defs),
            Isa::Arm64 => arm64::define(shared_defs),
        })
        .collect()
}
