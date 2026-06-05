use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::ShmError;
use crate::layout::{AccountSlotHeader, ShmHeader};

const MAX_SPIN_COUNT: u32 = 10_000;
const HEARTBEAT_TIMEOUT_NS: u64 = 1_000_000_000;

#[inline]
fn current_time_ns() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as u64,
        Err(_) => 0,
    }
}

pub fn seqlock_write_begin(header: &AccountSlotHeader) {
    let version = header.seqlock_ver.load(Ordering::Relaxed);
    header.seqlock_ver.store(version.wrapping_add(1), Ordering::Release);
}

pub fn seqlock_write_end(header: &AccountSlotHeader) {
    let version = header.seqlock_ver.load(Ordering::Relaxed);
    header.seqlock_ver.store(version.wrapping_add(1), Ordering::Release);
}

pub fn seqlock_read_begin(
    slot_header: &AccountSlotHeader,
    shm_header: &ShmHeader,
) -> Result<u64, ShmError> {
    let mut spins = 0_u32;

    loop {
        let version = slot_header.seqlock_ver.load(Ordering::Acquire);
        if version & 1 == 0 {
            return Ok(version);
        }

        spins = spins.saturating_add(1);
        if spins >= MAX_SPIN_COUNT {
            check_writer_health(shm_header)?;
            spins = 0;
        }

        std::hint::spin_loop();
    }
}

#[must_use]
pub fn seqlock_read_validate(header: &AccountSlotHeader, start_version: u64) -> bool {
    std::sync::atomic::fence(Ordering::Acquire);
    header.seqlock_ver.load(Ordering::Relaxed) == start_version
}

fn check_writer_health(shm_header: &ShmHeader) -> Result<(), ShmError> {
    let heartbeat = shm_header.writer_heartbeat.load(Ordering::Relaxed);
    let now = current_time_ns();

    if now.saturating_sub(heartbeat) <= HEARTBEAT_TIMEOUT_NS {
        return Ok(());
    }

    let pid = shm_header.writer_pid.load(Ordering::Relaxed);
    if pid > 0 {
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result != 0 {
            return Err(ShmError::WriterDead(pid));
        }
    }

    Err(ShmError::WriterStale)
}

#[cfg(test)]
mod seqlock_tests {
    use super::*;
    use crate::layout::{AccountSlotHeader, ShmHeader};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
    use std::thread;

    struct SharedPayload {
        primary: AtomicU64,
        mirror: AtomicU64,
    }

    fn make_test_slot_header() -> AccountSlotHeader {
        AccountSlotHeader {
            seqlock_ver: AtomicU64::new(0),
            solana_slot: 0,
            owner: [0; 32],
            lamports: 0,
            data_len: 0,
            executable: 0,
            _pad: [0; 3],
        }
    }

    fn make_test_shm_header() -> ShmHeader {
        ShmHeader {
            magic: 0,
            format_version: 0,
            num_slots: 0,
            _pad0: 0,
            global_solana_slot: AtomicU64::new(0),
            writer_heartbeat: AtomicU64::new(current_time_ns()),
            writer_pid: AtomicU32::new(std::process::id()),
            ready: AtomicU8::new(1),
            _reserved: [0; 83],
        }
    }

    #[test]
    fn seqlock_single_thread_roundtrip() {
        let header = make_test_slot_header();
        let shm_header = make_test_shm_header();

        seqlock_write_begin(&header);
        assert_eq!(header.seqlock_ver.load(Ordering::Acquire), 1);

        seqlock_write_end(&header);
        assert_eq!(header.seqlock_ver.load(Ordering::Acquire), 2);

        let version = seqlock_read_begin(&header, &shm_header)
            .expect("single-thread read_begin should succeed");
        assert_eq!(version, 2);
        assert!(seqlock_read_validate(&header, version));
    }

    #[test]
    fn seqlock_stress_multi_thread() {
        const WRITER_ITERATIONS: u64 = 10_000;
        const READER_COUNT: usize = 10;

        let header = Arc::new(make_test_slot_header());
        let shm_header = Arc::new(make_test_shm_header());
        let payload =
            Arc::new(SharedPayload { primary: AtomicU64::new(0), mirror: AtomicU64::new(0) });
        let writer_done = Arc::new(AtomicBool::new(false));
        let torn_reads = Arc::new(AtomicU64::new(0));

        let writer_header = Arc::clone(&header);
        let writer_shm_header = Arc::clone(&shm_header);
        let writer_payload = Arc::clone(&payload);
        let writer_done_flag = Arc::clone(&writer_done);

        let writer = thread::spawn(move || {
            for next in 1..=WRITER_ITERATIONS {
                writer_shm_header.writer_heartbeat.store(current_time_ns(), Ordering::Relaxed);
                seqlock_write_begin(&writer_header);
                writer_payload.primary.store(next, Ordering::Relaxed);
                writer_payload.mirror.store(next, Ordering::Relaxed);
                seqlock_write_end(&writer_header);
            }

            writer_shm_header.writer_heartbeat.store(current_time_ns(), Ordering::Relaxed);
            writer_done_flag.store(true, Ordering::Release);
        });

        let mut readers = Vec::with_capacity(READER_COUNT);
        for _ in 0..READER_COUNT {
            let reader_header = Arc::clone(&header);
            let reader_shm_header = Arc::clone(&shm_header);
            let reader_payload = Arc::clone(&payload);
            let reader_done_flag = Arc::clone(&writer_done);
            let reader_torn_reads = Arc::clone(&torn_reads);

            readers.push(thread::spawn(move || {
                let mut stable_reads = 0_u64;

                while stable_reads < WRITER_ITERATIONS {
                    let start_version = seqlock_read_begin(&reader_header, &reader_shm_header)
                        .expect("reader should observe healthy writer");
                    let first = reader_payload.primary.load(Ordering::Relaxed);
                    let second = reader_payload.mirror.load(Ordering::Relaxed);

                    if !seqlock_read_validate(&reader_header, start_version) {
                        continue;
                    }

                    if first != second {
                        reader_torn_reads.fetch_add(1, Ordering::Relaxed);
                    }

                    stable_reads = stable_reads.max(first);

                    if reader_done_flag.load(Ordering::Acquire) && first >= WRITER_ITERATIONS {
                        break;
                    }
                }
            }));
        }

        writer.join().expect("writer thread should finish cleanly");
        for reader in readers {
            reader.join().expect("reader thread should finish cleanly");
        }

        let torn_reads_total = torn_reads.load(Ordering::Acquire);
        println!("10000 iterations, {} torn reads", torn_reads_total);
        assert_eq!(torn_reads_total, 0);
    }

    #[test]
    fn seqlock_crash_detection_writer_stale() {
        let header = make_test_slot_header();
        let shm_header = make_test_shm_header();
        let stale_heartbeat = current_time_ns().saturating_sub(HEARTBEAT_TIMEOUT_NS + 1);

        header.seqlock_ver.store(1, Ordering::Release);
        shm_header.writer_heartbeat.store(stale_heartbeat, Ordering::Relaxed);
        shm_header.writer_pid.store(std::process::id(), Ordering::Relaxed);

        let error = seqlock_read_begin(&header, &shm_header)
            .expect_err("stale writer should be detected while version is odd");
        assert!(matches!(error, ShmError::WriterStale));
    }

    #[test]
    fn seqlock_crash_detection_writer_dead() {
        let header = make_test_slot_header();
        let shm_header = make_test_shm_header();
        let stale_heartbeat = current_time_ns().saturating_sub(HEARTBEAT_TIMEOUT_NS + 1);

        header.seqlock_ver.store(1, Ordering::Release);
        shm_header.writer_heartbeat.store(stale_heartbeat, Ordering::Relaxed);
        shm_header.writer_pid.store(u32::MAX - 1, Ordering::Relaxed);

        let error = seqlock_read_begin(&header, &shm_header)
            .expect_err("dead writer should be detected while version is odd");
        assert!(matches!(error, ShmError::WriterDead(_)));
    }
}
