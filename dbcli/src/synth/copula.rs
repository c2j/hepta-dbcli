use rand::Rng;
use rand::SeedableRng;
use rand_distr::StandardNormal;

pub struct GaussianCopula {
    dimension: usize,
    cholesky: Vec<Vec<f64>>,
}

impl GaussianCopula {
    pub fn new(correlation: Vec<Vec<f64>>) -> Self {
        let dim = correlation.len();
        let matrix = ensure_psd(correlation);
        let cholesky = cholesky_decomposition(&matrix);

        Self {
            dimension: dim,
            cholesky,
        }
    }

    pub fn sample(&self, n: usize, seed: Option<u64>) -> Vec<Vec<f64>> {
        let mut rng = if let Some(s) = seed {
            rand::rngs::StdRng::seed_from_u64(s)
        } else {
            rand::rngs::StdRng::from_entropy()
        };

        let mut independent = Vec::with_capacity(self.dimension);
        for _ in 0..self.dimension {
            let col: Vec<f64> = (0..n).map(|_| rng.sample(StandardNormal)).collect();
            independent.push(col);
        }

        let mut correlated = vec![vec![0.0; n]; self.dimension];

        for t in 0..n {
            for (i, corr_row) in correlated.iter_mut().enumerate().take(self.dimension) {
                let mut sum = 0.0;
                for (j, ind_col) in independent.iter().enumerate().take(i + 1) {
                    sum += self.cholesky[i][j] * ind_col[t];
                }
                corr_row[t] = sum;
            }
        }

        correlated
            .iter()
            .map(|col| col.iter().map(|&x| normal_cdf(x)).collect())
            .collect()
    }
}

#[allow(clippy::needless_range_loop)]
fn ensure_psd(mut matrix: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    let dim = matrix.len();

    for i in 0..dim {
        for j in (i + 1)..dim {
            let avg = (matrix[i][j] + matrix[j][i]) / 2.0;
            matrix[i][j] = avg;
            matrix[j][i] = avg;
        }
    }

    for (i, row) in matrix.iter_mut().enumerate() {
        let row_sum: f64 = row
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, &val)| val.abs())
            .sum();
        if row[i] < row_sum {
            row[i] = row_sum + 0.001;
        }
    }

    matrix
}

#[allow(clippy::needless_range_loop)]
fn cholesky_decomposition(matrix: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = matrix.len();
    let mut l = vec![vec![0.0; n]; n];

    for i in 0..n {
        for j in 0..=i {
            let mut sum = 0.0;
            for k in 0..j {
                sum += l[i][k] * l[j][k];
            }
            if i == j {
                let diff = matrix[i][i] - sum;
                l[i][j] = if diff > 0.0 { diff.sqrt() } else { 0.001 };
            } else {
                l[i][j] = (matrix[i][j] - sum) / l[j][j];
            }
        }
    }

    l
}

fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

fn erf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

fn pearson_correlation(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean_x = x.iter().sum::<f64>() / n;
    let mean_y = y.iter().sum::<f64>() / n;

    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;

    for (xi, yi) in x.iter().zip(y.iter()) {
        let dx = xi - mean_x;
        let dy = yi - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }

    cov / (var_x * var_y).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copula_generates_correct_dimensions() {
        let correlation = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let copula = GaussianCopula::new(correlation);
        let samples = copula.sample(100, Some(42));
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].len(), 100);
    }

    #[test]
    fn copula_handles_non_psd_matrix() {
        let correlation = vec![vec![1.0, 0.9], vec![0.9, 0.5]];
        let _copula = GaussianCopula::new(correlation);
    }

    #[test]
    fn copula_sampling_preserves_correlation() {
        let correlation = vec![vec![1.0, 0.8], vec![0.8, 1.0]];
        let copula = GaussianCopula::new(correlation);
        let samples = copula.sample(10000, Some(42));

        let corr = pearson_correlation(&samples[0], &samples[1]);
        assert!((corr - 0.8).abs() < 0.2);
    }

    #[test]
    fn cholesky_produces_lower_triangular() {
        let matrix = vec![vec![4.0, 2.0], vec![2.0, 3.0]];
        let l = cholesky_decomposition(&matrix);
        assert!((l[0][1]).abs() < 1e-10);
    }
}
