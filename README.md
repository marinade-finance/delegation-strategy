# Delegation Strategy

## Scoring
- Scoring based on the result of previous epochs (based on https://github.com/solana-labs/stake-o-matic)
    - Factoring in:
        - Epoch credits
        - Commission
        - Decentralization
        - Version
        - ...
    - We pick top 300 validators (this top 300 validators are different each epoch, so the stake is delegated to more than 400 validators at this moment)
- Scoring based on the performance within current epoch
    - Credits observed within epoch (relative to others)
        - < 80 % of average -> emergency unstake
        - < 90 % of average -> score set to to 50 %
    - APY relative to others
        - very low APY -> emergency unstake
        - low APY -> score set to 50 %
    - Delinquency based on `solana validators`
- Scoring adjustments
    - Blacklisted validators -> emergency unstake
    - Adjust scores for over-staked
        - if score is 0 (e.g. not in the top 300) and the validator's Marinade stake is > 0.45 % of all Marinade stake (with the exception of validators where Marinade stake is > 20 % of their total stake) -> emergency unstake (todo: change to partial unstake when available)
        - if score is not 0 but the validator got over 250 % of what they should have (with the exception of validators where Marinade stake is > 20 % of their total stake) -> emergency unstake (todo: change to partial unstake when available)
    - Max stake capping - the total stake given by Marinade to a single validator does not exceed 1.5 % of the total Marinade stake
    - Validator's stake delta capping:
        - if the validator is severely under-staked (they could potentially receive more than 2x of their current Marinade stake): cap score to 80 % of their current score
        - if the validator is under-staked: cap the score so the received stake is no more than 0.1 %  of the overall stake currently delegated to all validators
        - if the validator is over-staked: noop
