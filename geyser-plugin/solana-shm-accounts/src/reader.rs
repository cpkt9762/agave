use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind};
use std::ptr;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use memmap2::Mmap;

use crate::error::ShmError;
use crate::layout::{
    AccountSlotHeader, OffsetTableEntry, SHM_FORMAT_VERSION, SHM_MAGIC, ShmHeader, ShmLayout,
};
use crate::seqlock::{seqlock_read_begin, seqlock_read_validate};

pub struct ShmAccountSnapshot {
    pub owner: [u8; 32],
    pub lamports: u64,
    pub data: Vec<u8>,
    pub executable: bool,
    pub solana_slot: u64,
}

pub struct ShmReader {
    mmap: Mmap,
    layout: ShmLayout,
    index: HashMap<[u8; 32], usize>,
    capacities: Vec<u32>,
}

impl ShmReader {
    pub fn open(shm_path: &str) -> Result<Self, ShmError> {
        let file = OpenOptions::new().read(true).open(shm_path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        validate_header(&mmap)?;

        let shm_header = shm_header_from_bytes(&mmap);
        let capacities = read_capacities(&mmap, shm_header.num_slots as usize)?;
        let layout = ShmLayout::new(shm_header.num_slots, &capacities);
        validate_offsets(&mmap, &layout, &capacities)?;

        let mut reader = Self { mmap, layout, index: HashMap::new(), capacities };
        reader.build_index()?;
        Ok(reader)
    }

    pub fn wait_ready(&self, timeout: Duration) -> Result<(), ShmError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.shm_header().ready.load(Ordering::Acquire) == 1 {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(1));
        }

        Err(ShmError::NotReady)
    }

    pub fn read_account(&self, pubkey: &[u8; 32]) -> Result<ShmAccountSnapshot, ShmError> {
        let slot_index = self.index.get(pubkey).copied().ok_or(ShmError::PubkeyNotFound)?;
        let capacity = self.capacities[slot_index] as usize;
        let shm_header = self.shm_header();
        let slot_header = self.slot_header(slot_index);

        loop {
            let start_version = seqlock_read_begin(slot_header, shm_header)?;
            let data_len = slot_header.data_len as usize;
            if data_len > capacity {
                return Err(ShmError::SlotOverflow {
                    actual: slot_header.data_len,
                    capacity: self.capacities[slot_index],
                });
            }

            let mut data = vec![0_u8; data_len];
            unsafe {
                ptr::copy_nonoverlapping(
                    self.slot_data_ptr(slot_index),
                    data.as_mut_ptr(),
                    data_len,
                );
            }

            let snapshot = ShmAccountSnapshot {
                owner: slot_header.owner,
                lamports: slot_header.lamports,
                data,
                executable: slot_header.executable != 0,
                solana_slot: slot_header.solana_slot,
            };

            if seqlock_read_validate(slot_header, start_version) {
                return Ok(snapshot);
            }
        }
    }

    pub fn read_version(&self, pubkey: &[u8; 32]) -> Result<u64, ShmError> {
        let slot_index = self.index.get(pubkey).copied().ok_or(ShmError::PubkeyNotFound)?;
        Ok(self.slot_header(slot_index).seqlock_ver.load(Ordering::Acquire))
    }

    pub fn global_solana_slot(&self) -> u64 {
        self.shm_header().global_solana_slot.load(Ordering::Acquire)
    }

    pub fn build_index(&mut self) -> Result<(), ShmError> {
        let num_slots = self.shm_header().num_slots as usize;
        let mut index = HashMap::with_capacity(num_slots);
        for slot_index in 0..num_slots {
            let entry = offset_entry(&self.mmap, &self.layout, slot_index)?;
            index.insert(entry.pubkey, slot_index);
        }
        self.index = index;
        Ok(())
    }

    fn shm_header(&self) -> &ShmHeader {
        shm_header_from_bytes(&self.mmap)
    }

    fn slot_header(&self, index: usize) -> &AccountSlotHeader {
        let offset = self.layout.slot_header_offset(index, &self.capacities);
        unsafe { &*(self.mmap.as_ptr().add(offset) as *const AccountSlotHeader) }
    }

    fn slot_data_ptr(&self, index: usize) -> *const u8 {
        let offset = self.layout.slot_data_offset(index, &self.capacities);
        unsafe { self.mmap.as_ptr().add(offset) }
    }
}

fn validate_header(mmap: &[u8]) -> Result<(), ShmError> {
    let header = shm_header_from_bytes(mmap);
    if header.magic != SHM_MAGIC {
        return Err(ShmError::InvalidMagic { expected: SHM_MAGIC, actual: header.magic });
    }
    if header.format_version != SHM_FORMAT_VERSION {
        return Err(ShmError::VersionMismatch {
            expected: SHM_FORMAT_VERSION,
            actual: header.format_version,
        });
    }

    Ok(())
}

fn read_capacities(mmap: &[u8], num_slots: usize) -> Result<Vec<u32>, ShmError> {
    let layout = ShmLayout::new(num_slots as u32, &vec![0; num_slots]);
    let mut capacities = Vec::with_capacity(num_slots);
    for slot_index in 0..num_slots {
        capacities.push(offset_entry(mmap, &layout, slot_index)?.data_capacity);
    }
    Ok(capacities)
}

fn validate_offsets(mmap: &[u8], layout: &ShmLayout, capacities: &[u32]) -> Result<(), ShmError> {
    for slot_index in 0..capacities.len() {
        let entry = offset_entry(mmap, layout, slot_index)?;
        let expected = layout.slot_data_offset(slot_index, capacities) as u64;
        if entry.data_offset != expected {
            return Err(ShmError::Io(Error::new(
                ErrorKind::InvalidData,
                "offset table does not match computed layout",
            )));
        }
    }
    Ok(())
}

fn offset_entry(
    mmap: &[u8],
    layout: &ShmLayout,
    slot_index: usize,
) -> Result<OffsetTableEntry, ShmError> {
    let offset = layout.offset_table_start + slot_index * std::mem::size_of::<OffsetTableEntry>();
    let end = offset + std::mem::size_of::<OffsetTableEntry>();
    if end > mmap.len() {
        return Err(ShmError::Io(Error::new(
            ErrorKind::UnexpectedEof,
            "offset table entry out of bounds",
        )));
    }
    Ok(unsafe { ptr::read(mmap.as_ptr().add(offset) as *const OffsetTableEntry) })
}

fn shm_header_from_bytes(mmap: &[u8]) -> &ShmHeader {
    unsafe { &*(mmap.as_ptr() as *const ShmHeader) }
}
