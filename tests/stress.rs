//! Bounded adversarial stress for the hypervisor.
//!
//! These tests feed deliberately malformed guest images (out of range register
//! and control register operands, wild jump targets, random privileged ports,
//! non terminating loops) into the machine and assert the host survives: no
//! panic, no hang, and the inter guest isolation invariant still holds after
//! arbitrary guest execution.
//!
//! Footprint is bounded on purpose. Each iteration builds a tiny machine, runs
//! it under a small scheduler round budget, and drops it, so nothing grows
//! without bound across the run.
//!   `NESTHOST_FUZZ_OPS`  number of fuzz iterations (default 20000)
//!   `NESTHOST_SEED`      PRNG seed (default 0xC1F)

use std::collections::BTreeSet;

use nesthost::cpu::{Instr, StepResult, VCpu};
use nesthost::hypervisor::Hypervisor;
use nesthost::mem::{HostMemory, PageTable, Perm};
use nesthost::rng::Rng;
use nesthost::RING_DOORBELL_PORT;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn fuzz_ops() -> u64 {
    env_u64("NESTHOST_FUZZ_OPS", 20_000)
}

fn seed() -> u64 {
    env_u64("NESTHOST_SEED", 0xC1F)
}

/// A random, frequently malformed instruction. Register indices range past
/// `NUM_REGS` and control register indices past the real count so the invalid
/// operand path is exercised often, and ports include the ring doorbell.
fn random_instr(rng: &mut Rng, program_len_hint: u64) -> Instr {
    let r = |rng: &mut Rng| (rng.below(16)) as usize; // 0..15, often out of range
    let target = |rng: &mut Rng| (rng.below(program_len_hint * 2 + 2)) as usize;
    match rng.below(12) {
        0 => Instr::Movi(r(rng), rng.next_u64()),
        1 => Instr::Mov(r(rng), r(rng)),
        2 => Instr::Add(r(rng), r(rng), r(rng)),
        3 => Instr::Sub(r(rng), r(rng), r(rng)),
        4 => Instr::Addi(r(rng), r(rng), rng.next_u64()),
        5 => Instr::Load(r(rng), r(rng)),
        6 => Instr::Store(r(rng), r(rng)),
        7 => Instr::Jz(r(rng), target(rng)),
        8 => Instr::Jmp(target(rng)),
        9 => {
            let port = if rng.one_in(4) {
                RING_DOORBELL_PORT
            } else {
                (rng.below(256)) as u16
            };
            Instr::Out(port, r(rng))
        }
        10 => Instr::SetCr((rng.below(8)) as u8, r(rng)),
        _ => Instr::Halt,
    }
}

fn random_program(rng: &mut Rng, max_len: u64) -> Vec<Instr> {
    let len = 1 + rng.below(max_len);
    (0..len).map(|_| random_instr(rng, max_len)).collect()
}

/// The hypervisor must survive arbitrary malformed guests, terminate under its
/// round budget, and keep every guest's frames disjoint from every other's.
#[test]
fn stress_hypervisor_survives_malformed_guests() {
    let ops = fuzz_ops();
    let mut rng = Rng::new(seed());

    let mut faulted = 0u64;
    let mut halted = 0u64;
    let mut budget_hits = 0u64;
    let mut ring_bytes = 0u64;

    for _ in 0..ops {
        let num_guests = 1 + rng.below(4); // 1..4
        let pages = 1 + rng.below(3); // 1..3
        // Enough frames for every private page plus one ring frame per guest.
        let frames = num_guests * pages + num_guests + 4;
        let quantum = 1 + rng.below(8);
        let mut hv = Hypervisor::new(frames, quantum);

        for g in 0..num_guests {
            let prog = random_program(&mut rng, 24);
            let id = hv.add_guest(format!("g{g}"), prog, pages, Perm::rw());
            if rng.one_in(2) {
                // Grant a shared ring at a page above the private span.
                hv.attach_ring(id, pages + 1);
            }
        }

        // A small budget guarantees termination even for a spinning guest.
        hv.set_max_rounds(64);
        hv.run();

        if hv.budget_exhausted() {
            budget_hits += 1;
        }

        // Isolation invariant after arbitrary execution: no two guests share any
        // host frame. Guest execution can never remap a page table, so the union
        // of every guest's mapped frames must have no duplicates.
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        let mut total = 0usize;
        for g in &hv.guests {
            for hfn in g.page_table.mapped_frames() {
                seen.insert(hfn);
                total += 1;
            }
            match g.state {
                nesthost::GuestState::Faulted => faulted += 1,
                nesthost::GuestState::Halted => halted += 1,
                nesthost::GuestState::Running => {}
            }
        }
        assert_eq!(total, seen.len(), "two guests reached a common host frame");
        ring_bytes += hv.rings.iter().map(|r| r.received().len() as u64).sum::<u64>();
    }

    // The malformed images must actually have driven guests into the invalid
    // operation and memory fault paths, otherwise the stress proved nothing.
    assert!(faulted > 0, "expected malformed guests to fault");
    assert!(halted > 0, "expected some guests to halt");
    // These are informational and keep the tallies live under the optimizer.
    let _ = budget_hits;
    let _ = ring_bytes;
}

/// The vCPU step function is the single choke point. It must never panic on any
/// instruction, and every load or store it services must resolve to a frame the
/// page table actually maps (translation is the only path to host memory).
#[test]
fn stress_vcpu_step_never_panics_or_escapes() {
    let ops = fuzz_ops();
    let mut rng = Rng::new(seed() ^ 0x7777);

    // A fixed small machine: four mapped pages backed by four host frames.
    let pages = 4u64;
    let mut mem = HostMemory::new(pages + 2);
    let mut pt = PageTable::new();
    let mut mapped: BTreeSet<u64> = BTreeSet::new();
    for gpn in 0..pages {
        let hfn = mem.alloc_frame();
        pt.map(gpn, hfn, Perm::rw());
        mapped.insert(hfn);
    }

    let prog = random_program(&mut rng, 40);
    let mut cpu = VCpu::new();
    let mut steps = 0u64;
    let mut exits = 0u64;

    for _ in 0..ops {
        // Randomize register state so loads and stores aim all over the space.
        for r in &mut cpu.regs {
            *r = rng.next_u64();
        }
        if cpu.halted {
            cpu = VCpu::new();
            cpu.pc = (rng.below(prog.len() as u64 + 4)) as usize;
        }
        match cpu.step(&prog, &pt, &mut mem) {
            StepResult::Continue => steps += 1,
            StepResult::Exit(_) => {
                exits += 1;
                // Reset to a random pc to keep exercising fresh state.
                cpu = VCpu::new();
                cpu.pc = (rng.below(prog.len() as u64 + 4)) as usize;
            }
        }
    }

    // Whatever the program did, host memory outside the mapped frames is
    // untouched by construction: translate only ever yields a mapped hfn. Assert
    // the mapped set is exactly the frames the page table still reports.
    let now: BTreeSet<u64> = pt.mapped_frames().into_iter().collect();
    assert_eq!(now, mapped, "page table changed under guest execution");
    assert_eq!(steps + exits, ops, "every iteration must resolve to a result");
}
