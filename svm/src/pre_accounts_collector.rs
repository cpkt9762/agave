use {
    crate::{
        account_loader::AccountLoader,
        transaction_processing_callback::TransactionProcessingCallback,
    },
    solana_account::ReadableAccount,
    solana_pubkey::Pubkey,
    solana_svm_transaction::svm_transaction::SVMTransaction,
    std::collections::HashSet,
};

/// Pre-execution account state captured before SVM transaction execution.
#[derive(Debug, Clone)]
pub struct PreAccountEntry {
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
    pub executable: bool,
    pub rent_epoch: u64,
}

type TxPreAccounts = Vec<PreAccountEntry>;
type BatchPreAccounts = Vec<TxPreAccounts>;

// Trait for operating cleanly over Option<PreAccountsCollector>
pub(crate) trait PreAccountsCollectionRoutines {
    fn collect_pre_accounts<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        program_ids: Option<&HashSet<Pubkey>>,
        cpi_program_ids: Option<&HashSet<Pubkey>>,
    );

    fn push_empty(&mut self);
}

#[derive(Debug, Default)]
pub struct PreAccountsCollector {
    pre_accounts: BatchPreAccounts,
}

impl PreAccountsCollector {
    pub(crate) fn new_with_transaction_count(transaction_count: usize) -> Self {
        Self {
            pre_accounts: Vec::with_capacity(transaction_count),
        }
    }

    pub fn into_vecs(self) -> BatchPreAccounts {
        self.pre_accounts
    }

    pub(crate) fn lengths_match_expected(&self, expected_len: usize) -> bool {
        self.pre_accounts.len() == expected_len
    }
}

impl PreAccountsCollectionRoutines for PreAccountsCollector {
    fn collect_pre_accounts<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        program_ids: Option<&HashSet<Pubkey>>,
        cpi_program_ids: Option<&HashSet<Pubkey>>,
    ) {
        let has_matching_program = program_ids.is_some_and(|ids| {
            transaction
                .program_instructions_iter()
                .any(|(program_id, _)| ids.contains(program_id))
        }) || cpi_program_ids.is_some_and(|ids| {
            // account_keys includes resolved ALT addresses. Presence is sufficient:
            // this policy deliberately also captures keys not invoked at runtime.
            !ids.is_empty()
                && transaction
                    .account_keys()
                    .iter()
                    .any(|key| ids.contains(key))
        });

        if !has_matching_program {
            self.push_empty();
            return;
        }

        let mut tx_pre_accounts = vec![];
        for (index, key) in transaction.account_keys().iter().enumerate() {
            if !transaction.is_invoked(index)
                && let Some(account) = account_loader.load_account(key)
            {
                tx_pre_accounts.push(PreAccountEntry {
                    pubkey: *key,
                    owner: *account.owner(),
                    lamports: account.lamports(),
                    data: account.data().to_vec(),
                    executable: account.executable(),
                    rent_epoch: account.rent_epoch(),
                });
            }
        }

        self.pre_accounts.push(tx_pre_accounts);
    }

    fn push_empty(&mut self) {
        self.pre_accounts.push(vec![]);
    }
}

impl PreAccountsCollectionRoutines for Option<PreAccountsCollector> {
    fn collect_pre_accounts<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        program_ids: Option<&HashSet<Pubkey>>,
        cpi_program_ids: Option<&HashSet<Pubkey>>,
    ) {
        if let Some(inner) = self {
            inner.collect_pre_accounts(account_loader, transaction, program_ids, cpi_program_ids)
        }
    }

    fn push_empty(&mut self) {
        if let Some(inner) = self {
            inner.push_empty()
        }
    }
}

/// Whether either independently configured snapshot selection policy is active.
pub fn pre_accounts_enabled(
    program_ids: Option<&HashSet<Pubkey>>,
    cpi_program_ids: Option<&HashSet<Pubkey>>,
) -> bool {
    program_ids.is_some_and(|ids| !ids.is_empty())
        || cpi_program_ids.is_some_and(|ids| !ids.is_empty())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::{
            Message, SanitizedMessage,
            v0::{LoadedAddresses, LoadedMessage},
        },
        solana_signature::Signature,
        solana_svm_feature_set::SVMFeatureSet,
        solana_svm_transaction::svm_message::SVMMessage,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::borrow::Cow,
        test_case::test_case,
    };

    struct Callbacks(Pubkey, AccountSharedData);

    impl TransactionProcessingCallback for Callbacks {
        fn get_account_shared_data(&self, key: &Pubkey) -> Option<(AccountSharedData, u64)> {
            (*key == self.0).then(|| (self.1.clone(), 0))
        }
    }

    // The target key is an instruction account, not a top-level program. It need
    // not actually be invoked: account-key presence is precisely the B3 policy.
    #[test_case(false, false, false, false; "default_none")]
    #[test_case(false, false, true, false; "default_empty")]
    #[test_case(true, false, false, true; "top_level_only")]
    #[test_case(false, true, false, true; "cpi_key_only_not_invoked")]
    #[test_case(true, true, false, true; "both_copy_once")]
    fn test_pre_accounts_cpi_selection(top: bool, cpi: bool, empty: bool, expected: bool) {
        let payer = Pubkey::new_unique();
        let target = Pubkey::new_unique();
        let outer = Pubkey::new_unique();
        let transaction = SanitizedTransaction::from_transaction_for_tests(
            Transaction::new_unsigned(Message::new(
                &[Instruction::new_with_bytes(
                    outer,
                    &[],
                    vec![AccountMeta::new_readonly(target, false)],
                )],
                Some(&payer),
            )),
        );
        let mut account = AccountSharedData::new(42, 3, &outer);
        account.set_data_from_slice(&[1, 2, 3]);
        let callbacks = Callbacks(payer, account.clone());
        let features = SVMFeatureSet::default();
        let mut loader =
            AccountLoader::new_with_loaded_accounts_capacity(None, &callbacks, &features, 3);
        let top_ids = HashSet::from([outer]);
        let cpi_ids = if empty {
            HashSet::new()
        } else {
            HashSet::from([target])
        };
        let mut collector = PreAccountsCollector::new_with_transaction_count(2);
        collector.collect_pre_accounts(
            &mut loader,
            &transaction,
            top.then_some(&top_ids),
            (cpi || empty).then_some(&cpi_ids),
        );
        // An unregistered key never matches and must still append an empty vec.
        let other_ids = HashSet::from([Pubkey::new_unique()]);
        collector.collect_pre_accounts(&mut loader, &transaction, None, Some(&other_ids));
        let snapshots = collector.into_vecs();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[1].is_empty());
        assert_eq!(snapshots[0].len(), usize::from(expected));
        if expected {
            let entry = &snapshots[0][0];
            assert_eq!(entry.pubkey, payer);
            assert_eq!(entry.owner, *account.owner());
            assert_eq!(entry.lamports, account.lamports());
            assert_eq!(entry.data, account.data());
            assert_eq!(entry.executable, account.executable());
            assert_eq!(entry.rent_epoch, account.rent_epoch());
        }
    }

    #[test]
    fn test_pre_accounts_cpi_alt_loaded_key() {
        let payer = Pubkey::new_unique();
        let outer = Pubkey::new_unique();
        let target = Pubkey::new_unique();
        let message = SanitizedMessage::V0(LoadedMessage {
            message: Cow::Owned(solana_message::v0::Message {
                header: solana_message::MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                account_keys: vec![payer, outer],
                recent_blockhash: Hash::default(),
                instructions: vec![solana_message::compiled_instruction::CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                }],
                address_table_lookups: vec![solana_message::v0::MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![],
                    readonly_indexes: vec![0],
                }],
            }),
            loaded_addresses: Cow::Owned(LoadedAddresses {
                writable: vec![],
                readonly: vec![target],
            }),
            is_writable_account_cache: vec![true, false, false],
        });
        let transaction = SanitizedTransaction::try_new_from_fields(
            message,
            Hash::default(),
            false,
            vec![Signature::default()],
        )
        .expect("valid resolved v0 transaction");
        assert!(
            !transaction
                .message()
                .static_account_keys()
                .contains(&target)
        );
        assert!(transaction.account_keys().iter().any(|key| key == &target));
        let callbacks = Callbacks(payer, AccountSharedData::new(42, 0, &outer));
        let features = SVMFeatureSet::default();
        let mut loader =
            AccountLoader::new_with_loaded_accounts_capacity(None, &callbacks, &features, 3);
        let mut collector = PreAccountsCollector::new_with_transaction_count(1);
        collector.collect_pre_accounts(
            &mut loader,
            &transaction,
            None,
            Some(&HashSet::from([target])),
        );
        assert_eq!(collector.into_vecs()[0][0].lamports, 42);
    }

    #[test_case(false, false, false; "neither")]
    #[test_case(true, false, true; "top_only")]
    #[test_case(false, true, true; "cpi_only")]
    #[test_case(true, true, true; "both")]
    fn test_pre_accounts_cpi_enablement(top: bool, cpi: bool, expected: bool) {
        let ids = HashSet::from([Pubkey::new_unique()]);
        assert_eq!(
            pre_accounts_enabled(top.then_some(&ids), cpi.then_some(&ids)),
            expected
        );
        assert!(!pre_accounts_enabled(
            Some(&HashSet::new()),
            Some(&HashSet::new())
        ));
    }
}
