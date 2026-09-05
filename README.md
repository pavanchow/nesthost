# Nesthost

Nesthost is a deterministic type-1 hypervisor simulator written in pure Rust std, with zero external dependencies.

It runs several guest virtual machines on one small emulated CPU and models the three jobs a real virtual machine monitor performs:

- vCPU scheduling. A single physical CPU is time sliced across guest vCPUs with a deterministic round robin.
- Memory virtualization. Each guest sees its own guest physical address space. A per guest second level page table maps guest physical addresses (GPA) to host physical addresses (HPA), so a guest can only reach host frames mapped into its own space.
- Trap and emulate. When a guest executes a privileged instruction the vCPU does not perform the effect. It takes a VM exit to the hypervisor, which emulates the operation and resumes the guest with a VM entry.

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

The demo boots three guests. Two print a short string by writing it into their own memory and reading it back through the device console. The third deliberately reaches outside its mapped address space and faults, leaking nothing. The output shows the memory map, the scheduling timeline, the VM exit log, and each guest's isolated console.

## API

The crate is a library plus a binary. Core types:

- `Hypervisor::new(frame_count, quantum)` builds the machine and its scheduler.
- `Hypervisor::add_guest(name, program, pages, perm)` registers a guest, allocating `pages` disjoint host frames for its address space.
- `Hypervisor::share_frame(guest_id, gpn, hfn, perm)` is the only opt in way two guests can reach the same host frame.
- `Hypervisor::run()` runs every guest to completion with the deterministic round robin and returns the number of rounds.
- `VCpu::step(program, page_table, host_mem)` executes one instruction and returns either `Continue` or a VM `Exit`.
- `PageTable::translate(gpa, write)` performs second level address translation and is the only path a guest has to host memory.

Instruction set: `Movi`, `Mov`, `Add`, `Sub`, `Addi`, `Load`, `Store`, `Jz`, `Jmp`, `Halt`, and the two privileged instructions `Out` (device IO) and `SetCr` (set control register) that always trap.

## The correctness gate

Three gates in `tests/gates.rs`, each proving one claim, plus unit tests per module.

1. Inter guest memory isolation. Over many randomized guest memory accesses, a guest only ever reaches host frames mapped into its own space, an in range read always returns that guest's own sentinel value and never another guest's, no guest frame aliases another guest's, and an access to an unmapped GPA faults instead of leaking.
2. Trap and emulate correctness. A privileged instruction always causes a VM exit, never executes in guest context, is emulated correctly by the hypervisor, and resumes the guest with the expected state. A guest cannot bypass the trap.
3. Multi guest execution and determinism. Two guests each running a small program produce correct, isolated outputs while time sliced, and the whole run is reproducible bit for bit.

The randomized gates are bounded for CI and controllable:

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
