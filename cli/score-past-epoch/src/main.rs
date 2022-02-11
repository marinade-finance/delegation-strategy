use {
    crate::{classification::*, config::*, participants::*},
    log::*,
    std::error,
};

mod classification;
mod config;
mod data_center_info;
mod participants;
mod report;
mod rpc_client_utils;
mod validators_app;
mod validators_list;

type BoxResult<T> = Result<T, Box<dyn error::Error>>;

fn main() -> BoxResult<()> {
    solana_logger::setup_with("info");
    info!("Starting scoring of the last epoch");

    let (config, rpc_client) = get_config()?;

    let (mainnet_identity_to_participant, testnet_identity_to_participant) =
        get_participants_identity_maps()?;

    let (validator_list, identity_to_participant) = match config.cluster {
        Cluster::MainnetBeta => (
            mainnet_identity_to_participant.keys().cloned().collect(),
            mainnet_identity_to_participant,
        ),
        Cluster::Testnet => (
            validators_list::testnet_validators().into_iter().collect(),
            testnet_identity_to_participant,
        ),
    };

    let epoch = rpc_client.get_epoch_info()?.epoch;
    info!("Epoch: {:?}", epoch);
    assert!(epoch > 0);

    info!("Data directory: {:?}", config.cluster_db_path());

    if EpochClassification::exists(epoch, &config.cluster_db_path()) {
        error!("Classification for epoch {} already exists", epoch);
        panic!("Cannot overwrite the previous classification!");
    }

    let epoch_classification = classify(
        &rpc_client,
        &config,
        epoch,
        &validator_list,
        &identity_to_participant,
    )?;

    EpochClassification::new(epoch_classification).save(epoch, &config.cluster_db_path())?;
    report::generate_validators_csv(epoch, &config)?;

    Ok(())
}
