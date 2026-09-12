use rand::Rng;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStrategy {
    Uniform,
    Zipf,
}

pub struct FkPool {
    values: Vec<Value>,
    harmonic: f64,
}

impl FkPool {
    pub fn new(values: Vec<Value>) -> Self {
        let harmonic: f64 = (1..=values.len()).map(|k| 1.0 / k as f64).sum();
        Self { values, harmonic }
    }

    pub fn len(&self) -> usize {
        self.values.len()
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
        };
        self.values.get(idx).cloned()
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
}
