#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "water filling indexes are created from the same bounded input slice"
)]

use crate::SenderId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AllocationInput {
    pub(crate) sender: SenderId,
    pub(crate) demand: u64,
    pub(crate) weight: u16,
}

pub(crate) fn weighted_max_min(capacity: u64, inputs: &[AllocationInput]) -> Vec<u64> {
    let mut allocations = vec![0; inputs.len()];
    let mut active: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter_map(|(index, input)| (input.demand > 0).then_some(index))
        .collect();
    let mut remaining = capacity.min(inputs.iter().map(|input| input.demand).sum());

    while remaining > 0 && !active.is_empty() {
        let weight_sum: u64 = active
            .iter()
            .map(|index| u64::from(inputs[*index].weight))
            .sum();
        let mut capped = Vec::new();
        for index in &active {
            let room = inputs[*index].demand.saturating_sub(allocations[*index]);
            if u128::from(room) * u128::from(weight_sum)
                <= u128::from(remaining) * u128::from(inputs[*index].weight)
            {
                capped.push(*index);
            }
        }
        if !capped.is_empty() {
            for index in &capped {
                let grant = inputs[*index].demand.saturating_sub(allocations[*index]);
                allocations[*index] = allocations[*index].saturating_add(grant);
                remaining = remaining.saturating_sub(grant);
            }
            active.retain(|index| !capped.contains(index));
            continue;
        }

        let mut granted = 0_u64;
        for index in &active {
            let grant = remaining.saturating_mul(u64::from(inputs[*index].weight)) / weight_sum;
            allocations[*index] = allocations[*index].saturating_add(grant);
            granted = granted.saturating_add(grant);
        }
        remaining = remaining.saturating_sub(granted);
        let mut ordered = active.clone();
        ordered.sort_by_key(|index| inputs[*index].sender);
        for index in ordered {
            if remaining == 0 {
                break;
            }
            if allocations[index] < inputs[index].demand {
                allocations[index] = allocations[index].saturating_add(1);
                remaining -= 1;
            }
        }
        break;
    }
    allocations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender(value: u16) -> SenderId {
        SenderId::new(value).expect("nonzero sender")
    }

    #[test]
    fn allocator_is_demand_capped_conserving_and_weighted() {
        let values = weighted_max_min(
            1_000,
            &[
                AllocationInput {
                    sender: sender(1),
                    demand: 100,
                    weight: 1,
                },
                AllocationInput {
                    sender: sender(2),
                    demand: 2_000,
                    weight: 1,
                },
                AllocationInput {
                    sender: sender(3),
                    demand: 2_000,
                    weight: 3,
                },
            ],
        );
        assert_eq!(values, [100, 225, 675]);
    }

    #[test]
    fn allocator_distributes_rounding_remainder_by_sender_id() {
        let values = weighted_max_min(
            2,
            &[
                AllocationInput {
                    sender: sender(2),
                    demand: 10,
                    weight: 1,
                },
                AllocationInput {
                    sender: sender(1),
                    demand: 10,
                    weight: 1,
                },
                AllocationInput {
                    sender: sender(3),
                    demand: 10,
                    weight: 1,
                },
            ],
        );
        assert_eq!(values, [1, 1, 0]);
    }
}
