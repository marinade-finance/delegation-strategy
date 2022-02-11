use {
    crate::rpc_client_utils::*,
    clap::{
        crate_description, crate_name, value_t, value_t_or_exit, App, AppSettings, Arg, ArgMatches,
        SubCommand,
    },
    log::*,
    solana_clap_utils::{
        input_parsers::lamports_of_sol,
        input_validators::{
            is_amount, is_keypair, is_parsable, is_pubkey_or_keypair, is_url, is_valid_percentage,
        },
    },
    solana_client::rpc_client::RpcClient,
    solana_sdk::native_token::*,
    std::{error, path::PathBuf, time::Duration},
};

type BoxResult<T> = Result<T, Box<dyn error::Error>>;

fn is_release_version(string: String) -> Result<(), String> {
    if string.starts_with('v') && semver::Version::parse(string.split_at(1).1).is_ok() {
        return Ok(());
    }
    semver::Version::parse(&string)
        .map(|_| ())
        .map_err(|err| format!("{:?}", err))
}

fn release_version_of(matches: &ArgMatches<'_>, name: &str) -> Option<semver::Version> {
    matches
        .value_of(name)
        .map(ToString::to_string)
        .map(|string| {
            if string.starts_with('v') {
                semver::Version::parse(string.split_at(1).1)
            } else {
                semver::Version::parse(&string)
            }
            .expect("semver::Version")
        })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cluster {
    Testnet,
    MainnetBeta,
}

impl std::fmt::Display for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Testnet => "testnet",
                Self::MainnetBeta => "mainnet-beta",
            }
        )
    }
}

#[derive(Debug)]
pub struct Config {
    pub json_rpc_url: String,
    pub cluster: Cluster,
    pub db_path: PathBuf,

    /// compute score foll all validators in the cluster
    pub score_all: bool,
    /// max commission accepted to score (0-100)
    pub score_max_commission: u8,
    /// score min stake required
    pub score_min_stake: u64,
    /// score discount per concentration percentage point
    pub score_concentration_point_discount: u32,
    /// min average position considering credits_observed, 50.0 = average
    pub min_avg_position: f64,

    /// Quality validators produce within this percentage of the cluster average skip rate over
    /// the previous epoch
    pub quality_block_producer_percentage: usize,

    /// Don't ever unstake more than this percentage of the cluster at one time for poor block
    /// production
    pub max_poor_block_producer_percentage: usize,

    /// Vote accounts with a larger commission than this amount will not be staked.
    pub max_commission: u8,

    /// If Some(), destake validators with a version less than this version subject to the
    /// `max_old_release_version_percentage` limit
    pub min_release_version: Option<semver::Version>,

    /// Do not unstake more than this percentage of the cluster at one time for running an
    /// older software version
    pub max_old_release_version_percentage: usize,

    /// Do not unstake more than this percentage of the cluster at one time for being poor
    /// voters
    pub max_poor_voter_percentage: usize,

    /// Vote accounts sharing infrastructure with larger than this amount will not be staked
    /// None: skip infrastructure concentration check
    pub max_infrastructure_concentration: Option<f64>,

    pub bad_cluster_average_skip_rate: usize,

    /// Destake if the validator's vote credits for the latest full epoch are less than this percentage
    /// of the cluster average
    pub min_epoch_credit_percentage_of_average: usize,

    /// Minimum amount of lamports a validator must stake on itself to be eligible for a delegation
    pub min_self_stake_lamports: u64,

    /// Validators with more than this amount of active stake are not eligible fora delegation
    pub max_active_stake_lamports: u64,

    /// If true, enforce the `min_self_stake_lamports` limit. If false, only warn on insufficient stake
    pub enforce_min_self_stake: bool,
}

impl Config {
    #[cfg(test)]
    pub fn default_for_test() -> Self {
        Self {
            json_rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            cluster: Cluster::MainnetBeta,
            db_path: PathBuf::default(),
            score_all: false,
            score_max_commission: 8,
            score_min_stake: sol_to_lamports(100.0),
            score_concentration_point_discount: 1_500,
            min_avg_position: 40.0,
            quality_block_producer_percentage: 15,
            max_poor_block_producer_percentage: 20,
            max_commission: 100,
            min_release_version: None,
            max_old_release_version_percentage: 10,
            max_poor_voter_percentage: 20,
            max_infrastructure_concentration: Some(100.0),
            bad_cluster_average_skip_rate: 50,
            min_epoch_credit_percentage_of_average: 50,
            min_self_stake_lamports: 0,
            max_active_stake_lamports: u64::MAX,
            enforce_min_self_stake: false,
        }
    }

    pub fn cluster_db_path_for(&self, cluster: Cluster) -> PathBuf {
        // store db on different dir for score-all to not mess with SPL-stake-pool distribution usage
        let dir = if self.score_all { "score-all" } else { "data" };
        self.db_path.join(format!("{}-{}", dir, cluster))
    }

    pub fn cluster_db_path(&self) -> PathBuf {
        self.cluster_db_path_for(self.cluster)
    }
}

fn app_version() -> String {
    // Determine version based on the environment variables set by Github Actions
    let tag = option_env!("GITHUB_REF")
        .and_then(|github_ref| github_ref.strip_prefix("refs/tags/").map(|s| s.to_string()));

    tag.unwrap_or_else(|| match option_env!("GITHUB_SHA") {
        None => "devbuild".to_string(),
        Some(commit) => commit[..8].to_string(),
    })
}

pub fn get_config() -> BoxResult<(Config, RpcClient)> {
    let app_version = &*app_version();
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(app_version)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .setting(AppSettings::VersionlessSubcommands)
        .setting(AppSettings::InferSubcommands)
        .arg(
            Arg::with_name("json_rpc_url")
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .validator(is_url)
                .help("JSON RPC URL for the cluster")
        )
        .arg(
            Arg::with_name("cluster")
                .long("cluster")
                .value_name("NAME")
                .possible_values(&["mainnet-beta", "testnet"])
                .takes_value(true)
                .default_value("testnet")
                .required(true)
                .help("Name of the cluster to operate on")
        )
        .arg(
            Arg::with_name("confirm")
                .long("confirm")
                .takes_value(false)
                .help("Confirm that the stake adjustments should actually be made")
        )
        .arg(
            Arg::with_name("markdown")
                .long("markdown")
                .takes_value(false)
                .help("Output markdown")
        )
        .arg(
            Arg::with_name("db_path")
                .long("db-path")
                .value_name("PATH")
                .takes_value(true)
                .default_value("db")
                .help("Location for storing staking history")
        )
        .arg(
            Arg::with_name("require_classification")
                .long("require-classification")
                .takes_value(false)
                .help("Fail if the classification for the previous epoch does not exist")
        )
        .arg(
            Arg::with_name("quality_block_producer_percentage")
                .long("quality-block-producer-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("15")
                .validator(is_valid_percentage)
                .help("Quality validators have a skip rate within this percentage of \
                       the cluster average in the previous epoch.")
        )
        .arg(
            Arg::with_name("bad_cluster_average_skip_rate")
                .long("bad-cluster-average-skip-rate")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("50")
                .validator(is_valid_percentage)
                .help("Threshold to notify for a poor average cluster skip rate.")
        )
        .arg(
            Arg::with_name("max_poor_block_producer_percentage")
                .long("max-poor-block-producer-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("20")
                .validator(is_valid_percentage)
                .help("Do not add or remove bonus stake if at least this \
                       percentage of all validators are poor block producers")
        )
        .arg(
            Arg::with_name("min_epoch_credit_percentage_of_average")
                .long("min-epoch-credit-percentage-of-average")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("50")
                .validator(is_valid_percentage)
                .help("Validator vote credits for the latest full epoch must \
                       be at least this percentage of the cluster average vote credits")
        )
        .arg(
            Arg::with_name("max_commission")
                .long("max-commission")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("100")
                .validator(is_valid_percentage)
                .help("Vote accounts with a larger commission than this amount will not be staked")
        )
        .arg(
            Arg::with_name("min_release_version")
                .long("min-release-version")
                .value_name("SEMVER")
                .takes_value(true)
                .validator(is_release_version)
                .help("Remove the base and bonus stake from validators with \
                       a release version older than this one")
        )
        .arg(
            Arg::with_name("max_poor_voter_percentage")
                .long("max-poor-voter-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("20")
                .validator(is_valid_percentage)
                .help("Do not remove stake from validators poor voting history \
                       if more than this percentage of all validators have a \
                       poor voting history")
        )
        .arg(
            Arg::with_name("max_old_release_version_percentage")
                .long("max-old-release-version-percentage")
                .value_name("PERCENTAGE")
                .takes_value(true)
                .default_value("10")
                .validator(is_valid_percentage)
                .help("Do not remove stake from validators running older \
                       software versions if more than this percentage of \
                       all validators are running an older software version")
        )
        .arg(
            Arg::with_name("max_infrastructure_concentration")
                .long("max-infrastructure-concentration")
                .takes_value(true)
                .value_name("PERCENTAGE")
                .validator(is_valid_percentage)
                .help("Vote accounts sharing infrastructure with larger than this amount will not be staked")
        )
        .arg(
            Arg::with_name("min_self_stake")
                .long("min-self-stake")
                .value_name("AMOUNT")
                .takes_value(true)
                .validator(is_amount)
                .default_value("0")
                .required(true)
                .help("Minimum amount of SOL a validator must stake on itself to be eligible for a delegation"),
        )
        .arg(
            Arg::with_name("max_active_stake")
                .long("max-active-stake")
                .value_name("AMOUNT")
                .takes_value(true)
                .validator(is_amount)
                .default_value("3500000")
                .required(true)
                .help("Maximum amount of stake a validator may have to be eligible for a delegation"),
        )
        .arg(
            Arg::with_name("enforce_min_self_stake")
                .long("enforce-min-self-stake")
                .takes_value(false)
                .help("Enforce the minimum self-stake requirement")
        )
        .arg(
            Arg::with_name("min_testnet_participation")
                .long("min-testnet-participation")
                .value_name("N M")
                .multiple(true)
                .min_values(2)
                .max_values(2)
                .validator(is_parsable::<usize>)
                .help("Require that the participant's mainnet-beta validator be staked for N out of the \
                       last M epochs to be delegable for mainnet-beta stake.\n\
                       This setting is ignored if the --cluster is not `mainnet-beta`")
        )
        .arg(
            Arg::with_name("enforce_testnet_participation")
                .long("enforce-testnet-participation")
                .takes_value(false)
                .help("Enforce the minimum testnet participation requirement.\n
                       This setting is ignored if the --cluster is not `mainnet-beta`")
        )
        .subcommand(
            SubCommand::with_name("stake-pool-v0").about("Use the stake-pool v0 solution")
            .arg(
                Arg::with_name("reserve_stake_address")
                    .index(1)
                    .value_name("RESERVE_STAKE_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .validator(is_pubkey_or_keypair)
                    .help("The reserve stake account used to fund the stake pool")
            )
            .arg(
                Arg::with_name("authorized_staker")
                    .index(2)
                    .value_name("KEYPAIR")
                    .validator(is_keypair)
                    .required(true)
                    .takes_value(true)
                    .help("Keypair of the authorized staker")
            )
            .arg(
                Arg::with_name("min_reserve_stake_balance")
                    .long("min-reserve-stake-balance")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("1")
                    .validator(is_amount)
                    .help("The minimum balance to keep in the reserve stake account")
            )
            .arg(
                Arg::with_name("baseline_stake_amount")
                    .long("baseline-stake-amount")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("5000")
                    .validator(is_amount)
            )
        )
        .subcommand(
            SubCommand::with_name("stake-pool").about("Use a stake pool")
            .arg(
                Arg::with_name("pool_address")
                    .index(1)
                    .value_name("POOL_ADDRESS")
                    .takes_value(true)
                    .required(true)
                    .validator(is_pubkey_or_keypair)
                    .help("The stake pool address")
            )
            .arg(
                Arg::with_name("authorized_staker")
                    .index(2)
                    .value_name("KEYPAIR")
                    .validator(is_keypair)
                    .required(true)
                    .takes_value(true)
                    .help("Keypair of the authorized staker")
            )
            .arg(
                Arg::with_name("baseline_stake_amount")
                    .long("baseline-stake-amount")
                    .value_name("SOL")
                    .takes_value(true)
                    .default_value("5000")
                    .validator(is_amount)
            )
        )
        .subcommand(
            SubCommand::with_name("score-all").about("Score all validators in the cluster")
            .arg(
                Arg::with_name("score_max_commission")
                    .long("score-max-commission")
                    .takes_value(true)
                    .required(false)
                    .help("scoring max accepted commission")
            )
            .arg(
                Arg::with_name("score_min_stake")
                    .long("score-min-stake")
                    .takes_value(true)
                    .required(false)
                    .help("scoring min stake required")
            )
            .arg(
                Arg::with_name("commission_point_discount")
                    .long ("commission-point-discount")
                    .takes_value(true)
                    .required(false)
                    .help("score to discount for each commission point")
            )
            .arg(
                Arg::with_name("concentration_point_discount")
                    .long ("concentration-point-discount")
                    .takes_value(true)
                    .required(false)
                    .help("score to discount for each concentration percentage point")
            )
            .arg(
                Arg::with_name("min_avg_position")
                    .long ("min-avg-position")
                    .takes_value(true)
                    .required(false)
                    .help("min avg position required considering epoch_credits")
            )
        )
        .get_matches();

    let cluster = match value_t_or_exit!(matches, "cluster", String).as_str() {
        "mainnet-beta" => Cluster::MainnetBeta,
        "testnet" => Cluster::Testnet,
        _ => unreachable!(),
    };
    let quality_block_producer_percentage =
        value_t_or_exit!(matches, "quality_block_producer_percentage", usize);
    let min_epoch_credit_percentage_of_average =
        value_t_or_exit!(matches, "min_epoch_credit_percentage_of_average", usize);
    let max_commission = value_t_or_exit!(matches, "max_commission", u8);
    let max_poor_voter_percentage = value_t_or_exit!(matches, "max_poor_voter_percentage", usize);
    let max_poor_block_producer_percentage =
        value_t_or_exit!(matches, "max_poor_block_producer_percentage", usize);
    let max_old_release_version_percentage =
        value_t_or_exit!(matches, "max_old_release_version_percentage", usize);
    let min_release_version = release_version_of(&matches, "min_release_version");

    let enforce_min_self_stake = matches.is_present("enforce_min_self_stake");
    let min_self_stake_lamports = lamports_of_sol(&matches, "min_self_stake").unwrap();
    let max_active_stake_lamports = lamports_of_sol(&matches, "max_active_stake").unwrap();

    let json_rpc_url = match cluster {
        Cluster::MainnetBeta => value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| "http://api.mainnet-beta.solana.com".into()),
        Cluster::Testnet => value_t!(matches, "json_rpc_url", String)
            .unwrap_or_else(|_| "http://api.testnet.solana.com".into()),
    };
    let db_path = value_t_or_exit!(matches, "db_path", PathBuf);

    let bad_cluster_average_skip_rate =
        value_t!(matches, "bad_cluster_average_skip_rate", usize).unwrap_or(50);
    let max_infrastructure_concentration =
        value_t!(matches, "max_infrastructure_concentration", f64).ok();

    // score-all command and arguments
    let (
        score_all,
        score_max_commission,
        score_min_stake,
        score_concentration_point_discount,
        min_avg_position,
    ) = match matches.subcommand() {
        ("score-all", Some(matches)) => (
            true,
            value_t!(matches, "score_max_commission", u8).unwrap_or(10),
            value_t!(matches, "score_min_stake", u64).unwrap_or(sol_to_lamports(100.0)),
            value_t!(matches, "concentration_point_discount", u32).unwrap_or(2000),
            value_t!(matches, "min_avg_position", f64).unwrap_or(50.0),
        ),
        _ => (false, 0, 0, 0, 0.0),
    };

    let config = Config {
        json_rpc_url,
        cluster,
        db_path,
        score_all,
        score_max_commission,
        score_min_stake,
        score_concentration_point_discount,
        min_avg_position,
        quality_block_producer_percentage,
        max_poor_block_producer_percentage,
        max_commission,
        min_release_version,
        max_old_release_version_percentage,
        max_poor_voter_percentage,
        max_infrastructure_concentration,
        bad_cluster_average_skip_rate,
        min_epoch_credit_percentage_of_average,
        min_self_stake_lamports,
        max_active_stake_lamports,
        enforce_min_self_stake,
    };

    info!("RPC URL: {}", config.json_rpc_url);
    let rpc_client =
        RpcClient::new_with_timeout(config.json_rpc_url.clone(), Duration::from_secs(180));

    rpc_client_health_check(&rpc_client);

    Ok((config, rpc_client))
}
