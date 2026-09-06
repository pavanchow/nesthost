//! The emulated guest CPU and its instruction set.
//!
//! The ISA is deliberately small: eight general registers, a program counter,
//! word sized memory, simple arithmetic and branches, a halt, and two
//! privileged instructions. A guest is never allowed to perform a privileged
//! operation itself. When the vCPU decodes one it stops and returns a
//! [`StepResult::Exit`] carrying the decoded request, which is a VM exit. The
//! hypervisor emulates the request and performs a VM entry to resume the guest.
//!
//! Instructions live in a separate program vector indexed by the program counter
//! (a Harvard split). Data lives in the guest's virtualized physical address
//! space and is reached only through second level translation, so every load and
//! store is subject to the isolation checks in [`crate::mem`].

use crate::mem::{HostMemory, PageTable, PhysFault};

/// Number of general purpose registers.
pub const NUM_REGS: usize = 8;

/// Number of shadow control registers.
pub const CR_COUNT: usize = 4;

/// A register index, `r0` through `r7`.
pub type Reg = usize;

/// The guest instruction set.
///
/// Register operands are indices in `0..NUM_REGS`. Jump targets are program
/// counter values (instruction indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    /// `rd = imm`.
    Movi(Reg, u64),
    /// `rd = rs`.
    Mov(Reg, Reg),
    /// `rd = rs + rt` (wrapping).
    Add(Reg, Reg, Reg),
    /// `rd = rs - rt` (wrapping).
    Sub(Reg, Reg, Reg),
    /// `rd = rs + imm` (wrapping).
    Addi(Reg, Reg, u64),
    /// `rd = mem[rs]`, a guest physical load through the SLAT.
    Load(Reg, Reg),
    /// `mem[rd] = rs`, a guest physical store through the SLAT.
    Store(Reg, Reg),
    /// `if rs == 0 { pc = target }`.
    Jz(Reg, usize),
    /// `pc = target`.
    Jmp(usize),
    /// Privileged. Device IO write of `rs` to `port`. Traps to the hypervisor.
    Out(u16, Reg),
    /// Privileged. Set virtual control register `cr` to `rs`. Traps to the
    /// hypervisor.
    SetCr(u8, Reg),
    /// Stop this vCPU.
    Halt,
}

impl Instr {
    /// Whether this instruction is privileged and must trap rather than execute
    /// in guest context.
    #[must_use]
    pub fn is_privileged(self) -> bool {
        matches!(self, Instr::Out(..) | Instr::SetCr(..))
    }

    /// Whether every register and control register operand this instruction
    /// names is architecturally in range. A malformed guest image that encodes
    /// an out of range operand must not be able to index host state, so the vCPU
    /// treats it as an invalid operation fault rather than executing it.
    #[must_use]
    pub fn operands_in_range(self) -> bool {
        let reg_ok = |r: Reg| r < NUM_REGS;
        match self {
            Instr::Movi(rd, _) => reg_ok(rd),
            Instr::Mov(rd, rs)
            | Instr::Addi(rd, rs, _)
            | Instr::Load(rd, rs)
            | Instr::Store(rd, rs) => reg_ok(rd) && reg_ok(rs),
            Instr::Add(rd, rs, rt) | Instr::Sub(rd, rs, rt) => {
                reg_ok(rd) && reg_ok(rs) && reg_ok(rt)
            }
            Instr::Jz(rs, _) | Instr::Out(_, rs) => reg_ok(rs),
            Instr::SetCr(cr, rs) => (cr as usize) < CR_COUNT && reg_ok(rs),
            Instr::Jmp(_) | Instr::Halt => true,
        }
    }
}

/// The direction of a faulting memory access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemAccess {
    Read,
    Write,
}

/// The kind of a guest memory fault, mirroring [`PhysFault`] plus the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    Unmapped,
    Protection,
}

impl From<PhysFault> for FaultKind {
    fn from(f: PhysFault) -> Self {
        match f {
            PhysFault::Unmapped => FaultKind::Unmapped,
            PhysFault::Protection => FaultKind::Protection,
        }
    }
}

/// Why a vCPU stopped running and returned control to the hypervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest executed [`Instr::Halt`].
    Halt,
    /// The guest ran off the end of its program (no more instructions).
    EndOfProgram,
    /// A privileged device IO write. Carries the decoded `port` and `value`.
    DeviceIo { port: u16, value: u64 },
    /// A privileged control register write. Carries the decoded `cr` and value.
    SetControlReg { cr: u8, value: u64 },
    /// A guest physical memory access could not be completed.
    MemFault { gpa: u64, access: MemAccess, kind: FaultKind },
    /// The guest tried to execute an instruction whose operands are out of
    /// architectural range (a malformed guest image). The equivalent of an
    /// invalid opcode fault. Nothing is executed in guest context.
    InvalidOp { pc: usize },
}

/// The result of stepping a vCPU once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// The instruction executed in guest context. Keep running.
    Continue,
    /// The vCPU took a VM exit. Control belongs to the hypervisor.
    Exit(ExitReason),
}

/// The architectural state of one virtual CPU.
#[derive(Debug, Clone)]
pub struct VCpu {
    pub regs: [u64; NUM_REGS],
    pub pc: usize,
    pub halted: bool,
    /// Shadow control registers, written only by the hypervisor when emulating a
    /// [`Instr::SetCr`] exit. The guest can never write these directly.
    pub cr: [u64; CR_COUNT],
    /// Count of instructions retired in guest context (excludes trapped ones).
    pub retired: u64,
}

impl Default for VCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl VCpu {
    /// A freshly reset vCPU.
    #[must_use]
    pub fn new() -> Self {
        Self {
            regs: [0; NUM_REGS],
            pc: 0,
            halted: false,
            cr: [0; CR_COUNT],
            retired: 0,
        }
    }

    /// Execute a single instruction of `program`.
    ///
    /// Non privileged instructions execute in guest context and return
    /// [`StepResult::Continue`]. Privileged instructions, halts, end of program,
    /// and memory faults do not change device or control state here: they return
    /// [`StepResult::Exit`] so the hypervisor can decide what happens. This is
    /// the single choke point that makes trap and emulate unbypassable, because
    /// the guest CPU literally has no code path that performs a privileged
    /// effect.
    ///
    /// Memory operands are translated through `page_table` and serviced from
    /// `host_mem`, so a guest can only ever touch host frames its own page table
    /// maps.
    pub fn step(
        &mut self,
        program: &[Instr],
        page_table: &PageTable,
        host_mem: &mut HostMemory,
    ) -> StepResult {
        if self.halted {
            return StepResult::Exit(ExitReason::Halt);
        }
        let Some(&instr) = program.get(self.pc) else {
            self.halted = true;
            return StepResult::Exit(ExitReason::EndOfProgram);
        };

        // A malformed instruction is rejected before any operand is used, so an
        // out of range register or control register index can never index host
        // state. pc is left on the faulting instruction, as for a memory fault.
        if !instr.operands_in_range() {
            return StepResult::Exit(ExitReason::InvalidOp { pc: self.pc });
        }

        // A privileged instruction never executes in guest context. Leave the pc
        // pointing at the next instruction so the hypervisor resumes cleanly, and
        // hand the decoded request up as a VM exit.
        if instr.is_privileged() {
            self.pc += 1;
            return match instr {
                Instr::Out(port, rs) => {
                    StepResult::Exit(ExitReason::DeviceIo { port, value: self.regs[rs] })
                }
                Instr::SetCr(cr, rs) => {
                    StepResult::Exit(ExitReason::SetControlReg { cr, value: self.regs[rs] })
                }
                _ => unreachable!("is_privileged covers all privileged variants"),
            };
        }

        self.pc += 1;
        match instr {
            Instr::Movi(rd, imm) => self.regs[rd] = imm,
            Instr::Mov(rd, rs) => self.regs[rd] = self.regs[rs],
            Instr::Add(rd, rs, rt) => {
                self.regs[rd] = self.regs[rs].wrapping_add(self.regs[rt]);
            }
            Instr::Sub(rd, rs, rt) => {
                self.regs[rd] = self.regs[rs].wrapping_sub(self.regs[rt]);
            }
            Instr::Addi(rd, rs, imm) => {
                self.regs[rd] = self.regs[rs].wrapping_add(imm);
            }
            Instr::Load(rd, rs) => {
                let gpa = self.regs[rs];
                match page_table.translate(gpa, false) {
                    Ok((hfn, off)) => self.regs[rd] = host_mem.read_word(hfn, off),
                    Err(f) => {
                        // Undo the pc advance so the fault is reported at the
                        // faulting instruction, then exit.
                        self.pc -= 1;
                        return StepResult::Exit(ExitReason::MemFault {
                            gpa,
                            access: MemAccess::Read,
                            kind: f.into(),
                        });
                    }
                }
            }
            Instr::Store(rd, rs) => {
                let gpa = self.regs[rd];
                match page_table.translate(gpa, true) {
                    Ok((hfn, off)) => host_mem.write_word(hfn, off, self.regs[rs]),
                    Err(f) => {
                        self.pc -= 1;
                        return StepResult::Exit(ExitReason::MemFault {
                            gpa,
                            access: MemAccess::Write,
                            kind: f.into(),
                        });
                    }
                }
            }
            Instr::Jz(rs, target) => {
                if self.regs[rs] == 0 {
                    self.pc = target;
                }
            }
            Instr::Jmp(target) => self.pc = target,
            Instr::Halt => {
                self.pc -= 1;
                self.halted = true;
                return StepResult::Exit(ExitReason::Halt);
            }
            Instr::Out(..) | Instr::SetCr(..) => unreachable!("handled above"),
        }
        self.retired += 1;
        StepResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::{ExitReason, Instr, MemAccess, StepResult, VCpu};
    use crate::mem::{HostMemory, PageTable, Perm};

    fn one_frame_pt(mem: &mut HostMemory) -> PageTable {
        let hfn = mem.alloc_frame();
        let mut pt = PageTable::new();
        pt.map(0, hfn, Perm::rw());
        pt
    }

    #[test]
    fn arithmetic_semantics() {
        let mut mem = HostMemory::new(1);
        let pt = one_frame_pt(&mut mem);
        let prog = [
            Instr::Movi(0, 5),
            Instr::Movi(1, 7),
            Instr::Add(2, 0, 1),
            Instr::Sub(3, 1, 0),
            Instr::Addi(4, 2, 100),
        ];
        let mut cpu = VCpu::new();
        for _ in 0..prog.len() {
            assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Continue);
        }
        assert_eq!(cpu.regs[2], 12);
        assert_eq!(cpu.regs[3], 2);
        assert_eq!(cpu.regs[4], 112);
        assert_eq!(cpu.retired, 5);
    }

    #[test]
    fn load_store_round_trip() {
        let mut mem = HostMemory::new(1);
        let pt = one_frame_pt(&mut mem);
        let prog = [
            Instr::Movi(0, 0xABCD), // value
            Instr::Movi(1, 4),      // gpa
            Instr::Store(1, 0),     // mem[4] = r0
            Instr::Movi(0, 0),      // clobber
            Instr::Load(2, 1),      // r2 = mem[4]
        ];
        let mut cpu = VCpu::new();
        for _ in 0..prog.len() {
            assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Continue);
        }
        assert_eq!(cpu.regs[2], 0xABCD);
    }

    #[test]
    fn privileged_out_traps_without_effect() {
        let mut mem = HostMemory::new(1);
        let pt = one_frame_pt(&mut mem);
        let prog = [Instr::Movi(0, 65), Instr::Out(1, 0)];
        let mut cpu = VCpu::new();
        assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Continue);
        let r = cpu.step(&prog, &pt, &mut mem);
        assert_eq!(r, StepResult::Exit(ExitReason::DeviceIo { port: 1, value: 65 }));
        // The trap advanced pc past the Out so the hypervisor resumes after it,
        // and no retired count was charged for the trapped instruction.
        assert_eq!(cpu.pc, 2);
        assert_eq!(cpu.retired, 1);
    }

    #[test]
    fn unmapped_access_faults_at_instruction() {
        let mut mem = HostMemory::new(1);
        let pt = one_frame_pt(&mut mem);
        // gpa 100 lives in an unmapped page.
        let prog = [Instr::Movi(0, 100), Instr::Load(1, 0)];
        let mut cpu = VCpu::new();
        assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Continue);
        let r = cpu.step(&prog, &pt, &mut mem);
        match r {
            StepResult::Exit(ExitReason::MemFault { gpa, access, .. }) => {
                assert_eq!(gpa, 100);
                assert_eq!(access, MemAccess::Read);
            }
            other => panic!("expected mem fault, got {other:?}"),
        }
        // pc stayed on the faulting Load.
        assert_eq!(cpu.pc, 1);
    }

    #[test]
    fn halt_exits_and_sticks() {
        let mut mem = HostMemory::new(1);
        let pt = one_frame_pt(&mut mem);
        let prog = [Instr::Halt];
        let mut cpu = VCpu::new();
        assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Exit(ExitReason::Halt));
        assert!(cpu.halted);
        assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Exit(ExitReason::Halt));
    }
}
