use {
    log::*,
    solana_client::{
        rpc_client::RpcClient,
        rpc_response::{RpcVoteAccountInfo, RpcVoteAccountStatus},
    },
    solana_sdk::{clock::Epoch, pubkey::Pubkey},
    std::{collections::HashMap, error, process, str::FromStr, time::Duration},
};

pub struct VoteAccountInfo {
    pub identity: Pubkey,
    pub vote_address: Pubkey,
    pub commission: u8,
    pub active_stake: u64,

    /// Credits earned in the epoch
    pub epoch_credits: u64,
}

pub fn get_vote_account_info(
    rpc_client: &RpcClient,
    epoch: Epoch,
) -> Result<(Vec<VoteAccountInfo>, u64), Box<dyn error::Error>> {
    let RpcVoteAccountStatus {
        current,
        delinquent,
    } = rpc_client.get_vote_accounts()?;

    let mut latest_vote_account_info = HashMap::<String, _>::new();

    let mut total_active_stake = 0;
    for vote_account_info in current.into_iter().chain(delinquent.into_iter()) {
        total_active_stake += vote_account_info.activated_stake;

        let entry = latest_vote_account_info
            .entry(vote_account_info.node_pubkey.clone())
            .or_insert_with(|| vote_account_info.clone());

        // If the validator has multiple staked vote accounts then select the vote account that
        // voted most recently
        if entry.last_vote < vote_account_info.last_vote {
            *entry = vote_account_info.clone();
        }
    }

    Ok((
        latest_vote_account_info
            .values()
            .map(
                |RpcVoteAccountInfo {
                     commission,
                     node_pubkey,
                     vote_pubkey,
                     epoch_credits,
                     activated_stake,
                     ..
                 }| {
                    let epoch_credits = if let Some((_last_epoch, credits, prev_credits)) =
                        epoch_credits.iter().find(|ec| ec.0 == epoch)
                    {
                        credits.saturating_sub(*prev_credits)
                    } else {
                        0
                    };
                    let identity = Pubkey::from_str(node_pubkey).unwrap();
                    let vote_address = Pubkey::from_str(vote_pubkey).unwrap();

                    VoteAccountInfo {
                        identity,
                        vote_address,
                        active_stake: *activated_stake,
                        commission: *commission,
                        epoch_credits,
                    }
                },
            )
            .collect(),
        total_active_stake,
    ))
}

pub fn rpc_client_health_check(rpc_client: &RpcClient) -> () {
    let mut retries = 12u8;
    let retry_delay = Duration::from_secs(10);
    loop {
        match rpc_client.get_health() {
            Ok(()) => {
                info!("RPC endpoint healthy");
                break;
            }
            Err(err) => {
                warn!("RPC endpoint is unhealthy: {:?}", err);
            }
        }
        if retries == 0 {
            process::exit(1);
        }
        retries = retries.saturating_sub(1);
        info!(
            "{} retries remaining, sleeping for {} seconds",
            retries,
            retry_delay.as_secs()
        );
        std::thread::sleep(retry_delay);
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        borsh::BorshSerialize,
        indicatif::{ProgressBar, ProgressStyle},
        solana_client::client_error,
        solana_sdk::{
            borsh::get_packed_len,
            clock::Epoch,
            program_pack::Pack,
            pubkey::Pubkey,
            stake::{
                instruction as stake_instruction,
                state::{Authorized, Lockup},
            },
            system_instruction,
        },
        solana_vote_program::{vote_instruction, vote_state::VoteInit},
        spl_stake_pool::{
            find_stake_program_address, find_withdraw_authority_program_address,
            state::{Fee, StakePool, ValidatorList},
        },
        spl_token::state::{Account, Mint},
    };

    fn new_spinner_progress_bar() -> ProgressBar {
        let progress_bar = ProgressBar::new(42);
        progress_bar
            .set_style(ProgressStyle::default_spinner().template("{spinner:.green} {wide_msg}"));
        progress_bar.enable_steady_tick(100);
        progress_bar
    }

    pub fn wait_for_next_epoch(rpc_client: &RpcClient) -> client_error::Result<Epoch> {
        let current_epoch = rpc_client.get_epoch_info()?.epoch;

        let progress_bar = new_spinner_progress_bar();
        loop {
            let epoch_info = rpc_client.get_epoch_info()?;
            if epoch_info.epoch > current_epoch {
                return Ok(epoch_info.epoch);
            }
            progress_bar.set_message(&format!(
                "Waiting for epoch {} ({} slots remaining)",
                current_epoch + 1,
                epoch_info
                    .slots_in_epoch
                    .saturating_sub(epoch_info.slot_index),
            ));

            sleep(Duration::from_millis(200));
        }
    }

    pub fn create_vote_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        identity_keypair: &Keypair,
        vote_keypair: &Keypair,
    ) -> client_error::Result<()> {
        let mut transaction = Transaction::new_with_payer(
            &vote_instruction::create_account(
                &payer.pubkey(),
                &vote_keypair.pubkey(),
                &VoteInit {
                    node_pubkey: identity_keypair.pubkey(),
                    authorized_voter: identity_keypair.pubkey(),
                    authorized_withdrawer: identity_keypair.pubkey(),
                    commission: 10,
                },
                sol_to_lamports(1.),
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, identity_keypair, vote_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn create_stake_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        authority: &Pubkey,
        amount: u64,
    ) -> client_error::Result<Keypair> {
        let stake_keypair = Keypair::new();
        let mut transaction = Transaction::new_with_payer(
            &stake_instruction::create_account(
                &payer.pubkey(),
                &stake_keypair.pubkey(),
                &Authorized::auto(authority),
                &Lockup::default(),
                amount,
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, &stake_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| stake_keypair)
    }

    pub fn delegate_stake(
        rpc_client: &RpcClient,
        authority: &Keypair,
        stake_address: &Pubkey,
        vote_address: &Pubkey,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[stake_instruction::delegate_stake(
                stake_address,
                &authority.pubkey(),
                vote_address,
            )],
            Some(&authority.pubkey()),
            &[authority],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub struct ValidatorAddressPair {
        pub identity: Pubkey,
        pub vote_address: Pubkey,
    }

    pub fn create_validators(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        num_validators: u32,
    ) -> client_error::Result<Vec<ValidatorAddressPair>> {
        let mut validators = vec![];

        for _ in 0..num_validators {
            let identity_keypair = Keypair::new();
            let vote_keypair = Keypair::new();

            create_vote_account(
                rpc_client,
                authorized_staker,
                &identity_keypair,
                &vote_keypair,
            )?;

            validators.push(ValidatorAddressPair {
                identity: identity_keypair.pubkey(),
                vote_address: vote_keypair.pubkey(),
            });
        }

        Ok(validators)
    }

    pub fn create_mint(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        manager: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let mint_rent = rpc_client.get_minimum_balance_for_rent_exemption(Mint::LEN)?;
        let mint_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &mint_keypair.pubkey(),
                    mint_rent,
                    Mint::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_mint(
                    &spl_token::id(),
                    &mint_keypair.pubkey(),
                    manager,
                    None,
                    0,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &mint_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| mint_keypair.pubkey())
    }

    pub fn create_token_account(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let account_rent = rpc_client.get_minimum_balance_for_rent_exemption(Account::LEN)?;
        let account_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &account_keypair.pubkey(),
                    account_rent,
                    Account::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_account(
                    &spl_token::id(),
                    &account_keypair.pubkey(),
                    mint,
                    owner,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &account_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| account_keypair.pubkey())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_stake_pool(
        rpc_client: &RpcClient,
        payer: &Keypair,
        stake_pool: &Keypair,
        reserve_stake: &Pubkey,
        pool_mint: &Pubkey,
        pool_token_account: &Pubkey,
        manager: &Keypair,
        staker: &Pubkey,
        max_validators: u32,
    ) -> client_error::Result<()> {
        let stake_pool_size = get_packed_len::<StakePool>();
        let stake_pool_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(stake_pool_size)
            .unwrap();
        let validator_list = ValidatorList::new(max_validators);
        let validator_list_size = validator_list.try_to_vec().unwrap().len();
        let validator_list_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(validator_list_size)
            .unwrap();
        let validator_list = Keypair::new();
        let fee = Fee {
            numerator: 10,
            denominator: 100,
        };

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &payer.pubkey(),
                    &stake_pool.pubkey(),
                    stake_pool_rent,
                    stake_pool_size as u64,
                    &spl_stake_pool::id(),
                ),
                system_instruction::create_account(
                    &payer.pubkey(),
                    &validator_list.pubkey(),
                    validator_list_rent,
                    validator_list_size as u64,
                    &spl_stake_pool::id(),
                ),
                spl_stake_pool::instruction::initialize(
                    &spl_stake_pool::id(),
                    &stake_pool.pubkey(),
                    &manager.pubkey(),
                    staker,
                    &validator_list.pubkey(),
                    reserve_stake,
                    pool_mint,
                    pool_token_account,
                    &spl_token::id(),
                    None,
                    fee,
                    max_validators,
                ),
            ],
            Some(&payer.pubkey()),
        );
        transaction.sign(
            &[payer, stake_pool, &validator_list, manager],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn deposit_into_stake_pool(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        stake_pool_address: &Pubkey,
        stake_pool: &StakePool,
        vote_address: &Pubkey,
        stake_address: &Pubkey,
        pool_token_address: &Pubkey,
    ) -> client_error::Result<()> {
        let validator_stake_address =
            find_stake_program_address(&spl_stake_pool::id(), vote_address, stake_pool_address).0;
        let pool_withdraw_authority =
            find_withdraw_authority_program_address(&spl_stake_pool::id(), stake_pool_address).0;
        let transaction = Transaction::new_signed_with_payer(
            &spl_stake_pool::instruction::deposit(
                &spl_stake_pool::id(),
                stake_pool_address,
                &stake_pool.validator_list,
                &pool_withdraw_authority,
                stake_address,
                &authorized_staker.pubkey(),
                &validator_stake_address,
                &stake_pool.reserve_stake,
                pool_token_address,
                &stake_pool.pool_mint,
                &spl_token::id(),
            ),
            Some(&authorized_staker.pubkey()),
            &[authorized_staker],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn transfer(
        rpc_client: &RpcClient,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        lamports: u64,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &from_keypair.pubkey(),
                to_address,
                lamports,
            )],
            Some(&from_keypair.pubkey()),
            &[from_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }
}
