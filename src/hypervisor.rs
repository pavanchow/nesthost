//! The hypervisor: frame allocation, the vCPU scheduler, and the trap and
//! emulate loop.
//!
//! The hypervisor owns host physical memory and every guest. It hands host
//! frames to guests (keeping the handouts disjoint so guests stay isolated),
//! time slices the single physical CPU across guest vCPUs with a deterministic
//! round robin, and services every VM exit. It is the only component that
//! touches device or control register state, which is what makes trap and
//! emulate unbypassable.

use crate::cpu::{ExitReason, Instr, StepResult};
use crate::guest::{Guest, GuestState};
use crate::mem::{HostMemory, PageTable, Perm};

/// One serviced VM exit, recorded for inspection and for the determinism gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRecord {
    pub tick: u64,
    pub guest_id: u32,
    pub pc: usize,
    pub reason: ExitReason,
}

/// One scheduler decision: guest `guest_id` was entered for `quantum` and
/// retired `ran` instructions before the slice ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleTick {
    pub tick: u64,
    pub guest_id: u32,
    pub instructions_run: u64,
}

/// The hypervisor and the machine it runs.
#[derive(Debug)]
pub struct Hypervisor {
    pub host_mem: HostMemory,
    pub guests: Vec<Guest>,
    pub quantum: u64,
    pub exits: Vec<ExitRecord>,
    pub schedule: Vec<ScheduleTick>,
    tick: u64,
    vm_entries: u64,
}

impl Hypervisor {
    /// Create a hypervisor backed by `frame_count` host frames, using a
    /// round robin `quantum` of instructions per slice.
    ///
    /// # Panics
    /// Panics if `quantum` is zero.
    #[must_use]
    pub fn new(frame_count: u64, quantum: u64) -> Self {
        assert!(quantum > 0, "quantum must be non zero");
        Self {
            host_mem: HostMemory::new(frame_count),
            guests: Vec::new(),
            quantum,
            exits: Vec::new(),
            schedule: Vec::new(),
            tick: 0,
            vm_entries: 0,
        }
    }

    /// Create and register a guest whose guest physical space is `pages` pages,
    /// each backed by a freshly allocated (therefore disjoint) host frame with
    /// the given permission. Returns the new guest's id.
    ///
    /// Because every guest's frames come from the same monotonic allocator, no
    /// two guests ever share a host frame through this path, which is the
    /// mechanical root of inter guest isolation.
    pub fn add_guest(
        &mut self,
        name: impl Into<String>,
        program: Vec<Instr>,
        pages: u64,
        perm: Perm,
    ) -> u32 {
        let id = self.guests.len() as u32;
        let mut pt = PageTable::new();
        for gpn in 0..pages {
            let hfn = self.host_mem.alloc_frame();
            pt.map(gpn, hfn, perm);
        }
        self.guests.push(Guest::new(id, name, program, pt));
        id
    }

    /// Explicitly share host frame `hfn` into guest `guest_id`'s address space at
    /// guest page `gpn`. This is the only sanctioned way two guests can reach the
    /// same host frame, and it is opt in.
    ///
    /// # Panics
    /// Panics if `guest_id` is out of range.
    pub fn share_frame(&mut self, guest_id: u32, gpn: u64, hfn: u64, perm: Perm) {
        self.guests[guest_id as usize].page_table.map(gpn, hfn, perm);
    }

    /// Total VM entries performed (one per scheduler slice actually run).
    #[must_use]
    pub fn vm_entries(&self) -> u64 {
        self.vm_entries
    }

    /// Total VM exits recorded.
    #[must_use]
    pub fn vm_exits(&self) -> u64 {
        self.exits.len() as u64
    }

    /// Run one round robin slice for the guest at `index`, up to `quantum`
    /// instructions. Device IO and control register writes are emulated in place
    /// and the guest keeps running within the slice. A halt, end of program, or
    /// memory fault ends the guest.
    fn run_slice(&mut self, index: usize) {
        let guest = &mut self.guests[index];
        if !guest.is_runnable() {
            return;
        }
        self.vm_entries += 1;
        let entry_tick = self.tick;
        let mut ran = 0u64;

        while ran < self.quantum {
            let guest = &mut self.guests[index];
            let pc_before = guest.vcpu.pc;
            match guest.vcpu.step(&guest.program, &guest.page_table, &mut self.host_mem) {
                StepResult::Continue => ran += 1,
                StepResult::Exit(reason) => {
                    self.tick += 1;
                    let guest = &mut self.guests[index];
                    self.exits.push(ExitRecord {
                        tick: self.tick,
                        guest_id: guest.id,
                        pc: pc_before,
                        reason,
                    });
                    match reason {
                        ExitReason::DeviceIo { value, .. } => {
                            guest.console.write(value);
                            guest.io_exits += 1;
                        }
                        ExitReason::SetControlReg { cr, value } => {
                            guest.vcpu.cr[cr as usize % guest.vcpu.cr.len()] = value;
                            guest.cr_exits += 1;
                        }
                        ExitReason::Halt | ExitReason::EndOfProgram => {
                            guest.state = GuestState::Halted;
                            break;
                        }
                        ExitReason::MemFault { .. } => {
                            guest.fault_exits += 1;
                            guest.state = GuestState::Faulted;
                            break;
                        }
                    }
                }
            }
        }

        let guest_id = self.guests[index].id;
        self.schedule.push(ScheduleTick {
            tick: entry_tick,
            guest_id,
            instructions_run: ran,
        });
    }

    /// Run all guests to completion with the deterministic round robin. Returns
    /// the number of scheduler rounds performed.
    pub fn run(&mut self) -> u64 {
        let mut rounds = 0u64;
        loop {
            let mut any_runnable = false;
            for index in 0..self.guests.len() {
                if self.guests[index].is_runnable() {
                    any_runnable = true;
                    self.run_slice(index);
                }
            }
            if !any_runnable {
                break;
            }
            rounds += 1;
        }
        rounds
    }

    /// Borrow a guest by id.
    ///
    /// # Panics
    /// Panics if `id` is out of range.
    #[must_use]
    pub fn guest(&self, id: u32) -> &Guest {
        &self.guests[id as usize]
    }

    /// A compact printable memory map: every guest's guest page to host frame
    /// mappings with permissions.
    #[must_use]
    pub fn memory_map(&self) -> String {
        let mut out = String::new();
        for g in &self.guests {
            out.push_str(&format!("guest {} \"{}\" GPA -> HPA\n", g.id, g.name));
            for (gpn, hfn, perm) in g.page_table.iter() {
                let mode = match (perm.read, perm.write) {
                    (true, true) => "rw",
                    (true, false) => "ro",
                    _ => "--",
                };
                out.push_str(&format!(
                    "  gpn {gpn:>3} -> hfn {hfn:>3}  [{mode}]\n"
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Hypervisor;
    use crate::cpu::Instr;
    use crate::mem::Perm;

    fn print_prog(byte: u64) -> Vec<Instr> {
        vec![Instr::Movi(0, byte), Instr::Out(0, 0), Instr::Halt]
    }

    #[test]
    fn two_guests_have_disjoint_frames() {
        let mut hv = Hypervisor::new(16, 8);
        hv.add_guest("a", print_prog(65), 2, Perm::rw());
        hv.add_guest("b", print_prog(66), 2, Perm::rw());
        let a: std::collections::BTreeSet<u64> =
            hv.guest(0).page_table.mapped_frames().into_iter().collect();
        let b: std::collections::BTreeSet<u64> =
            hv.guest(1).page_table.mapped_frames().into_iter().collect();
        assert!(a.is_disjoint(&b));
    }

    #[test]
    fn run_produces_isolated_output_and_exits() {
        let mut hv = Hypervisor::new(16, 8);
        hv.add_guest("a", print_prog(u64::from(b'A')), 2, Perm::rw());
        hv.add_guest("b", print_prog(u64::from(b'B')), 2, Perm::rw());
        hv.run();
        assert_eq!(hv.guest(0).console.as_string(), "A");
        assert_eq!(hv.guest(1).console.as_string(), "B");
        // Each guest took exactly one device IO exit and one halt exit.
        assert_eq!(hv.guest(0).io_exits, 1);
        assert_eq!(hv.guest(1).io_exits, 1);
        assert!(hv.vm_exits() >= 4);
        assert!(hv.vm_entries() >= 2);
    }

    #[test]
    fn determinism_same_config_same_logs() {
        let build = || {
            let mut hv = Hypervisor::new(16, 3);
            hv.add_guest("a", print_prog(u64::from(b'A')), 2, Perm::rw());
            hv.add_guest("b", print_prog(u64::from(b'B')), 2, Perm::rw());
            hv.run();
            hv
        };
        let x = build();
        let y = build();
        assert_eq!(x.schedule, y.schedule);
        assert_eq!(x.exits, y.exits);
        assert_eq!(x.guest(0).console.bytes(), y.guest(0).console.bytes());
        assert_eq!(x.guest(1).console.bytes(), y.guest(1).console.bytes());
    }
}
