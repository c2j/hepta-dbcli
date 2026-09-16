//! Child-cardinality learning and sampling (issue #72, HMA-lite).
//!
//! A relationship's "cardinality" is how many child rows reference one parent
//! key. Training on a real table usually shows a long tail (many parents with
//! zero children), while the plain generator produces a fixed child row count
//! with every parent key equally drawable. This module learns the count
//! distribution so `cardinality: modeled` can reproduce it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Counts above this are merged into this bucket: a distribution whose tail
/// runs to hundreds of rows per parent is not usefully distinguished, and the
/// bucket keeps the stored model small.
pub const MAX_COUNT_BUCKET: u64 = 50;

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

/// Distribution of "child rows per parent key".
///
/// `counts` maps a child-row count to the share of parent keys that have that
/// many children (the `0` bucket is included when the parent key count is
/// known). `null_share` is the share of child rows whose FK is NULL and so
/// reference no parent; it is kept apart because NULL rows are not part of any
/// parent's cardinality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardinalityDist {
    pub counts: BTreeMap<u64, f64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub null_share: f64,
}

impl CardinalityDist {
    /// Build from per-parent child counts over `parent_universe` parent keys
    /// (counts above [`MAX_COUNT_BUCKET`] are merged into it; parents with no
    /// children form the `0` bucket). `null_share` is the share of child rows
    /// whose FK is NULL.
    pub fn from_counts(
        per_parent_counts: impl IntoIterator<Item = u64>,
        parent_universe: usize,
        null_share: f64,
    ) -> Option<CardinalityDist> {
        let mut histogram: BTreeMap<u64, f64> = BTreeMap::new();
        let mut referenced = 0usize;
        for count in per_parent_counts {
            referenced += 1;
            *histogram.entry(count.min(MAX_COUNT_BUCKET)).or_insert(0.0) += 1.0;
        }
        if referenced == 0 {
            return None;
        }
        let zero_parents = parent_universe.saturating_sub(referenced);
        let total = (referenced + zero_parents) as f64;
        if total <= 0.0 {
            return None;
        }
        let mut counts: BTreeMap<u64, f64> = BTreeMap::new();
        if zero_parents > 0 {
            counts.insert(0, zero_parents as f64 / total);
        }
        for (bucket, parents) in histogram {
            *counts.entry(bucket).or_insert(0.0) += parents / total;
        }
        Some(CardinalityDist { counts, null_share })
    }

    /// Draw a child count from the distribution using `uniform` in `[0, 1)`.
    pub fn sample_count(&self, uniform: f64) -> u64 {
        let total: f64 = self.counts.values().sum();
        if total <= 0.0 {
            return 0;
        }
        let mut threshold = uniform.clamp(0.0, 1.0 - f64::EPSILON) * total;
        for (count, share) in &self.counts {
            threshold -= share;
            if threshold < 0.0 {
                return *count;
            }
        }
        self.counts.keys().next_back().copied().unwrap_or(0)
    }

    /// Total-variation distance between the two **count** distributions, over
    /// the union of their buckets. Both bucket vectors are probabilities over
    /// the same parent-key universe, so the result is in `[0, 1]`.
    ///
    /// `null_share` is a row-level fraction, not a parent-level probability,
    /// so mixing it in would break the simplex; compare it separately.
    pub fn total_variation(&self, other: &CardinalityDist) -> f64 {
        let mut buckets: std::collections::BTreeSet<u64> = self.counts.keys().copied().collect();
        buckets.extend(other.counts.keys().copied());
        let mut l1 = 0.0;
        for bucket in buckets {
            let left = self.counts.get(&bucket).copied().unwrap_or(0.0);
            let right = other.counts.get(&bucket).copied().unwrap_or(0.0);
            l1 += (left - right).abs();
        }
        (l1 / 2.0).clamp(0.0, 1.0)
    }
}

/// Learn the child-row count distribution for one FK column.
///
/// `child_fk_values` are the FK values of the sampled child rows (NULL
/// allowed). `parent_distinct` is the number of distinct parent keys, when the
/// parent table was also sampled: it supplies the `0` bucket (parents nothing
/// references). Returns `None` when no child row references any parent.
pub fn learn_cardinality(
    child_fk_values: &[Value],
    parent_distinct: Option<usize>,
) -> Option<CardinalityDist> {
    if child_fk_values.is_empty() {
        return None;
    }

    let mut per_parent: BTreeMap<String, u64> = BTreeMap::new();
    let mut null_rows = 0u64;
    for value in child_fk_values {
        if value.is_null() {
            null_rows += 1;
            continue;
        }
        *per_parent.entry(value.to_string()).or_insert(0) += 1;
    }
    if per_parent.is_empty() {
        return None;
    }

    let referenced = per_parent.len();
    // A parent key universe smaller than the referenced set would be a caller
    // bug; clamp so the `0` bucket can never go negative.
    let universe = parent_distinct.unwrap_or(referenced).max(referenced);
    let total_rows = child_fk_values.len() as f64;
    CardinalityDist::from_counts(
        per_parent.values().copied(),
        universe,
        null_rows as f64 / total_rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(values: &[i64]) -> Vec<Value> {
        values.iter().map(|v| Value::from(*v)).collect()
    }

    #[test]
    fn should_learn_counts_and_zero_share_from_parent_distinct() {
        // 4 child rows over 3 distinct parents: one parent twice, two once.
        // Parent has 6 distinct keys, so 3 parents have zero children.
        let dist = learn_cardinality(&ints(&[1, 1, 2, 3]), Some(6)).unwrap();

        assert!((dist.counts[&0] - 3.0 / 6.0).abs() < 1e-9);
        assert!((dist.counts[&1] - 2.0 / 6.0).abs() < 1e-9);
        assert!((dist.counts[&2] - 1.0 / 6.0).abs() < 1e-9);
        assert_eq!(dist.null_share, 0.0);
    }

    #[test]
    fn should_keep_null_share_out_of_the_count_buckets() {
        let values = vec![Value::Null, Value::from(1), Value::from(1)];
        let dist = learn_cardinality(&values, Some(4)).unwrap();

        assert!((dist.null_share - 1.0 / 3.0).abs() < 1e-9);
        // 1 referenced parent, 3 zero parents, total 4.
        assert!((dist.counts[&0] - 3.0 / 4.0).abs() < 1e-9);
        assert!((dist.counts[&2] - 1.0 / 4.0).abs() < 1e-9);
    }

    #[test]
    fn should_omit_the_zero_bucket_when_parent_distinct_is_unknown() {
        let dist = learn_cardinality(&ints(&[1, 1, 1]), None).unwrap();
        assert_eq!(dist.counts.len(), 1);
        assert!((dist.counts[&3] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn should_merge_the_tail_into_the_max_bucket() {
        // One parent referenced 60 times: its count is clamped to the max
        // bucket so the tail cannot bloat the stored distribution.
        let values = ints(&vec![1; MAX_COUNT_BUCKET as usize + 10]);
        let dist = learn_cardinality(&values, None).unwrap();
        assert_eq!(dist.counts.len(), 1);
        assert!((dist.counts[&MAX_COUNT_BUCKET] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn should_return_none_when_no_parent_is_referenced() {
        assert!(learn_cardinality(&[Value::Null, Value::Null], Some(5)).is_none());
        assert!(learn_cardinality(&[], Some(5)).is_none());
    }

    #[test]
    fn should_sample_counts_deterministically_within_the_support() {
        let dist = CardinalityDist {
            counts: BTreeMap::from([(0, 0.5), (1, 0.3), (2, 0.2)]),
            null_share: 0.0,
        };
        assert_eq!(dist.sample_count(0.0), 0);
        assert_eq!(dist.sample_count(0.6), 1);
        assert_eq!(dist.sample_count(0.9), 2);
    }

    #[test]
    fn should_measure_total_variation_over_the_union_of_buckets() {
        let left = CardinalityDist {
            counts: BTreeMap::from([(0, 0.5), (1, 0.5)]),
            null_share: 0.0,
        };
        let right = CardinalityDist {
            counts: BTreeMap::from([(0, 0.5), (2, 0.5)]),
            null_share: 0.0,
        };
        assert!((left.total_variation(&right) - 0.5).abs() < 1e-9);
        assert_eq!(left.total_variation(&left), 0.0);
    }
    #[test]
    fn should_build_a_distribution_from_parent_counts() {
        // 3 referenced parents (counts 1, 1, 2) over a 6-key universe.
        let dist = CardinalityDist::from_counts([1u64, 1, 2], 6, 0.25).unwrap();
        assert!((dist.counts[&0] - 3.0 / 6.0).abs() < 1e-9);
        assert!((dist.counts[&1] - 2.0 / 6.0).abs() < 1e-9);
        assert!((dist.counts[&2] - 1.0 / 6.0).abs() < 1e-9);
        assert_eq!(dist.null_share, 0.25);
        assert!(CardinalityDist::from_counts([], 5, 0.0).is_none());
    }

    #[test]
    fn should_keep_total_variation_within_the_count_simplex() {
        let all_zero = CardinalityDist {
            counts: BTreeMap::from([(0, 1.0)]),
            null_share: 1.0,
        };
        let all_five = CardinalityDist {
            counts: BTreeMap::from([(5, 1.0)]),
            null_share: 0.0,
        };
        let tv = all_zero.total_variation(&all_five);
        assert!(tv <= 1.0, "tv {tv} must stay in [0, 1]");
        assert!((tv - 1.0).abs() < 1e-9);

        // A NULL-share difference alone is not a count-distribution shift.
        let no_nulls = CardinalityDist {
            counts: BTreeMap::from([(0, 1.0)]),
            null_share: 0.0,
        };
        assert_eq!(all_zero.total_variation(&no_nulls), 0.0);
    }
}
