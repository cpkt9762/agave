use {
    super::*,
    crate::{
        account_loader::AccountLoader,
        pre_accounts_collector::{PreAccountsCollectionRoutines, PreAccountsCollector},
    },
    solana_account::AccountSharedData,
    solana_instruction::{AccountMeta, Instruction},
    solana_message::Message,
    solana_svm_feature_set::SVMFeatureSet,
    solana_transaction::{Transaction, sanitized::SanitizedTransaction},
    std::{cell::RefCell, collections::HashMap},
    test_case::test_case,
};

struct Reader(RefCell<HashMap<Pubkey, AccountSharedData>>);
impl TransactionProcessingCallback for Reader {
    fn get_account_shared_data(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.0
            .borrow()
            .get(key)
            .cloned()
            .map(|account| (account, 0))
    }
}

fn pool_data() -> Vec<u8> {
    let mut data = vec![0; 653];
    data[..8].copy_from_slice(&WHIRLPOOL_DISCRIMINATOR);
    data[41..43].copy_from_slice(&1u16.to_le_bytes());
    data
}

fn pool_account() -> AccountSharedData {
    let mut account = AccountSharedData::new(1_000_000_000, 653, &WHIRLPOOL);
    account.set_data_from_slice(&pool_data());
    account
}

fn transaction(pools: &[Pubkey], writable: bool, extra: &[Pubkey]) -> SanitizedTransaction {
    let mut metas: Vec<_> = pools
        .iter()
        .map(|key| {
            if writable {
                AccountMeta::new(*key, false)
            } else {
                AccountMeta::new_readonly(*key, false)
            }
        })
        .collect();
    metas.extend(
        extra
            .iter()
            .map(|key| AccountMeta::new_readonly(*key, false)),
    );
    SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(Message::new(
        &[Instruction::new_with_bytes(
            solana_sdk_ids::system_program::id(),
            &[],
            metas,
        )],
        Some(&Pubkey::new_unique()),
    )))
}

#[test_case(false, true, true, false; "disabled")]
#[test_case(true, false, true, false; "readonly_pool")]
#[test_case(true, true, false, false; "unselected")]
#[test_case(true, true, true, true; "selected_writable")]
fn pre_accounts_derived_selection(enabled: bool, writable: bool, selected: bool, expected: bool) {
    let pool = Pubkey::new_unique();
    let reader = Reader(RefCell::new(HashMap::from([(pool, pool_account())])));
    let features = SVMFeatureSet::default();
    let mut loader = AccountLoader::new_with_loaded_accounts_capacity(None, &reader, &features, 4);
    let tx = transaction(&[pool], writable, &[]);
    let ids = HashSet::from([solana_sdk_ids::system_program::id()]);
    let config = PreAccountsDerivationConfig {
        whirlpool: enabled,
        ..Default::default()
    };
    let mut collector = PreAccountsCollector::new_with_transaction_count(1);
    collector.collect_pre_accounts(&mut loader, &tx, selected.then_some(&ids), None, &config);
    let entries = collector.into_vecs().remove(0);
    assert_eq!(
        entries.len(),
        usize::from(selected) + if expected { 11 } else { 0 }
    );
    if expected {
        for entry in entries.iter().filter(|entry| entry.pubkey != pool) {
            assert_eq!(entry.owner, solana_sdk_ids::system_program::id());
            assert_eq!(entry.lamports, 0);
            assert_eq!(entry.rent_epoch, 0);
            assert!(entry.data.is_empty());
            assert!(!entry.executable);
        }
    }
}

#[test_case(10, usize::MAX; "keys")]
#[test_case(32, 10 * std::mem::size_of::<PreAccountEntry>(); "bytes")]
fn pre_accounts_derived_rejects_whole_pool(max_keys: usize, max_bytes: usize) {
    let pool = Pubkey::new_unique();
    let reader = Reader(RefCell::new(HashMap::from([(pool, pool_account())])));
    let mut entries = Vec::new();
    append_derived_accounts(
        &reader,
        &transaction(&[pool], true, &[]),
        &mut entries,
        &PreAccountsDerivationConfig {
            whirlpool: true,
            max_keys,
            max_bytes,
            ..Default::default()
        },
    );
    assert!(entries.is_empty());
}

#[test_case(10, usize::MAX; "keys")]
#[test_case(32, 10 * std::mem::size_of::<PreAccountEntry>(); "bytes")]
fn pre_accounts_derived_dedup_and_transaction_budget(max_keys: usize, max_bytes: usize) {
    let pools = [Pubkey::new_unique(), Pubkey::new_unique()];
    let reader = Reader(RefCell::new(
        pools.map(|pool| (pool, pool_account())).into(),
    ));
    let existing = whirlpool_keys(&pools[0], &pool_data()).expect("known pool layout")[0];
    let mut entries = Vec::new();
    append_derived_accounts(
        &reader,
        &transaction(&pools, true, &[existing]),
        &mut entries,
        &PreAccountsDerivationConfig {
            whirlpool: true,
            max_keys,
            max_bytes,
            ..Default::default()
        },
    );
    assert_eq!(
        entries.len(),
        10,
        "first pool fits after dedup; second rejected in full"
    );
    assert!(!entries.iter().any(|entry| entry.pubkey == existing));
    let expected: HashSet<_> = whirlpool_keys(&pools[0], &pool_data())
        .expect("pool")
        .into_iter()
        .filter(|key| *key != existing)
        .collect();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.pubkey)
            .collect::<HashSet<_>>(),
        expected
    );
}

#[test]
fn pre_accounts_derived_callback_does_not_insert_absence_or_existing_accounts() {
    let pool = Pubkey::new_unique();
    let keys = whirlpool_keys(&pool, &pool_data()).expect("pool layout");
    let reader = Reader(RefCell::new(HashMap::from([
        (pool, pool_account()),
        (keys[1], AccountSharedData::new(100, 0, &WHIRLPOOL)),
    ])));
    let features = SVMFeatureSet::default();
    let mut loader = AccountLoader::new_with_loaded_accounts_capacity(None, &reader, &features, 4);
    let mut entries = Vec::new();
    append_derived_accounts(
        &loader,
        &transaction(&[pool], true, &[]),
        &mut entries,
        &PreAccountsDerivationConfig {
            whirlpool: true,
            ..Default::default()
        },
    );
    assert_eq!(entries.len(), 11);
    for key in &keys[..2] {
        reader
            .0
            .borrow_mut()
            .insert(*key, AccountSharedData::new(200, 0, &WHIRLPOOL));
        assert_eq!(
            loader
                .load_account(key)
                .expect("fresh Bank value remains loadable")
                .lamports(),
            200,
            "neither absence nor present extra account was inserted into loader cache"
        );
    }
}

#[test]
fn pre_accounts_derived_unknown_layout_is_skipped() {
    let pool = Pubkey::new_unique();
    let config = PreAccountsDerivationConfig {
        dlmm: true,
        whirlpool: true,
        ..Default::default()
    };
    assert!(derived_keys(&pool, &DLMM, &pool_data(), &config).is_none());
    let mut data = pool_data();
    data[0] ^= 1;
    assert!(derived_keys(&pool, &WHIRLPOOL, &data, &config).is_none());
    assert!(derived_keys(&pool, &WHIRLPOOL, &pool_data()[..652], &config).is_none());
    data = pool_data();
    data[41..43].fill(0);
    assert!(derived_keys(&pool, &WHIRLPOOL, &data, &config).is_none());
    assert!(derived_keys(&pool, &Pubkey::new_unique(), &pool_data(), &config).is_none());
}

#[test]
fn pre_accounts_derived_real_pool_addresses_match_exporter() {
    let config = PreAccountsDerivationConfig {
        dlmm: true,
        whirlpool: true,
        ..Default::default()
    };
    let fixtures: &[(&str, &str, &[u8], &str)] = &[
        (
            "8Z9m9PzPSMJymPHY7Rsbye4QCJHzxTrban5kcgujc6bw",
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
            include_bytes!("../tests/fixtures/pre_accounts_derived/whirlpool0.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/whirlpool0.keys"),
        ),
        (
            "G7gMCLqY8Jp76DuLM2v9p3oUk5tftjreX7zkM5m1UXbA",
            "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
            include_bytes!("../tests/fixtures/pre_accounts_derived/dlmm0.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/dlmm0.keys"),
        ),
        (
            "G8YJWXiVkzAB295NePJMomvCeSj3eWwgtpQyK6nHddLQ",
            "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
            include_bytes!("../tests/fixtures/pre_accounts_derived/dlmm1.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/dlmm1.keys"),
        ),
        (
            "2H2TNJuqivWWM87oZF3pgBa7KhH3ugo6qea5rjkFey2b",
            "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
            include_bytes!("../tests/fixtures/pre_accounts_derived/dlmm2.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/dlmm2.keys"),
        ),
        (
            "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
            include_bytes!("../tests/fixtures/pre_accounts_derived/whirlpool1.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/whirlpool1.keys"),
        ),
        (
            "J5jzvT22u1Mt6de4gkBhEsTSTjBfYS7A6aF5jzu9ihkC",
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
            include_bytes!("../tests/fixtures/pre_accounts_derived/whirlpool2.bin"),
            include_str!("../tests/fixtures/pre_accounts_derived/whirlpool2.keys"),
        ),
    ];
    for (pool, owner, data, expected) in fixtures {
        let pool: Pubkey = pool.parse().expect("fixture pool address");
        let owner: Pubkey = owner.parse().expect("fixture program address");
        let actual: HashSet<_> = derived_keys(&pool, &owner, data, &config)
            .expect("known real pool layout")
            .into_iter()
            .collect();
        let expected: HashSet<Pubkey> = expected
            .lines()
            .map(|key| key.parse().expect("exporter address"))
            .collect();
        assert_eq!(actual, expected, "same prestate pool {pool}");
    }
}

#[test]
fn pre_accounts_derived_disabled_preserves_base_entries() {
    let pool = Pubkey::new_unique();
    let reader = Reader(RefCell::new(HashMap::from([(pool, pool_account())])));
    let tx = transaction(&[pool], true, &[]);
    let features = SVMFeatureSet::default();
    let ids = HashSet::from([solana_sdk_ids::system_program::id()]);
    for config in [
        PreAccountsDerivationConfig::default(),
        PreAccountsDerivationConfig {
            max_keys: 0,
            max_bytes: 0,
            ..Default::default()
        },
    ] {
        let mut loader =
            AccountLoader::new_with_loaded_accounts_capacity(None, &reader, &features, 4);
        let mut collector = PreAccountsCollector::new_with_transaction_count(1);
        collector.collect_pre_accounts(&mut loader, &tx, Some(&ids), None, &config);
        let snapshots = collector.into_vecs();
        assert_eq!(
            snapshots[0].len(),
            1,
            "B3 captures exactly the existing non-program pool account"
        );
        let entry = &snapshots[0][0];
        assert_eq!(
            (
                entry.pubkey,
                entry.owner,
                entry.lamports,
                entry.data.as_slice(),
                entry.executable,
                entry.rent_epoch
            ),
            (
                pool,
                WHIRLPOOL,
                1_000_000_000,
                pool_data().as_slice(),
                false,
                0
            )
        );
    }
}
