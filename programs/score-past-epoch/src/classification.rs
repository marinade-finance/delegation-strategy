use {
    crate::{
        // generic_stake_pool::ValidatorStakeState,
        config::*,
        data_center_info::{self, *},
        rpc_client_utils::*,
    },
    log::*,
    serde::{Deserialize, Serialize},
    solana_client::rpc_client::RpcClient,
    solana_sdk::{
        account::from_account,
        account_utils::StateMut,
        clock::{Epoch, Slot},
        commitment_config::CommitmentConfig,
        native_token::*,
        pubkey::Pubkey,
        slot_history::{self, SlotHistory},
        stake::{self, state::StakeState},
        stake_history::StakeHistory,
        sysvar,
    },
    solana_vote_program::vote_state::VoteState,
    std::{
        collections::HashMap,
        collections::HashSet,
        error,
        fs::{self, File},
        io::{self, Write},
        path::{Path, PathBuf},
        str::FromStr,
    },
};

type BoxResult<T> = Result<T, Box<dyn error::Error>>;
type ValidatorList = HashSet<Pubkey>;
type IdentityToParticipant = HashMap<Pubkey, Pubkey>;

#[derive(Debug, PartialEq, Clone, Copy, Deserialize, Serialize)]
pub enum ValidatorStakeState {
    None,     // Validator should receive no stake
    Baseline, // Validator has earned the baseline stake level
    Bonus,    // Validator has earned the bonus stake level
}

impl Default for ValidatorStakeState {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Default, Clone, Deserialize, Serialize)]
pub struct ScoreDiscounts {
    pub can_halt_the_network_group: bool,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct ByIdentityInfo {
    pub data_center_id: DataCenterId,
    pub keybase_id: String,
    pub name: String,
    pub www_url: String,
}

#[derive(Default, Clone, Deserialize, Serialize)]
/// computed score (more granular than ValidatorStakeState)
pub struct ScoreData {
    /// epoch_credits is the base score
    pub epoch_credits: u64,
    /// 50 => Average, 0=>worst, 100=twice the average
    pub average_position: f64,
    pub score_discounts: ScoreDiscounts,
    pub commission: u8,
    pub active_stake: u64,
    pub data_center_concentration: f64,
    pub validators_app_info: ByIdentityInfo,
}

#[derive(Default, Clone, Deserialize, Serialize)]
pub struct ValidatorClassification {
    pub identity: Pubkey, // Validator identity
    pub vote_address: Pubkey,

    pub stake_state: ValidatorStakeState,
    pub stake_state_reason: String,

    // added optional validator scoring data
    pub score_data: Option<ScoreData>,

    // Summary of the action was taken this epoch to advance the validator's stake
    pub stake_action: Option<String>,

    // The data center that the validator was observed at for this classification
    pub current_data_center: Option<DataCenterId>,

    // The identity of the staking program participant, used to establish a link between
    // testnet and mainnet validator classifications
    pub participant: Option<Pubkey>,

    // The validator was not funded this epoch and should be prioritized next epoch
    pub prioritize_funding_in_next_epoch: Option<bool>,
}

impl ScoreData {
    pub fn score(&self, config: &Config) -> u64 {
        if self.score_discounts.can_halt_the_network_group
            || self.active_stake < config.score_min_stake
            || self.average_position < config.min_avg_position
            // if config.min_avg_position=100 => everybody passes
            // if config.min_avg_position=50 => only validators above avg pass
            || self.commission > config.score_max_commission
        {
            0
        } else {
            // if data_center_concentration = 25%, lose all score,
            // data_center_concentration = 10%, lose 40% (rounded)
            let discount_because_data_center_concentration = (self.data_center_concentration
                * config.score_concentration_point_discount as f64)
                as u64;

            // score discounts according to commission
            // apply commission % as a discount to credits_observed.
            // The rationale es:
            // If you're the top performer validator and get 300K credits, but you have 50% commission,
            // from our user's point of view, it's the same as a 150K credits validator with 0% commission,
            // both represent the same APY for the user.
            // So to treat both the same we apply commission to self.epoch_credits
            let discount_because_commission = self.commission as u64 * self.epoch_credits / 100;

            // give extra score to above average validators in order to increase APY for our users
            let points_added_above_average: u64 = if self.average_position > 50.0 {
                let above = self.average_position - 50.0;
                let multiplier = if above * above > 25.0 {
                    25.0
                } else {
                    above * above
                };
                (multiplier * self.epoch_credits as f64) as u64
            } else {
                0
            };

            //result
            self.epoch_credits
                .saturating_sub(discount_because_commission)
                .saturating_sub(discount_because_data_center_concentration)
                .saturating_add(points_added_above_average)
        }
    }
}

pub type ValidatorClassificationByIdentity =
    HashMap<solana_sdk::pubkey::Pubkey, ValidatorClassification>;

#[derive(Default, Deserialize, Serialize, Clone)]
pub struct EpochClassificationV1 {
    // Data Center observations for this epoch
    pub data_center_info: Vec<DataCenterInfo>,

    // `None` indicates a pause due to unusual observations during classification
    pub validator_classifications: Option<ValidatorClassificationByIdentity>,

    // Informational notes regarding this epoch
    pub notes: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub enum EpochClassification {
    V1(EpochClassificationV1),
}

impl Default for EpochClassification {
    fn default() -> Self {
        Self::V1(EpochClassificationV1::default())
    }
}

impl EpochClassification {
    pub fn new(v1: EpochClassificationV1) -> Self {
        EpochClassification::V1(v1)
    }

    pub fn into_current(self) -> EpochClassificationV1 {
        match self {
            EpochClassification::V1(v1) => v1,
        }
    }

    fn file_name<P>(epoch: Epoch, path: P) -> PathBuf
    where
        P: AsRef<Path>,
    {
        path.as_ref().join(format!("epoch-{}.yml", epoch))
    }

    pub fn exists<P>(epoch: Epoch, path: P) -> bool
    where
        P: AsRef<Path>,
    {
        Self::file_name(epoch, path).exists()
    }

    pub fn load<P>(epoch: Epoch, path: P) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let file = File::open(Self::file_name(epoch, path))?;
        serde_yaml::from_reader(file)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{:?}", err)))
    }

    pub fn save<P>(&self, epoch: Epoch, path: P) -> Result<(), io::Error>
    where
        P: AsRef<Path>,
    {
        let serialized = serde_yaml::to_string(self)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{:?}", err)))?;

        fs::create_dir_all(&path)?;
        let mut file = File::create(Self::file_name(epoch, path))?;
        file.write_all(&serialized.into_bytes())?;

        Ok(())
    }
}

fn get_self_stake_by_vote_account(
    rpc_client: &RpcClient,
    epoch: Epoch,
    vote_account_info: &[VoteAccountInfo],
) -> BoxResult<HashMap<Pubkey, u64>> {
    let mut self_stake_by_vote_account = HashMap::new();

    info!("Building list of authorized voters...");

    let mut authorized_withdrawer = HashMap::new();
    for VoteAccountInfo { vote_address, .. } in vote_account_info {
        let vote_account = rpc_client.get_account(vote_address)?;

        if let Some(vote_state) = VoteState::from(&vote_account) {
            authorized_withdrawer.insert(vote_address, vote_state.authorized_withdrawer);
        }
    }

    info!("Fetching stake accounts...");
    let all_stake_accounts = rpc_client.get_program_accounts(&stake::program::id())?;
    info!("{} stake accounts", all_stake_accounts.len());

    let stake_history_account = rpc_client
        .get_account_with_commitment(&sysvar::stake_history::id(), CommitmentConfig::finalized())?
        .value
        .unwrap();

    let stake_history: StakeHistory =
        from_account(&stake_history_account).ok_or("Failed to deserialize stake history")?;

    for (_stake_pubkey, stake_account) in all_stake_accounts {
        if let Ok(StakeState::Stake(meta, stake)) = stake_account.state() {
            let vote_address = &stake.delegation.voter_pubkey;
            if let Some(vote_account_authorized_withdrawer) =
                authorized_withdrawer.get(vote_address)
            {
                if *vote_account_authorized_withdrawer == meta.authorized.withdrawer {
                    let effective_stake = stake
                        .delegation
                        .stake_activating_and_deactivating(epoch, Some(&stake_history), true)
                        .0;
                    if effective_stake > 0 {
                        *self_stake_by_vote_account.entry(*vote_address).or_default() +=
                            effective_stake;
                    }
                }
            }
        }
    }

    Ok(self_stake_by_vote_account)
}

fn get_confirmed_blocks(
    rpc_client: &RpcClient,
    start_slot: Slot,
    end_slot: Slot,
) -> BoxResult<HashSet<Slot>> {
    info!(
        "loading slot history. slot range is [{},{}]",
        start_slot, end_slot
    );
    let slot_history_account = rpc_client
        .get_account_with_commitment(&sysvar::slot_history::id(), CommitmentConfig::finalized())?
        .value
        .unwrap();

    let slot_history: SlotHistory =
        from_account(&slot_history_account).ok_or("Failed to deserialize slot history")?;

    if start_slot >= slot_history.oldest() && end_slot <= slot_history.newest() {
        info!("slot range within the SlotHistory sysvar");
        Ok((start_slot..=end_slot)
            .filter(|slot| slot_history.check(*slot) == slot_history::Check::Found)
            .collect())
    } else {
        Err("slot range is not within the SlotHistory sysvar".into())
    }
}

type ClassifyResult = (
    // quality
    ValidatorList,
    // poor
    ValidatorList,
    // classification reason
    HashMap<Pubkey, String>,
    // cluster_skip_rate
    usize,
    // too_many_poor_block_producers
    bool,
);

fn classify_producers(
    first_slot_in_epoch: Slot,
    confirmed_blocks: HashSet<u64>,
    leader_schedule: HashMap<String, Vec<usize>>,
    config: &Config,
) -> BoxResult<ClassifyResult> {
    let mut poor_block_producers = HashSet::new();
    let mut quality_block_producers = HashSet::new();
    let mut blocks_and_slots = HashMap::new();
    let mut reason_msg = HashMap::new();

    let mut total_blocks = 0;
    let mut total_slots = 0;
    for (validator_identity, relative_slots) in leader_schedule {
        let mut validator_blocks = 0;
        let mut validator_slots = 0;
        for relative_slot in relative_slots {
            let slot = first_slot_in_epoch + relative_slot as Slot;
            total_slots += 1;
            validator_slots += 1;
            if confirmed_blocks.contains(&slot) {
                total_blocks += 1;
                validator_blocks += 1;
            }
        }
        if validator_slots > 0 {
            let validator_identity = Pubkey::from_str(&validator_identity)?;
            let e = blocks_and_slots.entry(validator_identity).or_insert((0, 0));
            e.0 += validator_blocks;
            e.1 += validator_slots;
        }
    }
    let cluster_average_skip_rate = 100 - total_blocks * 100 / total_slots;
    for (validator_identity, (blocks, slots)) in blocks_and_slots {
        let skip_rate: usize = 100 - (blocks * 100 / slots);

        let msg = format!(
            "{} blocks in {} slots, {:.2}% skip rate",
            blocks, slots, skip_rate
        );
        trace!("Validator {} produced {}", validator_identity, msg);
        reason_msg.insert(validator_identity, msg);

        if skip_rate.saturating_sub(config.quality_block_producer_percentage)
            > cluster_average_skip_rate
        {
            poor_block_producers.insert(validator_identity);
        } else {
            quality_block_producers.insert(validator_identity);
        }
    }

    let poor_block_producer_percentage = poor_block_producers.len() * 100
        / (quality_block_producers.len() + poor_block_producers.len());
    let too_many_poor_block_producers =
        poor_block_producer_percentage > config.max_poor_block_producer_percentage;

    info!("cluster_average_skip_rate: {}", cluster_average_skip_rate);
    info!("quality_block_producers: {}", quality_block_producers.len());
    trace!("quality_block_producers: {:?}", quality_block_producers);
    info!("poor_block_producers: {}", poor_block_producers.len());
    trace!("poor_block_producers: {:?}", poor_block_producers);
    info!(
        "poor_block_producer_percentage: {}% (too many poor producers={})",
        poor_block_producer_percentage, too_many_poor_block_producers,
    );

    Ok((
        quality_block_producers,
        poor_block_producers,
        reason_msg,
        cluster_average_skip_rate,
        too_many_poor_block_producers,
    ))
}

/// Split validators into quality/poor lists based on their block production over the given `epoch`
fn classify_block_producers(
    rpc_client: &RpcClient,
    config: &Config,
    epoch: Epoch,
) -> BoxResult<ClassifyResult> {
    let epoch_schedule = rpc_client.get_epoch_schedule()?;
    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);
    let last_slot_in_epoch = epoch_schedule.get_last_slot_in_epoch(epoch);

    let confirmed_blocks =
        get_confirmed_blocks(rpc_client, first_slot_in_epoch, last_slot_in_epoch)?;

    let leader_schedule = rpc_client
        .get_leader_schedule_with_commitment(
            Some(first_slot_in_epoch),
            CommitmentConfig::finalized(),
        )?
        .unwrap();

    classify_producers(
        first_slot_in_epoch,
        confirmed_blocks,
        leader_schedule,
        config,
    )
}

fn classify_poor_voters(
    config: &Config,
    vote_account_info: &[VoteAccountInfo],
) -> (ValidatorList, usize, u64, u64, bool) {
    let avg_epoch_credits = vote_account_info
        .iter()
        .map(|vai| vai.epoch_credits)
        .sum::<u64>()
        / vote_account_info.len() as u64;

    let min_epoch_credits =
        avg_epoch_credits * (100 - config.min_epoch_credit_percentage_of_average as u64) / 100;

    let poor_voters = vote_account_info
        .iter()
        .filter_map(|vai| {
            if vai.epoch_credits < min_epoch_credits {
                Some(vai.identity)
            } else {
                None
            }
        })
        .collect::<HashSet<_>>();

    let max_poor_voters = vote_account_info.len() * config.max_poor_voter_percentage / 100;
    let poor_voter_percentage = poor_voters.len() * 100 / vote_account_info.len();
    let too_many_poor_voters = poor_voters.len() > max_poor_voters;

    info!("Cluster average epoch credits: {}", avg_epoch_credits);
    info!("Minimum required epoch credits: {}", min_epoch_credits);
    info!("Poor voter: {}%", poor_voter_percentage);
    debug!(
        "poor_voters: {}, max poor_voters: {}",
        poor_voters.len(),
        max_poor_voters
    );
    trace!("poor_voters: {:?}", poor_voters);

    (
        poor_voters,
        poor_voter_percentage,
        min_epoch_credits,
        avg_epoch_credits,
        too_many_poor_voters,
    )
}

pub fn classify(
    rpc_client: &RpcClient,
    config: &Config,
    epoch: Epoch,
    validator_list: &ValidatorList,
    identity_to_participant: &IdentityToParticipant,
) -> BoxResult<EpochClassificationV1> {
    let last_epoch = epoch - 1;

    let data_centers = match data_center_info::get(&config.cluster.to_string()) {
        Ok(data_centers) => {
            // Sanity check the infrastructure stake percent data.  More than 35% indicates there's
            // probably a bug in the data source. Abort if so.
            let max_infrastucture_stake_percent = data_centers
                .info
                .iter()
                .map(|dci| dci.stake_percent.round() as usize)
                .max()
                .unwrap_or(100);

            info!(
                "Largest data center stake concentration: ~{}%",
                max_infrastucture_stake_percent
            );
            if max_infrastucture_stake_percent > 35 {
                return Err("Largest data center stake concentration is too high".into());
            }
            data_centers
        }
        Err(err) => {
            if config.max_infrastructure_concentration.is_some() {
                return Err(err);
            }
            warn!("infrastructure concentration skipped: {}", err);
            crate::data_center_info::DataCenters::default()
        }
    };

    let infrastructure_concentration_too_high = data_centers
        .info
        .iter()
        .filter_map(|dci| {
            if let Some(max_infrastructure_concentration) = config.max_infrastructure_concentration
            {
                if dci.stake_percent > max_infrastructure_concentration {
                    return Some((dci.validators.clone(), dci.stake_percent));
                }
            }
            None
        })
        .flat_map(|(v, sp)| v.into_iter().map(move |v| (v, sp)))
        .collect::<HashMap<_, _>>();

    let (mut vote_account_info, total_active_stake) =
        get_vote_account_info(rpc_client, last_epoch)?;

    // compute cumulative_stake_limit => active_stake of the last validator inside the can-halt-the-network group
    // we later set score=0 to all validators whose stake >= concentrated_validators_stake_limit
    // sort by active_stake
    vote_account_info.sort_by(|a, b| b.active_stake.cmp(&a.active_stake));
    let mut accumulated: u64 = 0;
    let mut count_halt_group: u32 = 0;
    let limit: u64 = total_active_stake / 100 * 33;
    let mut last_under_nakamoto_active_stake = limit;
    for info in &vote_account_info {
        last_under_nakamoto_active_stake = info.active_stake;
        accumulated += info.active_stake;
        count_halt_group += 1;
        if accumulated > limit {
            break;
        }
    }
    info!(
        "validators:{} total_active_stake:{}, can_halt_the_network:top {}, last under-nakamoto-coefficient active-stake: {}",
        &vote_account_info.len(),
        total_active_stake,
        count_halt_group,
        lamports_to_sol(last_under_nakamoto_active_stake),
    );

    // Note: get_self_stake_by_vote_account is expensive because it does a RPC call for each validator
    // we skip this data gathering if config.min_self_stake_lamports==0
    let self_stake_by_vote_account = if config.min_self_stake_lamports > 0 {
        get_self_stake_by_vote_account(rpc_client, epoch, &vote_account_info)?
    } else {
        HashMap::new()
    };

    let (cluster_nodes_with_old_version, min_release_version): (HashMap<String, _>, _) =
        match config.min_release_version {
            Some(ref min_release_version) => (
                rpc_client
                    .get_cluster_nodes()?
                    .into_iter()
                    .filter_map(|rpc_contact_info| {
                        if let Ok(identity) = Pubkey::from_str(&rpc_contact_info.pubkey) {
                            if config.score_all || validator_list.contains(&identity) {
                                if let Some(ref version) = rpc_contact_info.version {
                                    if let Ok(semver) = semver::Version::parse(version) {
                                        if semver < *min_release_version {
                                            return Some((rpc_contact_info.pubkey, semver));
                                        }
                                    }
                                }
                            }
                        }
                        None
                    })
                    .collect(),
                min_release_version.to_string(),
            ),
            None => (HashMap::default(), "".to_string()),
        };

    if let Some(ref min_release_version) = config.min_release_version {
        info!(
            "Validators running a release older than {}: {:?}",
            min_release_version, cluster_nodes_with_old_version,
        );
    }

    let (
        quality_block_producers,
        poor_block_producers,
        block_producer_classification_reason,
        cluster_average_skip_rate,
        too_many_poor_block_producers,
    ) = classify_block_producers(rpc_client, config, last_epoch)?;

    let not_in_leader_schedule: ValidatorList = validator_list
        .difference(
            &quality_block_producers
                .intersection(&poor_block_producers)
                .cloned()
                .collect(),
        )
        .cloned()
        .collect();

    let too_many_old_validators = cluster_nodes_with_old_version.len()
        > (poor_block_producers.len() + quality_block_producers.len())
            * config.max_old_release_version_percentage
            / 100;

    let (
        poor_voters,
        poor_voter_percentage,
        min_epoch_credits,
        avg_epoch_credits,
        too_many_poor_voters,
    ) = classify_poor_voters(config, &vote_account_info);

    let mut notes = vec![
        format!(
            "Minimum vote credits required for epoch {}: {} (cluster average: {}, grace: {}%)",
            last_epoch,
            min_epoch_credits,
            avg_epoch_credits,
            config.min_epoch_credit_percentage_of_average,
        ),
        format!(
            "Maximum allowed skip rate for epoch {}: {:.2}% (cluster average: {:.2}%, grace: {}%)",
            last_epoch,
            cluster_average_skip_rate + config.quality_block_producer_percentage,
            cluster_average_skip_rate,
            config.quality_block_producer_percentage,
        ),
        format!("Solana release {} or greater required", min_release_version),
        format!("Maximum commission: {}%", config.max_commission),
        format!(
            "Minimum required self stake: {}",
            Sol(config.min_self_stake_lamports)
        ),
        format!(
            "Maximum active stake allowed: {}",
            Sol(config.max_active_stake_lamports)
        ),
    ];
    if let Some(max_infrastructure_concentration) = config.max_infrastructure_concentration {
        notes.push(format!(
            "Maximum infrastructure concentration: {:0}%",
            max_infrastructure_concentration
        ));
    }

    if cluster_average_skip_rate > config.bad_cluster_average_skip_rate {
        notes.push("Cluster average skip rate is poor".to_string());
    }
    if too_many_poor_voters {
        notes.push(format!(
            "Too many validators classified as poor voters for epoch {}: {}% (limit: {}%)",
            last_epoch, poor_voter_percentage, config.max_poor_voter_percentage
        ));
    }
    if too_many_old_validators {
        notes.push(format!(
            "Over {}% of validators classified as running an older release",
            config.max_old_release_version_percentage
        ));
    }
    if too_many_poor_block_producers {
        notes.push(format!(
            "Over {}% of validators classified as poor block producers in epoch {}",
            config.max_poor_block_producer_percentage, last_epoch,
        ));
    }

    let validator_classifications = if too_many_poor_voters
        || too_many_old_validators
        || too_many_poor_block_producers
    {
        notes.push("Stake adjustments skipped this epoch".to_string());
        None
    } else {
        let mut validator_classifications = HashMap::new();
        let mut total_skipped: u32 = 0;

        for VoteAccountInfo {
            identity,
            vote_address,
            commission,
            active_stake,
            epoch_credits,
        } in vote_account_info
        {
            if !config.score_all && !validator_list.contains(&identity) {
                total_skipped += 1;
                continue;
            }

            let mut score_discounts = ScoreDiscounts::default();

            let participant = identity_to_participant.get(&identity).cloned();

            let validators_app_info = data_centers
                .by_identity
                .get(&identity)
                .cloned()
                .unwrap_or_default();

            let current_data_center = validators_app_info.data_center_id.clone();

            // score: check data center concentration
            let data_center_info = data_centers
                .info
                .iter()
                .find(|x| x.id == current_data_center)
                .unwrap();

            let self_stake = self_stake_by_vote_account
                .get(&vote_address)
                .cloned()
                .unwrap_or_default();

            let block_producer_classification_reason_msg = block_producer_classification_reason
                .get(&identity)
                .cloned()
                .unwrap_or_default();
            let vote_credits_msg =
                format!("{} credits earned in epoch {}", epoch_credits, last_epoch);

            // no score if in the can-halt-the-network group
            score_discounts.can_halt_the_network_group =
                active_stake >= last_under_nakamoto_active_stake;

            let (stake_state, reason) = if let Some(concentration) =
                infrastructure_concentration_too_high.get(&identity)
            {
                (
                    ValidatorStakeState::None,
                    format!(
                        "infrastructure concentration {:.1}% is too high; consider finding a new data center",
                        *concentration
                    ),
                )
            } else if config.enforce_min_self_stake && self_stake < config.min_self_stake_lamports {
                (
                    ValidatorStakeState::None,
                    format!("insufficient self stake: {}", Sol(self_stake)),
                )
            } else if active_stake > config.max_active_stake_lamports {
                (
                    ValidatorStakeState::None,
                    format!("Active stake is too high: {}", Sol(active_stake)),
                )
            } else if commission > config.max_commission {
                (
                    ValidatorStakeState::None,
                    format!("Commission is too high: {}% commission", commission),
                )
            } else if poor_voters.contains(&identity) {
                (
                    ValidatorStakeState::None,
                    format!("Insufficient vote credits: {}", vote_credits_msg),
                )
            } else if cluster_nodes_with_old_version.contains_key(&identity.to_string()) {
                (
                    ValidatorStakeState::None,
                    format!(
                        "Outdated Solana release: {}",
                        cluster_nodes_with_old_version
                            .get(&identity.to_string())
                            .unwrap()
                    ),
                )
            } else if quality_block_producers.contains(&identity) {
                (
                    ValidatorStakeState::Bonus,
                    format!(
                        "Good block production during epoch {}: {}",
                        last_epoch, block_producer_classification_reason_msg
                    ),
                )
            } else if poor_block_producers.contains(&identity) {
                (
                    ValidatorStakeState::Baseline,
                    format!(
                        "Poor block production during epoch {}: {}",
                        last_epoch, block_producer_classification_reason_msg
                    ),
                )
            } else {
                assert!(!poor_voters.contains(&identity));
                assert!(config.score_all || not_in_leader_schedule.contains(&identity));
                (
                    ValidatorStakeState::Baseline,
                    format!("No leader slots; {}", vote_credits_msg),
                )
            };

            debug!(
                "\nidentity: {} ({:?})\n\
                    - vote address: {}\n\
                    - data center: {:?}, self stake: {}\n",
                identity,
                participant,
                vote_address,
                current_data_center,
                Sol(self_stake),
            );

            validator_classifications.insert(
                identity,
                ValidatorClassification {
                    identity,
                    vote_address,
                    stake_state,
                    score_data: Some(ScoreData {
                        epoch_credits,
                        average_position: epoch_credits as f64 / avg_epoch_credits as f64 * 50.0,
                        score_discounts,
                        commission,
                        active_stake,
                        data_center_concentration: data_center_info.stake_percent,
                        validators_app_info,
                    }),
                    stake_action: None,
                    stake_state_reason: reason,
                    current_data_center: Some(current_data_center.clone()),
                    participant,
                    prioritize_funding_in_next_epoch: None,
                },
            );
        }
        notes.push(format!(
            "{} validators processed",
            validator_classifications.len()
        ));
        info!(
            "{} validators, {} skipped",
            &validator_classifications.len(),
            total_skipped
        );

        Some(validator_classifications)
    };
    notes.push(format!("Active stake: {}", Sol(total_active_stake)));

    Ok(EpochClassificationV1 {
        data_center_info: data_centers.info,
        validator_classifications,
        notes,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_staked_for() {
        let mut vc = ValidatorClassification::default();

        assert!(!vc.staked_for(0, 0));
        assert!(!vc.staked_for(1, 0));
        assert!(!vc.staked_for(0, 1));

        vc.stake_states = Some(vec![
            (ValidatorStakeState::None, String::new()),
            (ValidatorStakeState::Baseline, String::new()),
            (ValidatorStakeState::Bonus, String::new()),
        ]);
        assert!(!vc.staked_for(3, 3));
        assert!(vc.staked_for(2, 3));
    }
}
