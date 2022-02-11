use {
    crate::{classification::*, config::*},
    log::*,
    solana_sdk::{clock::Epoch, native_token::*},
    std::error,
    std::{fs::File, io::Write},
};

type BoxResult<T> = Result<T, Box<dyn error::Error>>;

pub fn generate_validators_csv(epoch: Epoch, config: &Config) -> BoxResult<()> {
    let epoch_classification =
        EpochClassification::load(epoch, &config.cluster_db_path())?.into_current();

    if let Some(ref validator_classifications) = epoch_classification.validator_classifications {
        let mut validator_detail_csv = vec![];
        validator_detail_csv.push("epoch,keybase_id,name,identity,vote_address,score,average_position,commission,active_stake,epoch_credits,data_center_concentration,can_halt_the_network_group,stake_state,stake_state_reason,www_url".into());
        let mut validator_classifications = validator_classifications.iter().collect::<Vec<_>>();
        // sort by credits, desc
        validator_classifications.sort_by(|a, b| {
            b.1.score_data
                .as_ref()
                .unwrap()
                .epoch_credits
                .cmp(&a.1.score_data.as_ref().unwrap().epoch_credits)
        });
        for (identity, classification) in validator_classifications {
            //epoch,keybase_id,name,identity,vote_address,score,average_position,commission,active_stake,epoch_credits,data_center_concentration,can_halt_the_network_group,stake_state,stake_state_reason,www_url
            if let Some(score_data) = &classification.score_data {
                let score = score_data.score(config);

                let csv_line = format!(
                    r#"{},"{}","{}","{}","{}",{},{},{},{},{},{:.4},{},"{:?}","{}","{}""#,
                    epoch,
                    escape_quotes(&score_data.validators_app_info.keybase_id),
                    escape_quotes(&score_data.validators_app_info.name),
                    identity.to_string(),
                    classification.vote_address,
                    score,
                    score_data.average_position,
                    score_data.commission,
                    lamports_to_sol(score_data.active_stake),
                    score_data.epoch_credits,
                    score_data.data_center_concentration,
                    score_data.score_discounts.can_halt_the_network_group,
                    classification.stake_state,
                    escape_quotes(&classification.stake_state_reason),
                    escape_quotes(&score_data.validators_app_info.www_url),
                );
                validator_detail_csv.push(csv_line);
            }
        }
        // save {cluster}-validator-detail.csv (repeating the cluster in the name is intentional)
        let filename = config
            .cluster_db_path()
            .join(format!("{}-validator-detail.csv", config.cluster));
        info!("Writing {}", filename.display());
        let mut file = File::create(filename)?;
        file.write_all(&validator_detail_csv.join("\n").into_bytes())?;
    }

    Ok(())
}

fn escape_quotes(original: &String) -> String {
    original.replace("\"", "\"\"")
}
