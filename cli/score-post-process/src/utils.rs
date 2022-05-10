pub fn weighted_distribution(amount: u64, weights: Vec<u64>) -> Vec<u64> {
    let mut distribution = Vec::with_capacity(weights.len());

    let mut remaining_weight: u64 = weights.iter().sum();
    let mut remaining_amount: u64 = amount;
    assert_ne!(remaining_weight, 0, "Sum of weights is 0!");

    for weight in weights {
        let weighted_amount =
            (remaining_amount as u128) * (weight as u128) / (remaining_weight as u128);
        remaining_amount -= weighted_amount as u64;
        remaining_weight -= weight;

        distribution.push(weighted_amount as u64);
    }

    distribution
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribute_0_amount() {
        assert_eq!(weighted_distribution(0, vec![0, 1, 2]), vec![0, 0, 0]);
    }

    #[test]
    #[should_panic]
    fn distribute_to_0_weight() {
        weighted_distribution(0, vec![0]);
    }

    #[test]
    fn distribute_remainder_correctly() {
        assert_eq!(weighted_distribution(100, vec![1, 1, 1]), vec![33, 33, 34]);
        assert_eq!(weighted_distribution(1, vec![100, 100, 100]), vec![0, 0, 1]);
        assert_eq!(weighted_distribution(180, vec![6, 2, 1]), vec![120, 40, 20]);
    }

    #[test]
    fn distribute_disproportional() {
        assert_eq!(
            weighted_distribution(1_000_000_000, vec![1, 2, 1]),
            vec![250_000_000, 500_000_000, 250_000_000]
        );
        assert_eq!(
            weighted_distribution(10, vec![250_000_000, 500_000_000, 250_000_000]),
            vec![2, 5, 3]
        );
    }

    #[test]
    fn distribute_large() {
        assert_eq!(
            weighted_distribution(
                0xFFFF_FFFF_FFFF_FFFF,
                vec![0xFFFF_FFFF_00000000, 0xFFFF_0000, 0xFFFF]
            ),
            vec![0xFFFF_FFFF_00000000, 0xFFFF_0000, 0xFFFF]
        );
    }
}
