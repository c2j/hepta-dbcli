use crate::synth::marginal::CategoricalParams;
use rand::Rng;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStrategy {
    Uniform,
    Zipf,
    Weighted,
}

pub struct FkPool {
    values: Vec<Value>,
    harmonic: f64,
    weights: Vec<f64>,
    weight_sum: f64,
    es_prepared: bool,
}

fn value_key(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

impl FkPool {
    pub fn new(values: Vec<Value>) -> Self {
        let harmonic: f64 = (1..=values.len()).map(|k| 1.0 / k as f64).sum();
        Self {
            values,
            harmonic,
            weights: Vec::new(),
            weight_sum: 0.0,
            es_prepared: false,
        }
    }

    /// Collapse `values` to unique entries weighted by observed frequency.
    /// When `model` is a categorical marginal, those training weights overlay
    /// the counts so unique parent keys still follow the observed share.
    pub fn from_observed_weights(values: Vec<Value>, model: Option<&CategoricalParams>) -> Self {
        let mut order: Vec<Value> = Vec::new();
        let mut counts: HashMap<String, f64> = HashMap::new();
        for v in values {
            let key = value_key(&v);
            if let Some(c) = counts.get_mut(&key) {
                *c += 1.0;
            } else {
                counts.insert(key, 1.0);
                order.push(v);
            }
        }
        let weights: Vec<f64> = order
            .iter()
            .map(|v| {
                let key = value_key(v);
                if let Some(p) = model {
                    if let Some(i) = p
                        .values
                        .iter()
                        .position(|s| s == &key || v.as_str() == Some(s.as_str()))
                    {
                        return p
                            .weights
                            .get(i)
                            .copied()
                            .filter(|w| *w > 0.0)
                            .unwrap_or(1.0);
                    }
                }
                counts.get(&key).copied().unwrap_or(1.0)
            })
            .collect();
        let weight_sum: f64 = weights.iter().sum();
        let harmonic: f64 = (1..=order.len()).map(|k| 1.0 / k as f64).sum();
        Self {
            values: order,
            harmonic,
            weights,
            weight_sum,
            es_prepared: false,
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn distinct_len(&self) -> usize {
        self.values
            .iter()
            .map(|v| v.to_string())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn sample_one(&self, strategy: SelectionStrategy, rng: &mut impl Rng) -> Option<Value> {
        if self.values.is_empty() {
            return None;
        }
        let idx = match strategy {
            SelectionStrategy::Uniform => rng.gen_range(0..self.values.len()),
            SelectionStrategy::Zipf => self.zipf_index(rng),
            SelectionStrategy::Weighted => self.weighted_index(rng),
        };
        self.values.get(idx).cloned()
    }

    pub fn sample_unique(
        &mut self,
        strategy: SelectionStrategy,
        rng: &mut impl Rng,
    ) -> Option<Value> {
        if self.values.is_empty() {
            return None;
        }
        match strategy {
            SelectionStrategy::Uniform => {
                let idx = rng.gen_range(0..self.values.len());
                Some(self.values.swap_remove(idx))
            }
            SelectionStrategy::Zipf | SelectionStrategy::Weighted => {
                if !self.es_prepared {
                    self.prepare_efraimidis_spirakis(strategy, rng);
                    self.es_prepared = true;
                }
                self.values.pop()
            }
        }
    }

    /// Efraimidis–Spirakis: key = u^(1/w) once, then draw in descending key
    /// order. O(n log n) prepare, O(1) per subsequent pop.
    fn prepare_efraimidis_spirakis(&mut self, strategy: SelectionStrategy, rng: &mut impl Rng) {
        let n = self.values.len();
        let mut keyed: Vec<(f64, usize)> = (0..n)
            .map(|i| {
                let u = rng.gen::<f64>().clamp(1e-12, 1.0);
                let w = match strategy {
                    SelectionStrategy::Zipf => 1.0 / (i + 1) as f64,
                    SelectionStrategy::Weighted => self
                        .weights
                        .get(i)
                        .copied()
                        .filter(|w| *w > 0.0)
                        .unwrap_or(1.0),
                    SelectionStrategy::Uniform => 1.0,
                };
                (u.powf(1.0 / w), i)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let old = std::mem::take(&mut self.values);
        self.values = keyed.into_iter().map(|(_, i)| old[i].clone()).collect();
    }

    // P(rank) ∝ 1/rank 的逆变换采样；walk 均摊 O(1)（Zipf 集中在头部）
    fn zipf_index(&self, rng: &mut impl Rng) -> usize {
        if self.harmonic <= 0.0 {
            return 0;
        }
        let u: f64 = rng.gen_range(0.0..self.harmonic);
        let mut acc = 0.0;
        for (i, k) in (1..=self.values.len()).enumerate() {
            acc += 1.0 / k as f64;
            if u < acc {
                return i;
            }
        }
        self.values.len() - 1
    }

    fn weighted_index(&self, rng: &mut impl Rng) -> usize {
        if self.weights.is_empty() || self.weight_sum <= 0.0 {
            return rng.gen_range(0..self.values.len());
        }
        let u: f64 = rng.gen_range(0.0..self.weight_sum);
        let mut acc = 0.0;
        for (i, &w) in self.weights.iter().enumerate() {
            acc += w;
            if u < acc {
                return i;
            }
        }
        self.values.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn pool(n: usize) -> FkPool {
        FkPool::new((0..n).map(|i| Value::from(i as f64)).collect())
    }

    #[test]
    fn empty_pool_returns_none() {
        let p = pool(0);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        assert!(p.sample_one(SelectionStrategy::Uniform, &mut rng).is_none());
        assert!(p.sample_one(SelectionStrategy::Zipf, &mut rng).is_none());
    }

    #[test]
    fn uniform_covers_most_of_pool() {
        let p = pool(50);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2000 {
            if let Some(v) = p.sample_one(SelectionStrategy::Uniform, &mut rng) {
                seen.insert(v.as_f64().unwrap() as usize);
            }
        }
        assert!(
            seen.len() >= 45,
            "uniform covered only {} of 50",
            seen.len()
        );
    }

    #[test]
    fn zipf_skews_toward_head() {
        let p = pool(100);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut head_hits = 0usize;
        let mut tail_hits = 0usize;
        for _ in 0..5000 {
            let v = p
                .sample_one(SelectionStrategy::Zipf, &mut rng)
                .unwrap()
                .as_f64()
                .unwrap() as usize;
            if v < 10 {
                head_hits += 1;
            } else if v >= 90 {
                tail_hits += 1;
            }
        }
        assert!(
            head_hits > tail_hits * 10,
            "head {} should dominate tail {} under Zipf",
            head_hits,
            tail_hits
        );
    }

    #[test]
    fn sample_one_returns_pool_values() {
        let p = pool(3);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..20 {
            let v = p.sample_one(SelectionStrategy::Uniform, &mut rng).unwrap();
            let f = v.as_f64().unwrap();
            assert!((0.0..3.0).contains(&f));
        }
        assert_eq!(p.len(), 3);
        assert!(!p.is_empty());
    }

    #[test]
    fn sample_unique_never_repeats_and_exhausts() {
        let mut p = pool(10);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10 {
            let v = p
                .sample_unique(SelectionStrategy::Uniform, &mut rng)
                .unwrap();
            assert!(
                seen.insert(v.as_f64().unwrap().to_bits()),
                "unique draw repeated"
            );
        }
        assert_eq!(seen.len(), 10);
        assert!(p
            .sample_unique(SelectionStrategy::Uniform, &mut rng)
            .is_none());
    }

    #[test]
    fn sample_unique_on_empty_returns_none() {
        let mut p = pool(0);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        assert!(p
            .sample_unique(SelectionStrategy::Uniform, &mut rng)
            .is_none());
    }

    #[test]
    fn should_sample_unique_zipf_without_replacement() {
        let mut p = pool(1000);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut seen = std::collections::HashSet::new();
        let mut drawn = Vec::new();
        for _ in 0..100 {
            let v = p.sample_unique(SelectionStrategy::Zipf, &mut rng).unwrap();
            let idx = v.as_f64().unwrap() as usize;
            assert!(seen.insert(idx), "unique+zipf repeated {}", idx);
            drawn.push(idx);
        }
        assert_eq!(seen.len(), 100);
        let head = drawn.iter().filter(|&&i| i < 100).count();
        let tail = drawn.iter().filter(|&&i| i >= 900).count();
        assert!(
            head > tail * 2,
            "unique+zipf should prefer the head of the pool (head {head} vs tail {tail})"
        );

        let mut big = pool(10_000);
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            big.sample_unique(SelectionStrategy::Zipf, &mut rng)
                .expect("10k unique zipf draws from a 10k pool");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "10k unique zipf draws took {:?}, expected < 1s",
            elapsed
        );
    }

    #[test]
    fn should_weight_fk_references_by_parent_frequency() {
        let mut values = Vec::new();
        values.extend(std::iter::repeat_n(Value::from("a"), 600));
        values.extend(std::iter::repeat_n(Value::from("b"), 300));
        values.extend(std::iter::repeat_n(Value::from("c"), 100));
        let p = FkPool::from_observed_weights(values, None);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut counts = std::collections::HashMap::new();
        for _ in 0..10_000 {
            let v = p.sample_one(SelectionStrategy::Weighted, &mut rng).unwrap();
            *counts.entry(v.as_str().unwrap().to_string()).or_insert(0) += 1;
        }
        let share = |k: &str| *counts.get(k).unwrap_or(&0) as f64 / 10_000.0;
        assert!(
            (share("a") - 0.6).abs() < 0.05,
            "a share {} not within 5pp of 0.6",
            share("a")
        );
        assert!(
            (share("b") - 0.3).abs() < 0.05,
            "b share {} not within 5pp of 0.3",
            share("b")
        );
        assert!(
            (share("c") - 0.1).abs() < 0.05,
            "c share {} not within 5pp of 0.1",
            share("c")
        );
    }
}
