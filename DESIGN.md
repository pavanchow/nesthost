# Nesthost design

This document describes how Nesthost models a type-1 hypervisor and why each correctness gate proves the claim it targets. Nesthost is a deterministic simulator in pure Rust std. It is faithful to the shape of hypervisor mechanisms, not to any real hardware.

## Overview

One hypervisor owns all host physical memory and every guest. Each guest is a virtual machine with its own vCPU, its own guest physical address space, its own program, and its own console. The hypervisor time slices a single physical CPU across the guests, translates every guest memory access through a per guest second level page table, and services every VM exit.

The modules map to these roles:

- `cpu` is the emulated guest CPU and instruction set.
- `mem` is host physical memory and second level address translation.
- `device` is the virtual console and the virtio style shared memory ring.
- `guest` bundles the private state of one VM.
- `hypervisor` is frame allocation, the scheduler, and the trap and emulate loop.
- `rng` is a small deterministic PRNG used to drive the fuzz style isolation gate and the stress harness.

## The guest ISA

The guest CPU has eight general registers, a program counter, four shadow control registers, and word sized memory. Instructions live in a program vector indexed by the program counter, a Harvard split, while data lives in the guest's virtualized physical address space. Addresses are machine words, so a guest physical address is a word index into that guest's own space.

Instructions:

- `Movi rd, imm` sets a register to an immediate.
- `Mov rd, rs` copies a register.
- `Add`, `Sub`, `Addi` are wrapping arithmetic.
- `Load rd, rs` reads guest memory at the address in `rs`, through translation.
- `Store rd, rs` writes `rs` to guest memory at the address in `rd`, through translation.
- `Jz rs, target` and `Jmp target` are branches.
- `Halt` stops the vCPU.
- `Out port, rs` is privileged. It is a device IO write.
- `SetCr cr, rs` is privileged. It sets a control register.

The two privileged instructions are the ones a guest is not allowed to perform itself. They exist to model the trap boundary.

A guest image is untrusted input, so the vCPU validates every instruction's operands before it touches any state. An instruction that names a register or control register index outside the architectural range is a malformed image. The vCPU rejects it as an invalid operation VM exit rather than indexing host state out of bounds, which is the equivalent of an invalid opcode fault in real hardware. This is checked at the single step choke point, so no malformed instruction can reach an execution path.

## Memory virtualization, GPA to HPA

Host physical memory is a flat pool of fixed size frames, sixteen words each. Each guest owns a second level page table that maps its guest page numbers to host frame numbers with read and write permission bits. This is the equivalent of an extended page table or a nested page table in real hardware.

Translation of a guest physical address works in three steps. Split the address into a guest page number and an offset. Look up the guest page number in this guest's page table. If there is no entry, the access faults as unmapped. If the entry forbids the attempted direction, the access faults as a protection violation. Otherwise combine the host frame number with the offset and touch host memory there.

This single function is the only path a guest has to host memory. A guest never names a host frame directly. It names a guest physical address, and the hypervisor's page table decides which host frame, if any, that address reaches. That indirection is the whole of memory isolation.

Frames are handed out by a monotonic allocator. When the hypervisor creates a guest it allocates one fresh frame per page, so two guests created through the normal path never share a frame. Sharing is possible only through an explicit `share_frame` call, which is the opt in escape hatch a real hypervisor also provides for shared memory.

## Trap and emulate, VM exits and entries

The vCPU step function is the choke point that makes trap and emulate unbypassable. When it decodes a privileged instruction it does not perform the effect. It advances the program counter past the instruction and returns a VM exit carrying the decoded request. There is no code path in the guest CPU that writes a device or a control register. The only place those effects happen is the hypervisor's exit handler.

A VM entry is the hypervisor giving a guest a time slice and calling step in a loop. A VM exit is step returning control with a reason. The reasons are a halt, running off the end of the program, a device IO write, a control register write, a memory fault, and an invalid operation. On a device IO exit the hypervisor either writes the byte to that guest's own console or, if the write is to the reserved doorbell port, drains that guest's shared ring, then resumes. On a control register exit it updates that guest's shadow control register and resumes. On a halt, a memory fault, or an invalid operation it stops the guest. Every exit is recorded with its tick, guest, program counter, and reason, which is also what makes the run inspectable and testable.

## The shared ring device

The shared ring models a virtio style transport between one guest and the host. The hypervisor grants the guest a single freshly allocated host frame, mapped read write into the guest's address space at a chosen guest page. That frame is the only frame two owners, the guest and the host, ever touch. The frame's first word is a producer head written by the guest, the second word is a consumer tail written by the host, and the rest are ring slots. The guest fills slots and bumps the head with ordinary stores, then rings a doorbell with a privileged `Out` to a reserved port. Only on that doorbell trap does the host read the shared frame and drain the newly published entries, so the transfer still goes through trap and emulate and the guest never reaches host state directly.

The drain is defensive. The number of outstanding entries is clamped to the ring capacity, so even a malformed head that runs far ahead of the tail makes the host read only a bounded number of slots from the guest's own frame. The retained payload is capped, so a guest cannot grow host memory without bound by ringing the doorbell in a loop.

## vCPU scheduling

The scheduler is a deterministic round robin. Each pass over the guests gives every runnable guest one slice of up to `quantum` retired instructions. Within a slice, device IO and control register exits are emulated in place and the guest keeps running, so a slice ends on the instruction quantum, a halt, or a fault. The pass repeats until no guest is runnable. Because the guest order is fixed, the quantum is fixed, and there is no wall clock or randomness in the scheduler, the schedule is a pure function of the configuration.

A run is also bounded by a round budget. A guest can decline to ever halt, for example by jumping to itself. Without a bound the run loop would spin forever, so a non terminating guest would be a denial of service against the host. The scheduler caps the number of rounds a single run performs. When the cap is reached the run stops and reports that the budget was exhausted, leaving any still runnable guests in place. The default budget is large enough that it never truncates a legitimate workload, and it is adjustable for tests that want a tight bound.

## Isolation

Isolation rests on two facts. First, the only route from a guest to host memory is its own page table, checked on every access. Second, guest page tables map disjoint host frames unless the operator explicitly shares one. Together these mean a guest can read and write exactly the host frames its own table maps and nothing else. An address it has no mapping for faults rather than falling through to some other guest's frame. Devices are isolated the same way, since each guest has its own console and the console is only ever touched by the hypervisor on that guest's own exit.

Explicitly shared frames, whether from `share_frame` or an attached ring, refine but do not break this. A shared frame is reachable by the host and by exactly the one guest it was granted to. It is never mapped into a second guest, so it is a channel between one guest and the host, not a bridge between two guests. The isolation invariant is therefore that every host frame is owned either privately by one guest or shared with exactly one guest, and no frame is ever reachable by two different guests. Guest execution cannot change any of this, because a running guest has no instruction that edits a page table. The stress harness confirms this holds after arbitrary, often malformed, execution: it runs many random guests and asserts the union of all their mapped frames still has no duplicates.

## Why each gate proves its claim

Gate one, inter guest isolation. It builds several guests with disjoint frames plus one frame explicitly shared into the first guest, writes a distinct sentinel into each guest's memory, then issues thousands of randomized accesses split across the private span, the shared page, and out of range addresses. Every successful translation is checked against an independent external owner map, not the page tables it is validating, to confirm the reached frame is owned privately by the acting guest or shared with that same guest and never with another. Every in range read is checked to return that guest's own sentinel, and every out of range access is checked to fault. It confirms the shared frame is mapped only into the guest it was granted to, and that the in range, shared, and fault branches all actually ran, so it is not passing by skipping a path. Two companion gates round out robustness: one runs a guest whose image uses an out of range register and checks it faults cleanly, emits nothing, and leaves its neighbor's output intact, and one runs a guest that never halts and checks the round budget stops the run rather than hanging. The stress harness in `tests/stress.rs` then feeds tens of thousands of fully random, mostly malformed programs through the whole machine across many seeds and asserts no panic, termination under the budget, and that no two guests ever end up sharing a frame.

Gate two, trap and emulate. At the CPU level it generates privileged instructions with random operands and checks that step always returns a VM exit with the correct decoded operands and never mutates control state in guest context. End to end it runs a guest through the hypervisor and checks that the emulated device write and control register write actually landed and the guest resumed. A separate check confirms every device write in a program surfaces as exactly one device IO exit and that no Continue step is ever a privileged instruction, so nothing slips past the trap.

Gate three, multi guest determinism. It runs two printing guests under the scheduler, checks their outputs are correct and isolated, then builds and runs the identical machine a second time and checks the schedule, the exit log, and both consoles match bit for bit. It also checks the two guests really interleaved rather than one running to completion first. Correct outputs prove the guests executed, matching logs prove determinism, and interleaving proves the time slicing is real.

Gate four, the shared ring device. A guest writes payload bytes into its granted ring frame with ordinary stores, publishes them by bumping the head, and rings the doorbell. The gate checks the host drained exactly those bytes, that the drain happened through a device IO exit on the doorbell port rather than in guest context, and that a bystander guest never maps the ring frame. This proves the shared transport works and that sharing one frame with the host did not open a path to another guest.
