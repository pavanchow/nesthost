# Nesthost design

This document describes how Nesthost models a type-1 hypervisor and why each correctness gate proves the claim it targets. Nesthost is a deterministic simulator in pure Rust std. It is faithful to the shape of hypervisor mechanisms, not to any real hardware.

## Overview

One hypervisor owns all host physical memory and every guest. Each guest is a virtual machine with its own vCPU, its own guest physical address space, its own program, and its own console. The hypervisor time slices a single physical CPU across the guests, translates every guest memory access through a per guest second level page table, and services every VM exit.

The modules map to these roles:

- `cpu` is the emulated guest CPU and instruction set.
- `mem` is host physical memory and second level address translation.
- `device` is the virtual console.
- `guest` bundles the private state of one VM.
- `hypervisor` is frame allocation, the scheduler, and the trap and emulate loop.
- `rng` is a small deterministic PRNG used to drive the fuzz style isolation gate.

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

## Memory virtualization, GPA to HPA

Host physical memory is a flat pool of fixed size frames, sixteen words each. Each guest owns a second level page table that maps its guest page numbers to host frame numbers with read and write permission bits. This is the equivalent of an extended page table or a nested page table in real hardware.

Translation of a guest physical address works in three steps. Split the address into a guest page number and an offset. Look up the guest page number in this guest's page table. If there is no entry, the access faults as unmapped. If the entry forbids the attempted direction, the access faults as a protection violation. Otherwise combine the host frame number with the offset and touch host memory there.

This single function is the only path a guest has to host memory. A guest never names a host frame directly. It names a guest physical address, and the hypervisor's page table decides which host frame, if any, that address reaches. That indirection is the whole of memory isolation.

Frames are handed out by a monotonic allocator. When the hypervisor creates a guest it allocates one fresh frame per page, so two guests created through the normal path never share a frame. Sharing is possible only through an explicit `share_frame` call, which is the opt in escape hatch a real hypervisor also provides for shared memory.

## Trap and emulate, VM exits and entries

The vCPU step function is the choke point that makes trap and emulate unbypassable. When it decodes a privileged instruction it does not perform the effect. It advances the program counter past the instruction and returns a VM exit carrying the decoded request. There is no code path in the guest CPU that writes a device or a control register. The only place those effects happen is the hypervisor's exit handler.

A VM entry is the hypervisor giving a guest a time slice and calling step in a loop. A VM exit is step returning control with a reason. The reasons are a halt, running off the end of the program, a device IO write, a control register write, and a memory fault. On a device IO exit the hypervisor writes the byte to that guest's own console and resumes. On a control register exit it updates that guest's shadow control register and resumes. On a halt or a fault it stops the guest. Every exit is recorded with its tick, guest, program counter, and reason, which is also what makes the run inspectable and testable.

## vCPU scheduling

The scheduler is a deterministic round robin. Each pass over the guests gives every runnable guest one slice of up to `quantum` retired instructions. Within a slice, device IO and control register exits are emulated in place and the guest keeps running, so a slice ends on the instruction quantum, a halt, or a fault. The pass repeats until no guest is runnable. Because the guest order is fixed, the quantum is fixed, and there is no wall clock or randomness in the scheduler, the schedule is a pure function of the configuration.

## Isolation

Isolation rests on two facts. First, the only route from a guest to host memory is its own page table, checked on every access. Second, guest page tables map disjoint host frames unless the operator explicitly shares one. Together these mean a guest can read and write exactly the host frames its own table maps and nothing else. An address it has no mapping for faults rather than falling through to some other guest's frame. Devices are isolated the same way, since each guest has its own console and the console is only ever touched by the hypervisor on that guest's own exit.

## Why each gate proves its claim

Gate one, inter guest isolation. It builds several guests with disjoint frames, writes a distinct sentinel into each guest's memory, then issues thousands of randomized accesses split between in range and out of range addresses. Every successful translation is checked to land on a frame the acting guest owns, every in range read is checked to return that guest's own sentinel and never another's, and every out of range access is checked to fault. It also checks at rest that no frame is shared. If any guest could reach another's memory, one of these assertions would fire. The gate confirms both branches actually ran, so it is not passing by never exercising the fault path.

Gate two, trap and emulate. At the CPU level it generates privileged instructions with random operands and checks that step always returns a VM exit with the correct decoded operands and never mutates control state in guest context. End to end it runs a guest through the hypervisor and checks that the emulated device write and control register write actually landed and the guest resumed. A separate check confirms every device write in a program surfaces as exactly one device IO exit and that no Continue step is ever a privileged instruction, so nothing slips past the trap.

Gate three, multi guest determinism. It runs two printing guests under the scheduler, checks their outputs are correct and isolated, then builds and runs the identical machine a second time and checks the schedule, the exit log, and both consoles match bit for bit. It also checks the two guests really interleaved rather than one running to completion first. Correct outputs prove the guests executed, matching logs prove determinism, and interleaving proves the time slicing is real.
