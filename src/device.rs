//! Virtual devices shared across guests through trap and emulate.
//!
//! The only device modeled is a console. A guest writes to it with the
//! privileged [`crate::cpu::Instr::Out`] instruction, which cannot execute in
//! guest context. It traps, and the hypervisor calls [`VirtualConsole::write`]
//! on the guest's behalf. Because the device is only ever reached from the
//! hypervisor's exit handler, a guest cannot touch device state directly, and
//! each guest's output is kept in its own buffer so one guest cannot observe or
//! corrupt another's console.

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
