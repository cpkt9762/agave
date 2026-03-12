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

type TxPreAccounts = Vec<(Pubkey, Vec<u8>)>;
type BatchPreAccounts = Vec<TxPreAccounts>;

// Trait for operating cleanly over Option<PreAccountsCollector>
pub(crate) trait PreAccountsCollectionRoutines {
    fn collect_pre_accounts<CB: TransactionProcessingCallback>(
        &mut self,
        account_loader: &mut AccountLoader<CB>,
        transaction: &impl SVMTransaction,
        program_ids: &HashSet<Pubkey>,
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
        program_ids: &HashSet<Pubkey>,
    ) {
        let has_matching_program = transaction
            .program_instructions_iter()
            .any(|(program_id, _)| program_ids.contains(program_id));

        if !has_matching_program {
            self.push_empty();
            return;
        }

        let mut tx_pre_accounts = vec![];
        for (index, key) in transaction.account_keys().iter().enumerate() {
            if transaction.is_writable(index) && !transaction.is_invoked(index) {
                if let Some(account) = account_loader.load_account(key) {
                    tx_pre_accounts.push((*key, account.data().to_vec()));
                }
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
        program_ids: &HashSet<Pubkey>,
    ) {
        if let Some(inner) = self {
            inner.collect_pre_accounts(account_loader, transaction, program_ids)
        }
    }

    fn push_empty(&mut self) {
        if let Some(inner) = self {
            inner.push_empty()
        }
    }
}
