//! Utilities for reading E820 info about physical memory.
//!
//! This module provides an idomatic, safe interface for getting memory regions from the info
//! output by the E820 BIOS call.

use alloc::{collections::BTreeSet, vec::Vec};
use core::ops::Deref;

use x86_64::{
    structures::paging::{PageSize, Size4KiB},
    PhysAddr,
};

extern "C" {
    /// The number of entries in `memory_map`.
    static memory_map_count: u32;

    /// The E820 table in memory.
    static memory_map: [MemoryRegion; 32];
}

/// The Region Type value for a usable region.
const E820_MEMORY_USABLE: u32 = 1;

/// Represents an entry in the list of memory regions generated by the E820 BIOS call
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
struct MemoryRegion {
    base: u64,
    length: u64,
    region_type: u32,
    acpi: u32, // extended
}

impl MemoryRegion {
    /// Returns the start address of the region.
    pub fn start_addr(&self) -> u64 {
        self.base
    }

    /// Returns the length of the region (in bytes).
    pub fn len(&self) -> u64 {
        self.length
    }

    /// Returns the end address of the region (exclusive).
    ///
    /// NOTE: This won't work if you can actually have 2^64 bytes, but I think I will chance it...
    pub fn end_addr(&self) -> u64 {
        self.base + self.length
    }
}

/// Safe wrapper around the info from E820.
pub struct E820Info {
    regions: Vec<(usize, usize)>,
}

impl E820Info {
    /// Read the information from the E820 `memory_map` and parse into a safe wrapper.
    pub fn read() -> Self {
        // e820 regions in the memory map can overlap. Worse, overlapping regions can have
        // different usability info. Here we will be conservative and say that a portion of memory
        // is usable only if all overlapping regions are marked usable.

        // Also, this function is optimized for readability. Since we only have 32 regions at most,
        // performance is not an issue.

        // First, get all the info from e820.
        // TODO: Only the first `memory_map_count` entries are valid.
        let info: Vec<_> = unsafe { &memory_map }
            .iter()
            .take(unsafe { memory_map_count as usize })
            .filter(|region| region.len() > 0)
            .map(|region| (region.start_addr(), region.end_addr(), region.region_type))
            .collect();

        // To make life easy, we will break up partially overlapping regions so that if two regions
        // overlap, they overlap exactly (i.e. same start and end addr).
        let mut endpoints: BTreeSet<u64> = BTreeSet::new();
        for &(start, end, _) in info.iter() {
            endpoints.insert(start);
            endpoints.insert(end);
        }

        let mut info: Vec<_> = info
            .into_iter()
            .flat_map(|(start, end, ty)| {
                let mid: Vec<u64> = endpoints
                    .iter()
                    .map(|&x| x)
                    .filter(|&point| point >= start && point <= end)
                    .collect();
                let mut pieces = Vec::new();

                for i in 0..mid.len() - 1 {
                    pieces.push((mid[i], mid[i + 1], ty));
                }

                pieces
            })
            .collect();

        // Sort by start of region
        info.sort_by_key(|&(start, _, _)| start);

        // Finally, find out if each region is useable.
        let mut regions = Vec::new();
        for start in endpoints.into_iter() {
            let same_start: Vec<_> = info.drain_filter(|&mut (s, _, _)| start == s).collect();
            let all_usable = same_start
                .iter()
                .all(|&(s, e, ty)| s < e && ty == E820_MEMORY_USABLE);

            if same_start.len() > 0 && all_usable {
                // (same_start() will be empty for the last endpoint)
                regions.push(
                    same_start
                        .into_iter()
                        .next()
                        .map(|(s, e, _)| (s, e - 1))
                        .unwrap(),
                );
            }
        }

        // Convert to frame boundaries
        let regions = regions
            .into_iter()
            .map(|(s_bytes, e_bytes)| {
                // Round up to nearest page boundary
                let s_page = PhysAddr::new(s_bytes).align_up(Size4KiB::SIZE).as_u64();

                // Round down to nearest page boundary
                let e_page = PhysAddr::new(e_bytes).align_down(Size4KiB::SIZE).as_u64();

                (
                    (s_page / Size4KiB::SIZE) as usize,
                    (e_page / Size4KiB::SIZE) as usize,
                )
            })
            .filter(|(s, e)| s <= e)
            .collect();

        E820Info { regions }
    }

    /// Compute the number of physical pages available.
    pub fn num_phys_pages(&self) -> usize {
        self.regions
            .iter()
            .map(|(start, end)| end - start + 1)
            .sum()
    }
}

// Allows iterating over regions :)
impl Deref for E820Info {
    type Target = [(usize, usize)];

    fn deref(&self) -> &Self::Target {
        &self.regions
    }
}
