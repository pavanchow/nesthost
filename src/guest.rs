//! A guest virtual machine: one vCPU, one guest physical address space, one
//! program, and one console.
//!
//! A guest bundles everything private to a single VM. Its page table is the sole
//! description of which host frames it can reach, so two guests are isolated
//! exactly when their page tables map disjoint host frames.

use crate::cpu::VCpu;
use crate::device::VirtualConsole;
use crate::mem::PageTable;
use crate::cpu::Instr;

/// The lifecycle state of a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestState {
    /// Runnable and not yet halted or faulted.
    Running,
    /// Executed a halt or ran off the end of its program.
    Halted,
    /// Took an unrecoverable memory fault. The hypervisor stopped it.
    Faulted,
}

/// A single guest VM.
#[derive(Debug, Clone)]
pub struct Guest {
    pub id: u32,
    pub name: String,
    pub vcpu: VCpu,
    pub page_table: PageTable,
    pub program: Vec<Instr>,
    pub console: VirtualConsole,
    pub state: GuestState,
    /// VM exits taken by this guest, by category.
    pub io_exits: u64,
    pub cr_exits: u64,
    pub fault_exits: u64,
}

impl Guest {
    /// Create a runnable guest.
    #[must_use]
    pub fn new(id: u32, name: impl Into<String>, program: Vec<Instr>, page_table: PageTable) -> Self {
        Self {
            id,
            name: name.into(),
            vcpu: VCpu::new(),
            page_table,
            program,
            console: VirtualConsole::new(),
            state: GuestState::Running,
            io_exits: 0,
            cr_exits: 0,
            fault_exits: 0,
        }
    }

    /// Whether the scheduler should still give this guest time.
    #[must_use]
    pub fn is_runnable(&self) -> bool {
        self.state == GuestState::Running
    }
}
