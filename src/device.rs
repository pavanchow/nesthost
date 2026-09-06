//! Virtual devices shared across guests through trap and emulate.
//!
//! The only device modeled is a console. A guest writes to it with the
//! privileged [`crate::cpu::Instr::Out`] instruction, which cannot execute in
//! guest context. It traps, and the hypervisor calls [`VirtualConsole::write`]
//! on the guest's behalf. Because the device is only ever reached from the
//! hypervisor's exit handler, a guest cannot touch device state directly, and
//! each guest's output is kept in its own buffer so one guest cannot observe or
//! corrupt another's console.

use crate::mem::{HostMemory, PAGE_WORDS};

/// The device IO port a guest writes to ring the shared ring doorbell. Any other
/// port is an ordinary console write.
pub const RING_DOORBELL_PORT: u16 = 0x0D0;

/// A virtio style shared memory ring between one guest and the host.
///
/// The ring lives in a single host frame that the hypervisor grants into the
/// guest's address space read write (an explicit shared frame, the only frame
/// two owners may touch). The guest is the producer, the host the consumer.
/// Layout inside the frame, in words:
///   [0]    head, the producer index, written by the guest
///   [1]    tail, the consumer index, written by the host during emulation
///   [2..]  the ring slots, one payload word each
///
/// The guest fills a slot and bumps head with ordinary stores, then rings the
/// doorbell with a privileged [`crate::cpu::Instr::Out`], which traps. Only on
/// that trap does the host read the shared frame and drain new entries, so the
/// guest never reaches host state directly and the transfer still goes through
/// trap and emulate.
#[derive(Debug, Clone)]
pub struct SharedRing {
    /// The guest this ring is bound to. No other guest maps its frame.
    pub guest_id: u32,
    /// The host frame backing the ring.
    pub hfn: u64,
    /// The guest page the ring is mapped at in that guest's address space.
    pub gpn: u64,
    received: Vec<u64>,
}

impl SharedRing {
    /// Word index of the producer head.
    pub const HEAD: u64 = 0;
    /// Word index of the consumer tail.
    pub const TAIL: u64 = 1;
    /// Word index of the first ring slot.
    pub const FIRST_SLOT: u64 = 2;
    /// Number of usable ring slots in one frame.
    pub const CAP: u64 = PAGE_WORDS - Self::FIRST_SLOT;

    /// Bind a ring to guest `guest_id`, backed by host frame `hfn`, mapped at
    /// guest page `gpn`.
    #[must_use]
    pub fn new(guest_id: u32, hfn: u64, gpn: u64) -> Self {
        Self { guest_id, hfn, gpn, received: Vec::new() }
    }

    /// Consume every entry the guest has published since the last drain, advancing
    /// the consumer index in the shared frame. Returns how many entries were
    /// consumed.
    ///
    /// The number of outstanding entries is clamped to [`Self::CAP`], so even a
    /// malformed head that runs far ahead of tail can only make the host read a
    /// bounded number of slots from the guest's own shared frame. The retained
    /// history is capped at `max_keep` so a guest cannot grow host memory without
    /// bound by ringing the doorbell in a loop.
    pub fn drain(&mut self, mem: &mut HostMemory, max_keep: usize) -> u64 {
        let head = mem.read_word(self.hfn, Self::HEAD);
        let mut tail = mem.read_word(self.hfn, Self::TAIL);
        let available = head.wrapping_sub(tail).min(Self::CAP);
        for _ in 0..available {
            let slot = Self::FIRST_SLOT + (tail % Self::CAP);
            let value = mem.read_word(self.hfn, slot);
            if self.received.len() < max_keep {
                self.received.push(value);
            }
            tail = tail.wrapping_add(1);
        }
        mem.write_word(self.hfn, Self::TAIL, tail);
        available
    }

    /// The payload words the host has drained from this ring so far.
    #[must_use]
    pub fn received(&self) -> &[u64] {
        &self.received
    }

    /// The drained payload low bytes decoded as a UTF-8 string, lossily.
    #[must_use]
    pub fn received_string(&self) -> String {
        let bytes: Vec<u8> = self.received.iter().map(|&w| (w & 0xFF) as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// A per guest console that accumulates bytes written by the guest.
#[derive(Debug, Clone, Default)]
pub struct VirtualConsole {
    bytes: Vec<u8>,
    writes: u64,
}

impl VirtualConsole {
    /// A fresh empty console.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Emulate a device write. The low eight bits of `value` are appended as a
    /// byte, matching a simple character output port.
    pub fn write(&mut self, value: u64) {
        self.bytes.push((value & 0xFF) as u8);
        self.writes += 1;
    }

    /// The raw bytes written so far.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The number of device writes serviced.
    #[must_use]
    pub fn write_count(&self) -> u64 {
        self.writes
    }

    /// The output decoded as UTF-8, lossily.
    #[must_use]
    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::VirtualConsole;

    #[test]
    fn writes_low_byte_and_counts() {
        let mut c = VirtualConsole::new();
        c.write(u64::from(b'H'));
        c.write(u64::from(b'i'));
        c.write(0x1_0000 + u64::from(b'!')); // high bits ignored
        assert_eq!(c.as_string(), "Hi!");
        assert_eq!(c.write_count(), 3);
    }
}
