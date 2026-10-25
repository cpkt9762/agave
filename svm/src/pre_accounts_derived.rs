//! Bounded, opt-in copies of pool-derived accounts. These reads must not populate
//! AccountLoader: an unlocked extra key in its cache could affect later loading.
use {
    crate::{
        pre_accounts_collector::PreAccountEntry,
        transaction_processing_callback::TransactionProcessingCallback,
    },
    solana_account::ReadableAccount,
    solana_pubkey::{Pubkey, pubkey},
    solana_svm_transaction::svm_transaction::SVMTransaction,
    std::{
        collections::{BTreeSet, HashSet},
        sync::atomic::{AtomicU64, Ordering},
    },
};

// Copied from metis-rs/src/amm/program_ids.rs, not inferred from transaction data.
const DLMM: Pubkey = pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const WHIRLPOOL: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
const DLMM_DISCRIMINATOR: [u8; 8] = [0x21, 0x0b, 0x31, 0x62, 0xb5, 0x65, 0xb1, 0x0d];
const WHIRLPOOL_DISCRIMINATOR: [u8; 8] = [0x3f, 0x95, 0xd1, 0x0c, 0xe1, 0x80, 0x63, 0x09];
static KEY_LIMIT_POOLS: AtomicU64 = AtomicU64::new(0);
static BYTE_LIMIT_POOLS: AtomicU64 = AtomicU64::new(0);

/// Additional-account budget per selected transaction, independent of its own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreAccountsDerivationConfig {
    pub dlmm: bool,
    pub whirlpool: bool,
    pub max_keys: usize,
    pub max_bytes: usize,
}

impl Default for PreAccountsDerivationConfig {
    fn default() -> Self {
        Self {
            dlmm: false,
            whirlpool: false,
            max_keys: 32,
            max_bytes: 128 * 1024,
        }
    }
}

impl PreAccountsDerivationConfig {
    pub fn enabled(&self) -> bool {
        self.dlmm || self.whirlpool
    }
}

/// Monotonic process counters; rejection logs are emitted at powers of two.
pub fn pre_accounts_derived_rejected_pools() -> (u64, u64) {
    (
        KEY_LIMIT_POOLS.load(Ordering::Relaxed),
        BYTE_LIMIT_POOLS.load(Ordering::Relaxed),
    )
}

fn reject_pool(counter: &AtomicU64, reason: &str) {
    let count = counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if count.is_power_of_two() {
        log::warn!("pre_accounts_derived: rejected whole pool: {reason}; cumulative pools={count}");
    }
}

fn read_i32(data: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn bitmap_set(data: &[u8], index: i32) -> Option<bool> {
    let bit = usize::try_from(index.checked_add(512)?).ok()?;
    let offset = 8 + 576 + bit / 64 * 8;
    let word = u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?);
    Some(word & (1u64 << (bit % 64)) != 0)
}

fn dlmm_keys(pool: &Pubkey, data: &[u8]) -> Option<Vec<Pubkey>> {
    if data.len() != 904
        || data.get(..8)? != DLMM_DISCRIMINATOR
        || *data.get(8 + 27)? > 2
        || *data.get(8 + 28)? > 1
    {
        return None;
    }
    let active = read_i32(data, 8 + 68)?;
    if !(-443_636..=443_636).contains(&active) {
        return None;
    }
    let center = active.div_euclid(70);
    let mut indexes = BTreeSet::new();
    // Match adapter_accounts_export's collection envelope: default subscription
    // for the current and both adjacent centers, including two nearest set
    // internal bitmap entries on each side. No extension traversal is invented.
    for adjacent in -1..=1 {
        let start = center + adjacent - 2;
        let end = center + adjacent + 2;
        indexes.extend(start..=end);
        for (mut index, step) in [
            (start.saturating_sub(1).min(511), -1),
            (end.saturating_add(1).max(-512), 1),
        ] {
            let mut found = 0;
            while (-512..=511).contains(&index) && found < 2 {
                if bitmap_set(data, index)? {
                    indexes.insert(index);
                    found += 1;
                }
                index += step;
            }
        }
    }
    let mut keys = Vec::with_capacity(indexes.len() + 1);
    for index in indexes {
        keys.push(
            Pubkey::try_find_program_address(
                &[b"bin_array", pool.as_ref(), &i64::from(index).to_le_bytes()],
                &DLMM,
            )?
            .0,
        );
    }
    keys.push(Pubkey::try_find_program_address(&[b"bitmap", pool.as_ref()], &DLMM)?.0);
    Some(keys)
}

fn whirlpool_keys(pool: &Pubkey, data: &[u8]) -> Option<Vec<Pubkey>> {
    if data.len() != 653 || data.get(..8)? != WHIRLPOOL_DISCRIMINATOR {
        return None;
    }
    let spacing = u16::from_le_bytes(data.get(41..43)?.try_into().ok()?);
    let tick = read_i32(data, 81)?;
    if spacing == 0 || !(-443_636..=443_636).contains(&tick) {
        return None;
    }
    let step = i32::from(spacing) * 88;
    let start = tick.div_euclid(step) * step;
    // Six arrays per direction, with the active array shared: eleven keys.
    let mut keys = Vec::with_capacity(11);
    for offset in -5..=5 {
        let index = (start + offset * step).to_string();
        keys.push(
            Pubkey::try_find_program_address(
                &[b"tick_array", pool.as_ref(), index.as_bytes()],
                &WHIRLPOOL,
            )?
            .0,
        );
    }
    Some(keys)
}

pub(crate) fn derived_keys(
    pool: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
    config: &PreAccountsDerivationConfig,
) -> Option<Vec<Pubkey>> {
    if config.dlmm && *owner == DLMM {
        dlmm_keys(pool, data)
    } else if config.whirlpool && *owner == WHIRLPOOL {
        whirlpool_keys(pool, data)
    } else {
        None
    }
}

/// `reader` must be AccountLoader's read-only callback implementation, not Bank
/// and not load_account. It observes earlier batch writes and tombstones without
/// inserting extra keys. The caller supplies only transactions with valid locks.
pub(crate) fn append_derived_accounts(
    reader: &impl TransactionProcessingCallback,
    transaction: &impl SVMTransaction,
    entries: &mut Vec<PreAccountEntry>,
    config: &PreAccountsDerivationConfig,
) {
    if !config.enabled() {
        return;
    }
    let mut seen: HashSet<_> = transaction.account_keys().iter().copied().collect();
    let mut used_keys = 0usize;
    let mut used_bytes = 0usize;
    for (index, pool) in transaction.account_keys().iter().enumerate() {
        // Pool write lock serializes protocol data writes in other batches.
        // Same-batch writes are accounted for by the read-only loader callback.
        if !transaction.is_writable(index) {
            continue;
        }
        let Some((account, _)) = reader.get_account_shared_data(pool) else {
            continue;
        };
        let Some(keys) = derived_keys(pool, account.owner(), account.data(), config) else {
            continue;
        };
        let keys: Vec<_> = keys.into_iter().filter(|key| !seen.contains(key)).collect();
        if keys.len() > config.max_keys.saturating_sub(used_keys) {
            reject_pool(&KEY_LIMIT_POOLS, "key limit");
            continue;
        }
        // Keep shared account data until the whole pool passes its byte budget.
        // This avoids allocating/copying a huge data buffer just to reject it.
        let mut pending = Vec::with_capacity(keys.len());
        let mut pool_bytes = 0usize;
        let mut over_budget = false;
        for key in keys {
            let account = reader
                .get_account_shared_data(&key)
                .map(|(account, _)| account)
                .filter(|account| account.lamports() != 0);
            let bytes = std::mem::size_of::<PreAccountEntry>()
                .saturating_add(account.as_ref().map_or(0, |account| account.data().len()));
            pool_bytes = pool_bytes.saturating_add(bytes);
            if pool_bytes > config.max_bytes.saturating_sub(used_bytes) {
                over_budget = true;
                break;
            }
            pending.push((key, account));
        }
        if over_budget {
            reject_pool(&BYTE_LIMIT_POOLS, "byte limit");
            continue;
        }
        used_keys += pending.len();
        used_bytes += pool_bytes;
        for (pubkey, account) in pending {
            seen.insert(pubkey);
            let entry = if let Some(account) = account {
                PreAccountEntry {
                    pubkey,
                    owner: *account.owner(),
                    lamports: account.lamports(),
                    data: account.data().to_vec(),
                    executable: account.executable(),
                    rent_epoch: account.rent_epoch(),
                }
            } else {
                PreAccountEntry {
                    pubkey,
                    owner: solana_sdk_ids::system_program::id(),
                    lamports: 0,
                    data: Vec::new(),
                    executable: false,
                    rent_epoch: 0,
                }
            };
            entries.push(entry);
        }
    }
}

#[cfg(test)]
#[path = "pre_accounts_derived_tests.rs"]
mod tests;
