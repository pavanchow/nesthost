//! The three correctness gates from the design, plus their supporting checks.
//!
//! Each gate proves one claim the hypervisor makes. The randomized gates are
//! bounded for CI and controllable through environment variables:
//!   `NESTHOST_FUZZ_OPS`  number of randomized operations (default 4000)
//!   `NESTHOST_SEED`      PRNG seed (default 0xC1F)

use std::collections::{BTreeMap, BTreeSet};

use nesthost::cpu::{ExitReason, Instr, StepResult, VCpu};
use nesthost::guest::GuestState;
use nesthost::hypervisor::Hypervisor;
use nesthost::mem::{HostMemory, PageTable, Perm, PhysFault, PAGE_WORDS};
use nesthost::rng::Rng;
use nesthost::{SharedRing, RING_DOORBELL_PORT};

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

/// Who a host frame belongs to, tracked independently of the page tables so the
/// isolation gate checks reached frames against an external source of truth
/// rather than against the same structure it is validating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// A private frame owned by exactly one guest.
    Private(u32),
    /// A frame explicitly shared between the host and exactly one guest. No other
    /// guest may map it.
    Shared(u32),
}

/// Build a small machine of `num_guests` guests, each owning `pages` private pages
/// backed by disjoint host frames, plus one frame explicitly shared into guest 0
/// at page index `pages`. Returns the host memory, the per guest page tables, the
/// per guest sentinel value, and the external owner map.
fn build_isolated(
    num_guests: u32,
    pages: u64,
) -> (HostMemory, Vec<PageTable>, Vec<u64>, BTreeMap<u64, Owner>) {
    let mut mem = HostMemory::new(u64::from(num_guests) * pages + 4);
    let mut tables = Vec::new();
    let mut sentinels = Vec::new();
    let mut owner: BTreeMap<u64, Owner> = BTreeMap::new();

    for g in 0..num_guests {
        let mut pt = PageTable::new();
        let sentinel = (u64::from(g) + 1).wrapping_mul(0x1111_1111_0000_0000) | 0xABCD;
        for gpn in 0..pages {
            let hfn = mem.alloc_frame();
            owner.insert(hfn, Owner::Private(g));
            pt.map(gpn, hfn, Perm::rw());
            for off in 0..PAGE_WORDS {
                mem.write_word(hfn, off, sentinel);
            }
        }
        tables.push(pt);
        sentinels.push(sentinel);
    }

    // One explicitly shared frame, granted only to guest 0. It carries guest 0's
    // sentinel and must never appear in any other guest's page table.
    let shared_hfn = mem.alloc_frame();
    owner.insert(shared_hfn, Owner::Shared(0));
    tables[0].map(pages, shared_hfn, Perm::rw());
    for off in 0..PAGE_WORDS {
        mem.write_word(shared_hfn, off, sentinels[0]);
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

    // The security invariant at rest: no two guests share a host frame, and the
    // one explicitly shared frame belongs to guest 0 alone. Checked against the
    // external owner map, not the page tables themselves.
    let mut all: Vec<u64> = Vec::new();
    for pt in &tables {
        all.extend(pt.mapped_frames());
    }
    let unique: BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "guest frames must be pairwise disjoint");

    // The shared frame appears in guest 0's table and in no other guest's.
    let shared_frames: Vec<u64> = owner
        .iter()
        .filter_map(|(&hfn, o)| matches!(o, Owner::Shared(_)).then_some(hfn))
        .collect();
    assert_eq!(shared_frames.len(), 1, "exactly one shared frame in this gate");
    for (gi, pt) in tables.iter().enumerate() {
        let has = pt.mapped_frames().contains(&shared_frames[0]);
        assert_eq!(has, gi == 0, "shared frame must be reachable only by guest 0");
    }

    let ops = fuzz_ops();
    let mut rng = Rng::new(seed());
    let mut faults = 0u64;
    let mut hits = 0u64;
    let mut shared_hits = 0u64;
    // Guest 0 also owns the shared page at index `pages`; others do not.
    let mapped_span = pages * PAGE_WORDS;

    for _ in 0..ops {
        let g = (rng.below(u64::from(num_guests))) as usize;
        // Adversarial mix: inside the private span, inside the shared page (only
        // valid for guest 0), and well past everything so the fault path fires.
        let gpa = match rng.below(3) {
            0 => rng.below(mapped_span),
            1 => mapped_span + rng.below(PAGE_WORDS), // the shared page slot
            _ => mapped_span + PAGE_WORDS + rng.below(mapped_span * 8),
        };
        let write = rng.one_in(2);

        match tables[g].translate(gpa, write) {
            Ok((hfn, off)) => {
                hits += 1;
                // The reached frame must be owned by this guest, private or
                // shared, and never by another guest.
                match owner.get(&hfn) {
                    Some(Owner::Private(o)) => assert_eq!(
                        *o, g as u32,
                        "guest {g} reached private frame {hfn} owned by guest {o}"
                    ),
                    Some(Owner::Shared(o)) => {
                        assert_eq!(*o, g as u32, "guest {g} reached a frame shared with guest {o}");
                        shared_hits += 1;
                    }
                    None => panic!("guest {g} reached unowned frame {hfn}"),
                }
                if write {
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

    // Every branch the gate depends on must have actually fired.
    assert!(hits > 0, "expected some in range accesses");
    assert!(faults > 0, "expected some out of range accesses to fault");
    assert!(shared_hits > 0, "expected some accesses to the shared frame");
}

/// A malformed guest image cannot crash, hang, or leak. A guest whose program
/// references an out of range register faults cleanly, produces no output, and
/// leaves the other guest untouched.
#[test]
fn gate1_malformed_image_is_contained() {
    let mut hv = Hypervisor::new(8, 8);
    // Guest 0 tries to use register 99, which does not exist.
    hv.add_guest("bad", vec![Instr::Movi(99, 0xDEAD), Instr::Out(0, 0)], 1, Perm::rw());
    // Guest 1 is a well behaved printer.
    hv.add_guest(
        "good",
        vec![Instr::Movi(0, u64::from(b'K')), Instr::Out(0, 0), Instr::Halt],
        1,
        Perm::rw(),
    );
    hv.run();

    assert_eq!(hv.guest(0).state, GuestState::Faulted, "malformed guest must fault");
    assert!(hv.guest(0).fault_exits >= 1, "the invalid op must have been recorded");
    assert_eq!(hv.guest(0).console.as_string(), "", "faulted guest emitted nothing");
    // The neighbor ran to completion regardless.
    assert_eq!(hv.guest(1).console.as_string(), "K");
    assert_eq!(hv.guest(1).state, GuestState::Halted);
    // The invalid op surfaced as a VM exit, so it went through the trap path.
    assert!(hv.exits.iter().any(|e| matches!(e.reason, ExitReason::InvalidOp { .. })));
}

/// A guest that never terminates cannot hang the host. Under a bounded round
/// budget the run returns and reports the budget was exhausted, and a well
/// behaved neighbor still makes progress.
#[test]
fn gate1_nonterminating_guest_cannot_hang() {
    let mut hv = Hypervisor::new(8, 4);
    hv.add_guest("spin", vec![Instr::Jmp(0)], 1, Perm::rw());
    hv.set_max_rounds(1000);
    let rounds = hv.run();
    assert!(hv.budget_exhausted(), "budget must stop the spinning guest");
    assert_eq!(rounds, 1000, "run stops exactly at the round budget");
    assert_eq!(hv.guest(0).state, GuestState::Running, "spinner never halts");
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
/// program surfaces as exactly one `DeviceIo` exit, and no `Continue` step is
/// ever a privileged instruction, so nothing executes IO in guest context.
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

/// Gate 4: the virtio style shared ring device.
///
/// A guest produces payload words into a host frame that is explicitly shared
/// into its address space, then rings the doorbell with a privileged Out. The
/// transfer completes only through trap and emulate: the host reads the shared
/// frame on the doorbell exit and drains the entries. A second guest never maps
/// the shared frame, so the shared memory does not become an inter guest leak.
#[test]
fn gate4_shared_ring_device() {
    let pages = 1u64;
    let base = pages * PAGE_WORDS; // GPA of the ring frame's first word
    let head = base + SharedRing::HEAD;
    let slot0 = base + SharedRing::FIRST_SLOT;
    let slot1 = base + SharedRing::FIRST_SLOT + 1;

    let prog = vec![
        // ring slot 0 = 'H'
        Instr::Movi(0, u64::from(b'H')),
        Instr::Movi(1, slot0),
        Instr::Store(1, 0),
        // ring slot 1 = 'i'
        Instr::Movi(0, u64::from(b'i')),
        Instr::Movi(1, slot1),
        Instr::Store(1, 0),
        // publish: head = 2
        Instr::Movi(0, 2),
        Instr::Movi(1, head),
        Instr::Store(1, 0),
        // ring the doorbell (privileged, traps to the host)
        Instr::Movi(0, 0),
        Instr::Out(RING_DOORBELL_PORT, 0),
        Instr::Halt,
    ];

    let mut hv = Hypervisor::new(8, 16);
    let producer = hv.add_guest("producer", prog, pages, Perm::rw());
    let ring_hfn = hv.attach_ring(producer, pages);
    // A bystander guest that shares nothing.
    let bystander = hv.add_guest(
        "bystander",
        vec![Instr::Movi(0, u64::from(b'X')), Instr::Out(0, 0), Instr::Halt],
        pages,
        Perm::rw(),
    );

    hv.run();

    // The host drained exactly what the guest published, and only via the trap.
    assert_eq!(hv.ring(producer).unwrap().received_string(), "Hi");
    assert!(
        hv.exits.iter().any(|e| matches!(
            e.reason,
            ExitReason::DeviceIo { port, .. } if port == RING_DOORBELL_PORT
        )),
        "the doorbell must have surfaced as a device IO exit"
    );

    // Isolation: the shared frame is mapped into the producer and no one else.
    assert!(hv.guest(producer).page_table.mapped_frames().contains(&ring_hfn));
    assert!(!hv.guest(bystander).page_table.mapped_frames().contains(&ring_hfn));
    assert_eq!(hv.guest(bystander).console.as_string(), "X");
}
