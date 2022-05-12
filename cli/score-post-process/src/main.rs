#![cfg_attr(not(debug_assertions), deny(warnings))]

use anyhow::bail;
use cli_common::{Cluster, ExpandedPath, InputPubkey};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
// use utils::Cluster;

use log::{debug, error, info};

use std::{str::FromStr, sync::Arc};
use structopt::StructOpt;

pub mod process_scores;

use process_scores::ProcessScoresOptions;

#[derive(Debug, StructOpt)]
pub struct Common {
    #[structopt(short = "c", default_value = "~/.config/solana/cli/config.yml")]
    config_file: ExpandedPath,

    #[structopt(
        short = "i",
        env = "MARINADE_INSTANCE",
        default_value = "auto" //select default instance based on cluster
        // other possible values:
        //default_value = "~/.config/mardmin/instance.json"
        //default_value = "9tA9pzAZWimw2EMZgMjmUwzB2qPKrHhFNaC2ZvCrReeh"
    )]
    instance: InputPubkey,
}

#[derive(Debug, StructOpt)]
struct Params {
    #[structopt(flatten)]
    common: Common,

    #[structopt(subcommand)]
    command: MardminCommand,
}

#[derive(Debug, StructOpt)]
enum MardminCommand {
    ProcessScores(ProcessScoresOptions),
}

fn main() -> anyhow::Result<()> {
    let mut params = Params::from_args();

    solana_logger::setup_with("info");

    let cli_config = match solana_cli_config::Config::load(&params.common.config_file.to_string()) {
        Ok(cli_config) => cli_config,
        Err(err) => {
            error!(
                "Solana CLI config {} reading error: {}",
                params.common.config_file.to_string(),
                err
            );
            bail!(
                "Solana CLI config {} reading error: {}",
                params.common.config_file.to_string(),
                err
            );
        }
    };
    let cluster = Cluster::from_url(&cli_config.json_rpc_url);
    info!(
        "Cluster: {:?}, commitment: {}",
        cluster, &cli_config.commitment
    );

    // if instance is "auto" use default per cluster
    if let InputPubkey::Auto = params.common.instance {
        params.common.instance = InputPubkey::Pubkey(cluster.default_instance());
    };
    info!("Instance: {:?}", params.common.instance);

    debug!("Solana config: {:?}", cli_config);

    let client = Arc::new(RpcClient::new_with_commitment(
        cli_config.json_rpc_url,
        CommitmentConfig::from_str(&cli_config.commitment).unwrap(),
    ));

    Ok(match params.command {
        MardminCommand::ProcessScores(options) => options.process(params.common, client, cluster),
    }?)
}
