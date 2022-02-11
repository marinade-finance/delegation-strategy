use anchor_lang::prelude::Pubkey;
use anyhow::bail;
use log::{error, warn};
use solana_client::{client_error::ClientError, rpc_client::RpcClient};
use solana_sdk::account::Account;

pub trait RpcClientHelpers {
    fn get_account_retrying(&self, account_pubkey: &Pubkey)
        -> Result<Option<Account>, ClientError>;
    fn get_account_data_retrying(&self, account_pubkey: &Pubkey) -> anyhow::Result<Vec<u8>>;
}

impl RpcClientHelpers for RpcClient {
    fn get_account_retrying(
        &self,
        account_pubkey: &Pubkey,
    ) -> Result<Option<Account>, ClientError> {
        Ok(loop {
            match self.get_account_with_commitment(account_pubkey, self.commitment()) {
                Ok(account) => break account,
                Err(err) => warn!("RPC error {}. Retrying", err),
            }
        }
        .value)
    }

    fn get_account_data_retrying(&self, account_pubkey: &Pubkey) -> anyhow::Result<Vec<u8>> {
        if let Some(account) = self.get_account_retrying(account_pubkey)? {
            Ok(account.data)
        } else {
            error!("Can not find account {}", account_pubkey);
            bail!("Can not find account {}", account_pubkey);
        }
    }
}
