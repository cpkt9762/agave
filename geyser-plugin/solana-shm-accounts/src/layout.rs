use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};

#[allow(unused_imports)]
use crossbeam_utils::CachePadded;

pub const SHM_MAGIC: u64 = 0x534F4C_53484D_01;
pub const SHM_FORMAT_VERSION: u64 = 1;

#[repr(C, align(64))]
pub struct ShmHeader {
    pub magic: u64,
    pub format_version: u64,
    pub num_slots: u32,
    pub _pad0: u32,
    pub global_solana_slot: AtomicU64,
    pub writer_heartbeat: AtomicU64,
    pub writer_pid: AtomicU32,
    pub ready: AtomicU8,
    pub _reserved: [u8; 83],
}

#[repr(C)]
pub struct OffsetTableEntry {
    pub pubkey: [u8; 32],
    pub data_offset: u64,
    pub data_capacity: u32,
    pub _pad: u32,
}

#[repr(C, align(64))]
pub struct AccountSlotHeader {
    pub seqlock_ver: AtomicU64,
    pub solana_slot: u64,
    pub owner: [u8; 32],
    pub lamports: u64,
    pub data_len: u32,
    pub executable: u8,
    pub _pad: [u8; 3],
}

pub struct ShmLayout {
    pub header_size: usize,
    pub offset_table_start: usize,
    pub offset_table_size: usize,
    pub data_region_start: usize,
    pub total_size: usize,
}

pub struct ShmConfig {
    pub shm_path: String,
    pub accounts: Vec<[u8; 32]>,
    pub capacities: Vec<u32>,
}

impl ShmLayout {
    pub fn new(num_slots: u32, capacities: &[u32]) -> Self {
        let header_size = std::mem::size_of::<ShmHeader>();
        let offset_table_start = header_size;
        let offset_table_size = num_slots as usize * std::mem::size_of::<OffsetTableEntry>();
        let data_region_start = align_up(offset_table_start + offset_table_size, 64);
        let total_size = data_region_start + total_slot_size(capacities);

        Self { header_size, offset_table_start, offset_table_size, data_region_start, total_size }
    }

    pub fn slot_header_offset(&self, index: usize, capacities: &[u32]) -> usize {
        self.data_region_start + total_slot_size(&capacities[..index])
    }

    pub fn slot_data_offset(&self, index: usize, capacities: &[u32]) -> usize {
        self.slot_header_offset(index, capacities) + std::mem::size_of::<AccountSlotHeader>()
    }
}

fn align_up(val: usize, alignment: usize) -> usize {
    (val + alignment - 1) & !(alignment - 1)
}

fn total_slot_size(capacities: &[u32]) -> usize {
    capacities
        .iter()
        .map(|capacity| align_up(std::mem::size_of::<AccountSlotHeader>() + *capacity as usize, 64))
        .sum()
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use std::mem;

    #[test]
    fn test_shm_header_size_and_align() {
        assert_eq!(mem::size_of::<ShmHeader>(), 128);
        assert_eq!(mem::align_of::<ShmHeader>(), 64);
    }

    #[test]
    fn test_offset_table_entry_size() {
        assert_eq!(mem::size_of::<OffsetTableEntry>(), 48);
    }

    #[test]
    fn test_account_slot_header_size_and_align() {
        assert_eq!(mem::size_of::<AccountSlotHeader>(), 64);
        assert_eq!(mem::align_of::<AccountSlotHeader>(), 64);
    }

    #[test]
    fn test_shm_layout_calculation() {
        let capacities = vec![1024u32, 2048, 512];
        let layout = ShmLayout::new(3, &capacities);
        assert_eq!(layout.header_size, 128);
        assert_eq!(layout.offset_table_start, 128);
        assert_eq!(layout.offset_table_size, 3 * 48);
        assert_eq!(layout.data_region_start, 320);
        assert_eq!(layout.total_size, 4096);
    }

    #[test]
    fn test_shm_layout_offsets() {
        let capacities = vec![1024u32, 2048, 512];
        let layout = ShmLayout::new(3, &capacities);
        assert_eq!(layout.slot_header_offset(0, &capacities), 320);
        assert_eq!(layout.slot_data_offset(0, &capacities), 384);
        assert_eq!(layout.slot_header_offset(1, &capacities), 1408);
    }
}
