//! Nesthost is a deterministic type-1 hypervisor simulator written in pure std.
//!
//! It runs several guest virtual machines on a small emulated CPU. It models the
//! three jobs a real virtual machine monitor performs:
//!
//! 1. vCPU scheduling. A single physical CPU is time sliced across guest vCPUs
//!    with a deterministic round robin.
//! 2. Memory virtualization. Each guest sees its own guest physical address
//!    space. A per guest second level page table maps guest physical addresses
//!    (GPA) to host physical addresses (HPA). A guest can only reach host frames
//!    mapped into its own address space.
//! 3. Trap and emulate. When a guest executes a privileged instruction the vCPU
//!    does not perform the effect. It takes a VM exit to the hypervisor, which
//!    emulates the operation and resumes the guest with a VM entry.
//!
//! This is a teaching accurate model, not a real VMM. There is no hardware, no
//! ring transition, and no host kernel. Every mechanism is a plain Rust data
//! structure so the behavior is easy to read and fully deterministic.

pub mod cpu;
pub mod device;
pub mod guest;
pub mod hypervisor;
pub mod mem;
pub mod rng;

pub use cpu::{ExitReason, FaultKind, Instr, MemAccess, StepResult, VCpu, CR_COUNT, NUM_REGS};
pub use device::{SharedRing, VirtualConsole, RING_DOORBELL_PORT};
pub use guest::{Guest, GuestState};
pub use hypervisor::{
    ExitRecord, Hypervisor, ScheduleTick, DEFAULT_MAX_ROUNDS, RING_MAX_KEEP,
};
pub use mem::{HostMemory, PageTable, Perm, PhysFault, PAGE_WORDS};
pub use rng::Rng;
