use rand::Rng;
use rand::SeedableRng;
use rand_distr::StandardNormal;

pub struct GaussianCopula {
    dimension: usize,
    matrix: Vec<Vec<f64>>,
    cholesky: Vec<Vec<f64>>,
}

impl GaussianCopula {
    pub fn new(correlation: Vec<Vec<f64>>) -> Self {
        let dim = correlation.len();
        let matrix = ensure_psd(correlation);
        let cholesky = cholesky_decomposition(&matrix);

        Self {
            dimension: dim,
            matrix,
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

    /// Sample uniforms (Φ(z)) while pinning the listed dimensions to exact
    /// z-scores. The remaining dimensions are drawn from the conditional
    /// multivariate normal `z_free | z_fixed = a`, so the learned correlation
    /// structure is preserved instead of dropped (issue #68
    /// `copula_conditional` mode).
    ///
    /// Returns one row per dimension, matching [`Self::sample`].
    pub fn sample_with_fixed_z(
        &self,
        n: usize,
        fixed: &[(usize, f64)],
        seed: Option<u64>,
    ) -> Result<Vec<Vec<f64>>, String> {
        let mut pinned: Vec<Option<f64>> = vec![None; self.dimension];
        for (index, z) in fixed {
            if *index >= self.dimension {
                return Err(format!(
                    "fixed dimension {} is out of range for a {}-dimensional copula",
                    index, self.dimension
                ));
            }
            if pinned[*index].is_some() {
                return Err(format!("duplicate fixed dimension {}", index));
            }
            if !z.is_finite() {
                return Err(format!("fixed dimension {} has a non-finite z", index));
            }
            pinned[*index] = Some(*z);
        }

        let free: Vec<usize> = (0..self.dimension)
            .filter(|index| pinned[*index].is_none())
            .collect();

        // No free dimension: nothing left to sample, so no RNG is consumed.
        if free.is_empty() {
            let mut out = Vec::with_capacity(self.dimension);
            for z in &pinned {
                match z {
                    Some(z) => out.push(vec![normal_cdf(*z); n]),
                    None => {
                        return Err(
                            "internal error: a dimension is unpinned but nothing is free"
                                .to_string(),
                        )
                    }
                }
            }
            return Ok(out);
        }

        let (mean, covariance) = self.conditional_moments(&pinned, &free)?;
        let chol = cholesky_decomposition(&covariance);

        let mut rng = if let Some(s) = seed {
            rand::rngs::StdRng::seed_from_u64(s)
        } else {
            rand::rngs::StdRng::from_entropy()
        };

        let mut independent = Vec::with_capacity(free.len());
        for _ in 0..free.len() {
            let col: Vec<f64> = (0..n).map(|_| rng.sample(StandardNormal)).collect();
            independent.push(col);
        }

        let mut out = vec![vec![0.0; n]; self.dimension];
        for (dim, z) in pinned.iter().enumerate() {
            if let Some(z) = z {
                out[dim] = vec![normal_cdf(*z); n];
            }
        }

        for t in 0..n {
            for (i, free_dim) in free.iter().enumerate() {
                let mut sum = mean[i];
                for (j, ind_col) in independent.iter().enumerate().take(i + 1) {
                    sum += chol[i][j] * ind_col[t];
                }
                out[*free_dim][t] = normal_cdf(sum);
            }
        }

        Ok(out)
    }

    /// Convenience wrapper over [`Self::sample_with_fixed_z`]: pin dimensions
    /// to uniform values, converting each through the inverse normal CDF.
    /// The fixed dimension's returned value is the round trip Φ(Φ⁻¹(u)), which
    /// matches `u` to roughly `1e-6`; callers that need the literal value must
    /// still write it (the fixed column is written by the column rule).
    pub fn sample_with_fixed_uniforms(
        &self,
        n: usize,
        fixed: &[(usize, f64)],
        seed: Option<u64>,
    ) -> Result<Vec<Vec<f64>>, String> {
        let mut zs = Vec::with_capacity(fixed.len());
        for (index, u) in fixed {
            if !(*u > 0.0 && *u < 1.0) {
                return Err(format!(
                    "fixed uniform for dimension {} must be in (0, 1), got {}",
                    index, u
                ));
            }
            zs.push((*index, normal_quantile(*u)));
        }
        self.sample_with_fixed_z(n, &zs, seed)
    }

    /// `z_free | z_fixed = a` has mean `Σ_BA Σ_AA⁻¹ a` and covariance
    /// `Σ_BB − Σ_BA Σ_AA⁻¹ Σ_AB`.
    fn conditional_moments(
        &self,
        pinned: &[Option<f64>],
        free: &[usize],
    ) -> Result<(Vec<f64>, Vec<Vec<f64>>), String> {
        let fixed: Vec<(usize, f64)> = pinned
            .iter()
            .enumerate()
            .filter_map(|(index, z)| z.map(|z| (index, z)))
            .collect();

        let mut sigma_aa = vec![vec![0.0; fixed.len()]; fixed.len()];
        for (i, (row, _)) in fixed.iter().enumerate() {
            for (j, (col, _)) in fixed.iter().enumerate() {
                sigma_aa[i][j] = self.matrix[*row][*col];
            }
        }
        let inv_aa = invert(&sigma_aa).ok_or_else(|| {
            "fixed dimensions are perfectly correlated; the conditional distribution is undefined"
                .to_string()
        })?;

        let a: Vec<f64> = fixed.iter().map(|(_, z)| *z).collect();

        // Σ_BA Σ_AA⁻¹ is (free x fixed).
        let mut ba_inv = vec![vec![0.0; fixed.len()]; free.len()];
        for (i, row) in free.iter().enumerate() {
            for j in 0..fixed.len() {
                let mut sum = 0.0;
                for k in 0..fixed.len() {
                    sum += self.matrix[*row][fixed[k].0] * inv_aa[k][j];
                }
                ba_inv[i][j] = sum;
            }
        }

        let mean: Vec<f64> = ba_inv
            .iter()
            .map(|row| row.iter().zip(a.iter()).map(|(x, z)| x * z).sum())
            .collect();

        // Σ_BB − (Σ_BA Σ_AA⁻¹) Σ_AB
        let mut covariance = vec![vec![0.0; free.len()]; free.len()];
        for i in 0..free.len() {
            for j in 0..free.len() {
                let mut sum = 0.0;
                for k in 0..fixed.len() {
                    sum += ba_inv[i][k] * self.matrix[fixed[k].0][free[j]];
                }
                covariance[i][j] = self.matrix[free[i]][free[j]] - sum;
            }
        }

        Ok((mean, covariance))
    }
}

/// Gauss-Jordan inverse with partial pivoting; `None` when the matrix is
/// singular within numerical tolerance.
fn invert(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = matrix.len();
    if n == 0 {
        return Some(Vec::new());
    }
    let mut work: Vec<Vec<f64>> = matrix.to_vec();
    let mut inverse = vec![vec![0.0; n]; n];
    for (i, row) in inverse.iter_mut().enumerate() {
        row[i] = 1.0;
    }

    for col in 0..n {
        let pivot = (col..n).max_by(|a, b| work[*a][col].abs().total_cmp(&work[*b][col].abs()))?;
        if work[pivot][col].abs() < 1e-12 {
            return None;
        }
        work.swap(col, pivot);
        inverse.swap(col, pivot);

        let divisor = work[col][col];
        for value in work[col].iter_mut() {
            *value /= divisor;
        }
        for value in inverse[col].iter_mut() {
            *value /= divisor;
        }

        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = work[row][col];
            if factor == 0.0 {
                continue;
            }
            for k in 0..n {
                work[row][k] -= factor * work[col][k];
                inverse[row][k] -= factor * inverse[col][k];
            }
        }
    }

    Some(inverse)
}

/// Inverse standard normal CDF (Acklam's rational approximation with a Halley
/// refinement step). Used to turn a pinned uniform into a z-score.
pub(crate) fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383_577_518_672_69e2,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    const LOW: f64 = 0.02425;

    let p = p.clamp(f64::MIN_POSITIVE, 1.0 - f64::EPSILON);

    let approx = if p < LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p > 1.0 - LOW {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    };

    // One Newton step against our own `normal_cdf` closes the remaining gap.
    let error = normal_cdf(approx) - p;
    approx - error / (0.3989422804014327 * (-0.5 * approx * approx).exp())
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

    // ─── 条件采样（#68 copula_conditional 的数学部分）─────────────────────

    #[test]
    fn should_pin_fixed_dimension_to_requested_z() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.5], vec![0.5, 1.0]]);
        let samples = copula
            .sample_with_fixed_z(8, &[(0, 0.5)], Some(42))
            .unwrap();

        let expected = normal_cdf(0.5);
        assert_eq!(samples.len(), 2);
        for value in &samples[0] {
            assert_eq!(*value, expected);
        }
    }

    #[test]
    fn should_condition_free_dimension_on_fixed_z() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.8], vec![0.8, 1.0]]);
        let samples = copula
            .sample_with_fixed_z(20000, &[(0, 1.0)], Some(7))
            .unwrap();

        // z_free | z_fixed = 1  ~  N(0.8, 1 - 0.8^2)
        let z: Vec<f64> = samples[1].iter().map(|u| normal_quantile(*u)).collect();
        let mean = z.iter().sum::<f64>() / z.len() as f64;
        assert!((mean - 0.8).abs() < 0.03, "conditional mean was {mean}");
    }

    #[test]
    fn should_reject_fixed_dimension_out_of_range() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        let error = copula
            .sample_with_fixed_z(4, &[(5, 0.0)], Some(1))
            .unwrap_err();
        assert!(error.contains('5'), "error was {error}");
    }

    #[test]
    fn should_reject_duplicate_fixed_dimensions() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        let error = copula
            .sample_with_fixed_z(4, &[(0, 0.0), (0, 1.0)], Some(1))
            .unwrap_err();
        assert!(error.contains("duplicate"), "error was {error}");
    }

    #[test]
    fn should_reject_fixed_uniform_outside_open_unit_interval() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert!(copula
            .sample_with_fixed_uniforms(4, &[(0, 1.0)], Some(1))
            .is_err());
        assert!(copula
            .sample_with_fixed_uniforms(4, &[(0, 0.0)], Some(1))
            .is_err());
    }

    #[test]
    fn should_round_trip_fixed_uniform_through_inverse_normal() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.3], vec![0.3, 1.0]]);
        let samples = copula
            .sample_with_fixed_uniforms(4, &[(1, 0.975)], Some(3))
            .unwrap();
        for value in &samples[1] {
            assert!(
                (value - 0.975).abs() < 1e-6,
                "fixed uniform drifted: {value}"
            );
        }
    }

    #[test]
    fn should_reproduce_conditional_samples_for_same_seed() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.4], vec![0.4, 1.0]]);
        let first = copula
            .sample_with_fixed_z(16, &[(0, -0.5)], Some(99))
            .unwrap();
        let second = copula
            .sample_with_fixed_z(16, &[(0, -0.5)], Some(99))
            .unwrap();
        assert_eq!(first, second);
    }
}
