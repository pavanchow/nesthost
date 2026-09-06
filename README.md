# Nesthost

Nesthost is a deterministic type-1 hypervisor simulator written in pure Rust std, with zero external dependencies.

It runs several guest virtual machines on one small emulated CPU and models the three jobs a real virtual machine monitor performs:

- vCPU scheduling. A single physical CPU is time sliced across guest vCPUs with a deterministic round robin, bounded by a round budget so a guest that never halts cannot hang the host.
- Memory virtualization. Each guest sees its own guest physical address space. A per guest second level page table maps guest physical addresses (GPA) to host physical addresses (HPA), so a guest can only reach host frames mapped into its own space.
- Trap and emulate. When a guest executes a privileged instruction the vCPU does not perform the effect. It takes a VM exit to the hypervisor, which emulates the operation and resumes the guest with a VM entry.
- Shared device. A virtio style shared memory ring lets one guest hand payloads to the host through a single explicitly shared frame, drained only on a doorbell trap.

## Honest framing

Nesthost is a teaching accurate model, not a real VMM. There is no hardware, no ring transition, no VT-x or AMD-V, and no host kernel. Every mechanism is a plain Rust data structure. What it is faithful to is the shape of the mechanisms: second level address translation, VM exits and entries on privileged instructions, per guest isolation of memory and devices, and deterministic time slicing. If you want to understand how a hypervisor keeps guests apart and virtualizes their memory and devices, you can read the whole thing in an afternoon and run every claim as a test.

## The gap it fills

Real hypervisors are enormous and their core ideas are buried under hardware interfaces and performance work. Small emulators usually model one CPU, not the nesting of many guests under one monitor. Nesthost sits in the middle. It is small enough to read end to end and it keeps the parts that make virtualization interesting: the second level of address translation, the trap boundary between guest and hypervisor, and the scheduler that shares one CPU among many guests.

## Quickstart

```
cargo run --release            # run the demo
cargo run --release -- --help  # usage
cargo test                     # unit tests plus the three correctness gates
```

The demo boots four guests. Two print a short string by writing it into their own memory and reading it back through the device console. A third deliberately reaches outside its mapped address space and faults, leaking nothing. The fourth publishes bytes into a shared memory ring and rings a doorbell, and the host drains the ring on the resulting trap. The output shows the memory map, the scheduling timeline, the VM exit log, each guest's isolated console, and the ring the host received.

## API

The crate is a library plus a binary. Core types:

- `Hypervisor::new(frame_count, quantum)` builds the machine and its scheduler.
- `Hypervisor::add_guest(name, program, pages, perm)` registers a guest, allocating `pages` disjoint host frames for its address space.
- `Hypervisor::share_frame(guest_id, gpn, hfn, perm)` is the only opt in way two guests can reach the same host frame.
- `Hypervisor::attach_ring(guest_id, gpn)` grants a guest a virtio style shared ring frame, mapped read write at `gpn`, drained by the host on the doorbell trap.
- `Hypervisor::set_max_rounds(n)` caps the scheduler round budget for one run, and `budget_exhausted()` reports whether a run stopped on that cap.
- `Hypervisor::run()` runs every guest until none is runnable or the round budget is hit, and returns the number of rounds.
- `VCpu::step(program, page_table, host_mem)` executes one instruction and returns either `Continue` or a VM `Exit`.
- `PageTable::translate(gpa, write)` performs second level address translation and is the only path a guest has to host memory.

Instruction set: `Movi`, `Mov`, `Add`, `Sub`, `Addi`, `Load`, `Store`, `Jz`, `Jmp`, `Halt`, and the two privileged instructions `Out` (device IO, and the ring doorbell on a reserved port) and `SetCr` (set control register) that always trap.

## Robustness against malformed and hostile guests

The host survives arbitrary guest images. An instruction whose register or control register operand is out of architectural range decodes to an invalid operation fault (`ExitReason::InvalidOp`) and the guest is stopped, rather than indexing host state and crashing the host. A guest that never terminates is bounded by the scheduler round budget, so it cannot spin the host forever. The shared ring clamps outstanding entries to the ring capacity and caps retained payload, so a guest cannot make the host over read its frame or grow host memory without bound. These properties are exercised by a bounded stress harness in `tests/stress.rs` that feeds tens of thousands of malformed programs across many seeds and asserts no panic, no hang, and that isolation still holds after arbitrary execution.

## The correctness gate

The gates live in `tests/gates.rs`, each proving one claim, plus unit tests per module and the stress harness in `tests/stress.rs`.

1. Inter guest memory isolation. Over many randomized guest memory accesses, a guest only ever reaches host frames mapped into its own space, checked against an independent external owner map, not the page tables themselves. An in range read always returns that guest's own sentinel value and never another guest's, no guest frame aliases another guest's, an explicitly shared frame is reachable only by the guest it was granted to, and an access to an unmapped GPA faults instead of leaking. Companion gates confirm a malformed guest image faults cleanly and leaves its neighbor untouched, and a non terminating guest is stopped by the round budget.
2. Trap and emulate correctness. A privileged instruction always causes a VM exit, never executes in guest context, is emulated correctly by the hypervisor, and resumes the guest with the expected state. A guest cannot bypass the trap.
3. Multi guest execution and determinism. Two guests each running a small program produce correct, isolated outputs while time sliced, and the whole run is reproducible bit for bit.
4. Shared ring device. A guest publishes payloads into its explicitly shared ring frame and rings the doorbell, and the host drains them only on the resulting trap. A second guest never maps the shared frame, so the shared memory does not become an inter guest leak.

The randomized gates and the stress harness are bounded for CI and controllable:

```
NESTHOST_FUZZ_OPS=50000 NESTHOST_SEED=1 cargo test
```

## Playground

An interactive teaching reimplementation runs in the browser, no build required:

https://pavanchow.github.io/nesthost/

It shows guest VMs side by side, the vCPU scheduling timeline, the GPA to HPA memory map with isolation made visible, an out of bounds access faulting, and the VM exit log as guests hit privileged instructions.

## Design

See `DESIGN.md` for the guest ISA, the memory virtualization model, the trap and emulate path, the scheduler, the isolation argument, and why each gate proves its claim.

## License

MIT.
