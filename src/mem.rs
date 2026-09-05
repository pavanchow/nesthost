//! Memory virtualization: guest physical to host physical address translation.
//!
//! Host physical memory is a flat pool of fixed size frames. Each guest owns a
//! second level page table (a SLAT, the equivalent of an EPT or a nested page
//! table) that maps its guest page numbers to host frame numbers with access
//! permissions. Address units are machine words (`u64`), so a "guest physical
//! address" here is a word index into the guest's own physical space.
//!
//! The isolation guarantee lives entirely in this file. A guest can only reach a
//! host frame that its own page table maps. An access to an unmapped guest page,
//! or a write to a read only page, faults instead of touching host memory.

use std::collections::BTreeMap;

/// Words per page (and per host frame). Small on purpose so the memory map is
/// easy to print and reason about.
pub const PAGE_WORDS: u64 = 16;

/// Access permission bits for a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
}

impl Perm {
    /// Read and write.
    #[must_use]
    pub const fn rw() -> Self {
        Self { read: true, write: true }
    }

    /// Read only.
    #[must_use]
    pub const fn ro() -> Self {
        Self { read: true, write: false }
    }
}

/// Why a guest physical access could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysFault {
    /// The guest page number has no mapping in this guest's page table.
    Unmapped,
    /// The mapping exists but forbids the attempted access (for example a write
    /// to a read only page).
    Protection,
}

/// A single second level mapping entry.
#[derive(Debug, Clone, Copy)]
struct Pte {
    hfn: u64,
    perm: Perm,
}

/// Host physical memory: a pool of frames shared by all guests.
///
/// A frame belongs to exactly one purpose. The hypervisor hands frame numbers to
/// guests through their page tables, and the disjointness of those handouts is
/// what keeps guests isolated.
#[derive(Debug, Clone)]
pub struct HostMemory {
    frames: Vec<[u64; PAGE_WORDS as usize]>,
    next_free: u64,
}

impl HostMemory {
    /// Create host memory with `frame_count` zeroed frames.
    #[must_use]
    pub fn new(frame_count: u64) -> Self {
        Self {
            frames: vec![[0u64; PAGE_WORDS as usize]; frame_count as usize],
            next_free: 0,
        }
    }

    /// Total number of host frames.
    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.frames.len() as u64
    }

    /// Allocate the next free host frame, returning its number.
    ///
    /// # Panics
    /// Panics if host memory is exhausted.
    pub fn alloc_frame(&mut self) -> u64 {
        let hfn = self.next_free;
        assert!(hfn < self.frame_count(), "out of host frames");
        self.next_free += 1;
        hfn
    }

    /// Read a word directly from a host frame. This is the raw host side access
    /// used by translation; guests never call it directly.
    ///
    /// # Panics
    /// Panics if `hfn` or `offset` is out of range.
    #[must_use]
    pub fn read_word(&self, hfn: u64, offset: u64) -> u64 {
        self.frames[hfn as usize][offset as usize]
    }

    /// Write a word directly to a host frame.
    ///
    /// # Panics
    /// Panics if `hfn` or `offset` is out of range.
    pub fn write_word(&mut self, hfn: u64, offset: u64, value: u64) {
        self.frames[hfn as usize][offset as usize] = value;
    }
}

/// A per guest second level page table mapping guest page numbers to host frame
/// numbers.
#[derive(Debug, Clone, Default)]
pub struct PageTable {
    entries: BTreeMap<u64, Pte>,
}

impl PageTable {
    /// Create an empty page table.
    #[must_use]
    pub fn new() -> Self {
        Self { entries: BTreeMap::new() }
    }

    /// Map guest page `gpn` to host frame `hfn` with the given permission.
    pub fn map(&mut self, gpn: u64, hfn: u64, perm: Perm) {
        self.entries.insert(gpn, Pte { hfn, perm });
    }

    /// The host frame number backing guest page `gpn`, if mapped.
    #[must_use]
    pub fn host_frame(&self, gpn: u64) -> Option<u64> {
        self.entries.get(&gpn).map(|p| p.hfn)
    }

    /// The set of host frame numbers this page table can reach.
    #[must_use]
    pub fn mapped_frames(&self) -> Vec<u64> {
        self.entries.values().map(|p| p.hfn).collect()
    }

    /// Iterate mappings as `(gpn, hfn, perm)` ordered by guest page number.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64, Perm)> + '_ {
        self.entries.iter().map(|(&gpn, pte)| (gpn, pte.hfn, pte.perm))
    }

    /// Translate a guest physical address (a word index) plus the intended
    /// access into a concrete `(hfn, offset)` host location, or a fault.
    ///
    /// This is second level address translation. It is the only path a guest has
    /// to host memory, and it enforces both presence and permission.
    ///
    /// # Errors
    /// Returns [`PhysFault::Unmapped`] if the guest page is not mapped, or
    /// [`PhysFault::Protection`] if the mapping forbids the access.
    pub fn translate(&self, gpa: u64, write: bool) -> Result<(u64, u64), PhysFault> {
        let gpn = gpa / PAGE_WORDS;
        let offset = gpa % PAGE_WORDS;
        let pte = self.entries.get(&gpn).ok_or(PhysFault::Unmapped)?;
        if write && !pte.perm.write {
            return Err(PhysFault::Protection);
        }
        if !write && !pte.perm.read {
            return Err(PhysFault::Protection);
        }
        Ok((pte.hfn, offset))
    }
}

#[cfg(test)]
mod tests {
    use super::{HostMemory, PageTable, Perm, PhysFault, PAGE_WORDS};

    #[test]
    fn alloc_is_monotonic_and_disjoint() {
        let mut mem = HostMemory::new(4);
        let a = mem.alloc_frame();
        let b = mem.alloc_frame();
        assert_ne!(a, b);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
    }

    #[test]
    fn translate_maps_page_and_offset() {
        let mut pt = PageTable::new();
        pt.map(0, 7, Perm::rw());
        // gpa 3 is inside page 0 at offset 3, backed by host frame 7.
        let (hfn, off) = pt.translate(3, false).unwrap();
        assert_eq!(hfn, 7);
        assert_eq!(off, 3);
        // gpa PAGE_WORDS is the first word of page 1, which is unmapped.
        assert_eq!(pt.translate(PAGE_WORDS, false), Err(PhysFault::Unmapped));
    }

    #[test]
    fn read_only_page_faults_on_write() {
        let mut pt = PageTable::new();
        pt.map(0, 1, Perm::ro());
        assert!(pt.translate(0, false).is_ok());
        assert_eq!(pt.translate(0, true), Err(PhysFault::Protection));
    }

    #[test]
    fn round_trip_through_translation() {
        let mut mem = HostMemory::new(2);
        let hfn = mem.alloc_frame();
        let mut pt = PageTable::new();
        pt.map(0, hfn, Perm::rw());
        let (h, off) = pt.translate(5, true).unwrap();
        mem.write_word(h, off, 0xDEAD_BEEF);
        let (h2, off2) = pt.translate(5, false).unwrap();
        assert_eq!(mem.read_word(h2, off2), 0xDEAD_BEEF);
    }
}
