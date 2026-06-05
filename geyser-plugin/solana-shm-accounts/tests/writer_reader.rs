use std::time::Duration;

use rstest::rstest;
use solana_shm_accounts::error::ShmError;
use solana_shm_accounts::layout::ShmConfig;
use solana_shm_accounts::reader::ShmReader;
use solana_shm_accounts::writer::ShmWriter;
use tempfile::tempdir;

fn test_accounts(count: usize) -> Vec<[u8; 32]> {
    (0..count)
        .map(|index| {
            let mut pubkey = [0_u8; 32];
            pubkey[..8].copy_from_slice(&(index as u64).to_le_bytes());
            pubkey
        })
        .collect()
}

fn owner_for(index: usize) -> [u8; 32] {
    let mut owner = [0_u8; 32];
    owner[..8].copy_from_slice(&((index as u64) + 10_000).to_le_bytes());
    owner
}

fn data_for(index: usize) -> Vec<u8> {
    vec![index as u8; 16 + index]
}

#[rstest]
fn writer_reader_roundtrip() {
    let tempdir = tempdir().expect("tempdir should be created");
    let shm_path = tempdir.path().join("accounts.shm");
    let accounts = test_accounts(10);
    let config = ShmConfig {
        shm_path: shm_path.to_string_lossy().into_owned(),
        accounts: accounts.clone(),
        capacities: vec![128; accounts.len()],
    };

    let writer = ShmWriter::create(&config).expect("writer should create shm");
    for (index, pubkey) in accounts.iter().enumerate() {
        writer
            .write_account(
                pubkey,
                &owner_for(index),
                1_000 + index as u64,
                index % 2 == 0,
                200 + index as u64,
                &data_for(index),
                500 + index as u64,
            )
            .expect("writer should write account");
    }
    writer.set_ready();

    let reader = ShmReader::open(&config.shm_path).expect("reader should open shm");
    reader.wait_ready(Duration::from_millis(200)).expect("reader should observe ready flag");

    for (index, pubkey) in accounts.iter().enumerate() {
        let snapshot = reader.read_account(pubkey).expect("reader should read account");
        assert_eq!(snapshot.owner, owner_for(index));
        assert_eq!(snapshot.lamports, 1_000 + index as u64);
        assert_eq!(snapshot.data, data_for(index));
        assert_eq!(snapshot.executable, index % 2 == 0);
        assert_eq!(snapshot.solana_slot, 500 + index as u64);
    }

    assert_eq!(reader.global_solana_slot(), 509);
}

#[rstest]
fn writer_reader_open_existing() {
    let tempdir = tempdir().expect("tempdir should be created");
    let shm_path = tempdir.path().join("accounts.shm");
    let accounts = test_accounts(2);
    let config = ShmConfig {
        shm_path: shm_path.to_string_lossy().into_owned(),
        accounts: accounts.clone(),
        capacities: vec![128; accounts.len()],
    };

    {
        let writer = ShmWriter::create(&config).expect("writer should create shm");
        writer.set_ready();
    }

    let writer = ShmWriter::open(&config.shm_path).expect("writer should reopen shm");
    writer
        .write_account(&accounts[0], &owner_for(0), 77, true, 88, &data_for(0), 99)
        .expect("writer should update reopened shm");
    writer.set_ready();

    let reader = ShmReader::open(&config.shm_path).expect("reader should open shm");
    let snapshot = reader.read_account(&accounts[0]).expect("reader should read account");
    assert_eq!(snapshot.owner, owner_for(0));
    assert_eq!(snapshot.lamports, 77);
    assert_eq!(snapshot.data, data_for(0));
    assert!(snapshot.executable);
    assert_eq!(snapshot.solana_slot, 99);
}

#[rstest]
fn writer_reader_wait_ready_timeout() {
    let tempdir = tempdir().expect("tempdir should be created");
    let shm_path = tempdir.path().join("accounts.shm");
    let accounts = test_accounts(1);
    let config = ShmConfig {
        shm_path: shm_path.to_string_lossy().into_owned(),
        accounts,
        capacities: vec![64],
    };

    let _writer = ShmWriter::create(&config).expect("writer should create shm");
    let reader = ShmReader::open(&config.shm_path).expect("reader should open shm");
    let error = reader
        .wait_ready(Duration::from_millis(100))
        .expect_err("reader should time out when ready flag is unset");

    assert!(matches!(error, ShmError::NotReady));
}

#[rstest]
fn writer_reader_read_version_incremental() {
    let tempdir = tempdir().expect("tempdir should be created");
    let shm_path = tempdir.path().join("accounts.shm");
    let accounts = test_accounts(1);
    let config = ShmConfig {
        shm_path: shm_path.to_string_lossy().into_owned(),
        accounts: accounts.clone(),
        capacities: vec![128],
    };

    let writer = ShmWriter::create(&config).expect("writer should create shm");
    writer
        .write_account(&accounts[0], &owner_for(0), 10, false, 20, &data_for(0), 30)
        .expect("first write should succeed");
    writer.set_ready();

    let reader = ShmReader::open(&config.shm_path).expect("reader should open shm");
    let first_version = reader.read_version(&accounts[0]).expect("version should be readable");

    writer
        .write_account(&accounts[0], &owner_for(0), 11, true, 21, &data_for(0), 31)
        .expect("second write should succeed");

    let second_version = reader.read_version(&accounts[0]).expect("version should change");
    assert_ne!(first_version, second_version);
}
