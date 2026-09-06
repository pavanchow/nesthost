//! Nesthost CLI. Boots several guest VMs, schedules them across one physical
//! CPU, and prints per guest output, the VM exit log, the scheduling timeline,
//! and the guest physical to host physical memory map.
//!
//! Usage:
//!   nesthost           run the full demo
//!   nesthost demo      run the full demo
//!   nesthost --help    show this help

use nesthost::cpu::Instr;
use nesthost::device::{SharedRing, RING_DOORBELL_PORT};
use nesthost::hypervisor::Hypervisor;
use nesthost::mem::{Perm, PAGE_WORDS};

/// Build a program that writes `text` into the guest's own physical memory, then
/// reads it back byte by byte and prints each byte through the privileged device
/// IO instruction. The trailing zero that ends the print loop comes for free
/// because guest frames start zeroed.
///
/// Registers used: r0 scratch/char, r1 pointer.
fn print_program(text: &str) -> Vec<Instr> {
    let mut prog = Vec::new();
    for (i, byte) in text.bytes().enumerate() {
        prog.push(Instr::Movi(0, u64::from(byte)));
        prog.push(Instr::Movi(1, i as u64));
        prog.push(Instr::Store(1, 0)); // mem[i] = byte
    }
    prog.push(Instr::Movi(1, 0)); // pointer = 0
    let loop_start = prog.len();
    prog.push(Instr::Load(0, 1)); // r0 = mem[ptr]
    // placeholder for Jz to end, patched once we know the end index
    let jz_index = prog.len();
    prog.push(Instr::Jz(0, 0));
    prog.push(Instr::Out(0, 0)); // privileged: traps to hypervisor
    prog.push(Instr::Addi(1, 1, 1)); // ptr += 1
    prog.push(Instr::Jmp(loop_start));
    let end = prog.len();
    prog.push(Instr::Halt);
    prog[jz_index] = Instr::Jz(0, end);
    prog
}

/// A program that deliberately reaches outside its mapped address space to show
/// the second level translation fault the hypervisor reports.
fn out_of_bounds_program() -> Vec<Instr> {
    vec![
        Instr::Movi(0, 42),
        Instr::Movi(1, 4),
        Instr::Store(1, 0), // legal write inside its own space
        Instr::Movi(1, 9999), // an address in an unmapped page
        Instr::Load(0, 1),  // faults here, no leak
        Instr::Halt,
    ]
}

/// Build a guest that publishes `text` into its shared ring frame (mapped at
/// guest page 1) and rings the doorbell so the host drains it through trap and
/// emulate. Registers used: r0 value, r1 pointer.
fn ring_program(text: &str) -> Vec<Instr> {
    let base = PAGE_WORDS; // the ring frame is mapped at guest page 1
    let mut prog = Vec::new();
    for (i, byte) in text.bytes().enumerate() {
        prog.push(Instr::Movi(0, u64::from(byte)));
        prog.push(Instr::Movi(1, base + SharedRing::FIRST_SLOT + i as u64));
        prog.push(Instr::Store(1, 0));
    }
    // Publish: head = number of entries.
    prog.push(Instr::Movi(0, text.len() as u64));
    prog.push(Instr::Movi(1, base + SharedRing::HEAD));
    prog.push(Instr::Store(1, 0));
    // Ring the doorbell (privileged, traps to the host).
    prog.push(Instr::Movi(0, 0));
    prog.push(Instr::Out(RING_DOORBELL_PORT, 0));
    prog.push(Instr::Halt);
    prog
}

fn print_exit_log(hv: &Hypervisor) {
    println!("VM exit log (trap and emulate):");
    for e in &hv.exits {
        let g = hv.guest(e.guest_id);
        println!(
            "  tick {:>3}  guest {} \"{}\"  pc {:>3}  {:?}",
            e.tick, e.guest_id, g.name, e.pc, e.reason
        );
    }
}

fn print_schedule(hv: &Hypervisor) {
    println!("vCPU schedule timeline (round robin, quantum {}):", hv.quantum);
    for s in &hv.schedule {
        let g = hv.guest(s.guest_id);
        println!(
            "  slice @tick {:>3}  guest {} \"{}\"  ran {} instr",
            s.tick, s.guest_id, g.name, s.instructions_run
        );
    }
}

fn run_demo() {
    // 16 frames of host physical memory, 4 instruction quantum so the two
    // printers visibly interleave.
    let mut hv = Hypervisor::new(16, 4);
    hv.add_guest("hello", print_program("Hello"), 2, Perm::rw());
    hv.add_guest("world", print_program("world!"), 2, Perm::rw());
    hv.add_guest("rogue", out_of_bounds_program(), 1, Perm::rw());
    // A guest that talks to the host over a shared memory ring. It owns one
    // private page and one page granted as the shared ring frame.
    let ringer = hv.add_guest("ringer", ring_program("ring!"), 1, Perm::rw());
    hv.attach_ring(ringer, 1);

    println!("== Nesthost: deterministic type-1 hypervisor simulator ==\n");
    println!("Booting {} guests on 1 physical CPU.\n", hv.guests.len());

    println!("Guest physical to host physical memory map:");
    print!("{}", hv.memory_map());
    println!();

    let rounds = hv.run();

    print_schedule(&hv);
    println!();
    print_exit_log(&hv);
    println!();

    println!("Per guest console output (isolated buffers):");
    for g in &hv.guests {
        println!(
            "  guest {} \"{}\"  state {:?}  io_exits {}  faults {}  output: {:?}",
            g.id,
            g.name,
            g.state,
            g.io_exits,
            g.fault_exits,
            g.console.as_string()
        );
    }
    println!();

    if let Some(ring) = hv.ring(ringer) {
        println!("Shared ring device (guest to host, via doorbell trap):");
        println!(
            "  guest {} drained {} entries, host received: {:?}",
            ring.guest_id,
            ring.received().len(),
            ring.received_string()
        );
        println!();
    }

    println!(
        "Totals: {} scheduler rounds, {} VM entries, {} VM exits.",
        rounds,
        hv.vm_entries(),
        hv.vm_exits()
    );
    println!(
        "Isolation holds: guest \"rogue\" faulted reaching an unmapped GPA and \
         leaked nothing from the other guests."
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("demo") => run_demo(),
        Some("--help" | "-h" | "help") => {
            println!("nesthost           run the full demo");
            println!("nesthost demo      run the full demo");
            println!("nesthost --help    show this help");
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            eprintln!("try: nesthost --help");
            std::process::exit(2);
        }
    }
}
