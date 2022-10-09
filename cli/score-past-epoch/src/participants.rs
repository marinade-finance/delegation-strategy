use log::*;
use solana_client::rpc_client::RpcClient;
use solana_foundation_delegation_program_cli::get_participants_with_state;
use solana_foundation_delegation_program_registry::state::{Participant, ParticipantState};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::error;

pub type IdentityToParticipant = HashMap<Pubkey, Pubkey>;

pub fn get_participants_identity_maps(
    rpc_client: &RpcClient,
) -> Result<(IdentityToParticipant, IdentityToParticipant), Box<dyn error::Error>> {
    let participants = get_participants_with_state(rpc_client, Some(ParticipantState::Approved))?;

    info!("{} participants loaded", participants.len());
    assert!(participants.len() > 450);

    let (mainnet_identity_to_participant, testnet_identity_to_participant): (
        IdentityToParticipant,
        IdentityToParticipant,
    ) = participants
        .iter()
        .map(
            |(
                participant,
                Participant {
                    mainnet_identity,
                    testnet_identity,
                    ..
                },
            )| {
                (
                    (*mainnet_identity, *participant),
                    (*testnet_identity, *participant),
                )
            },
        )
        .unzip();

    Ok((
        mainnet_identity_to_participant,
        testnet_identity_to_participant,
    ))
}
