#![allow(unused_imports)]
use crate::Common;
use anyhow::bail;
use cli_common::{
    rpc_client_helpers::RpcClientHelpers,
    rpc_marinade::{RpcMarinade, StakeInfo},
    Cluster,
};
use cli_common::{ExpandedPath, InputKeypair, InputPubkey};
use csv::*;
use log::{debug, error, info, warn};
use marinade_finance::{
    calc::proportional, state::StateHelpers, validator_system::ValidatorRecord,
};
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    account::from_account,
    account_utils::StateMut,
    clock::{Epoch, Slot},
    commitment_config::CommitmentConfig,
    epoch_info::EpochInfo,
    native_token::*,
    pubkey::Pubkey,
    signature::Signer,
    slot_history::{self, SlotHistory},
    stake::{self, state::StakeState},
    stake_history::StakeHistory,
    system_program, sysvar,
};

use std::io::{Read, Write};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use std::sync::Arc;
use structopt::StructOpt;

// deposit stake account control:
// we allow users to deposit stake accounts from validators with AT MOST 20% commission
const HEALTHY_VALIDATOR_MAX_COMMISSION: u8 = 20;
// Solana foundation do not stakes in validators if they're below 40% average
const MIN_AVERAGE_POSITION: f64 = 35.0;

#[derive(Debug, StructOpt)]
pub struct ProcessScoresOptions {
    #[structopt(
        long = "apy-file",
        help = "json APY file from stake-view.app to avoid adding low APY validators"
    )]
    apy_file: Option<String>,

    #[structopt(long = "avg-file", help = "CSV file with averaged scores")]
    avg_file: String,

    #[structopt(
        long = "validators-file",
        help = "JSON file with the output from `solana validators` command"
    )]
    validators_file: String,

    #[structopt(long = "result-file", help = "Path to the output CSV file")]
    result_file: String,

    #[structopt(
        long = "pct-cap",
        help = "Cap max percentage of total stake given to a single validator",
        default_value = "1.5" // %
    )]
    pct_cap: f64,

    #[structopt(
        long = "min-release-version",
        help = "Minimum node version not to be emergency unstaked"
    )]
    pub min_release_version: Option<semver::Version>,

    #[structopt(long = "gauge-meister", help = "Gauge meister of the vote gauges.")]
    gauge_meister: Option<Pubkey>,

    #[structopt(long = "escrow-relocker", help = "Escrow relocker program address.")]
    escrow_relocker_address: Option<Pubkey>,

    #[structopt(
        long = "vote-gauges-stake-pct",
        help = "How much of total stake is affected by votes.",
        default_value = "20" // %
    )]
    pub vote_gauges_stake_pct: u32,

    #[structopt(
        long = "stake-top-n-validators",
        help = "How many validators are guaranteed to keep their scores.",
        default_value = "430"
    )]
    stake_top_n_validators: usize,

    #[structopt(
        long = "marinade-referral-program-id",
        help = "Address of the Marinade referral program",
        default_value = "MR2LqxoSbw831bNy68utpu5n4YqBH3AzDmddkgk9LQv"
    )]
    marinade_referral_program_id: Pubkey,

    #[structopt(
        long = "stake-from-colalteral-max-pct",
        help = "How much of total stake can be given to validators with stake from the referral/collateral.",
        default_value = "30"
    )]
    stake_from_collateral_max_pct: u64,

    #[structopt(
        long = "stake-delta",
        help = "Stake delta considered for stake target",
        default_value = "100000"
    )]
    stake_delta: i64,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct ValidatorScoreRecord {
    rank: u32,
    pct: f64,
    epoch: u64,
    keybase_id: String,
    name: String,
    vote_address: String,
    score: u32,
    average_position: f64,
    commission: u8,
    max_commission: u8,
    epoch_credits: u64,
    data_center_concentration: f64,
    data_center_asn: u64,
    data_center_location: String,
    base_score: f64,
    mult: f64,
    avg_score: f64,
    avg_active_stake: f64,
    can_halt_the_network_group: bool,
    identity: String,
    stake_conc: f64,
    url: String,
    version: String,
}

// post-process data
#[derive(Debug, serde::Serialize)]
struct ValidatorScore {
    epoch: u64,
    rank: u32,
    score: u32,
    marinade_score: u32,
    collateral_score: u32,
    collateral_shares: u64,
    vote_score: u32,
    votes_read: u64,
    votes_effective: u64,
    name: String,
    credits_observed: u64,
    vote_address: String,
    commission: u8,
    max_commission: u8,
    average_position: f64,
    data_center_concentration: f64,
    data_center_asn: u64,
    data_center_location: String,
    avg_active_stake: f64,
    apy: Option<f64>,
    delinquent: bool,
    this_epoch_credits: u64,
    pct: f64,
    marinade_staked: f64,
    should_have: f64,
    remove_level: u8,
    remove_level_reason: String,
    under_nakamoto_coefficient: bool,
    keybase_id: String,
    identity: String,
    stake_concentration: f64,
    base_score: u64,
    url: String,
    version: String,
}

impl ValidatorScore {
    // we need all "healthy" validators in the on-chain list,
    // to enable "restricted_mode" deposit-stake-account (when auto_add_validator_enabled=false)
    // When auto_add_validator_enabled==false, you can only deposit stake-accounts
    // from validators already in the list, so we need to add all validators,
    // even those with with score==0, so people can deposit stake-accounts from those validators.
    // Having 0 score, the stake will be eventually moved to other validators
    /// Note: We only add validators in the on-chain list (allowing stake-account-deposits from those validators)
    /// when commission<HEALTHY_VALIDATOR_MAX_COMMISSION (30%)
    /// AND when average_position > 40 (50=average, 40=> at most 10% below average credits_observed)
    /// returns: 0=healthy, 1=warn (score *= 0.5), 2=unstake, 3=unstake & remove from list
    pub fn is_healthy(
        &self,
        avg_this_epoch_credits: u64,
        min_release_version: Option<&semver::Version>,
    ) -> (u8, String) {
        let version_zero = semver::Version::parse("0.0.0").unwrap();
        //
        // remove from concentrated validators
        if self.under_nakamoto_coefficient {
            return (
                2,
                format!(
                    "This validator is currently part of the superminority and cannot receive stake from Marinade."
                ),
            );
        } else if self.commission > HEALTHY_VALIDATOR_MAX_COMMISSION {
            return (
                3,
                format!(
                    "The commission of this validator ({}%) is above {}% and won’t allow it to receive stake from Marinade.",
                    self.commission, HEALTHY_VALIDATOR_MAX_COMMISSION
                ),
            );
        // Note: self.delinquent COMMENTED, a good validator could be delinquent for several minutes during an upgrade
        // it's better to consider this_epoch_credits as filter and not the on/off flag of self.delinquent
//         } else if self.delinquent {
//             return (2, format!("DELINQUENT")); // keep delinquent validators in the list so people can escape by depositing stake accounts from them into Marinade
        } else if self.credits_observed == 0 {
            return (2, format!("This validator isn’t producing credits and will not be able to receive stake from Marinade."));
        // keep them in the list so people can escape by depositing stake accounts from them into Marinade
        } else if semver::Version::parse(&self.version)
            .as_ref()
            .unwrap_or(&version_zero)
            < min_release_version.unwrap_or(&version_zero)
        {
            return (2, format!("The node version of this validator is below the required version, it will not be able to receive stake from Marinade."));
        } else if self.this_epoch_credits < avg_this_epoch_credits * 8 / 10 {
            return (
                2,
                format!(
                    "The credits observed for this validator are too low compared to the average to be able to receive stake from Marinade. ({} % of the average)",
                    if avg_this_epoch_credits == 0 {
                        0
                    } else {
                        self.this_epoch_credits * 100 / avg_this_epoch_credits
                    }
                ),
            ); // keep delinquent validators in the list so people can escape by depositing stake accounts from them into Marinade
        } else if self.this_epoch_credits < avg_this_epoch_credits * 9 / 10 {
            return (
                1,
                format!(
                    "The validator has low production ({}% of credits average).",
                    if avg_this_epoch_credits == 0 {
                        0
                    } else {
                        self.this_epoch_credits * 100 / avg_this_epoch_credits
                    }
                ),
            ); // keep delinquent validators in the list so people can escape by depositing stake accounts from them into Marinade
        } else if self.average_position < MIN_AVERAGE_POSITION {
            (
                1,
                format!("Low average position {}%.", self.average_position),
            )
        } else {
            (0, "healthy".into())
        }
    }
}

impl ProcessScoresOptions {
    pub fn process(
        self,
        common: Common,
        client: Arc<RpcClient>,
        _cluster: Cluster,
    ) -> anyhow::Result<()> {
        let marinade = RpcMarinade::new(client, &common.instance.as_pubkey())?;

        let epoch_info = marinade.client.get_epoch_info()?;

        // Read file csv with averages into validator_scores:Vec
        let mut validator_scores: Vec<ValidatorScore> = self.load_avg_file(&epoch_info)?;

        // Sort validator_scores by marinade_score desc
        validator_scores.sort_by(|a, b| b.marinade_score.cmp(&a.marinade_score));

        // Get APY Data from stakeview.app
        self.load_apy_file(&mut validator_scores)?;

        // Get this_epoch_credits & delinquent data from 'solana validators' output
        let avg_this_epoch_credits = self.load_solana_validators_file(&mut validator_scores)?;
        info!("Average this epoch credits: {}", avg_this_epoch_credits);

        // Find unhealthy validators and set their scores to 0 or 50 %
        self.decrease_scores_for_unhealthy(&mut validator_scores, avg_this_epoch_credits);

        // Some validators do not play fair, let's set their scores to 0
        self.apply_blacklist(&mut validator_scores);

        // imagine a +100K stake delta
        let total_stake_target = marinade.state.validator_system.total_active_balance;

        let total_stake_target = if self.stake_delta < 0 {
            total_stake_target.saturating_sub(sol_to_lamports(self.stake_delta.abs() as f64))
        } else {
            total_stake_target.saturating_add(sol_to_lamports(self.stake_delta.abs() as f64))
        };

        let total_collateral_shares =
            self.load_shares_from_collateral(&marinade, &mut validator_scores)?;

        let total_stake_from_collateral = total_collateral_shares
            .min(self.stake_from_collateral_max_pct * total_stake_target / 100);

        let stake_target_without_collateral = total_stake_target - total_stake_from_collateral;

        info!(
            "Total stake target: {}",
            lamports_to_sol(total_stake_target)
        );
        info!(
            "Total collateral shares: {}",
            lamports_to_sol(total_collateral_shares)
        );
        info!(
            "Total stake from collateral: {}",
            lamports_to_sol(total_stake_from_collateral)
        );
        info!(
            "Stake target without collateral: {}",
            lamports_to_sol(stake_target_without_collateral)
        );

        // Compute marinade_staked from the current on-chain validator data
        self.load_marinade_staked(&marinade, &mut validator_scores)?;

        // Set scores of validators out of top N to zero unless we have a stake with them
        // This makes sure that we do not constantly stake/unstake people near the end of the list.
        self.adjust_scores_of_validators_below_line(&mut validator_scores);

        self.apply_commission_bonus(&mut validator_scores);

        self.update_should_have(&mut validator_scores, stake_target_without_collateral);

        self.adjust_marinade_score_for_overstaked(&mut validator_scores);

        // Loads votes from gauges
        self.load_votes(&marinade, &mut validator_scores)?;

        // Zero votes for misbehaving validators
        self.calc_effective_votes(&mut validator_scores);

        // We remove x % from everybody, we distribute x % based on scores, we sum marinade_score and vote_score
        self.distribute_vote_score(&mut validator_scores);

        // Apply cap
        self.recompute_score_with_capping(&mut validator_scores, stake_target_without_collateral)?;

        self.apply_stake_from_collateral(&mut validator_scores, total_stake_from_collateral);

        // Final assertions
        self.check_final_scores(&validator_scores);

        // Sort validator_scores by score desc
        validator_scores.sort_by(|a, b| b.score.cmp(&a.score));

        self.write_results_to_file(validator_scores)?;
        Ok(())
    }

    fn load_shares_from_collateral(
        &self,
        marinade: &RpcMarinade,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> anyhow::Result<u64> {
        let deposits_to_referral =
            marinade.fetch_deposits_to_referral(self.marinade_referral_program_id)?;

        let current_collateral = marinade.get_current_collateral()?;

        let shares: HashMap<_, _> = deposits_to_referral.iter().map(|(vote, deposit)| {
            let deposit = *deposit;
            let collateral = *current_collateral.get(vote).unwrap_or(&0);

            let share = if collateral < deposit {
                log::warn!("Validator {} has deposited {} through referral but has only {} in collateral!", vote, lamports_to_sol(deposit), lamports_to_sol(collateral));
                collateral
            } else {
                log::info!("Validator {} has deposited {} through referral and still has {} in collateral!", vote, lamports_to_sol(deposit), lamports_to_sol(collateral));
                deposit
            };

            (vote.clone(), share)
        }).collect();

        for validator_score in validator_scores.iter_mut() {
            validator_score.collateral_shares =
                *shares.get(&validator_score.vote_address).unwrap_or(&0);
        }

        Ok(validator_scores.iter().map(|s| s.collateral_shares).sum())
    }

    fn apply_stake_from_collateral(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
        total_stake_from_collateral: u64,
    ) {
        let sum_shares: u64 = validator_scores.iter().map(|s| s.collateral_shares).sum();
        let mut sum_score = 0;
        for v in validator_scores.iter_mut() {
            if v.collateral_shares > 0 {
                v.collateral_score =
                    (proportional(total_stake_from_collateral, v.collateral_shares, sum_shares)
                        .unwrap()
                        / LAMPORTS_PER_SOL) as u32;
                v.score += v.collateral_score;
                sum_score += v.collateral_score;
                v.should_have += v.collateral_score as f64;
                if v.remove_level > 0 {
                    v.remove_level = 0;
                    v.remove_level_reason = "self stake override".to_string();
                }
            }
        }
        log::info!("Total score from collateral: {}", sum_score);
    }

    fn apply_commission_bonus(&self, validator_scores: &mut Vec<ValidatorScore>) -> () {
        for v in validator_scores.iter_mut() {
            let multiplier = match v.commission {
                c if c <= 6 => 5,
                7 => 4,
                8 => 3,
                9 => 2,
                _ => 1,
            };

            v.marinade_score *= multiplier;
        }
    }

    // Set scores of validators out of top N to zero unless we have a stake with them
    // This makes sure that we do not constantly stake/unstake people near the end of the list.
    fn adjust_scores_of_validators_below_line(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> () {
        for (index, validator) in validator_scores.iter_mut().enumerate() {
            if index >= self.stake_top_n_validators && validator.marinade_staked == 0.0 {
                validator.marinade_score = 0;
            }
        }
    }

    fn distribute_vote_score(&self, validator_scores: &mut Vec<ValidatorScore>) -> () {
        for v in validator_scores.iter_mut() {
            v.score = v.marinade_score;
        }

        if self.vote_gauges_stake_pct == 0 {
            return ();
        }

        assert!(self.vote_gauges_stake_pct <= 100);

        let effective_votes_sum: u64 = validator_scores.iter().map(|v| v.votes_effective).sum();

        if effective_votes_sum == 0 {
            return ();
        }

        let marinade_score_sum: u64 = validator_scores
            .iter()
            .map(|v| v.marinade_score as u64)
            .sum();

        let vote_score_target_sum = marinade_score_sum * self.vote_gauges_stake_pct as u64 / 100;

        // We remove x % from everybody, we distribute x % based on scores, we sum marinade_score and vote_score
        for v in validator_scores.iter_mut() {
            v.marinade_score = v.marinade_score * (100 - self.vote_gauges_stake_pct) / 100;
            v.vote_score = (v.votes_effective as u128 * vote_score_target_sum as u128
                / effective_votes_sum as u128) as u32;
            v.score = v.marinade_score + v.vote_score;
        }
    }

    fn calc_effective_votes(&self, validator_scores: &mut Vec<ValidatorScore>) -> () {
        for v in validator_scores.iter_mut() {
            v.votes_effective = if v.remove_level > 1 { 0 } else { v.votes_read };
        }
    }

    fn check_final_scores(&self, validator_scores: &Vec<ValidatorScore>) -> () {
        let total_score: u64 = validator_scores.iter().map(|s| s.score as u64).sum();
        let count_of_positive_validators = validator_scores.iter().filter(|s| s.score > 0).count();

        log::info!("Total score: {}", total_score);
        log::info!(
            "Count of validators with positive score: {}",
            count_of_positive_validators
        );

        assert!(total_score > 0, "Total score must be a positive number!");
        assert!(
            count_of_positive_validators > 300,
            "Total score of validators with positive score is too low!"
        );
    }

    fn load_votes(
        &self,
        rpc_marinade: &RpcMarinade,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> anyhow::Result<()> {
        let (escrow_relocker_address, gauge_meister) =
            match (self.escrow_relocker_address, self.gauge_meister) {
                (Some(e), Some(g)) => (e, g),
                _ => {
                    info!("Arguments necessary for fetching votes are missing");
                    return Ok(());
                }
            };

        let votes_from_gauges = rpc_marinade.fetch_votes(escrow_relocker_address, gauge_meister)?;

        for validator_score in validator_scores.iter_mut() {
            if let Some(validator_votes) = votes_from_gauges.get(&validator_score.vote_address) {
                validator_score.votes_read = *validator_votes;
            }
        }

        Ok(())
    }

    fn load_avg_file(&self, epoch_info: &EpochInfo) -> anyhow::Result<Vec<ValidatorScore>> {
        let mut validator_scores: Vec<ValidatorScore> = Vec::with_capacity(2000);

        info!("Start from scores file {}", self.avg_file);
        let mut validator_details_file_contents = String::new();
        let mut file = std::fs::File::open(&self.avg_file)?;
        file.read_to_string(&mut validator_details_file_contents)?;
        let mut reader = csv::Reader::from_reader(validator_details_file_contents.as_bytes());
        for record in reader.deserialize() {
            let record: ValidatorScoreRecord = record?;
            validator_scores.push(ValidatorScore {
                epoch: epoch_info.epoch,
                rank: record.rank,
                marinade_score: record.score,
                collateral_score: 0,
                collateral_shares: 0,
                vote_score: 0,
                votes_read: 0,
                votes_effective: 0,
                score: 0,
                name: record.name,
                credits_observed: record.epoch_credits,
                vote_address: record.vote_address,
                commission: record.commission,
                max_commission: record.max_commission,
                average_position: record.average_position,
                data_center_concentration: record.data_center_concentration,
                data_center_asn: record.data_center_asn,
                data_center_location: record.data_center_location,
                avg_active_stake: record.avg_active_stake,
                apy: None,
                delinquent: false,
                this_epoch_credits: 0,
                marinade_staked: 0.0,
                pct: 0.0,
                should_have: 0.0,
                remove_level: 0,
                remove_level_reason: String::from(""),
                identity: record.identity,
                keybase_id: record.keybase_id,
                under_nakamoto_coefficient: record.can_halt_the_network_group,
                stake_concentration: record.stake_conc,
                base_score: record.base_score as u64,
                url: record.url,
                version: record.version,
            });
        }

        info!(
            "Processing {} records in {} file",
            validator_scores.len(),
            self.avg_file
        );

        assert!(
            validator_scores.len() > 100,
            "Too little validators found in the CSV with average scores"
        );

        let total_marinade_score: u64 = validator_scores
            .iter()
            .map(|s| s.marinade_score as u64)
            .sum();
        info!(
            "avg file contains {} records, total_score {}",
            validator_scores.len(),
            total_marinade_score
        );

        Ok(validator_scores)
    }

    fn index_validator_scores(
        &self,
        validator_scores: &Vec<ValidatorScore>,
    ) -> HashMap<String, usize> {
        validator_scores
            .iter()
            .enumerate()
            .map(|(index, validator)| (validator.vote_address.to_string(), index))
            .collect()
    }

    fn index_validator_records(
        &self,
        validator_records: &Vec<ValidatorRecord>,
    ) -> HashMap<Pubkey, usize> {
        validator_records
            .iter()
            .enumerate()
            .map(|(index, validator)| (validator.validator_account, index))
            .collect()
    }

    fn load_apy_file(&self, validator_scores: &mut Vec<ValidatorScore>) -> anyhow::Result<f64> {
        let mut avg_apy: f64 = 5.0;
        const MIN_APY_TO_CONSIDER_FOR_AVG_APY: f64 = 4.0;

        // create a hashmap vote-key->index
        let validator_indices: HashMap<String, usize> =
            self.index_validator_scores(validator_scores);

        // get APY Data from stakeview.app
        // update "apy" field in validator_scores
        if let Some(apy_file) = &self.apy_file {
            info!("Read APY from {}", apy_file);
            {
                let file = std::fs::File::open(&apy_file)?;
                let json_data: serde_json::Value = serde_json::from_reader(file)?;
                let validators = &json_data["validators"];

                let mut count_apy_data_points: usize = 0;
                let mut sum_apy: f64 = 0.0;
                match validators {
                    serde_json::Value::Array(list) => {
                        assert!(
                            list.len() > 1000,
                            "Too little validators found in the APY report"
                        );
                        for apy_info in list {
                            if let Some(index) =
                                validator_indices.get(apy_info["vote"].as_str().unwrap())
                            {
                                let mut v = &mut validator_scores[*index];
                                if let serde_json::Value::Number(x) = &apy_info["apy"] {
                                    let apy = x.as_f64().unwrap() * 100.0;
                                    if apy > MIN_APY_TO_CONSIDER_FOR_AVG_APY {
                                        count_apy_data_points += 1;
                                        sum_apy += apy;
                                    }
                                    v.apy = Some(apy);
                                }
                            }
                        }
                    }
                    _ => panic!("invalid json"),
                }
                avg_apy = if count_apy_data_points == 0 {
                    4.5
                } else {
                    sum_apy / count_apy_data_points as f64
                };
                info!("Avg APY {}", avg_apy);
            }
        }

        Ok(avg_apy)
    }

    fn load_solana_validators_file(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> anyhow::Result<u64> {
        let avg_this_epoch_credits: u64;
        // create a hashmap vote-key->index
        let validator_indices: HashMap<String, usize> =
            self.index_validator_scores(validator_scores);

        // get this_epoch_credits & delinquent Data from 'solana validators' output
        // update field in validator_scores
        let mut count_credit_data_points: u64 = 0;
        let mut sum_this_epoch_credits: u64 = 0;
        info!(
            "Read solana validators output from {}",
            self.validators_file
        );
        let file = std::fs::File::open(&self.validators_file)?;
        let json_data: serde_json::Value = serde_json::from_reader(file)?;
        let validators = &json_data["validators"];

        match validators {
            serde_json::Value::Array(list) => {
                assert!(
                    list.len() > 100,
                    "Too little validators found in the result of `solana validators` command"
                );
                for json_info in list {
                    if let Some(index) =
                        validator_indices.get(json_info["voteAccountPubkey"].as_str().unwrap())
                    {
                        let mut v = &mut validator_scores[*index];
                        if let serde_json::Value::Bool(x) = &json_info["delinquent"] {
                            v.delinquent = *x
                        }
                        if let serde_json::Value::Number(x) = &json_info["epochCredits"] {
                            let credits = x.as_u64().unwrap();
                            if credits > 0 {
                                v.this_epoch_credits = credits;
                                sum_this_epoch_credits += credits;
                                count_credit_data_points += 1;
                            }
                        }
                    }
                }
                avg_this_epoch_credits = sum_this_epoch_credits / count_credit_data_points;
            }
            _ => panic!("invalid json"),
        }

        Ok(avg_this_epoch_credits)
    }

    fn write_results_to_file(&self, validator_scores: Vec<ValidatorScore>) -> anyhow::Result<()> {
        info!("Save scores to file {}", &self.result_file);

        let mut wtr = WriterBuilder::new()
            .flexible(true)
            .from_path(&self.result_file)?;
        let mut count = 0;
        for v in validator_scores {
            wtr.serialize(v)?;
            count += 1;
        }
        wtr.flush()?;
        info!("{} records", count);

        Ok(())
    }

    fn decrease_scores_for_unhealthy(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
        avg_this_epoch_credits: u64,
    ) -> () {
        info!("Set score = 0 if validator is not healthy (catch validators unhealthy now in this epoch)");
        for v in validator_scores.iter_mut() {
            let (remove_level, reason) =
                v.is_healthy(avg_this_epoch_credits, self.min_release_version.as_ref());
            v.remove_level = remove_level;
            v.remove_level_reason = reason;
            // if it is not healthy, adjust score to zero
            // score is computed based on last epoch, but APY & delinquent-status is current
            // so this will stop the bot staking on a validator that was very good last epochs
            // but delinquent on current epoch
            if remove_level == 1 {
                v.marinade_score /= 2;
            } else if remove_level > 1 {
                v.marinade_score = 0;
            }
        }
    }

    fn apply_blacklist(&self, validator_scores: &mut Vec<ValidatorScore>) -> () {
        let default_blacklist_reason = format!("This validator is blacklisted for bad behavior (cheating with credits, end of epoch change of commission). It won’t be able to receive stake from Marinade.");
        let blacklisted: HashMap<String, String> = HashMap::from([
            // manually slashed-paused
            // https://discord.com/channels/823564092379627520/856529851274887168/914462176205500446
            // Marinade is about to stake a validator that is intentionally delaying their votes to always vote in the correct fork. They changed the code so they don't waste any vote with consensus...
            // it seems like they are intentionally lagging their votes by about 50 slots or only voting on the fork with consensus, so that they don't vote on the wrong fork and so land every one of their votes... therefore their votes in effect don't contribute to the consensus of the network...
            // Response: slashing-pausing
            // 1) #14 Validator rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr, score-pct:0.6037%
            // ValidatorScoreRecord { rank: 14, pct: 0.827161338644014, epoch: 252, keybase_id: "replicantstaking", name: "Replicant Staking", vote_address: "rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr", score: 3211936, average_position: 57.8258431048359, commission: 0, epoch_credits: 364279, data_center_concentration: 0.03242, base_score: 363924.0, mult: 8.82584310483592, avg_score: 3211936.0, avg_active_stake: 6706.7905232706 }
            // avg-staked 6706.79, marinade-staked 50.13 (0.75%), should_have 39238.66, to balance +stake 39188.54 (accum +stake to this point 39188.54)
            (
                "rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr".into(),
                default_blacklist_reason.clone(),
            ),
            // manually slashed-paused
            // Same entity 4block-team with 2 validators
            // https://discord.com/channels/823564092379627520/856529851274887168/916268033352302633
            // 4block-team case at 2021-12-3
            // current marinade stake: (4block-team validator#1)
            // 3) Validator 6anBvYWGwkkZPAaPF6BmzF6LUPfP2HFVhQUAWckKH9LZ, marinade-staked 55816.30 SOL, score-pct:0.7280%, 1 stake-accounts
            // next potential marinade stake: (4block-team validator#2)
            // 0) #6 0.72% m.stk:0 should:49761 next:+49761 credits:373961 cm:0 dcc:0.29698 4BLOCK.TEAM 2 - Now 0% Fees → 1% from Q1/2023 GfZybqTfVXiiF7yjwnqfwWKm2iwP96sSbHsGdSpwGucH
            (
                "GfZybqTfVXiiF7yjwnqfwWKm2iwP96sSbHsGdSpwGucH".into(),
                default_blacklist_reason.clone(),
            ),
            // Scrooge_McDuck
            // changing commission from 0% to 100% on epoch boundaries
            // https://www.validators.app/commission-changes?locale=en&network=mainnet
            (
                "AxP8nEVvay26BvFqSVWFC73ciQ4wVtmhNjAkUz5szjCg".into(),
                default_blacklist_reason.clone(),
            ),
            // Node Brothers
            // changing commission from 0% to 10% on epoch boundaries
            // https://www.validators.app/commission-changes/6895?locale=en&network=mainnet
            (
                "DeFiDeAgFR29GgKdyyVZdvsELbDR8k4WqprWGtgtbi1o".into(),
                default_blacklist_reason.clone(),
            ),
            // VymD
            // Vote lagging
            (
                "8Pep3GmYiijRALqrMKpez92cxvF4YPTzoZg83uXh14pW".into(),
                default_blacklist_reason.clone(),
            ),
            // Parrot
            // Down for ~2 weeks
            (
                "GBU4potq4TjsmXCUSJXbXwnkYZP8725ZEaeDrLrdQhbA".into(),
                default_blacklist_reason.clone(),
            ),
            // The following validators were offline for at least 36 hours when solana was halted in May '22
            // Just a warning for now.
            // 2cFGQhgkuibqREEXvz7wEb5CwUqGHfBSTB2oa1hmhkcw
            // 2mQNruSKNnn6fWqJjKNGsQtpsMnuxxMzHsrKT6iVR7tW
            // 2vxNDV7aAbrb4Whnxs9LiuxCsm9oubX3c1hozXPsoD97
            // 5wNag8umJhaaj9gGdqmBz7Xwwy1NL5yQ1QbvPdQrDd3h
            // 7oX5QSP9yBjT1F1sRSDCX91ZxibETqemDM4WLDju5rTM
            // 9c5bpzVRbfsYY2fannb4hyX5CJUPg3BfH2cL6sR7kJM4
            // Cva4NEnBRYfFv8i3RtcMTbEYgyVNmewk2aAgh4fco2mP
            // EBam6FrvTP4xPSNVNFbwNioGeszDRvYDaqRmxbKJkybD

            // The following validators were offline for at least 36 hours when solana was halted in June '22
            // Just a warning for now.
            // DeGodsKvJrNTkxcnXVzo4FxpVagQ8XsLKibqxNqJPx27
            // 7K8DVxtNJGnMtUY1CQJT5jcs8sFGSZTDiG7kowvFpECh

            // The following validators were offline for at least 36 hours when solana was halted in February '23
            // Just a warning for now
            // 3qmEGZpEUFYxXiLU5CZjSaUy2X28oV51qVLenKEutYDe
            // 8Ey5FDayWYgJdVoquScT2hJDKWk7nQQfqzfGBt1emJpx
            // 9MySck9qTMMPMUwFnh1rAYk6eStsBkUbKSBTnAeKmJXu
            // 9Yb1epzjDEkyiAUKtAND6mHK4DgovbEub9LmRKG3JogZ
            // FUhBaeztzSza7n5sM1TVkhfCtsfYe2FmqUkjjpQ1PHin
            // Bxps8LSWxZVx618iK8rHvnCD5f9u7cTYuXz5sLsAGuSn

            // The following were down for more than 36 hours in halt #2 (May '22) and #3 (June '22)
            (
                "5wNag8umJhaaj9gGdqmBz7Xwwy1NL5yQ1QbvPdQrDd3h".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "7oX5QSP9yBjT1F1sRSDCX91ZxibETqemDM4WLDju5rTM".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "Cva4NEnBRYfFv8i3RtcMTbEYgyVNmewk2aAgh4fco2mP".into(),
                default_blacklist_reason.clone(),
            ),
            // Exiting mainnet:
            (
                "2vxNDV7aAbrb4Whnxs9LiuxCsm9oubX3c1hozXPsoD97".into(),
                default_blacklist_reason.clone(),
            ),
            // Marinade stake puts them in superminority, unstaking puts them back - this creates loop of stake/unstake
            // ("CogentC52e7kktFfWHwsqSmr8LiS1yAtfqhHcftCPcBJ".into(), "This validator is close to the superminority threshold and will not receive stake to avoid multiple stake/unstake operations on successive epochs.".to_string()),

            // changing commission between 0% and 10% on epoch boundaries
            (
                "42GfJFeWySe1zt7xYxXNFK1E2V7xXnf1Jpc6B4g63QTm".into(),
                default_blacklist_reason.clone(),
            ),
            // changing commission between 0% and 10% on epoch boundaries
            (
                "DpvUS8Losp2UGGaSGyupyKwQqHkmruzfwrZg2VYK7Zg7".into(),
                default_blacklist_reason.clone(),
            ),
            // changing commission before and after our bot's runs
            (
                "GUTjLTQTCmeBzTrBgCsWSM7G2JrsLvwXbXdafWvicqbr".into(),
                default_blacklist_reason.clone(),
            ),
            // changing commission on epoch boundaries (e.g. 3frtXYL2Wx8oDkmA2Me9xxKWDXp6vcdnJDT2Bcf7w17jNiVZ4vkAn9EQNqqUdJDnPoGpPDry7YTy8KSnjx8wtUD9, 4DS6MYpbsfL3p2afkbE16gcT5WtbW4ndQK4P3jCMWenrvkxnGBM3kXbkhkphB4KcS7DJBCDCMFsGRbigxREcDajn)
            (
                "G2v6wsh4xVHj1xMLtLFzX2hP6T1TTxti5ZxK3iv8TJQZ".into(),
                default_blacklist_reason.clone(),
            ),
            // changing commission on epoch boundaries (e.g. QgXGHawoM8vePwNASfhvMRvm8LgLNinUM5bdeSZMtoehnyP3VLHt2MFUNeyRNP1wGJs5VqrxQPXuxskMMvzjY7E, 3nx6GhUkTVNg7JcNV5GFEFoBx3tCtPgxbqe7NX3o8ZWbM3s4U2aWfSm1ExcMRWprfqaZ9nCoZJedbSU26u9EEiZ)
            (
                "4hDeRsRJBsvbA1KNjGmZ9zB1Nv3Cn2KbANNUCQwjBh29".into(),
                default_blacklist_reason.clone(),
            ),
            // commission
            (
                "65U5oJPjCpQPuLSUPJZVFWSQgRmVtgcZwo6fJREFiYoz".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "GUTjLTQTCmeBzTrBgCsWSM7G2JrsLvwXbXdafWvicqbr".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "3Ueg3qrAVv95tJzTiKM2dd33NswZT77yRf9wXcBDCn2c".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "ND5jXgjtiPC34Qf71oEiDrcim4hPhyPdhBrqeZidUxF".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "7zx69bryF4TnqRGTzE7CJkSXUZ495nFFZk4RCkXQEcgH".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "A5G8TTnkxPqTDkpeM9LPjwvE4mQ8E7vTzdBNvLqs1pTg".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "DdiWSFE9u9Gu1GqGVaPWqAAk6TuYA7t35tb54fCu37uS".into(),
                default_blacklist_reason.clone(),
            ),
            // commission
            (
                "DWCLHn3hzmru2K8Lg2MFhsBABPmEGDkd664V9z77NjCt".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "61rPRUxuPb4xy6X6AmKcSf4CiNaerttpaFC3GLvUu2Ba".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "auzyWJi8E1NVBEUYZezBc8bS1GZYTVnhpdc53zH7PPH".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "tnaKD5evRkBonwyW5n5yKoJrt7H871Aboh1AWXH9AFj".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "AJGaXvnzDEGxjcDX9nYSWQj8urAdtTmgCuwD1TtF97yz".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "4Ucwi2DKML7jBzDDTpiZ46vq7jQAHb93ZAFkYnT9TTyq".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "CfLRV8ZS41ksYMUzcQ8joz3ruPBLTv8LmRHtNCj15ovf".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "9DLtFk37Nxr9CbJAvxKnjEpCzCdyjtNcD6juCYdYktTM".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "JAvknH4Pn9b8jqGg5rpkGAFrnXRFcrmL5kumTXyacy5u".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "5Mergmrmd1XFeDRMHbzS4XLiorfG3Qsddwff9RkX4Lup".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "AkVoTV14wHZVB7sNiLxGCiE4tS1mXG2CkZSuqLzxHS2V".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "DEmZmtt9bDeDcBMExjKhpCFnA5yj46XbAkzu61CXPKFh".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "5maAYsh7z7iikpZs7x89wx1QsxXe8rpF7B5cvrDvWCej".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "BK8YruGZQMFmbKn8CcLL5i3UqVwmACnc77YhgPYqGkNh".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "Dhs6P4kjtszfhaLeZGbVZrFgPcimgQ91SGZXkAxcx1tp".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "9zdVCLZqSRR26Emy5su23P2yHwX5DF9doS462yjcNnHm".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "JBvqybAVc98GrvhjF7EXVdrgaZAEyy3Gi6D7uT3qsFwr".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "Ap3wiVMh2BJDvvvUPQMWMBCZCPeyxVhf8xjCWASoFWUa".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "BF2gZDHXdtxxzNU18qcme89zexND41yohyaLg5xytdg1".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "DM7agS8XHMXqxsT7BXxAPKzJ54JSEDTK59HtrQfEKJGa".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "2R2H7wHcCKwEq85HuScU3xvR9Rf1WJuAhGtpuUzUGhGJ".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "3vFELvmvHdkobLmgMCiXKqTFWwrbEWyxVX1uyMXFm6n8".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "DVD9Q9yZ8n9iWaqCCP7y6tf461aZTCshsaj5zm9aE7WV".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "B5a4ywXhokofcZDsdVT7RH64HiqW7WxvG9hMxQYgHzZL".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "GuPYoGPCQDp1bJ3A6ALzcHik6ziu6CX95ADHeQvbzMfQ".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "AiseS95iZjWhP8qowrg3efLxcDq5JWuGVVkyr5nG5osj".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "B5a4ywXhokofcZDsdVT7RH64HiqW7WxvG9hMxQYgHzZL".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "AG781KzvU89JSu9W69adSLkaVW11g9L3HNxYyuePrfrk".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "4tdrCXpoqAdSR7Zqbow6ikL1BGLHV2SK9XpwYsXvWGCW".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "8VNKjGimak6Y53b2vHfcg2fFZMN7gWM1DLm9bhDXw8QS".into(),
                default_blacklist_reason.clone(),
            ),
            (
                "3vFELvmvHdkobLmgMCiXKqTFWwrbEWyxVX1uyMXFm6n8".into(),
                default_blacklist_reason.clone(),
            ),
            ("8T8Lj1WEqEDuJAP1RJ6Wmm5aLJJyCPPnxjwwSZMngNaz".into(), default_blacklist_reason.clone()),
            ("G1pTmMKhNFEP1QpG7qnzQ8znZe7VZba4mntCd5z9i1Qo".into(), default_blacklist_reason.clone()),
            ("G3S7AjkVSEX47HzEYfKasjMX1dRueJusKsgg8ceTmtfT".into(), default_blacklist_reason.clone()),
            ("ATAQXMLxTz8rqTKVvuPiyzkPsbFxtwfdhYwGUnzurwGA".into(), default_blacklist_reason.clone()),
            ("AqU2ZDF88mkdc7SRE9a7pQGZricwdsqy61sf4xjJ5Bpy".into(), default_blacklist_reason.clone()),
            ("UZFxLfrRxbB4VnMj1HWMSLfMcA3fNP3AfxnaBGmJpH5".into(), default_blacklist_reason.clone()),
            ("8UE5sUmwGtwYBX4wL9GKPBV755V9kiTq7wnjy5WtWckd".into(), default_blacklist_reason.clone()),
            ("BfUy6zGqC7vW2SqTrxismVXRgTH5FiMKKPaMxcYDshMq".into(), default_blacklist_reason.clone()),
            ("FRnrA49NcN1nBfaR9BhcYoZwZiDh5Cup4d1gqhy99o44".into(), default_blacklist_reason.clone()),
            ("6c2FJC1NfzNvivapAzPW8vj9TW63dpHCVh7zzehwnNLH".into(), default_blacklist_reason.clone()),
            ("2KRUKzCXCuHbyL5QGERMpwsH9tgAdwGpeDXkRMm5gpZm".into(), default_blacklist_reason.clone()),
            ("J75kdoVKTTN8JNv2raUDKhFXGUokFxrZ4yfb3z5iUred".into(), default_blacklist_reason.clone()),
            ("6ksBdjCJbuX58KLD6H1wJ9NpmLi6qVezWFhVeXLPZCfo".into(), default_blacklist_reason.clone()),
            ("4fBWWrBQhaQdFiLwPr8RfG4mYsjTRGRX58CRKNtFHgLD".into(), default_blacklist_reason.clone()),
            ("CHZwfXZXMUXaES6LevEs1g9RCykMnRYmn8qHM43Vh5Cx".into(), default_blacklist_reason.clone()),
            ("Bzgg5GLA2H5ksyNLf2YQjzWEyvcExHo21nNYPkaHiLZW".into(), default_blacklist_reason.clone()),
            ("JBjqqaDq5N16VW5J8UGMpYvkxCYNRFixBgdAefeJj6iv".into(), default_blacklist_reason.clone()),
            ("2tCoK3hcQhRvrtTJCtL84Pu2J5V7wtVQt5MH1uMzp4XX".into(), default_blacklist_reason.clone()),
            ("7X39mPcpoDkb5pQ9XFkWN1Y2BAQxygDHgaz4K5KNRRuy".into(), default_blacklist_reason.clone()),
            ("9CCjy6LzcpkfCzVgnXzfoofKt7J6wZrATPR7ywovhWoc".into(), default_blacklist_reason.clone()),
            ("D71SXconcGZfftDKVJ5ksWD512QALK3hsBvyzvhEMxsb".into(), default_blacklist_reason.clone()),
            ("8BqUmPfVZrYrNL82UMn8Qyrg3bmryukk7kKPoCwkMPx3".into(), default_blacklist_reason.clone()),
            ("5eb7FxPo4gdtFTUMNQLeFqWLhRKMjdiu7dyXt7Anp5J4".into(), default_blacklist_reason.clone()),
            ("GATwWi9S9Y9RV7GjUubyxAe4bFjrbmVbUvsg5jnfjWEB".into(), default_blacklist_reason.clone()),
            ("8CB5nGGW1kRZaHAHmsq1c1kxvvFSPK6chgSn5CU2h7C9".into(), default_blacklist_reason.clone()),
            ("Dn2cRSWAfQpb3NyUJ2q33t1scBLxzo8TZBAyKsWhX7zh".into(), default_blacklist_reason.clone()),
            ("GQ6qBT1uvf5pXnvFc5C5jm9DeeFCSxgyXfUKigvkJCTp".into(), default_blacklist_reason.clone()),
            ("Dtwjm5bmZcpXY1YeTQ6cUhuyUXSasSjLUdcYMkY3o8ef".into(), default_blacklist_reason.clone()),
            ("5KQQUBxyCNJtCWAN5dxFWmBmYfZsKn27XhcKBqvwsVpG".into(), default_blacklist_reason.clone()),
            ("GaFBPJtPNqFaSuhc4rQNp4VwjjgYfRfHhaxYWze2Quaw".into(), default_blacklist_reason.clone()),
            ("6VcHke5SeCLSwwbYpHe5u9Hnx7S9RrLgqqj4uVj8dgzT".into(), default_blacklist_reason.clone()),
            ("HXj2GeMSxFh7qywqsE3m7CGtxhyZGHjufyM1ZKoAiYjF".into(), default_blacklist_reason.clone()),

            ("97MMdGkcDBPCgTbrqGyS4UCbZBPHHwGA2dxEVgQnCixj".into(), default_blacklist_reason.clone()),
            ("5j9mHgcsRTqsmqeaSNCfhzcEAzpz8YejQoHtpuxoF9hb".into(), default_blacklist_reason.clone()),
            ("8hpaVczvUK24kogYWxV6s3hajDAbaHb6KGZsVRLDoksi".into(), default_blacklist_reason.clone()),
            ("2wnbjxUJaosewQV2Ti49PDVzFLB5k8FznyGwc37h2JBn".into(), default_blacklist_reason.clone()),
            ("TomHwzaaYkX3wgXFVU7aaJHs9uxkL9M3Gcp3zZuMWJJ".into(), default_blacklist_reason.clone()),
            ("HB9mUjrtPof9YoNhxPKe62mzvSVZeM2PPnp5Ns4uLAnh".into(), default_blacklist_reason.clone()),
            ("9278wVShBFgF5JyDbivorgcCuvdP5J4gfMDBp6vpgb2X".into(), default_blacklist_reason.clone()),
            ("5tKVdfSjikqPUrYxYYTVqciTetXcCzy5y61jRjohuk4u".into(), default_blacklist_reason.clone()),
            ("CP99unpGKUeY4TwaMJYkArFwPsDWLTSMKo3pEWxjiWmZ".into(), default_blacklist_reason.clone()),
            ("6JjyRSGWNQKnrXV6CVnrH8obL6rTUZV7BjFD54WbF4V1".into(), default_blacklist_reason.clone()),
            ("TxDx1cjjxb15qEUSZWDpHyRnPb1vkYB8djt1scaFfhm".into(), default_blacklist_reason.clone()),
            ("6KmmUrKqxrogcXb1yefm975ovuhyoPfyJjVnQ7KsrqYu".into(), default_blacklist_reason.clone()),
            ("AdWoQBvHGj2u2Mb5bi7SppVrcVU4wRu12auVtdNjZpzt".into(), default_blacklist_reason.clone()),
            ("CLheCY8APDtyeT9ipzY23dkLxB3efaLEvS93w6RTfw6L".into(), default_blacklist_reason.clone()),
            ("4WYnyFxFczX52uGKt8ZDpjS6HsX13oLyQfbGCuKRa3A6".into(), default_blacklist_reason.clone()),
            ("6ABYDxLq9w8kFJYmoR1FfsUhxjRLPgHRTXoL2dk1QRBQ".into(), default_blacklist_reason.clone()),
            ("6jRRQHGTBa88VnmgrkXHTTyEcuak28J5AV2FAP4WFU86".into(), default_blacklist_reason.clone()),
            ("DZVbCCRTbSdyhRBa96rKh3CmX31TFi38CtVZpmoPLzBR".into(), default_blacklist_reason.clone()),
            ("HRDjVXVUfqgmPQn45cxaiECU6ZVx9SNxTC9NGPVkMCoj".into(), default_blacklist_reason.clone()),
            ("CtPVrVHadXPy6EJcs4sPSS6EWm8bF7uM9DSuCxf6PP8g".into(), default_blacklist_reason.clone()),
            ("HLgNQEwEC1jQ2HgmBRvmfap5MhnfadcxfXY9tmmhJ2mB".into(), default_blacklist_reason.clone()),
            ("HdLojNZAUDFYBQz6to3TwhiFRspAbLZyy7QTYCPbs5Mp".into(), default_blacklist_reason.clone()),
            ("6dpXz896kcPcx8vYpXPCqTjLcHEnEX3VbvFemkz7sck4".into(), default_blacklist_reason.clone()),
            ("2tiNTQ8a7QLTCivwMu1At5GoqoJRPvMpwmrLKdSdmNg6".into(), default_blacklist_reason.clone()),
            ("4FaZw6e4VTrnAb1Ua6VefVYSn8YFiC6jZ8kT9Ld6GBxW".into(), default_blacklist_reason.clone()),
            ("DEaLiXYzAMDYEwA9nUEXKndrNs4dkW9VSa5GmKhhfNot".into(), default_blacklist_reason.clone()),
            ("DL18yy8NUSQWTwUhk6MTg4v9njxg1oRnvQuLfEq2RmQq".into(), default_blacklist_reason.clone()),
            ("wHJqM9cri8Hss9tkPsZe4tMD9Zrbp3GH39VYUvfpmSp".into(), default_blacklist_reason.clone()),
            
            ("9hNNHBwm6BC5DSmQBp3hKKNL8paDMt6kW5q7J7oC2VJZ".into(), default_blacklist_reason.clone()),
            ("2Ka1ox6B4yse6QQMXotB9gRTF3ZmPynn1DuNfGLXZyey".into(), default_blacklist_reason.clone()),
            ("AruoPzGrtfAaqUFPUDVmzUHDBjxFJLD8nodzArr346yR".into(), default_blacklist_reason.clone()),
            ("Foo9xhhkDqP24egwYNaWcTh2ZdAAV79UJSuemrsMisLt".into(), default_blacklist_reason.clone()),
            ("5sJPiR5pbxkYwCKCeoWHU65nxYX3acAueXUGv4BLyboz".into(), default_blacklist_reason.clone()),
            ("HMadUcegZc1hsfuki1mnGmq6bjJcRBK9V3dnuBzxfmb4".into(), default_blacklist_reason.clone()),
            ("9UToETxJBEszyJxDtmXiPLdpYNUizUyZyJrPjBqHfm2c".into(), default_blacklist_reason.clone()),
            ("FF4mDqzcP7YQgaBnoqkYsP8KFDfMsQV3mtESkch1U8bw".into(), default_blacklist_reason.clone()),
            ("6cGL7sSyrmMnrpmckb7y5MJz3sDuNsxMvyyBfbMnQnex".into(), default_blacklist_reason.clone()),
            ("JEChFiyPuyRPqJZdhFqVgrzSmtE5buA5NvGesPQLXKQf".into(), default_blacklist_reason.clone()),
            ("6v3hEkQ89u9cchjFYp2ZLeHFspQcWmJVLmp2aStB5nEt".into(), default_blacklist_reason.clone()),
            
            ("AFPhtNfns7wHz1gJcVGt4SqbKsPt14YShywa75QGTJ97".into(), default_blacklist_reason.clone()),
            ("EmXn9UUNxkXZyKZJHFoVmBK9ETFrfTtFmmEY4U4RfbAH".into(), default_blacklist_reason.clone()),
            ("9Gy7YX8S3C2df5ghdQQuspRYw5rJMns1ZhSxeYNkmdwP".into(), default_blacklist_reason.clone()),
            ("CZw1sSfjZbCccsk2kTjbFeVSgfzEgV1JxHEsEW69Qce9".into(), default_blacklist_reason.clone()),
            ("C2Ky72Dr2V8Jz7Vbp5YJw6kTiH5cn7n8sYWJo8bbjPtR".into(), default_blacklist_reason.clone()),
            ("3JDvbUoSpaSv8FwPCRJm5DB8Mt3qckxFHNoGdLd7Zd1k".into(), default_blacklist_reason.clone()),
            ("9MqENXVpBBPTs4XbVK7TfqCCmBfJCxV1GYnPKmDSXjpo".into(), default_blacklist_reason.clone()),
            ("4GL3rCXPpvu9TePSEzVetJwAMhztHENWJ1bWtX5s6isc".into(), default_blacklist_reason.clone()),
            ("EH6FCQGTGrwnUcgApM7gyzBggAEGCYQZa61ku6jhGkhi".into(), default_blacklist_reason.clone()),
            ("8RWnrqVrZXXckoqfXz41uvzmbtREeLFaAquRwq3yQAd1".into(), default_blacklist_reason.clone()),
            
            ("C45RHYsHWCdeaLJ2x1VfsKH6PgFBbuH6XKHywRTgJCtd".into(), default_blacklist_reason.clone()),
            ("5d4ECeozGJN1spj1dBqMEkZDzRAXDa5hvpzZytCJMRav".into(), default_blacklist_reason.clone()),
            ("AY8LpVmVTaDMPmUsr41YCxivmy3VVtKtLvZmTNtJ92CC".into(), default_blacklist_reason.clone()),
            ("9Wz6CnPPkiu8wDbkfWpTREgEiWRN4QLykjct49DjZYEp".into(), default_blacklist_reason.clone()),
            ("4yw5YqiXrcQGoMPRP2qiyVsSzjhmucuq4585cq6arNVg".into(), default_blacklist_reason.clone()),
            ("BEnXovPGU8DgHTTpaHt4eTeJxmbBcEDxd8fFvR7PDvpY".into(), default_blacklist_reason.clone()),
            ("FQY5UU6THEhRNZRg7YXfYGQhJi45TLXrHg76EsXJmESc".into(), default_blacklist_reason.clone()),
            ("Ea2vEgAyt6KWD7GGXhvadxBNrQRfQkLNVqv4WbACotju".into(), default_blacklist_reason.clone()),
            ("AqSThC5LAYcfiPM73cdwDfgrCvDKHXe1TmCpUE5tnmSs".into(), default_blacklist_reason.clone()),
            ("denbgNhoGgvruFNaz1UiH1gc56RooG23TWr4gNSCmah".into(), default_blacklist_reason.clone()),
            
            ("3pSQk1HfYravmidy3JHzgVtD5s2Mbnd2feBYJdduB7Bq".into(), default_blacklist_reason.clone()),
            ("AfrkokqBnJSSdK4Eo7AX9iFqhbjY4drAMFf6W814tDei".into(), default_blacklist_reason.clone()),
            ("qx983oDJVnXRb87pDz7w1WWJgaAa8jHj8oVDTWJubo1".into(), default_blacklist_reason.clone()),
            ("4sw5oZBupkTe1g74mqCaFiw9YSrjgLbPAKtPa1Hv7LSi".into(), default_blacklist_reason.clone()),
            ("GHCvUqiyiQokj5bfy9CHCWrXAaFo9NNn43s3tNuZH3g7".into(), default_blacklist_reason.clone()),
            ("BtB9jAbgeE3YsPX67BoyxPDHAy6reTC7iim8C4agARZC".into(), default_blacklist_reason.clone()),
            ("8HA2gPaMXKXJDXpbFVxrmTDCtZZDPuEJqP3Vu3xwyq6M".into(), default_blacklist_reason.clone()),
            ("4DFitbACoNqRFe4ofhPxhR5ZjqZqS7UiLxLAkY5K9KfJ".into(), default_blacklist_reason.clone()),
            ("3MvtE9TsVm7hUiKaNAuJ4JZX8CaXRRdvMfAciJ9DCGLS".into(), default_blacklist_reason.clone()),
            ("HPx7uv5ygHanVpVYsJMQGF2L82JuXDJtbxxuS2bY2qgk".into(), default_blacklist_reason.clone()),
            ("5wacKbXahnCfRFwNwJT7ynyvRTmyHuqwvPvKspmgVcip".into(), default_blacklist_reason.clone()),
            ("3R8K6iWxNHmKSBcN3taMU2YuzfuNMRjdphnLtsG1TwZq".into(), default_blacklist_reason.clone()),
            
            ("5hTJiibi4ADun78r8nYSP6eza9U3vW5e9GoekCBuSz2R".into(), default_blacklist_reason.clone()),
            ("BwdLcSgJPHcomcs9YoDddNeWiSo2vVw8bGMxAyNA29Na".into(), default_blacklist_reason.clone()),
            ("7K316SSJjaLvm95CQe2XuLjYsntQqUDUAwQ71jZZdRxw".into(), default_blacklist_reason.clone()),
            ("7gds7PbCzmHbJStjxA5L5K8cu2LVUakmd3MDXFHSfcic".into(), default_blacklist_reason.clone()),
            ("voteyJUJ3XVr7yPVWwmpiKtRk2EyNJHZXqi3zGwcQ1Q".into(), default_blacklist_reason.clone()),
            ("LcmWVqpv45eunfxDo11aiE4EmbgaEaBTftJmj7bxufA".into(), default_blacklist_reason.clone()),
            ("3Ct86ehxKiPsD2EnymYECnt1nkfSSw8nxNbdJseY31EC".into(), default_blacklist_reason.clone()),
            ("AnemqvUhcyeXiSz7keY3ZLe69Xzdkjdt7ZmXUQXSksKR".into(), default_blacklist_reason.clone()),
            ("48RHTYNbfKg4iF2ixNGzmiAevp5FEb4ReTbDf95hhVhM".into(), default_blacklist_reason.clone()),
            ("2eCotsz461YrtDDvYbY4neV5oavNyf2o2Z7Zhb8RDc9Z".into(), default_blacklist_reason.clone()),
            ("6hTLQ5HSdWcpZkbXmZxXaGjCgTh7zh8UeWKWKgGE1BPp".into(), default_blacklist_reason.clone()),
            ("7vmQeg3tFytF8BSYbC5uEwzgCMs9vxs1s3MhSSi4VJC9".into(), default_blacklist_reason.clone()),
            ("CjV6Qcbn1UqqV1mXqRkEzD3MLijUfyVcdt7tK8Kgo4Bf".into(), default_blacklist_reason.clone()),
            ("7gds7PbCzmHbJStjxA5L5K8cu2LVUakmd3MDXFHSfcic".into(), default_blacklist_reason.clone()),
            ("AY2GALqtysVTvrhZghUzLwgGBPkcFoZGTQ1dQT2xw1KX".into(), default_blacklist_reason.clone()),
            ("2TmFPDTyCkuEAMQgf4HdEeSqim11oAJfVKzarEEyFUiU".into(), default_blacklist_reason.clone()),
            ("AJUWPjhNKgo37ta9ycv9bS3DYAT1DY4NP1DDi8DwLeXG".into(), default_blacklist_reason.clone()),
            ("6AqFc9V6PqyXJReuP12ATGggaVpG1Ppg4LFNvnqQYz8B".into(), default_blacklist_reason.clone()),
            ("HS2kSsdkGF7iktkPYFVsWWkUwEwj8jgvuKZiF6JNbGgy".into(), default_blacklist_reason.clone()),
            
            ("8oMTbpkwUbSoW6jS2sLZTxeXexSx1V1JbzL9PYC9JDnd".into(), default_blacklist_reason.clone()),
            ("3MvtE9TsVm7hUiKaNAuJ4JZX8CaXRRdvMfAciJ9DCGLS".into(), default_blacklist_reason.clone()),
            ("8yz8LvMFkrjN1qtokYvK1X11c6DveWb8ATZuq5mkmJNc".into(), default_blacklist_reason.clone()),
            ("CBWhs4dy6wfLyuEZX82CT3WHKKFdcDgRUTPqiq7r36WN".into(), default_blacklist_reason.clone()),
            ("DHdYsTEsd1wGrmthR1ognfRXPkWBmxAWAv2pKdAix3HY".into(), default_blacklist_reason.clone()),
            ("wECsfV2PCzt9VkWxiNLZFNfSeroQce3s8MoGi7EGE5T".into(), default_blacklist_reason.clone()),
            ("H13nDMS5zkPB2Dhbk1k47UyBaEFaVaHpA6rDDuEWEhfQ".into(), default_blacklist_reason.clone()),
            ("HGwcdwquyMtpK6VYLsgYTgftCLQabkxwpDykCHE2CWyg".into(), default_blacklist_reason.clone()),
            ("GbtCUJadbiNrpdDKFK8Tg785rFi3MzyHMm7Qvc5n7WFU".into(), default_blacklist_reason.clone()),
            ("6Q5YjejgNFCnXCt52nq6MPx5wTWFmbihgt2JgV3zxdiD".into(), default_blacklist_reason.clone()),
            ("B73HG2sLcNW4A5J9KwY3GWP7XML1SF7nkyc1MBVciW3G".into(), default_blacklist_reason.clone()),
            ("eRBjr1X7drpEprHddEyP8CLt2BjTcfQK74nF4YoMDN8".into(), default_blacklist_reason.clone()),
            ("BCFLyTNSoxQbVrTogK8n7ft1oYAgHYEdzafVfGgqz9WN".into(), default_blacklist_reason.clone()),
            ("Amhxcj1nt4BhnmTfy3ncqaoLzVr94QEfGMYY9Lqkg9en".into(), default_blacklist_reason.clone()),
            ("5uT8uw9o7c1AFi1xj4qFrFuKuyuoB1cGZKc973Cuk9qD".into(), default_blacklist_reason.clone()),
            
            ("6sgx1hJLphe5UK3YGBQ6roetRNzt5TBGoenwmZuAJUve".into(), default_blacklist_reason.clone()),
            ("CDnpa7PGGAaJhXEaL6exXW4TfnY5Qd5jyEusYsx282uk".into(), default_blacklist_reason.clone()),
            ("9Ejo54oXu5JD9jWMkSbokeUhUF5a3YfwZvVtvWSJ1nuH".into(), default_blacklist_reason.clone()),
            ("HuFGRk8DT9zw6FgSYKh1FngDLJjPbEADB4SAkLPr3iPR".into(), default_blacklist_reason.clone()),
            ("BQCSzReSQK1uWNGGJbpKW5auYBgxiqMrnnELBNMtBotz".into(), default_blacklist_reason.clone()),
            ("HPb7UffwnYV29n6XVz2XuUDfCB7HAW1inmPQSQRfkXau".into(), default_blacklist_reason.clone()),
            ("DP4wMyjbHWqgJhQHvfDXkg3t1WEScYnagh44Cz4SaN46".into(), default_blacklist_reason.clone()),
            ("3Lob7j9sNsbTDM4VnTFEuCEuMKQd2nwsWfPMzkRoKGHC".into(), default_blacklist_reason.clone()),
            ("Es32knSWdTsjy56mqQXH9xNcWARCAmbopGEtHrKWKE8s".into(), default_blacklist_reason.clone()),
            ("7y3tfYz8V3ui67XRJi1iiiS5GQ4zVyFoDfFAtouhB8gL".into(), default_blacklist_reason.clone()),
            ("5Ur6LJMMUC8pRxSanR9nJVEdMBoadRavF6xk8MTC6kzc".into(), default_blacklist_reason.clone()),
            
            ("C7NjhyfZ9Z7MhYkiuyj3EEXZqsiSEr3GwkruULx8QsWe".into(), default_blacklist_reason.clone()),
            ("2EoaPgNSGbB3JyP7nSfiK5Wq3eME3LgbbEbdPim4CnVm".into(), default_blacklist_reason.clone()),
            ("3VRZ8nDGRPoGo7djjmJKSxXj1JnWD63ZFMibyiN1xrBH".into(), default_blacklist_reason.clone()),
            ("3YpM2qJPx8Nw9gvzvdrhbyU2kNje8CWbhpa9YP25r6ac".into(), default_blacklist_reason.clone()),
            ("G82NyddqF8y4PtYy9AWDfd2mzdM3ACNG762KLvNkEbpu".into(), default_blacklist_reason.clone()),
            ("D3WtKqiGps8qivjm3VxJkrPJ7qvvm2KUNECc4bAtgoYH".into(), default_blacklist_reason.clone()),
            ("4jZvFhfE7AvDUfUCvBWpGvXgmhSgdYtbyqtr8yrbRUqF".into(), default_blacklist_reason.clone()),
            ("HdJ6mGGfz8FXPXUyVMXRMhaQ5vUiFswQUw64for5pyjb".into(), default_blacklist_reason.clone()),
            ("EMBiTsHqbkJuWvt9Y28av6uUd5H996iSta3ZyCBoLamz".into(), default_blacklist_reason.clone()),
            ("Gcu91CL5vrjeQfKabtmwe8cxP6bceK3TMndR3Rsse8de".into(), default_blacklist_reason.clone()),
            ("13zyX9jfGy1RvM28LcdqfLwR4VSowXx6whAL6AcFERCk".into(), default_blacklist_reason.clone()),
            ("EW45cgxYm6wEpzYYgsSHsPKkmYZ2duAhCzVuroS4k9Q2".into(), default_blacklist_reason.clone()),
            ("2jevuBmk1TrXA36bRZ4bhdJGPzGqcCDoVzRcyYtxzKHY".into(), default_blacklist_reason.clone()),
            ("DHasctf9Gs2hRY2QzSoRiLuJnuEkRcGHSrh2JUxthxwa".into(), default_blacklist_reason.clone()),
            ("9uygnf8zm2A88bS4tjqiYUPKAUuSWkJGeHxKLTndrs6v".into(), default_blacklist_reason.clone()),
            ("DZJWKjtj1fCJDWTYL1HvF9rLrxRRKKp6GQgyujEzqc22".into(), default_blacklist_reason.clone()),
            ("964w4qykexipZ7aCur1BEeJtexTMa1ehMUc9tCcxm9J3".into(), default_blacklist_reason.clone()),
            ("9xMyJXgxBABzV5bmiCuw4xZ8acao2xgvhC1G1yknW1Zj".into(), default_blacklist_reason.clone()),
            ("FrWFgD5vfjJkKiCY4WeKBk65X4C7sDhi2X1DVMFCPfJ5".into(), default_blacklist_reason.clone()),
            ("4e1B3jra6oS7nK5wLn9mPMtX1skUJsEvmhV9MscA6UA4".into(), default_blacklist_reason.clone()),
            ("HTpinijYNYPe2UhfwoX7fHKC9j44QEJoVmStCmfvYZxA".into(), default_blacklist_reason.clone()),
            
            ("6WeYy8AYNrC1KPDyqjyPDXCURKnhXQnvznQ5GxngyFUW".into(), default_blacklist_reason.clone()),
            ("HjUP5kR1p2vC9g4Rwd7oyfFU8PFBXC1JW44Uz9T64CUA".into(), default_blacklist_reason.clone()),
            ("CQCvXh6fDejoKVeMKXWirksnwCnhrLzb6XkyrBoQJzX5".into(), default_blacklist_reason.clone()),
            ("9tVayBaJ5drRmxLfpWTfSeAUvS4AnSnmhdVvEzUmGMBr".into(), default_blacklist_reason.clone()),
            ("EgLM96Cb4toRdAJ1hAvLevbfn16Y8n4Q8r354xjLeRn9".into(), default_blacklist_reason.clone()),
            ("DWJRav7H6ge9E3y4BNaRX6Wg4RuawDWWv5kjry9zgRAb".into(), default_blacklist_reason.clone()),
            ("s8n21npgkZdBGHCA4bwPz9uWYK5xXJTDJ7LBZhH24pu".into(), default_blacklist_reason.clone()),
            ("D1W44N9Ztmntz151AVnK1MYMjkhAzZ9EymWRHvFkcaxK".into(), default_blacklist_reason.clone()),
            ("B22LuYgwjK4qPzbUDNC2R6R9L9CHdJErQMi19hfWtjs5".into(), default_blacklist_reason.clone()),
            ("8pVbE9wLShpShyEgppjwG8UvsB54NCwfbiYh5Wmfg7fu".into(), default_blacklist_reason.clone()),
            ("A7uE85Qd4Wbw4TdF9sqbqpbPSKYpes6Q8Jc3DZeYfuMm".into(), default_blacklist_reason.clone()),
            ("3fAvmL3MsMCu5iw1FMujiBSZGFEXzq6S8bY5vHrp5mZk".into(), default_blacklist_reason.clone()),
            ("3Le35iTn2KXRfomruXiDLcMd4BVLKYVgQ7yssmrFJZXx".into(), default_blacklist_reason.clone()),
            ("4QMtvpJ2cFLWAa363dZsr46aBeDAnEsF66jomv4eVqu4".into(), default_blacklist_reason.clone()),
            ("wMH4ny9S8iDF8pWGQVVvJNurMuFQScFAhceYWdnS9Ko".into(), default_blacklist_reason.clone()),
            ("8g5tkmJwrmwD7kEBm7jTzmB66o2e4quRcB2G5SfakUA2".into(), default_blacklist_reason.clone()),
            ("9HvcpT4vGkgDU3TUXuJFtC5DPjMt5jb8MXFFTNTg9Mim".into(), default_blacklist_reason.clone()),
            ("4GhsFrzqekca4FiZQdwqbstzcEXsredqpemF9FdRQBqZ".into(), default_blacklist_reason.clone()),
            ("6zDrZWRXQ7GWi1W2fBTzSs59PSa2uj2k8w2qkc451rqG".into(), default_blacklist_reason.clone()),
            ("2ikGwX24ATJQHPtWpHupEAJvAyp63niaFL5R2sGXwfnd".into(), default_blacklist_reason.clone()),
            ("D6AkdRCEAvE8Rjh4AKDSCXZ5BaiKpp79da6XtUJotGq5".into(), default_blacklist_reason.clone()),
            ("8sNLx7RinHfPWeoYE1N4dixtNACvgiUrj9Ty81p7iMhb".into(), default_blacklist_reason.clone()),
            ("816finfFF5c56b8App1UAujg3cKDkPTZJe849ShnpDh4".into(), default_blacklist_reason.clone()),
            ("G5p5uNWuTUDfgeuSkaFbJChVB8CwHiKgvdtQFZKqGyNE".into(), default_blacklist_reason.clone()),
            ("9EMmPd6zKqTnpj74rgmkTjkYAsZSZ42jBWcqu6iaoGTR".into(), default_blacklist_reason.clone()),
            ("8QXUZU3TDjxpg9gAox4awS7Hn8nppCKpRvoM7rq9vadK".into(), default_blacklist_reason.clone()),
            ("7SkxD5JzbJCt1AZMMmU4Luiz7g93eSgLsGV7SQyYYR2u".into(), default_blacklist_reason.clone()),
            ("8PtwJFQmrz8HPj3R7EPRz27J5Uxa9bec6zDkSUBBDunf".into(), default_blacklist_reason.clone()),
            ("91Xm4Pz5GKJevFEfeUJYEh32jyNf4ZFNZy7WFY9Qwnd1".into(), default_blacklist_reason.clone()),
            
            ("xnYS1CG3eGK1XinCTRxoLZyCz7NcAmYGjJTPYYk4Bs1".into(), default_blacklist_reason.clone()),
            ("xCqxG4z9knKcWW69CuYXrkvDesSe1xNjqRpS529c1JD".into(), default_blacklist_reason.clone()),
            ("uxprzdst3wJL8xZ6YYgoC8H4aQBUT7svFZ9JrSJtmAS".into(), default_blacklist_reason.clone()),
            ("TutGQ8dvk3a7QFv1FVBChHZR18tjfKrCKQnjSzDCwNv".into(), default_blacklist_reason.clone()),
            ("sKnGgQLEETgBrq3fnbJXMSQoBQeZXHtLjKXLBRftJ3L".into(), default_blacklist_reason.clone()),
            ("Psbmr1qMd1qPAGaSw7epc7TYv1pxyvoUhCwSQJ2RykU".into(), default_blacklist_reason.clone()),
            ("JCHsvHwF6TgeM1fapxgAkhVKDU5QtPox3bfCR5sjWirP".into(), default_blacklist_reason.clone()),
            ("JB8zjnRE6FeT8N6Yq2182vj69kKHGdeKJ7kBAhHKuHRq".into(), default_blacklist_reason.clone()),
            ("J595rCZNHkRdvWUw1PLarnbT399MXGMy88dZj5h8YcbF".into(), default_blacklist_reason.clone()),
            ("HwS6NkG6XFd1umE2ZySrtd2viuoAbbEKrQFSi8Rwdvnd".into(), default_blacklist_reason.clone()),
            ("HskwvTWtfoWuP7C3fVxRhPHAamCHwiXHb5gaZgWZBLmS".into(), default_blacklist_reason.clone()),
            ("HrcY6Tewg1mWUoqCqSctc9i8Qhh53hNUFxMYz6AzGSWi".into(), default_blacklist_reason.clone()),
            ("HNeRuSiDf2zazfodgHw1Uaou3eLLZFHZqLKw1caGpBTS".into(), default_blacklist_reason.clone()),
            ("HjVDuh1kTuWtKrvDjwXjL4Cz2fVXtgBy7bMcUMjtFrcw".into(), default_blacklist_reason.clone()),
            ("H96ZL2dWX4xQtLSKE9PhDeUFY4dH1Uy6d8k8vJBBpREV".into(), default_blacklist_reason.clone()),
            ("GZXi3rnkvvQz5s7xf72zGUKf9TfQQBKGhr7spwJmZ8Av".into(), default_blacklist_reason.clone()),
            ("GV6qwu5Pc5VJ4HKQ9EVx1c4pnaXNs9Bnm9t9Pw9h9YUe".into(), default_blacklist_reason.clone()),
            ("GRuqWk1zN7LTTDhxVkEGFu9SYUThyYs7jHksWuVat6VB".into(), default_blacklist_reason.clone()),
            ("GrtgeXvmr4AuoiBGai6G8GbxaBy4oVhPozb9bv9BDYxL".into(), default_blacklist_reason.clone()),
            ("GkBrxrDjmx2kfTMUZJgYWAbar9fEpYJW7TgLatrZSjhN".into(), default_blacklist_reason.clone()),
            ("Gjpy5mTSRL2JxWNT52fgtNoMeCFWfvBY6ZDDMvw2B46w".into(), default_blacklist_reason.clone()),
            ("Gj63nResvnBrKLw4GyyfWFpTudwQWDe9bExkE9Z1LvB1".into(), default_blacklist_reason.clone()),
            ("GeFZK7cNMbtoBWKEyXPMSFogVyN6AtHpmp5GMzpXwRdE".into(), default_blacklist_reason.clone()),
            ("GbxaecfPrKMXPDDCHnJaqwnZfAumqWsqA57hhC48D8jy".into(), default_blacklist_reason.clone()),
            ("GAerry2FZncXgLJXohjgGmC3W4JKLDFxwhGz4beTgDeP".into(), default_blacklist_reason.clone()),
            ("GAEiWiaZCLLFYjEAWqiDQzF48sTTGRqcFMNRoogPz1QA".into(), default_blacklist_reason.clone()),
            ("GaDDtFwGqomMr9GnKCH3bbW2CUW2oHzjw2k1d2xCj55Q".into(), default_blacklist_reason.clone()),
            ("G8RgN7KuHypqfrcqKJaQaL6GTMVHNXa62rRa3yESyjMC".into(), default_blacklist_reason.clone()),
            ("G1jYoKytyTZEgGK8PGQQwJ2ti75aMJ2KXfAn1QKgMfwX".into(), default_blacklist_reason.clone()),
            ("FzyzTv3SkVjMMKvKdVhSc46u8XMhrXZmVg82ZtzbpPFn".into(), default_blacklist_reason.clone()),
            ("FzceyKW8NKaXP4ZodC4GTKQAoxGJsfnRmqkaJgjCZew4".into(), default_blacklist_reason.clone()),
            ("FXDYZ61mPgfD9THMis7iAQ4hQbx8RRCBbhwGxuxeAQ3v".into(), default_blacklist_reason.clone()),
            ("FvoZJRfV8LWMbkMeiswKzMHSZ2qvU8KVsEkUaW6MdN8m".into(), default_blacklist_reason.clone()),
            ("Fu4wz4US6dV6GzZrv9NnF18KeT47tdbDKRd7pA6DiyS4".into(), default_blacklist_reason.clone()),
            ("FSH9xke8FBpx6YxEEzNXVWgjmT3G5SN9HpipmCSVamV".into(), default_blacklist_reason.clone()),
            ("FpRGpTYyBNLqgWnSWTBG6Y7DHtE2E2oHrue3dEHbrcSk".into(), default_blacklist_reason.clone()),
            ("FN7gfdj5hoovXh7VBbuwzwt1L3db62EfarB8CZ3vJj24".into(), default_blacklist_reason.clone()),
            ("F4TutHgj3ZWrViLKDpaUXkL5wdM6K3mE6gGstG69jrC5".into(), default_blacklist_reason.clone()),
            ("EwJwtsU44Ayr1DSPAQ69XY5jxzSZJSyKPqVJ7MpfqEbj".into(), default_blacklist_reason.clone()),
            ("ESF3vCij1t6K437j7tzDyKspPeuMnYoEtooFN9Suzico".into(), default_blacklist_reason.clone()),
            ("ER87FSCghU7CwYpNR2jED6XZQE7T4QfU4591WAjzvi1Y".into(), default_blacklist_reason.clone()),
            ("EGEePZL2pmTEuxzDBewnK5Z2FQ7MjCpbunYQ7DT52mjT".into(), default_blacklist_reason.clone()),
            ("EeM1E8NaqrwxYRNoybbLHaUoWhNxPU5awfy4S3GozBFC".into(), default_blacklist_reason.clone()),
            ("Ed7PoLidA7TP3xK9jiYnYwNR3WJDnkHFU7LLhNzvMLG9".into(), default_blacklist_reason.clone()),
            ("EbzV57A1pQg9x8Q12un7m2uLPa2CJbFEegTs1r1VL8Rs".into(), default_blacklist_reason.clone()),
            ("EAZpeduar1WoSCyR8W4YhurN3FfVmuKwdPx4ruy58VU8".into(), default_blacklist_reason.clone()),
            ("DzhGmMUzpyQ5ruk5rRCfekTZMyvPXBXHtnn6aNnt94x4".into(), default_blacklist_reason.clone()),
            ("DY4dSYQXFYzaZKapkNHt1iG2yxBf6y2mxSD7UfWfvMee".into(), default_blacklist_reason.clone()),
            ("DxHviBB2HQejsBUBF3wvwfcJLvtJYSGDaRotHGUjGFBN".into(), default_blacklist_reason.clone()),
            ("DWqXepa5ufu1ju5pfdRr9exUZLkzzcFxvbm352CPdiu4".into(), default_blacklist_reason.clone()),
            ("dVcK6ZibNvBiiwqxadXEGheznJFsWY9SHiMb8afTQns".into(), default_blacklist_reason.clone()),
            ("DUgjvHMiBPpcwGSTcNnGWxDUyyjtSc2jHkUn87WVrmZw".into(), default_blacklist_reason.clone()),
            ("DREVB8Ce8nLp9Ha5m66sduRcjJtHeQo8B9BkYxjC4Zx3".into(), default_blacklist_reason.clone()),
            ("DqxQuDD9BZERufL2gTCipHhAqj7Bb2zoAEKfvHuXWNUL".into(), default_blacklist_reason.clone()),
            ("DPX6SNZWBcYTn8q46HcE4LZWu4qh6mN28T9K2mPp37dE".into(), default_blacklist_reason.clone()),
            ("Dn1qseaTD9269EvdpWGLZhaNY1PKbjjdRZcffeqKCFR5".into(), default_blacklist_reason.clone()),
            ("DGu5PHMFTRhrRcqXw3EfmMxHk3eYLVL6iWbieQ63GYoN".into(), default_blacklist_reason.clone()),
            ("DEynu4A313UtdCEL6Kga9EgP76dx3PCusjmE1ZRhE6UA".into(), default_blacklist_reason.clone()),
            ("DEiU4gssYyW9vYC7x7NyA1zcP56WSbEkt7k9owCPPfRj".into(), default_blacklist_reason.clone()),
            ("DdvbBv6WuiEi4BpVTbsckRVwkJekjZumq4z2xS2shTjd".into(), default_blacklist_reason.clone()),
            ("DcbnYSBPSscNZNUk39mj5xtjCUdXM6QA6oRa4KrnTdaC".into(), default_blacklist_reason.clone()),
            ("DbipVsWSC9e3wZesnDKM43pGFEvBCWpVHrgZhJLW3nj".into(), default_blacklist_reason.clone()),
            ("DaPBtYGAC3Pabq5JmDEM8pDsbP6GbbbGn862vQiwim1w".into(), default_blacklist_reason.clone()),
            ("D2KsbdXz16tAPaaiANS1fVt2bJm8SE5z8wiPN2bW3yE1".into(), default_blacklist_reason.clone()),
            ("CwEsA6kkUZHnuCK2HC1WVpriBpZWFJSKW9xxrdednm6J".into(), default_blacklist_reason.clone()),
            ("CuuHN7Lg4FkGcBEam23CuRRrsT4fVAHLgDUNzXmJJvZ".into(), default_blacklist_reason.clone()),
            ("CSryvZQs94UfwQUhgEAqj4LRKft4xzoukCiCS7eEEmYd".into(), default_blacklist_reason.clone()),
            ("CroZVDJM6dBS6DtR8wkaxBfxvc3gaWissAMqMnA4N1wm".into(), default_blacklist_reason.clone()),
            ("CnYYmAhuFcyocBbXxoVzPnu37a5ctpLaSr8ja1NGKNZ7".into(), default_blacklist_reason.clone()),
            ("CLdfLewYHCrdUjpw4jFtJS1ogiSgMRVc1gk5NnZaMukK".into(), default_blacklist_reason.clone()),
            ("CGWR68eEdSDoj5LUn2MGKBgxRC3By64DCBFHCoHdSV21".into(), default_blacklist_reason.clone()),
            ("CgG18EFLfstz7aLVNfnD7iDJEiM25P8cbZr2HDkr1MQq".into(), default_blacklist_reason.clone()),
            ("CfzGgitcUZEWw2vdfNqvRTchVcWXamoXv6CabAsvmBwD".into(), default_blacklist_reason.clone()),
            ("CFtrZKxqGfXSuZrM5G64prTfNM8GqWQFQa3GXq4tdzx2".into(), default_blacklist_reason.clone()),
            ("C7meWRbJLdvzDTtytJMMKHsfmEboBgRPR4Cr2SjmHvQA".into(), default_blacklist_reason.clone()),
            ("BxTUwfMiokzimVDLDupGfVPmWXfLSGVpkGr9TUmetn6b".into(), default_blacklist_reason.clone()),
            ("Bpf6nNTfVgxAA9BWWQtQaYSyBXFccdDxq2mnCBBAyNnd".into(), default_blacklist_reason.clone()),
            ("BMsyWucr3jnC7aGsTCfvnBeG8bGavPjYD9nDEHd2BFQy".into(), default_blacklist_reason.clone()),
            ("Bm3rPaD62YWXJxvpW5viF9jUVdMmd7Q2HYA6eTbDhxxW".into(), default_blacklist_reason.clone()),
            ("Bkn11ZJ4X5qZYtRaCnNtxJMiHN4GZKyeA1dRT1nBdnp3".into(), default_blacklist_reason.clone()),
            ("BJGxCgaYAK8GySL39jhgrv6waEfh6VokTfCjJqhAtU5K".into(), default_blacklist_reason.clone()),
            ("BHGHrKBJ9z6oE4Rjd7rBTsy9GLiFcTeDbkTkC5YmT5JG".into(), default_blacklist_reason.clone()),
            ("BFmCHyipf9eWB2KSCXifvm7zULWa3Q8upwJ6pwawqhUz".into(), default_blacklist_reason.clone()),
            ("BETgSbtotJ8bf9rzuAkwTc8SqZS8TpcYW1w3is9YUigK".into(), default_blacklist_reason.clone()),
            ("BeNvYv2pd3MRJBBSGiMPSRVYcafKAXocNNp79GoaHfoP".into(), default_blacklist_reason.clone()),
            ("BDhdtVWYV1F2Wx3Q174NFJ1dhFypy6mMb24hmDKTCvyx".into(), default_blacklist_reason.clone()),
            ("B8JCqqnJnMJjBGgVv7BDrBtFGDL6bU5Q8J2SxjUdt7Mo".into(), default_blacklist_reason.clone()),
            ("B6NRKJoqzju5YniVq7aaDbbL6n39VJ7yLmJPWNhZ9Hcq".into(), default_blacklist_reason.clone()),
            ("B2oxgLLGYQvrEwt3PXGU99Y6g2SwVT8zBG1fuzt3GK9a".into(), default_blacklist_reason.clone()),
            ("Azz9EmNuhtjoYrhWvidWx1Hfd14SNBsYyzXhA9Tnoca8".into(), default_blacklist_reason.clone()),
            ("AUgLtpPVz6zL4iVCXZwi3cifLERdvnHsuVhNKzqmW45i".into(), default_blacklist_reason.clone()),
            ("AU4yDLbrnLzcjk2pnxvXwNeKJsj9CiUDRXWQbeSbk6Y9".into(), default_blacklist_reason.clone()),
            ("AsK66jWAR62qsyP2iiFzP5zfMMNEQ3aQ61RXnd293pJr".into(), default_blacklist_reason.clone()),
            ("ARrmrz549nPaS1ypzb4J6jcqRx8tJM1jdK5Lgm7q4chV".into(), default_blacklist_reason.clone()),
            ("AQB1eoovP55TyjkecjCPTvfXBEzS1JH1sxWguBo1gu9d".into(), default_blacklist_reason.clone()),
            ("AMfPKMDGqtSUqwgLVE6w6v2u6vW65Atw8BuJjybdMUHk".into(), default_blacklist_reason.clone()),
            ("AE7LuQkprFdfDW3PxqxXKfc3989YPutY9XYo6UnYnsUm".into(), default_blacklist_reason.clone()),
            ("A9hwhEeQ7hNm8rPbRX7ZDAZRjTVrUCjgDEDD4Tt8rmT7".into(), default_blacklist_reason.clone()),
            ("A465fkGZut4A7FncUvzbCzGD8QE98yn2Lm8grr93c9dV".into(), default_blacklist_reason.clone()),
            ("9XZmY7Jm3gqfoJNVXp71J9upHGVwddqDHqnJF8vzXcNt".into(), default_blacklist_reason.clone()),
            ("9U9FA435GrerUYwG8yovEJCsBbB5ZXnUkn6p28r4Fsym".into(), default_blacklist_reason.clone()),
            ("9TTpcbiTDUQH9goeRvhAhk4X3ahtZ6XttCjRyH8Pu7MP".into(), default_blacklist_reason.clone()),
            ("9SDr1FgLSmwy2keHo2mo95bUrbFmuExa3jX6DhmoxRTA".into(), default_blacklist_reason.clone()),
            ("9Mi8M1JnRmtcYpB42DxYPVmYy2safgdYFmeHmMgkW8TG".into(), default_blacklist_reason.clone()),
            ("9h8QX2WVozmoLX9hgtJJHgvkPW1nPLcvnVpzX4upEZk".into(), default_blacklist_reason.clone()),
            ("9fDyXmKS8Qgf9TNsRoDw8q2FJJL5J8LN7Y52sddigqyi".into(), default_blacklist_reason.clone()),
            ("9f6Y6QcXQ3bhHueQftA19NiBxEYEuJCnmfRVL5Aruhn9".into(), default_blacklist_reason.clone()),
            ("9EzbogBnGi8hVeLXEyFu2xUo6qi5JdEELs4y3cQXQW33".into(), default_blacklist_reason.clone()),
            ("9DYSMwSwMbQcckH1Zi7EQ3E6ipJKkChqRVJCQjF5FCWp".into(), default_blacklist_reason.clone()),
            ("9D4icPEhihxFHocwWLZFo5PF1KMqK1v1zmQNDANpdNXC".into(), default_blacklist_reason.clone()),
            ("9b9F4xYHMenZfbD8pSLm45oJfoFYPQ9RVWPXSEmJQzVn".into(), default_blacklist_reason.clone()),
            ("98QkhxpjDcm9WxETyfUC8Qd6kF1gkR2FkA4BsVeSVHEp".into(), default_blacklist_reason.clone()),
            ("94qjbdjLsmW3TEstCApxpigUkbsP4d3voQrshRUYuZsy".into(), default_blacklist_reason.clone()),
            ("8XH1nZ69AMhQCE7a6RvT6AejiruoB56uAD3KdASCP5e1".into(), default_blacklist_reason.clone()),
            ("8vQRoFTL4vgEB5HQaLXshuosSLqMoge6rN9HDThMB66L".into(), default_blacklist_reason.clone()),
            ("8usnMxy6YunbfrjHDHPfRcpLWXigcSvrpVohv3F2v24H".into(), default_blacklist_reason.clone()),
            ("8sZ92vJxQ3dDyf1LJiHX5A2PEmZ18cSaozHziTNRvLwF".into(), default_blacklist_reason.clone()),
            ("8pef4CvPjVs8nS7crUhkXBrpeSYf8RByG5vZUHihRyKy".into(), default_blacklist_reason.clone()),
            ("8kcrp8M2c5LGYThHQxVgsp7BGfGjHZ9fLHa6YN3YpFNa".into(), default_blacklist_reason.clone()),
            ("8EvEr9TchMG24F7kSE75Qdw5fcEe8qh4anVSKwj9BVfJ".into(), default_blacklist_reason.clone()),
            ("8EKcqtghqfAKwPAw47mppb6Jq1viduME5pxxPUuPojXY".into(), default_blacklist_reason.clone()),
            ("8C49wG9uXaParCrhCP8mv48NYV4P952rUoQf8AwoJyQj".into(), default_blacklist_reason.clone()),
            ("8BdTWgytNJpBoVYppiftMMwFcPB2GddzE6TnFamXPqBj".into(), default_blacklist_reason.clone()),
            ("8aFubF2aPJMz4XnR94BNz1DbZ7Hte1axLHQ177pLeG6Y".into(), default_blacklist_reason.clone()),
            ("82n1Pd5fSmmTBXuRfuaXbaKYkT6EnzA6PaZwFRGs83cB".into(), default_blacklist_reason.clone()),
            ("7Sa7UMDBHU7tHw518tjLeVZGBjTHRgbhKNMXAKpjgwT2".into(), default_blacklist_reason.clone()),
            ("7QwGLymkdP7z2DpdzFXzNb9XEG7tsdBL2ucAVVjGmeVh".into(), default_blacklist_reason.clone()),
            ("7nGrgspv4vpyadsRGmn2MjFyLweVTTNJotVYaPFEHsnq".into(), default_blacklist_reason.clone()),
            ("7LCnWqQGpNCiUvBLznYG9Q6Zo7mcLkhAHA7YBjbg8SET".into(), default_blacklist_reason.clone()),
            ("7ijG5keU7kjADTaXxoanvfy6pbWxWq2aQ5p17VdK9fDQ".into(), default_blacklist_reason.clone()),
            ("7Hs9z4qsGCbQE9cy2aqgsvWupeZZGiKJgeb1eG4ZKYUH".into(), default_blacklist_reason.clone()),
            ("7G6ofiHAXKqpYsUmaXQvzfHkSKFZuHaKpEYe4aFeR3oJ".into(), default_blacklist_reason.clone()),
            ("7etUtJx6xEFsHiaD3wYxSgTEcFA16mnhspDQjCfPY42Y".into(), default_blacklist_reason.clone()),
            ("7dRg5vUwd2FpuqoE5mPU4aKC16m2EwkKpRpmEXJFAo2j".into(), default_blacklist_reason.clone()),
            ("76sb4FZPwewvxtST5tJMp9N43jj4hDC5DQ7bv8kBi1rA".into(), default_blacklist_reason.clone()),
            ("76gw97PmYsUXWYFfcrm5tLsWJcBzzaSsWu3eG1AYNHw1".into(), default_blacklist_reason.clone()),
            ("71WK84uVismh5QndrUkxExLdk8neukH6j2WKGnVttQSP".into(), default_blacklist_reason.clone()),
            ("6XimUrvgbdoAuavV9WGgSYdYKSw6ghajLGeQgZMG9aZU".into(), default_blacklist_reason.clone()),
            ("6vJGsbs5jYKEdQGUfMEYN4Nenwscgza1dBXB3WJraFyH".into(), default_blacklist_reason.clone()),
            ("6rRtyrx7gvxoX7UAbBpyzGb66iQo77LPLoTVRtnKdHNT".into(), default_blacklist_reason.clone()),
            ("6JaxrJYVHRhNLgSwaJcje5mTkVHpYHaALiCdawdDHJs6".into(), default_blacklist_reason.clone()),
            ("6J5K6igFYyjEtkBsAUm6cZLDn9sfTFbNqBZwczQhxuX7".into(), default_blacklist_reason.clone()),
            ("6g6jypXGeavZPVkWSu4Ny5bfhTMLFnuSepfGMQkQpWV1".into(), default_blacklist_reason.clone()),
            ("6EftrAURp1rwpmy7Jeqem4kwWYeSnKmgYWKbdX5gEBHQ".into(), default_blacklist_reason.clone()),
            ("6bioeuLGkwFpNBUVovvubeMMpUfe8XtjgT3GAby2xJpF".into(), default_blacklist_reason.clone()),
            ("6aYt3MFmJcrRsXyU5gksSNyDsLnnfQ4mauaNsncn3bVw".into(), default_blacklist_reason.clone()),
            ("63HPSnde6Yeru7ev7NTbbRc8vg83PzzGz15jgEWPNqNJ".into(), default_blacklist_reason.clone()),
            ("5ZySjU8k4tS32ekHm3PFDiSyVgKm6sp83YRBjVtqnJ8a".into(), default_blacklist_reason.clone()),
            ("5yzAjbVZ2HgSpbQBtA4LtRgRZ912PRCuK4vjHZFCUzxt".into(), default_blacklist_reason.clone()),
            ("5ycsa9WVK6zFcUR13m3yijxbevZehZeCbuSocSawsweW".into(), default_blacklist_reason.clone()),
            ("5xk3gjstftRwZRqQdme4vTuZonpkgs2wsm734M68Bq1Y".into(), default_blacklist_reason.clone()),
            ("5sQ5AuSxmX2avcS99p8ECcvQAAKV3pKL5s6AoAccwuww".into(), default_blacklist_reason.clone()),
            ("5HMtU9ngrq7vhQn4qPxFHzaVJRjbnT2VQxTTPdfwvbUL".into(), default_blacklist_reason.clone()),
            ("5gkyYDaQbJ7n82MVfqRb7otBGD5HipJ98t26tpMfRPVk".into(), default_blacklist_reason.clone()),
            ("5GAWwbb5CPnDc5Y8mT1feGBGjc6qPE3MoVJm3mK13353".into(), default_blacklist_reason.clone()),
            ("5cVjyEVyD2nKmFXUZjf3AeusXNGSBRmu7LpHcogBjjej".into(), default_blacklist_reason.clone()),
            ("5asX4eKb6wne3YynLZuqMoYGUWtxMAnXhNXL2z9ar2Dc".into(), default_blacklist_reason.clone()),
            ("59ygup4yDt6s7BEtACkbrUQnutK6883ThPx6ZS7QrFb4".into(), default_blacklist_reason.clone()),
            ("52HPZZNjjCtJC7xTC88zSfQSR9hxMRJdLNfoGgUvnmq5".into(), default_blacklist_reason.clone()),
            ("4ZyWM1Sy4HMioVXuwrjEvHCfcdeSbD1rahRKiZBaL4Jk".into(), default_blacklist_reason.clone()),
            ("4RyNsFHDccFEEnbYJAFt2cNufFduh8Se8eKTqXDVr82h".into(), default_blacklist_reason.clone()),
            ("4qSZsB9QjXr97HzhzPd1zuvB8z7tqqDuM1xbxB5PcPFh".into(), default_blacklist_reason.clone()),
            ("4qS6unxhpNh6fp2rRU3nnyMZEYyZ4hUbjnP7iEN7Jx1w".into(), default_blacklist_reason.clone()),
            ("4cHZGqSeyauFBH4pz9Y65shU7YV2R7HzfzJ6PkSh2y75".into(), default_blacklist_reason.clone()),
            ("43rzZoc9SKPFQwqtoGcc4MrYMVfYsqS6DHJKi11feQ2U".into(), default_blacklist_reason.clone()),
            ("3UG2VvNEWLAWSjy85n5kfj61demeACjL7a7zYxh2GzFJ".into(), default_blacklist_reason.clone()),
            ("3QhgJerJqkAtuwZzEcsnd7cTwZFnvHwTBsb5cyjunpBW".into(), default_blacklist_reason.clone()),
            ("3nHRjY8y9koWkEzKsh6i4tSmiyYFBXMMiMhZDHLbDcam".into(), default_blacklist_reason.clone()),
            ("3MwrAzw1XBajXzdwTSPnseDJGKUUZDUu68D3aGFtH1rG".into(), default_blacklist_reason.clone()),
            ("3Lu3VZ2dnjhjfv3H2tgNfGFxAsctdPRXN3KerdcU1uxr".into(), default_blacklist_reason.clone()),
            ("3ixYJkvabpe2i3cMNokPwpW3gki1r7nqAFMYhg2NrVPa".into(), default_blacklist_reason.clone()),
            ("3iD36QhXqWzx5b4HHhkRAyUcbEgCaC42hi1GcBePNsp2".into(), default_blacklist_reason.clone()),
            ("3hfyxXeuzA26dwWiP1fZid1LGbSFs8vfqXc5bKnKq6RH".into(), default_blacklist_reason.clone()),
            ("3ckBi9tmx1EEnTrSF9xaK9HRusjNnckEsExD3x8xfagJ".into(), default_blacklist_reason.clone()),
            ("3bsJekVS126hDwPiUBLXUa7t9PWfFXW1Xos1cKLGbuVn".into(), default_blacklist_reason.clone()),
            ("3BLMdNjzqPTsFTDFk2jL8RFwjKhQRUhwy1jBotBWuBPk".into(), default_blacklist_reason.clone()),
            ("36usFSB9Xkr47ANKZ6EytFVFsdf9ngnZvNeeXEy7MBh1".into(), default_blacklist_reason.clone()),
            ("2U7MwCTuLUe59aXMM8VXNTDjdajpZxDJD6vkvVJ8iDHf".into(), default_blacklist_reason.clone()),
            ("2qJHXAzWHdnYJr2eosEqhoddephQChSCESdnJCPkd9tA".into(), default_blacklist_reason.clone()),
            ("2GoR6ixTNd41wqmKSgsLBKoewws8AumKwGVSpSye47rY".into(), default_blacklist_reason.clone()),
            ("2Dwg3x37yN4q8SyrrwDaRPGQTp14atcwMPewe3Y8FDoL".into(), default_blacklist_reason.clone()),
            ("27LkBkFi8hwVrQiSdhqo9Rq5hkc6Tn1QYPXvyG7xa6TC".into(), default_blacklist_reason.clone()),
            ("25dZasb4qPYZEgUfSwfnFhhhGVigaS4RUtZ5ASghpKiF".into(), default_blacklist_reason.clone()),
            ("23sEtSYui1VmcvexGLnWFbXDF9cSHYMXtUXe9PuruLXm".into(), default_blacklist_reason.clone()),
            
            ("ytEKd8gXENNbci3pa3jvv1V8JAg8TxbrZ69eqJLEa5m".into(), default_blacklist_reason.clone()),
            ("xDnhMxMagvYC9f3CzSY8Xxk6SLCoqE875S35GL41uve".into(), default_blacklist_reason.clone()),
            ("V43FqSGFVw2FD2E4Q57QG356bYg353BiFJDxuEX3n5Y".into(), default_blacklist_reason.clone()),
            ("stakeM3cvfUVxmhDsareCrqksos9Jv5tN9BJwrFLv3j".into(), default_blacklist_reason.clone()),
            ("J9Go27V87fCdJtjMxmFJu48ctrHzFoe6xQpA6Ecq4Wkw".into(), default_blacklist_reason.clone()),
            ("HzHyM1ZCtLjakThcQifoSYersQy2cZTCz2Ah99beXYaL".into(), default_blacklist_reason.clone()),
            ("HRbhBxjcdJKSJ9PA8cxRqRghRAkDZZiNd8W5o6RZsqqq".into(), default_blacklist_reason.clone()),
            ("Hp1Eieo4bm5u94AG26mybZJB6NsKaB1KYR8DCPriupDA".into(), default_blacklist_reason.clone()),
            ("HFY5f6PF6cRyVAvVG1xV9X15q87qoZ1o6GDcyBzHSEnX".into(), default_blacklist_reason.clone()),
            ("HfDUty4Hg8AU4h8HGATnM1AmXM3FwWzT8hHCbuzteYwx".into(), default_blacklist_reason.clone()),
            ("HCZJjvZbaKaPTE96jz64HnBZTnXHBFv3pugqsBE5Z1D9".into(), default_blacklist_reason.clone()),
            ("HBhcKLdKZoM4MeYruYRmQYRNx6RWbdf4VVcJNhS3g13o".into(), default_blacklist_reason.clone()),
            ("H8gWisRkUanTX9MofS6VoQbf1qCqEZLJvJPKRXveSjN9".into(), default_blacklist_reason.clone()),
            ("H8BoV8s8v9h5ijzSxg27xbZbAJpcnv7RWpn6B5UdcRFX".into(), default_blacklist_reason.clone()),
            ("H3KvD8QMBqqy7TuWB1jdkPEmvNqrX52yDoq7XxMjsrNz".into(), default_blacklist_reason.clone()),
            ("H372sYDhyrLbQj7GM6xR3Qq7gx32Vg8EcxRHTEmkMu5o".into(), default_blacklist_reason.clone()),
            ("GwoUPSb4cyess6AAxaZ823jNyMEE3MJZKpLtGdbWbMfT".into(), default_blacklist_reason.clone()),
            ("GWjZZiWUDwGeFQgdGd2AijnDzJhjXpcA9CVV8EDLPmRQ".into(), default_blacklist_reason.clone()),
            ("GihqHEcYWGkEcix4iHU8bNAd2gCgpK51Dhm8HgJcJWfA".into(), default_blacklist_reason.clone()),
            ("GhKEDkvGkf2kceG45ppzqnPD6BPXi1PyW1xGNWJdh5QW".into(), default_blacklist_reason.clone()),
            ("G914HED3wNwnWp3qBijwWSGRz4Jd2Yx5QTtEFhboRYZp".into(), default_blacklist_reason.clone()),
            ("G8RGMTAQptDLqSW7pvsDjE5bN1u6PWZKAU8idTQM3qJ8".into(), default_blacklist_reason.clone()),
            ("G7hg9wqUirR1exYVkEAuraD1y6Z9oYJEV1QCM8HQ69g5".into(), default_blacklist_reason.clone()),
            ("G4RU9qUt7tG8M8E4L4ZfXtdnwPTcPpaWwLEvSxtdRNHF".into(), default_blacklist_reason.clone()),
            ("FvtcEKiwhSxXafpcqN6m4C5fiWedhn9NZJtS8Am22YUB".into(), default_blacklist_reason.clone()),
            ("Fqb6omRdS2gtnxmxCwQQ86mPJK8MtY7tjR4eQZX9n5J7".into(), default_blacklist_reason.clone()),
            ("Fq4uEo4wb12rns6B4VPcdeKCWf9YMA8Ahgx3kD12AqvJ".into(), default_blacklist_reason.clone()),
            ("FGanR6t8xwDdmkxGp5xoWHvocozfdbtjnFdkXvpy8dbh".into(), default_blacklist_reason.clone()),
            ("FFqpohintDeWe78z1GoZrzZPJ9vE1DKa5oTQXSv4zeQb".into(), default_blacklist_reason.clone()),
            ("FfE7rncxyYJvsqFu3Kn323sJpjBXkfMNXwd4d8kdURk9".into(), default_blacklist_reason.clone()),
            ("FBAHVUMdNozoSDk6MSAUU1nhLMj7mJ8cegFhyDutb4TH".into(), default_blacklist_reason.clone()),
            ("FaxFKPhpVB6xc8FRgBYNhEGXrTw7YqUA9HG5ggdHZ3TB".into(), default_blacklist_reason.clone()),
            ("FaNp2GzQyt2ktDFSQvyom45w6t8APvoQiey5SWcKJ87R".into(), default_blacklist_reason.clone()),
            ("F8WnGTm7wdqGzDWisKjDbZSbznJhQVh9EJDZLHzqQsGV".into(), default_blacklist_reason.clone()),
            ("F6caBWjhxQJVBDxPb2DDATLKuYgkguB41LofAKpXReKK".into(), default_blacklist_reason.clone()),
            ("EZCFgnoVjPViCiZvmpPZCnaBrQ6Hk1EqDbkPW8S4FHjb".into(), default_blacklist_reason.clone()),
            ("EwrqSY9iGTeMSfPKz2LGegdWCdXmfMyb4Ef49AjMiJER".into(), default_blacklist_reason.clone()),
            ("EreNRuWQjJDYSS5AcZCrfyVWQFqYrauYWHbMkJNNPTNJ".into(), default_blacklist_reason.clone()),
            ("EQWMwqeafVaFPDpiqdywtdD5ZjcochF1SxsBMDGALabA".into(), default_blacklist_reason.clone()),
            ("Ep9r9RWkc5mXsrVTQFpfFXSNHd3ynWbXZS3S1AKTgH1V".into(), default_blacklist_reason.clone()),
            ("EKqsABrkcP9s49Tu5WaH8QLyKjCgtco39uyBgSEVHQGj".into(), default_blacklist_reason.clone()),
            ("EbYJNamBbseewMdYNDPrSTWCgnXrccgJt3zLU9thAaaX".into(), default_blacklist_reason.clone()),
            ("EBam6FrvTP4xPSNVNFbwNioGeszDRvYDaqRmxbKJkybD".into(), default_blacklist_reason.clone()),
            ("EAT8y4rvy11Zd9V9FToPmpx5kZX34sjAsMcYwi5tbbtN".into(), default_blacklist_reason.clone()),
            ("E8ajDvUp25ibh2S5YDpLHABKNJA6JSAuymFGn4zaxPSn".into(), default_blacklist_reason.clone()),
            ("E3ho8z7xVRu1MTirWMoPrBL2J2AD7XJzsz5M4mUDqLBN".into(), default_blacklist_reason.clone()),
            ("DMYn88X6PkHAc2y5zDWm5jGZ2Tk2CyBUe8K1U2obF8jc".into(), default_blacklist_reason.clone()),
            ("Dk2nBDiNYgr34iFPqmfoWueepHtyVErPGrHBMtoKRtQu".into(), default_blacklist_reason.clone()),
            ("DhQYMPW34VWfnpCoNZKr9V3ps4yKuGScuF7peZfRP3cX".into(), default_blacklist_reason.clone()),
            ("DDiyGnCTNq9ZPeXXGc9ogTFHACXq1KUEUrmafWiaWiho".into(), default_blacklist_reason.clone()),
            ("DCQfUwptnbqqhD9ZnyxT7jqMmwBaSW8irG1Xq3tzqVM9".into(), default_blacklist_reason.clone()),
            ("DA888Fv41XwE7Montk8YCh76spzTG4kAJw89YfsUfJZ5".into(), default_blacklist_reason.clone()),
            ("D4gpd2comCTGZF2oUgMBFjwCcsFWZCGQs3Bsxy4iNCWK".into(), default_blacklist_reason.clone()),
            ("cyberEqssPKA6PbVXmGQhSqDuqpAM1r3avkqscUjmM2".into(), default_blacklist_reason.clone()),
            ("CwrSfUzU6CVPGTE1M5qJPkKGa5Ncw7htkZV2g8FKtFuK".into(), default_blacklist_reason.clone()),
            ("Cvu9WsGRH7XCiB5KQMzpaGQP3HXxj6pjtbZAjqvFk839".into(), default_blacklist_reason.clone()),
            ("CrZEDyNQfbxakxdFYzMc8dtrYq4XDoRZ51xBa12skDpJ".into(), default_blacklist_reason.clone()),
            ("cqpJdtrz63Z1ayC11aQg1FPas24r1RPqsaygyRSrCAz".into(), default_blacklist_reason.clone()),
            ("CkyA3Xe8SGRtXkZ16hbske6zV9qQ2WNk2agewovcfhsN".into(), default_blacklist_reason.clone()),
            ("CKBuZBQBsHwxSpgVsQC8p7p3fRgCxqGnwpAR7H54Mjom".into(), default_blacklist_reason.clone()),
            ("CdcojeBzFEEqNPTXRkhCHh73b4bFo2zNfqAfdw27pr9p".into(), default_blacklist_reason.clone()),
            ("CbjQjEFBiLF4F2UrGVGEpesYemgXRhFzFTZHwAP5PbHm".into(), default_blacklist_reason.clone()),
            ("C9nCGHyXXT5zreixJ5TvcyT7XE5U2hoAK5eiXmEZfeK5".into(), default_blacklist_reason.clone()),
            ("C33g1CBgcc47XFcrYksA3CEkBKaitKuhs9yD7LLtW98K".into(), default_blacklist_reason.clone()),
            ("C1udGNjscZYuFgngT9uY38kTDudBGuNpPcmkGa5mWizG".into(), default_blacklist_reason.clone()),
            ("BVKXAX6FvhuAMLTETgmJx9PEyZqj6hcoZVbh3dwiCQKt".into(), default_blacklist_reason.clone()),
            ("BTuRg32CvZjxh6LEA1fM1zv4dDQ9UwKWBH9fAn9CTY6a".into(), default_blacklist_reason.clone()),
            ("BPvrBMcWjic7uu2nWragvinycHPvBCo7bCNq4pncGiag".into(), default_blacklist_reason.clone()),
            ("BkGnajAD3wepDsxV9jDZfakZsZGuGiQLJH7AxyWw2FsY".into(), default_blacklist_reason.clone()),
            ("BfhSjaWCfSpxeULqYtgnBVw1DVNyBETUVHKBgwxLmLJp".into(), default_blacklist_reason.clone()),
            ("BeC5sBeHvooP1sNW5t6VJvc3pBBeGM8AeNixNdWm85Z2".into(), default_blacklist_reason.clone()),
            ("BCFXNa8HszvyKRb1xETVzAqUyCeVjYW6Ns1gwVvS2Pq6".into(), default_blacklist_reason.clone()),
            ("B8FoCVcA5jGm2onDN7R2EZnDnN26BKxmE9WHKRsbne67".into(), default_blacklist_reason.clone()),
            ("AxHCYkCtW3um59CGnY5q77WrdN4jXPSoGsuvbzKSKGnA".into(), default_blacklist_reason.clone()),
            ("AUYVsW5ZGwPMAiFJUuAYPtCB9Xp5CVA1osJyAasj8CLe".into(), default_blacklist_reason.clone()),
            ("AsHN1LUzpuyASKkZ78gV5XJxn6Yeq6PpUwy6AGHHKXZ6".into(), default_blacklist_reason.clone()),
            ("AQNEbXjfsvR8wSTag5GRTBeBXUNw3oCndjHV4grdwhqV".into(), default_blacklist_reason.clone()),
            ("AHiRW5DZPThxcqoNtXf5vTe7Z47DHymi7u1mRfm1WYsx".into(), default_blacklist_reason.clone()),
            ("AhcvnNdppGEcgdpK5gfcaZnAWz4ct8V4n7De5QiLiuzG".into(), default_blacklist_reason.clone()),
            ("AgCX6kL7diLpypEAsVhX9rCHNHPbNwnmn5tSK5XmhWy4".into(), default_blacklist_reason.clone()),
            ("Acf5uSCnLwBBeqfAhGWMbu8cFQyPQTZtoNWJLXrX6W5v".into(), default_blacklist_reason.clone()),
            ("A8XYMkTzKNceJT7BKtRwGrg5KGgaXcjyoAYuthrjfKUi".into(), default_blacklist_reason.clone()),
            ("A1n6Hur1GxnAd1CGSfwetjY2H99C3Qa8XdYAaCiAMfJv".into(), default_blacklist_reason.clone()),
            ("9uASGafRPWpvpfXeuwcA3TzMUuP5BoHfQWtcdGMyYR9x".into(), default_blacklist_reason.clone()),
            ("9QypR4m2iLGu3Q5tYoM9LqJ1qJyJcteFx16H8Rsai1FQ".into(), default_blacklist_reason.clone()),
            ("9eysRjQkycbokaxQ1R9i7nZ54iFKuVkrYp1HJozPGUpZ".into(), default_blacklist_reason.clone()),
            ("9DsSqMHnrSXkyHtG8sN4zPhjrsRUgfP9vBQ6hFEpEwM".into(), default_blacklist_reason.clone()),
            ("9DLJhfG4xrbGfryGLx4x8nK3Waic9nzvc5J2ki6E9Zex".into(), default_blacklist_reason.clone()),
            ("9APxFUuvWfxWQj31ZCW2XBzfrjnaaSNEzZvnWHqzpvhH".into(), default_blacklist_reason.clone()),
            ("97J262KBfyyQYAeLjHKwqXp6GdSyjTSqeykMtRRoFCBm".into(), default_blacklist_reason.clone()),
            ("97AgcJPr1KGkwhq7tSD2LDMADreeCpGoFcX6hWjEuQpi".into(), default_blacklist_reason.clone()),
            ("8Ub49zFyCpGDibN5pmn6wcHj6DfzyHYBaMAraacaBbUB".into(), default_blacklist_reason.clone()),
            ("8auuvvDPFBYT67zEJZZji4it3ihtiDdsTn9BFeGYGbAs".into(), default_blacklist_reason.clone()),
            ("8aGU18Nxn99AEWEQNrBYy1ZsJBhiHVFrcqYqHQPNhmEv".into(), default_blacklist_reason.clone()),
            ("85qJ2DWmav9YgKLLdo6mrVAVLLKRH3fDuWPyiViA362n".into(), default_blacklist_reason.clone()),
            ("7xLF6cxqjEzunLVVsTGtUzMhsXQFSgbJjxAByLZNqxYs".into(), default_blacklist_reason.clone()),
            ("7WmXq6EZBGaPqtfPY3hvmE8GQUwC37iRc4saRcyoSFWR".into(), default_blacklist_reason.clone()),
            ("7Vjw81NyL6fvBCT27Xw7BmWkeBZGJaxdX5ajvNw9XtC8".into(), default_blacklist_reason.clone()),
            ("7vJeGJzbaa3bghTsPsFi83T6VCy5uLaQ12r32JA6s8xc".into(), default_blacklist_reason.clone()),
            ("7tY6iXcrTSsiixRy3Vkn15CCRYAtcyQvPUcRpwE7kXHH".into(), default_blacklist_reason.clone()),
            ("7N55twSEwaY6z7s4GmvDQwLZBNjCFS2zioTzR4cb895g".into(), default_blacklist_reason.clone()),
            ("7LU239aHJ4kn96VzvhoHVgVUurRBBSXgV5zCCVz4m3r7".into(), default_blacklist_reason.clone()),
            ("7kWD7LbJ5K2CY4QqKnFRLKyV6ngi3UhybUbTUpR7FJSr".into(), default_blacklist_reason.clone()),
            ("7KNoQc4inYf4gdhrwAX3FM3MzMWhzkbxBziPCQ4r7Po4".into(), default_blacklist_reason.clone()),
            ("7hPeqN1TMeHMdpkNDkrnQE13nru8RfVcScrgEHHAPoS2".into(), default_blacklist_reason.clone()),
            ("7hn1WZh1YbjnJSMyPnWAVt31RmMUcBDVVBXUq4aFgrmy".into(), default_blacklist_reason.clone()),
            ("7GekE3x31ArGMZWxoSWyvguTogCQkrhm9isdjMRGWwL6".into(), default_blacklist_reason.clone()),
            ("7g26PHNjrxpur8dNcfFL95mc3UwN8P1VxLQC1MX5mhvK".into(), default_blacklist_reason.clone()),
            ("7cMFKn5AqUvprbguSik3xBn8V1eJSDoT6fJcwE1CbQvW".into(), default_blacklist_reason.clone()),
            ("7b1humfmpWiZyAAekt5BprG6vqzctGV7qKJBejiGyABi".into(), default_blacklist_reason.clone()),
            ("6uNPXya8zV2HDgxDwnGUryW7SnzeBEHp8rKDmzww3wMV".into(), default_blacklist_reason.clone()),
            ("6KZjN35XHMUgJp1aVV4ZCtB9Pb1MNWAP58mawHTPKPfL".into(), default_blacklist_reason.clone()),
            ("6HwstKwMNRpVJBE8dAugpFUugRe9vj43VSKhuZss6mZP".into(), default_blacklist_reason.clone()),
            ("6hVDgmWqiiYm5SdNQ8buDSgoFhPUZsTWxjDpCH466qNH".into(), default_blacklist_reason.clone()),
            ("6ffMj7zE7u9PKBzQHfv5nsHEGvDXZkntLnFeXRRudEEH".into(), default_blacklist_reason.clone()),
            ("6cbqpwpD5ScxKdp7AVQGuL7bjCrT1UAtJHczQpmgmBEV".into(), default_blacklist_reason.clone()),
            ("6C7nqxQqUzbqBFL89MWxqoK8kSivdsj8P7PHyNwuFKvZ".into(), default_blacklist_reason.clone()),
            ("69wYepwPQQLdVG9zT4fMtVrGzfvWb1gCdLv871jA6uZT".into(), default_blacklist_reason.clone()),
            ("69kotiHA3W2a18uskRRVQDmeDs8ypGqQVeVQzZxW9gav".into(), default_blacklist_reason.clone()),
            ("68PdgNn1RC2Ltz73ZY9EDzKphEwQWEDErKXpEPfqEtiS".into(), default_blacklist_reason.clone()),
            ("67WppeGg2JBxQWaFkDRQamNmRmcboXsdgKJYVxGiDn6V".into(), default_blacklist_reason.clone()),
            ("61b5sg8egkGJ4ZGKxtF4Z44qTFD4nRGoHnpA8qp7kdsM".into(), default_blacklist_reason.clone()),
            ("5xNmtkWam9CkaEy53wAbFa6Kxrx3SmrZeKhVqqQNniAW".into(), default_blacklist_reason.clone()),
            ("5x9owHqw5yuJxXgmAYcVDdmAuK9dpTvi4ahQajhmfcjj".into(), default_blacklist_reason.clone()),
            ("5pqis66S1mSpDGmg7T5wLFqPnb524T5gzgxu2iqF1kWN".into(), default_blacklist_reason.clone()),
            ("5KF6gMG6f5GCr4V6BXKzdroHxeXK68oKrLQdiujGsj9m".into(), default_blacklist_reason.clone()),
            ("5JR2Cy76A1wv4HpvmJbELkMucYpx5k1L8c26guxCNJQF".into(), default_blacklist_reason.clone()),
            ("5iKHfbGCttCBC9mS8v22FDTc1JVJuUX8uv5dMN812bmf".into(), default_blacklist_reason.clone()),
            ("5iJGmjy3msiPKvAfxZSULmQ1AtYYC1i1Pa3mDjzgGweT".into(), default_blacklist_reason.clone()),
            ("5hVPfoTZcfZTcyondKxjuVczaFap9pBGYBSPKXg9Jrg5".into(), default_blacklist_reason.clone()),
            ("5h4UTTYdYnAVP1LbQNWWHXeBN37tdUopoczCnsthZToE".into(), default_blacklist_reason.clone()),
            ("5GKFk6ptwtYTUVXZwofK3tgCJXRiQBfY6yS9w8dgZaSS".into(), default_blacklist_reason.clone()),
            ("5cy9aBTvFZXMXmn1gqaN6VP5pQR4VPnTG4nbLDBAhFzV".into(), default_blacklist_reason.clone()),
            ("59Mdh3SnLXwgs1Sc51HTis5oRbuYb6nyN9epd5bYFg1a".into(), default_blacklist_reason.clone()),
            ("56aneAqRs7zWRckhf5NUy8wfwXUjkVY8Zs3LM38zJ8TY".into(), default_blacklist_reason.clone()),
            ("55UVRdjjX12UNnY4fkTNTXFFd5Y1TsxktJA4JdxUggJz".into(), default_blacklist_reason.clone()),
            ("4YWwshguoARneC9HuSWVHcLAkh9J1NFoqMZ3VhtJSZuA".into(), default_blacklist_reason.clone()),
            ("4xKms4LsZtdLGusKLdr2haHLGKJ78SLiKcwU8N5GuS46".into(), default_blacklist_reason.clone()),
            ("4vcLWDyAfgNH5MMzbKZjCtyeiTZcSvFJFpwWPRdNM6UG".into(), default_blacklist_reason.clone()),
            ("4rxFGSzXiTXuF9GveXbMr4fJAPPnQVjHmpEZbWV8jz9m".into(), default_blacklist_reason.clone()),
            ("4RRvY19jRFToz66zTpq28f26i4P49irnYLydspY5Zun3".into(), default_blacklist_reason.clone()),
            ("4RnNdAQ4NBkNZDPvZ7ETX5HDPPYXfZLKHPDbNyn1WJHt".into(), default_blacklist_reason.clone()),
            ("4R2eqfCDqN3UesKPW4kSTZVd55955V4awbof4vBuWibY".into(), default_blacklist_reason.clone()),
            ("4PogyEBKix5WGuTTG27DtD7CC3yxKsvKEqfPoTp8w4Pj".into(), default_blacklist_reason.clone()),
            ("4kRxn6MsvYKp8imE6JPXRAjC8NUNXHUGWyfuo58UC3ua".into(), default_blacklist_reason.clone()),
            ("4GQovsSRvvFzko4iR3Dpdqhu7BbaU66hCgy6Jm9EWRtw".into(), default_blacklist_reason.clone()),
            ("4GEaWcSfgCyKQqUJGYbDi3bqzU3zL3iGuhvNhspHouVV".into(), default_blacklist_reason.clone()),
            ("4ebYDbb3D9mjgAYc813oJ8aQtSE3u6QRu3rBHxiJ72Pk".into(), default_blacklist_reason.clone()),
            ("4amh4YANQfyNww8xXJYxsM7CEkFjQFJt1wpuScqy675Y".into(), default_blacklist_reason.clone()),
            ("4aibvoup84a5H2HTKjjnLGh6SQm5JqegvTG9kWjs36Xs".into(), default_blacklist_reason.clone()),
            ("3yusuSWJXXzN8EiadtpcFZRm2FUvgaieN5d7rYifnamx".into(), default_blacklist_reason.clone()),
            ("3YKcH4c8eoAKkghQeGavg9HZ13fSe77RWM3QoFTCV2Gv".into(), default_blacklist_reason.clone()),
            ("3v6FfdWMT2bcoQQ9hN4F2syu7qhRHzNuCPPQqV12hsw2".into(), default_blacklist_reason.clone()),
            ("3sRH9ZiAkKN8GYSc6XQjYiaCc9nDAUBDQ1mPLhY8YtCk".into(), default_blacklist_reason.clone()),
            ("3Q5XWkkEvmWFYPRyneCuhvJvhaRawhHQc1oK6XZUFc6b".into(), default_blacklist_reason.clone()),
            ("3HSgNsx9rQsAFZrL7k2BAUuL8HpCjhgfxXjrPBK9cnjD".into(), default_blacklist_reason.clone()),
            ("2WSAYcYURcV7SfHxThsoaRh7NpZnRQRDruy6a3vm6qQs".into(), default_blacklist_reason.clone()),
            ("2LR74ZF3r6Kj34UbGNfvYvD8e7o92gqKfGfc8n2SzsKB".into(), default_blacklist_reason.clone()),
            ("2kQZfvm5tqcBhXnscT3xe5SbCDttkipxgy1wCqhzqL2a".into(), default_blacklist_reason.clone()),
            ("2Gj8Md6yPiaffXRHaGd5wdTqPFAgUCz2ZnBRSvKYbxNM".into(), default_blacklist_reason.clone()),
            ("22MB6hXJrCgubFstzM6eDYDK5mq7w6cYtSux9G8Sogk4".into(), default_blacklist_reason.clone()),
            
            // vote lagging
            ("3MiQSVriZTC1yNgwSsvXwGbk2UDLMZnZm9PGoDSsYZBf".into(), default_blacklist_reason.clone()),
            
            // compromised keys
            ("candyKFNNEGxMteGwNmR4YXim77gucQBP5JzGPdHqsK".into(), default_blacklist_reason.clone()),
        ]);

        for v in validator_scores.iter_mut() {
            if let Some(reason) = blacklisted.get(&v.vote_address) {
                info!("Blacklisted validator found: {}", v.vote_address);
                v.remove_level = 2;
                v.remove_level_reason = reason.clone();
                v.marinade_score = 0;
            }
        }
    }

    fn load_marinade_staked(
        &self,
        marinade: &RpcMarinade,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> anyhow::Result<()> {
        let (stakes, _max_stakes) = marinade.stakes_info()?;
        let (current_validators, max_validators) = marinade.validator_list()?;
        let total_marinade_score: u64 = validator_scores
            .iter()
            .map(|s| s.marinade_score as u64)
            .sum();
        info!(
            "Marinade on chain register: {} Validators of {} max capacity, total_marinade_score {}",
            current_validators.len(),
            max_validators,
            total_marinade_score
        );

        let validator_indices = self.index_validator_records(&current_validators);
        for v in validator_scores.iter_mut() {
            let vote = Pubkey::from_str(&v.vote_address)?;
            if let Some(_index) = validator_indices.get(&vote) {
                // get stakes
                let validator_stakes: Vec<&StakeInfo> = stakes
                    .iter()
                    .filter(|stake| {
                        if let Some(delegation) = stake.stake.delegation() {
                            // Only active stakes
                            delegation.deactivation_epoch == u64::MAX
                                && delegation.voter_pubkey == vote
                        } else {
                            false
                        }
                    })
                    .collect();
                let sum_stake: u64 = validator_stakes
                    .iter()
                    .map(|s| s.record.last_update_delegated_lamports)
                    .sum();

                // update on site, adjusted_score & sum_stake
                v.marinade_staked = lamports_to_sol(sum_stake);
            }
        }

        Ok(())
    }

    fn update_should_have(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
        stake_target_without_collateral: u64,
    ) -> () {
        let total_marinade_score: u64 = validator_scores
            .iter()
            .map(|s| s.marinade_score as u64)
            .sum();

        for v in validator_scores.iter_mut() {
            v.should_have = lamports_to_sol(
                (v.marinade_score as f64 * stake_target_without_collateral as f64
                    / total_marinade_score as f64) as u64,
            );
        }
    }

    fn adjust_marinade_score_for_overstaked(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
    ) -> () {
        // adjust score
        // we use v.should_have as score
        for v in validator_scores.iter_mut() {
            // if we need to unstake, set a score that's x% of what's staked
            // so we ameliorate how aggressive the stake bot is for the 0-marinade-staked
            // unless this validator is marked for unstake
            v.marinade_score = if v.should_have < v.marinade_staked {
                // unstake
                if v.remove_level > 1 {
                    0
                } else if v.remove_level == 1 {
                    (v.should_have * 0.5) as u32
                } else {
                    (v.should_have) as u32
                }
            } else {
                (v.should_have) as u32 // stake
            };
        }
    }

    fn recompute_score_with_capping(
        &self,
        validator_scores: &mut Vec<ValidatorScore>,
        stake_target_without_collateral: u64,
    ) -> anyhow::Result<()> {
        let total_score = validator_scores.iter().map(|s| s.score as u64).sum();

        if total_score == 0 {
            return Ok(());
        }

        let mut total_score_of_worse_or_same = total_score;
        let mut score_overflow_rem = 0u64;
        let mut total_score_redistributed = 0u64;
        // sort validator_scores by score desc
        validator_scores.sort_by(|a, b| b.score.cmp(&a.score));

        let score_cap = proportional(
            total_score,
            (self.pct_cap * 1_000_000.0) as u64,
            100 * 1_000_000,
        )?;
        // recompute should_have, rank and pct
        let mut rank: u32 = 1;
        for v in validator_scores.iter_mut() {
            let score_original: u64 = v.score.into();
            let fraction_of_worse_or_same = if total_score_of_worse_or_same == 0 {
                0f64
            } else {
                score_original as f64 / total_score_of_worse_or_same as f64
            };

            // calculate how much larger it is than the allowed maximum pct
            let score_overflow = if score_original > score_cap {
                score_original - score_cap
            } else {
                0
            };
            total_score_redistributed += score_overflow;
            score_overflow_rem += score_overflow;
            let score_to_receive = (fraction_of_worse_or_same * (score_overflow_rem as f64)) as u64;
            let score_new = (score_original + score_to_receive).min(score_cap);
            score_overflow_rem -= if score_new > score_original {
                score_new - score_original
            } else {
                0
            };

            v.score = score_new as u32;
            v.should_have = lamports_to_sol(proportional(
                v.score as u64,
                stake_target_without_collateral,
                total_score,
            )?);
            v.rank = rank;
            rank += 1;
            // compute pct with 6 decimals precision
            v.pct = (v.score as u64 * 100_000_000 / total_score) as f64 / 1_000_000.0;
            total_score_of_worse_or_same -= score_original;
        }

        info!(
            "Total score redistributed by capping at {}%: {}",
            self.pct_cap, total_score_redistributed
        );
        Ok(())
    }
}
