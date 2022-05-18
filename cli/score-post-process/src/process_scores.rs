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
use std::{collections::HashMap, str::FromStr};

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

    #[structopt(
        long = "gague-meister",
        help = "Gauge meister of the vote gauges.",
        // default_value = "9A7RaeYeHoFJ7QmTRGMP8a9eCaU6oC7zXPxNGTZ5cYMX"
    )]
    gauge_meister: Option<Pubkey>,

    #[structopt(
        long = "escrow-relocker",
        help = "Escrow relocker program address.",
        // default_value = "tovt1VkTE2T4caWoeFP6a2xSFoew5mNpd7FWidyyMuk"
    )]
    escrow_relocker_address: Option<Pubkey>,

    #[structopt(
        long = "vote-gauges-stake-pct",
        help = "How much of total stake is affected by votes.",
        default_value = "10" // %
    )]
    pub vote_gauges_stake_pct: u32,

    #[structopt(
        long = "stake-top-n-validators",
        help = "How many validators are guaranteed to keep their scores.",
        default_value = "430"
    )]
    stake_top_n_validators: usize,
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
                    "The validator is in the concentrated group (under the Nakamoto coefficient)."
                ),
            );
        } else if self.commission > HEALTHY_VALIDATOR_MAX_COMMISSION {
            return (
                3,
                format!(
                    "The commission {}% is above {}%",
                    self.commission, HEALTHY_VALIDATOR_MAX_COMMISSION
                ),
            );
            // Note: self.delinquent COMMENTED, a good validator could be delinquent for several minutes during an upgrade
            // it's better to consider this_epoch_credits as filter and not the on/off flag of self.delinquent
            // } else if self.delinquent {
            //     return (2, format!("DELINQUENT")); // keep delinquent validators in the list so people can escape by depositing stake accounts from them into Marinade
        } else if self.credits_observed == 0 {
            return (2, format!("Zero credits observed.")); // keep them in the list so people can escape by depositing stake accounts from them into Marinade
        } else if semver::Version::parse(&self.version)
            .as_ref()
            .unwrap_or(&version_zero)
            < min_release_version.unwrap_or(&version_zero)
        {
            return (2, format!("The node version {} is too old.", self.version));
        } else if self.this_epoch_credits < avg_this_epoch_credits * 8 / 10 {
            return (
                2,
                format!(
                    "The validator has very low production ({}% of credits average).",
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

        // Find unhealthy validators and set their scores to 0 or 50 %
        self.decrease_scores_for_unhealthy(&mut validator_scores, avg_this_epoch_credits);

        // Some validators do not play fair, let's set their scores to 0
        self.apply_blacklist(&mut validator_scores);

        // imagine a +100K stake delta
        let total_stake_target = marinade
            .state
            .validator_system
            .total_active_balance
            .saturating_add(sol_to_lamports(100000.0));

        // Compute marinade_staked from the current on-chain validator data
        self.load_marinade_staked(&marinade, &mut validator_scores)?;

        // Set scores of validators out of top N to zero unless we have a stake with them
        // This makes sure that we do not constantly stake/unstake people near the end of the list.
        self.adjust_scores_of_validators_below_line(&mut validator_scores);

        self.update_should_have(&mut validator_scores, total_stake_target);

        self.adjust_marinade_score_for_overstaked(&mut validator_scores);

        // Loads votes from gauges
        self.load_votes(&marinade, &mut validator_scores)?;

        // Zero votes for misbehaving validators
        self.calc_effective_votes(&mut validator_scores);

        // We remove x % from everybody, we distribute x % based on scores, we sum marinade_score and vote_score
        self.distribute_vote_score(&mut validator_scores);

        // Apply cap
        self.recompute_score_with_capping(&mut validator_scores, total_stake_target)?;

        // Final assertions
        self.check_final_scores(&validator_scores);

        // Sort validator_scores by score desc
        validator_scores.sort_by(|a, b| b.score.cmp(&a.score));

        self.write_results_to_file(validator_scores)?;
        Ok(())
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
            v.vote_score = (v.votes_effective * vote_score_target_sum / effective_votes_sum) as u32;
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
                            v.this_epoch_credits = credits;
                            sum_this_epoch_credits += credits;
                            count_credit_data_points += 1;
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
        let blacklisted: Vec<String> = vec![
            // manually slashed-paused
            // https://discord.com/channels/823564092379627520/856529851274887168/914462176205500446
            // Marinade is about to stake a validator that is intentionally delaying their votes to always vote in the correct fork. They changed the code so they don't waste any vote with consensus...
            // it seems like they are intentionally lagging their votes by about 50 slots or only voting on the fork with consensus, so that they don't vote on the wrong fork and so land every one of their votes... therefore their votes in effect don't contribute to the consensus of the network...
            // Response: slashing-pausing
            // 1) #14 Validator rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr, score-pct:0.6037%
            // ValidatorScoreRecord { rank: 14, pct: 0.827161338644014, epoch: 252, keybase_id: "replicantstaking", name: "Replicant Staking", vote_address: "rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr", score: 3211936, average_position: 57.8258431048359, commission: 0, epoch_credits: 364279, data_center_concentration: 0.03242, base_score: 363924.0, mult: 8.82584310483592, avg_score: 3211936.0, avg_active_stake: 6706.7905232706 }
            // avg-staked 6706.79, marinade-staked 50.13 (0.75%), should_have 39238.66, to balance +stake 39188.54 (accum +stake to this point 39188.54)
            "rep1xGEJzUiQCQgnYjNn76mFRpiPaZaKRwc13wm8mNr".into(),
            // manually slashed-paused
            // Same entity 4block-team with 2 validators
            // https://discord.com/channels/823564092379627520/856529851274887168/916268033352302633
            // 4block-team case at 2021-12-3
            // current marinade stake: (4block-team validator#1)
            // 3) Validator 6anBvYWGwkkZPAaPF6BmzF6LUPfP2HFVhQUAWckKH9LZ, marinade-staked 55816.30 SOL, score-pct:0.7280%, 1 stake-accounts
            // next potential marinade stake: (4block-team validator#2)
            // 0) #6 0.72% m.stk:0 should:49761 next:+49761 credits:373961 cm:0 dcc:0.29698 4BLOCK.TEAM 2 - Now 0% Fees → 1% from Q1/2023 GfZybqTfVXiiF7yjwnqfwWKm2iwP96sSbHsGdSpwGucH
            "GfZybqTfVXiiF7yjwnqfwWKm2iwP96sSbHsGdSpwGucH".into(),
            // Scrooge_McDuck
            // changing commission from 0% to 100% on epoch boundaries
            // https://www.validators.app/commission-changes?locale=en&network=mainnet
            "AxP8nEVvay26BvFqSVWFC73ciQ4wVtmhNjAkUz5szjCg".into(),
            // Node Brothers
            // changing commission from 0% to 10% on epoch boundaries
            // https://www.validators.app/commission-changes/6895?locale=en&network=mainnet
            "DeFiDeAgFR29GgKdyyVZdvsELbDR8k4WqprWGtgtbi1o".into(),
            // VymD
            // Vote lagging
            "8Pep3GmYiijRALqrMKpez92cxvF4YPTzoZg83uXh14pW".into(),
            // Parrot
            // Down for ~2 weeks
            "GBU4potq4TjsmXCUSJXbXwnkYZP8725ZEaeDrLrdQhbA".into(),
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
        ];

        for v in validator_scores.iter_mut() {
            if blacklisted.contains(&v.vote_address) {
                info!("Blacklisted validator found: {}", v.vote_address);
                v.remove_level = 2;
                v.remove_level_reason = format!("The validator is blacklisted for bad behavior.");
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
        total_stake_target: u64,
    ) -> () {
        let total_marinade_score: u64 = validator_scores
            .iter()
            .map(|s| s.marinade_score as u64)
            .sum();

        for v in validator_scores.iter_mut() {
            v.should_have = lamports_to_sol(
                (v.marinade_score as f64 * total_stake_target as f64 / total_marinade_score as f64)
                    as u64,
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
        total_stake_target: u64,
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
                total_stake_target,
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
