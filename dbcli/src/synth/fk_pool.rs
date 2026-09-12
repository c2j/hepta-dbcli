use rand::Rng;

pub enum FkPool {
    Projection(Vec<String>),
    Generated(Vec<String>),
}

pub enum FanoutStrategy {
    Fixed(usize),
    Uniform(usize, usize),
    Zipf(usize),
}

impl FkPool {
    pub fn sample(&self, strategy: &FanoutStrategy, rng: &mut impl Rng) -> Vec<String> {
        match self {
            FkPool::Projection(values) => {
                if values.is_empty() {
                    return vec![];
                }
                match strategy {
                    FanoutStrategy::Fixed(n) => {
                        let mut result = Vec::new();
                        for _ in 0..*n {
                            let idx = rng.gen_range(0..values.len());
                            result.push(values[idx].clone());
                        }
                        result
                    }
                    FanoutStrategy::Uniform(min, max) => {
                        let n = rng.gen_range(*min..=*max);
                        let mut result = Vec::new();
                        for _ in 0..n {
                            let idx = rng.gen_range(0..values.len());
                            result.push(values[idx].clone());
                        }
                        result
                    }
                    FanoutStrategy::Zipf(_n) => {
                        vec![values[0].clone()]
                    }
                }
            }
            FkPool::Generated(values) => {
                if values.is_empty() {
                    return vec![];
                }
                match strategy {
                    FanoutStrategy::Fixed(n) => {
                        let mut result = Vec::new();
                        for _ in 0..*n {
                            let idx = rng.gen_range(0..values.len());
                            result.push(values[idx].clone());
                        }
                        result
                    }
                    FanoutStrategy::Uniform(min, max) => {
                        let n = rng.gen_range(*min..=*max);
                        let mut result = Vec::new();
                        for _ in 0..n {
                            let idx = rng.gen_range(0..values.len());
                            result.push(values[idx].clone());
                        }
                        result
                    }
                    FanoutStrategy::Zipf(_n) => {
                        vec![values[0].clone()]
                    }
                }
            }
        }
    }

    pub fn new_projection(values: Vec<String>) -> Self {
        Self::Projection(values)
    }

    pub fn new_generated(values: Vec<String>) -> Self {
        Self::Generated(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn projection_pool_samples_correct_count() {
        let pool = FkPool::new_projection(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let result = pool.sample(&FanoutStrategy::Fixed(5), &mut rng);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn projection_pool_empty_returns_empty() {
        let pool = FkPool::new_projection(vec![]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let result = pool.sample(&FanoutStrategy::Fixed(5), &mut rng);
        assert!(result.is_empty());
    }

    #[test]
    fn uniform_fanout_generates_within_range() {
        let pool = FkPool::new_projection(vec!["x".to_string()]);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut min_seen = usize::MAX;
        let mut max_seen = usize::MIN;

        for _ in 0..100 {
            let result = pool.sample(&FanoutStrategy::Uniform(3, 7), &mut rng);
            min_seen = min_seen.min(result.len());
            max_seen = max_seen.max(result.len());
        }

        assert!(min_seen >= 3);
        assert!(max_seen <= 7);
    }
}
