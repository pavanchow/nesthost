//! The three correctness gates from the design, plus their supporting checks.
//!
//! Each gate proves one claim the hypervisor makes. The randomized gates are
//! bounded for CI and controllable through environment variables:
//!   NESTHOST_FUZZ_OPS  number of randomized operations (default 4000)
//!   NESTHOST_SEED      PRNG seed (default 0xC1F)

use std::collections::{BTreeMap, BTreeSet};

use nesthost::cpu::{ExitReason, Instr, StepResult, VCpu};
use nesthost::hypervisor::Hypervisor;
use nesthost::mem::{HostMemory, PageTable, Perm, PhysFault, PAGE_WORDS};
use nesthost::rng::Rng;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn fuzz_ops() -> u64 {
    env_u64("NESTHOST_FUZZ_OPS", 4000)
}

fn seed() -> u64 {
    env_u64("NESTHOST_SEED", 0xC1F)
}

/// Build a small machine of `num_guests` guests, each owning `pages` pages backed
/// by disjoint host frames. Returns the host memory, the per guest page tables,
/// and the sentinel value written into every word of each guest's memory.
fn build_isolated(
    num_guests: u32,
    pages: u64,
) -> (HostMemory, Vec<PageTable>, Vec<u64>, BTreeMap<u64, u32>) {
    let mut mem = HostMemory::new(u64::from(num_guests) * pages + 4);
    let mut tables = Vec::new();
    let mut sentinels = Vec::new();
    let mut owner: BTreeMap<u64, u32> = BTreeMap::new();

    for g in 0..num_guests {
        let mut pt = PageTable::new();
        let sentinel = (u64::from(g) + 1).wrapping_mul(0x1111_1111_0000_0000) | 0xABCD;
        for gpn in 0..pages {
            let hfn = mem.alloc_frame();
            owner.insert(hfn, g);
            pt.map(gpn, hfn, Perm::rw());
            for off in 0..PAGE_WORDS {
                mem.write_word(hfn, off, sentinel);
            }
        }
        tables.push(pt);
        sentinels.push(sentinel);
    }
    (mem, tables, sentinels, owner)
}

/// Gate 1: inter guest memory isolation.
///
/// Over many randomized guest physical accesses, a guest can only ever reach a
/// host frame mapped into its own address space. It never reads or writes
/// another guest's frame, an in range read always returns that guest's own
/// sentinel, and an access to an unmapped page faults instead of leaking.
#[test]
fn gate1_inter_guest_isolation() {
    let num_guests = 4u32;
    let pages = 3u64;
    let (mut mem, tables, sentinels, owner) = build_isolated(num_guests, pages);

    // The security invariant at rest: no two guests share a host frame.
    let mut all: Vec<u64> = Vec::new();
    for pt in &tables {
        all.extend(pt.mapped_frames());
    }
    let unique: BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "guest frames must be pairwise disjoint");

    let ops = fuzz_ops();
    let mut rng = Rng::new(seed());
    let mut faults = 0u64;
    let mut hits = 0u64;
    let mapped_span = pages * PAGE_WORDS;

    for _ in 0..ops {
        let g = (rng.below(u64::from(num_guests))) as usize;
        // Half the time aim inside the mapped span, half the time well outside
        // it so the unmapped path gets exercised too.
        let gpa = if rng.one_in(2) {
            rng.below(mapped_span)
        } else {
            mapped_span + rng.below(mapped_span * 8)
        };
        let write = rng.one_in(2);

        match tables[g].translate(gpa, write) {
            Ok((hfn, off)) => {
                hits += 1;
                // The reached frame must belong to this guest and no other.
                assert_eq!(
                    owner.get(&hfn),
                    Some(&(g as u32)),
                    "guest {g} reached host frame {hfn} it does not own"
                );
                if write {
                    // Writing keeps the sentinel identity so later reads still
                    // prove ownership.
                    mem.write_word(hfn, off, sentinels[g]);
                } else {
                    assert_eq!(
                        mem.read_word(hfn, off),
                        sentinels[g],
                        "guest {g} read a value that is not its own sentinel"
                    );
                }
            }
            Err(PhysFault::Unmapped) => faults += 1,
            Err(PhysFault::Protection) => {
                panic!("no read only pages in this gate, protection fault impossible")
            }
        }
    }

    // With a 50/50 in/out split and many ops both paths must have fired, so the
    // gate actually tested isolation and faulting rather than one branch.
    assert!(hits > 0, "expected some in range accesses");
    assert!(faults > 0, "expected some out of range accesses to fault");
}

/// Gate 2: trap and emulate correctness, and that the trap cannot be bypassed.
///
/// A privileged instruction always takes a VM exit, never mutating device or
/// control state in guest context. The hypervisor then emulates it correctly and
/// resumes the guest with the expected state.
#[test]
fn gate2_trap_and_emulate() {
    let ops = fuzz_ops();
    let mut rng = Rng::new(seed() ^ 0x5555);

    // Part A: at the CPU level, a privileged instruction always exits with the
    // decoded operands and never runs in guest context.
    for _ in 0..ops {
        let value = rng.next_u64();
        let mut mem = HostMemory::new(1);
        let hfn = mem.alloc_frame();
        let mut pt = PageTable::new();
        pt.map(0, hfn, Perm::rw());

        let use_out = rng.one_in(2);
        let prog = if use_out {
            vec![Instr::Movi(0, value), Instr::Out(7, 0)]
        } else {
            vec![Instr::Movi(0, value), Instr::SetCr(1, 0)]
        };
        let mut cpu = VCpu::new();
        assert_eq!(cpu.step(&prog, &pt, &mut mem), StepResult::Continue);
        let cr_before = cpu.cr;
        let result = cpu.step(&prog, &pt, &mut mem);
        // The privileged step never mutates the shadow control registers in
        // guest context: only the hypervisor does, on the exit path.
        assert_eq!(cpu.cr, cr_before, "guest context must not change cr state");
        match (use_out, result) {
            (true, StepResult::Exit(ExitReason::DeviceIo { port, value: v })) => {
                assert_eq!(port, 7);
                assert_eq!(v, value);
            }
            (false, StepResult::Exit(ExitReason::SetControlReg { cr, value: v })) => {
                assert_eq!(cr, 1);
                assert_eq!(v, value);
            }
            other => panic!("privileged instruction did not trap correctly: {other:?}"),
        }
    }

    // Part B: end to end through the hypervisor, the emulated effect lands and
    // the guest resumes. A guest that writes a byte then halts must show exactly
    // that byte on its console, delivered only via the exit handler.
    let mut hv = Hypervisor::new(4, 8);
    hv.add_guest(
        "io",
        vec![
            Instr::Movi(0, u64::from(b'Z')),
            Instr::Out(0, 0),
            Instr::Movi(1, 9),
            Instr::SetCr(2, 1),
            Instr::Halt,
        ],
        1,
        Perm::rw(),
    );
    hv.run();
    let g = hv.guest(0);
    assert_eq!(g.console.as_string(), "Z");
    assert_eq!(g.io_exits, 1);
    assert_eq!(g.cr_exits, 1);
    assert_eq!(g.vcpu.cr[2], 9, "hypervisor applied the emulated SetCr");
    // At least the IO exit, the CR exit, and the halt exit were recorded.
    assert!(hv.vm_exits() >= 3);
}

/// A guest cannot smuggle a device write past the trap. Every device IO in the
/// program surfaces as exactly one DeviceIo exit, and no Continue step is ever a
/// privileged instruction, so nothing executes IO in guest context.
#[test]
fn gate2_trap_cannot_be_bypassed() {
    let mut mem = HostMemory::new(1);
    let hfn = mem.alloc_frame();
    let mut pt = PageTable::new();
    pt.map(0, hfn, Perm::rw());

    let prog = vec![
        Instr::Movi(0, 1),
        Instr::Out(0, 0),
        Instr::Addi(0, 0, 1),
        Instr::Out(0, 0),
        Instr::Halt,
    ];
    let out_count = prog.iter().filter(|i| matches!(i, Instr::Out(..))).count();

    let mut cpu = VCpu::new();
    let mut io_exits = 0usize;
    loop {
        let pc = cpu.pc;
        match cpu.step(&prog, &pt, &mut mem) {
            StepResult::Continue => {
                // A Continue is only ever returned for a non privileged
                // instruction, so no privileged effect happened here.
                assert!(!prog[pc].is_privileged());
            }
            StepResult::Exit(ExitReason::DeviceIo { .. }) => io_exits += 1,
            StepResult::Exit(ExitReason::Halt | ExitReason::EndOfProgram) => break,
            StepResult::Exit(_) => {}
        }
    }
    assert_eq!(io_exits, out_count, "every device write must trap exactly once");
}

/// Gate 3: multi guest execution and determinism.
///
/// Two guests each running a small print program produce their correct, isolated
/// outputs while time sliced, and the entire run is reproducible: same
/// configuration, identical schedule, exits, and outputs.
#[test]
fn gate3_multi_guest_determinism() {
    fn print_program(text: &str) -> Vec<Instr> {
        let mut prog = Vec::new();
        for (i, byte) in text.bytes().enumerate() {
            prog.push(Instr::Movi(0, u64::from(byte)));
            prog.push(Instr::Movi(1, i as u64));
            prog.push(Instr::Store(1, 0));
        }
        prog.push(Instr::Movi(1, 0));
        let loop_start = prog.len();
        prog.push(Instr::Load(0, 1));
        let jz_index = prog.len();
        prog.push(Instr::Jz(0, 0));
        prog.push(Instr::Out(0, 0));
        prog.push(Instr::Addi(1, 1, 1));
        prog.push(Instr::Jmp(loop_start));
        let end = prog.len();
        prog.push(Instr::Halt);
        prog[jz_index] = Instr::Jz(0, end);
        prog
    }

    let build = || {
        let mut hv = Hypervisor::new(16, 5);
        hv.add_guest("alpha", print_program("alpha"), 2, Perm::rw());
        hv.add_guest("bravo", print_program("bravo"), 2, Perm::rw());
        hv.run();
        hv
    };

    let a = build();
    let b = build();

    // Correct, isolated outputs.
    assert_eq!(a.guest(0).console.as_string(), "alpha");
    assert_eq!(a.guest(1).console.as_string(), "bravo");

    // Fully deterministic across identical runs.
    assert_eq!(a.schedule, b.schedule, "schedule must be deterministic");
    assert_eq!(a.exits, b.exits, "exit log must be deterministic");
    assert_eq!(a.guest(0).console.bytes(), b.guest(0).console.bytes());
    assert_eq!(a.guest(1).console.bytes(), b.guest(1).console.bytes());

    // The two guests were actually interleaved (more than one slice each).
    let alpha_slices = a.schedule.iter().filter(|s| s.guest_id == 0).count();
    let bravo_slices = a.schedule.iter().filter(|s| s.guest_id == 1).count();
    assert!(alpha_slices >= 2 && bravo_slices >= 2, "guests must time slice");
}
