use {
    crate::{
        error::ShmError,
        layout::{
            AccountSlotHeader, OffsetTableEntry, ShmConfig, ShmHeader, ShmLayout,
            SHM_FORMAT_VERSION, SHM_MAGIC,
        },
        seqlock::{seqlock_write_begin, seqlock_write_end},
    },
    memmap2::MmapMut,
    std::{
        collections::HashMap,
        fs::OpenOptions,
        io::{Error, ErrorKind},
        path::Path,
        ptr,
        sync::atomic::Ordering,
        time::{SystemTime, UNIX_EPOCH},
    },
};

pub struct ShmWriter {
    mmap: MmapMut,
    layout: ShmLayout,
    index: HashMap<[u8; 32], usize>,
    capacities: Vec<u32>,
}

impl ShmWriter {
    pub fn create(config: &ShmConfig) -> Result<Self, ShmError> {
        validate_config(config)?;

        let num_slots = config.accounts.len() as u32;
        let layout = ShmLayout::new(num_slots, &config.capacities);
        let path = Path::new(&config.shm_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.set_len(layout.total_size as u64)?;

        let mut mmap = unsafe { MmapMut::map_mut(&file)? };
        mmap.as_mut().fill(0);

        write_shm_header(&mut mmap, num_slots);

        let mut index = HashMap::with_capacity(config.accounts.len());
        for (slot_index, (pubkey, capacity)) in config
            .accounts
            .iter()
            .zip(config.capacities.iter())
            .enumerate()
        {
            let entry = OffsetTableEntry {
                pubkey: *pubkey,
                data_offset: layout.slot_data_offset(slot_index, &config.capacities) as u64,
                data_capacity: *capacity,
                _pad: 0,
            };
            write_offset_entry(&mut mmap, &layout, slot_index, &entry);
            index.insert(*pubkey, slot_index);
        }

        let writer = Self {
            mmap,
            layout,
            index,
            capacities: config.capacities.clone(),
        };
        writer.update_heartbeat();
        writer
            .shm_header()
            .writer_pid
            .store(std::process::id(), Ordering::Release);
        Ok(writer)
    }

    pub fn open(shm_path: &str) -> Result<Self, ShmError> {
        let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        validate_header_bytes(&mmap)?;

        let header = shm_header_from_bytes(&mmap);
        let capacities = read_capacities(&mmap, header.num_slots as usize)?;
        let layout = ShmLayout::new(header.num_slots, &capacities);
        validate_offsets(&mmap, &layout, &capacities)?;
        let index = build_index_from_entries(&mmap, header.num_slots as usize)?;

        let writer = Self {
            mmap,
            layout,
            index,
            capacities,
        };
        writer.update_heartbeat();
        writer
            .shm_header()
            .writer_pid
            .store(std::process::id(), Ordering::Release);
        Ok(writer)
    }

    pub fn write_account(
        &self,
        pubkey: &[u8; 32],
        owner: &[u8; 32],
        lamports: u64,
        executable: bool,
        _rent_epoch: u64,
        data: &[u8],
        slot: u64,
    ) -> Result<(), ShmError> {
        let slot_index = self.lookup(pubkey).ok_or(ShmError::PubkeyNotFound)?;
        let capacity = self.capacities[slot_index];
        if data.len() > capacity as usize {
            return Err(ShmError::SlotOverflow {
                actual: data.len() as u32,
                capacity,
            });
        }

        let slot_header = self.slot_header(slot_index);
        seqlock_write_begin(slot_header);

        slot_header.solana_slot = slot;
        slot_header.owner = *owner;
        slot_header.lamports = lamports;
        slot_header.data_len = data.len() as u32;
        slot_header.executable = u8::from(executable);

        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), self.slot_data_ptr(slot_index), data.len());
        }

        if data.len() < capacity as usize {
            unsafe {
                ptr::write_bytes(
                    self.slot_data_ptr(slot_index).add(data.len()),
                    0,
                    capacity as usize - data.len(),
                );
            }
        }

        self.shm_header()
            .global_solana_slot
            .store(slot, Ordering::Release);
        self.update_heartbeat();
        seqlock_write_end(slot_header);
        Ok(())
    }

    pub fn update_heartbeat(&self) {
        let header = self.shm_header();
        header
            .writer_heartbeat
            .store(current_time_ns(), Ordering::Release);
        header
            .writer_pid
            .store(std::process::id(), Ordering::Release);
    }

    pub fn update_global_slot(&self, slot: u64) {
        self.shm_header()
            .global_solana_slot
            .store(slot, Ordering::Release);
        self.update_heartbeat();
    }

    pub fn set_ready(&self) {
        self.shm_header().ready.store(1, Ordering::Release);
    }

    pub fn lookup(&self, pubkey: &[u8; 32]) -> Option<usize> {
        self.index.get(pubkey).copied()
    }

    fn shm_header(&self) -> &ShmHeader {
        shm_header_from_bytes(&self.mmap)
    }

    fn slot_header(&self, index: usize) -> &mut AccountSlotHeader {
        let offset = self.layout.slot_header_offset(index, &self.capacities);
        unsafe { &mut *(self.mmap.as_ptr().add(offset) as *mut AccountSlotHeader) }
    }

    fn slot_data_ptr(&self, index: usize) -> *mut u8 {
        let offset = self.layout.slot_data_offset(index, &self.capacities);
        unsafe { self.mmap.as_ptr().add(offset) as *mut u8 }
    }
}

fn validate_config(config: &ShmConfig) -> Result<(), ShmError> {
    if config.accounts.len() != config.capacities.len() {
        return Err(ShmError::Io(Error::new(
            ErrorKind::InvalidInput,
            "accounts and capacities length mismatch",
        )));
    }

    Ok(())
}

fn write_shm_header(mmap: &mut MmapMut, num_slots: u32) {
    let header = shm_header_from_bytes_mut(mmap);
    header.magic = SHM_MAGIC;
    header.format_version = SHM_FORMAT_VERSION;
    header.num_slots = num_slots;
    header._pad0 = 0;
    header.global_solana_slot.store(0, Ordering::Relaxed);
    header.writer_heartbeat.store(0, Ordering::Relaxed);
    header.writer_pid.store(0, Ordering::Relaxed);
    header.ready.store(0, Ordering::Relaxed);
    header._reserved.fill(0);
}

fn write_offset_entry(
    mmap: &mut MmapMut,
    layout: &ShmLayout,
    slot_index: usize,
    entry: &OffsetTableEntry,
) {
    let offset = layout.offset_table_start + slot_index * std::mem::size_of::<OffsetTableEntry>();
    unsafe {
        ptr::write(
            mmap.as_mut_ptr().add(offset) as *mut OffsetTableEntry,
            OffsetTableEntry {
                pubkey: entry.pubkey,
                data_offset: entry.data_offset,
                data_capacity: entry.data_capacity,
                _pad: entry._pad,
            },
        );
    }
}

fn validate_header_bytes(mmap: &[u8]) -> Result<(), ShmError> {
    let header = shm_header_from_bytes(mmap);
    if header.magic != SHM_MAGIC {
        return Err(ShmError::InvalidMagic {
            expected: SHM_MAGIC,
            actual: header.magic,
        });
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
    let mut capacities = Vec::with_capacity(num_slots);
    for slot_index in 0..num_slots {
        capacities.push(offset_entry(mmap, slot_index)?.data_capacity);
    }
    Ok(capacities)
}

fn validate_offsets(mmap: &[u8], layout: &ShmLayout, capacities: &[u32]) -> Result<(), ShmError> {
    for slot_index in 0..capacities.len() {
        let entry = offset_entry(mmap, slot_index)?;
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

fn build_index_from_entries(
    mmap: &[u8],
    num_slots: usize,
) -> Result<HashMap<[u8; 32], usize>, ShmError> {
    let mut index = HashMap::with_capacity(num_slots);
    for slot_index in 0..num_slots {
        let entry = offset_entry(mmap, slot_index)?;
        index.insert(entry.pubkey, slot_index);
    }
    Ok(index)
}

fn offset_entry(mmap: &[u8], slot_index: usize) -> Result<OffsetTableEntry, ShmError> {
    let layout = ShmLayout::new(0, &[]);
    let offset = layout.offset_table_start + slot_index * std::mem::size_of::<OffsetTableEntry>();
    let end = offset + std::mem::size_of::<OffsetTableEntry>();
    if end > mmap.len() {
        return Err(ShmError::Io(Error::new(
            ErrorKind::UnexpectedEof,
            "offset table entry out of bounds",
        )));
    }

    let entry = unsafe { ptr::read(mmap.as_ptr().add(offset) as *const OffsetTableEntry) };
    Ok(entry)
}

fn shm_header_from_bytes(mmap: &[u8]) -> &ShmHeader {
    unsafe { &*(mmap.as_ptr() as *const ShmHeader) }
}

fn shm_header_from_bytes_mut(mmap: &mut [u8]) -> &mut ShmHeader {
    unsafe { &mut *(mmap.as_mut_ptr() as *mut ShmHeader) }
}

fn current_time_ns() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as u64,
        Err(_) => 0,
    }
}
