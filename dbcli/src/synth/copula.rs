use rand::Rng;
use rand::SeedableRng;
use rand_distr::StandardNormal;

/// `(free dimensions, Σ_BA Σ_AA⁻¹, Σ_BB − Σ_BA Σ_AA⁻¹ Σ_AB)`.
type ConditionalSetup = (Vec<usize>, Vec<Vec<f64>>, Vec<Vec<f64>>);

pub struct GaussianCopula {
    dimension: usize,
    matrix: Vec<Vec<f64>>,
    cholesky: Vec<Vec<f64>>,
}

impl GaussianCopula {
    pub fn new(correlation: Vec<Vec<f64>>) -> Result<Self, String> {
        let dim = correlation.len();
        let matrix = ensure_psd(correlation)?;
        let cholesky = cholesky_decomposition(&matrix);

        Ok(Self {
            dimension: dim,
            matrix,
            cholesky,
        })
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
        // One pinned z for every row: broadcast and reuse the per-row entry
        // point so both APIs share validation and error messages.
        let rows: Vec<(usize, Vec<f64>)> = fixed
            .iter()
            .map(|(index, z)| (*index, vec![*z; n]))
            .collect();
        self.sample_with_fixed_z_rows(n, &rows, seed)
    }

    /// Per-row variant of [`Self::sample_with_fixed_uniforms`]: each row of a
    /// pinned dimension gets its own uniform, converted through the inverse
    /// normal CDF. This is the entry point for `fixed_range` with
    /// `mode: copula_conditional`, where the pinned column draws a different
    /// quantile inside the range for every row.
    pub fn sample_with_fixed_uniform_rows(
        &self,
        n: usize,
        fixed: &[(usize, Vec<f64>)],
        seed: Option<u64>,
    ) -> Result<Vec<Vec<f64>>, String> {
        let mut zs = Vec::with_capacity(fixed.len());
        for (index, us) in fixed {
            let zs_for_dim: Vec<f64> = us
                .iter()
                .map(|u| {
                    if *u > 0.0 && *u < 1.0 {
                        Ok(normal_quantile(*u))
                    } else {
                        Err(format!(
                            "fixed uniform for dimension {} must be in (0, 1), got {}",
                            index, u
                        ))
                    }
                })
                .collect::<Result<_, String>>()?;
            zs.push((*index, zs_for_dim));
        }
        self.sample_with_fixed_z_rows(n, &zs, seed)
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
    /// `Σ_BB − Σ_BA Σ_AA⁻¹ Σ_AB`. Returns the free dimensions plus the
    /// constant pieces `(Σ_BA Σ_AA⁻¹, Σ_BB − Σ_BA Σ_AA⁻¹ Σ_AB)`, so a caller
    /// with a different `a` per row only recomputes the (linear) mean.
    fn conditional_setup(&self, fixed: &[usize]) -> Result<ConditionalSetup, String> {
        let pinned: std::collections::BTreeSet<usize> = fixed.iter().copied().collect();
        let free: Vec<usize> = (0..self.dimension)
            .filter(|index| !pinned.contains(index))
            .collect();

        let mut sigma_aa = vec![vec![0.0; fixed.len()]; fixed.len()];
        for (i, row) in fixed.iter().enumerate() {
            for (j, col) in fixed.iter().enumerate() {
                sigma_aa[i][j] = self.matrix[*row][*col];
            }
        }
        let inv_aa = invert(&sigma_aa).ok_or_else(|| {
            "fixed dimensions are perfectly correlated; the conditional distribution is undefined"
                .to_string()
        })?;

        // Σ_BA Σ_AA⁻¹ is (free x fixed).
        let mut ba_inv = vec![vec![0.0; fixed.len()]; free.len()];
        for (i, row) in free.iter().enumerate() {
            for (j, inv_row) in inv_aa.iter().enumerate() {
                let sum: f64 = fixed
                    .iter()
                    .zip(inv_row.iter())
                    .map(|(k, coeff)| self.matrix[*row][*k] * coeff)
                    .sum();
                ba_inv[i][j] = sum;
            }
        }

        // Σ_BB − (Σ_BA Σ_AA⁻¹) Σ_AB
        let mut covariance = vec![vec![0.0; free.len()]; free.len()];
        for (i, free_row) in free.iter().enumerate() {
            for (j, free_col) in free.iter().enumerate() {
                let correction: f64 = fixed
                    .iter()
                    .zip(ba_inv[i].iter())
                    .map(|(k, coeff)| coeff * self.matrix[*k][*free_col])
                    .sum();
                covariance[i][j] = self.matrix[*free_row][*free_col] - correction;
            }
        }

        Ok((free, ba_inv, covariance))
    }

    /// Sample uniforms while pinning **each row separately** to its own
    /// z-score. This is what `fixed_range` with `mode: copula_conditional`
    /// needs: the pinned column gets a different quantile per row, and the
    /// other columns follow the conditional distribution of that row.
    pub fn sample_with_fixed_z_rows(
        &self,
        n: usize,
        fixed: &[(usize, Vec<f64>)],
        seed: Option<u64>,
    ) -> Result<Vec<Vec<f64>>, String> {
        let mut dims: Vec<usize> = Vec::with_capacity(fixed.len());
        for (index, zs) in fixed {
            if *index >= self.dimension {
                return Err(format!(
                    "fixed dimension {} is out of range for a {}-dimensional copula",
                    index, self.dimension
                ));
            }
            if dims.contains(index) {
                return Err(format!("duplicate fixed dimension {}", index));
            }
            if zs.len() != n {
                return Err(format!(
                    "fixed dimension {} has {} z values but the table has {} rows",
                    index,
                    zs.len(),
                    n
                ));
            }
            if zs.iter().any(|z| !z.is_finite()) {
                return Err(format!("fixed dimension {} has a non-finite z", index));
            }
            dims.push(*index);
        }
        dims.sort_unstable();

        let (free, ba_inv, covariance) = self.conditional_setup(&dims)?;

        // Fixed dimensions come back as their exact Φ(z), row by row.
        let mut out = vec![vec![0.0; n]; self.dimension];
        for (index, zs) in fixed {
            out[*index] = zs.iter().map(|z| normal_cdf(*z)).collect();
        }

        if free.is_empty() {
            return Ok(out);
        }

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

        // `dims` is sorted, so `ba_inv` columns line up with `fixed` looked up
        // by dimension rather than by the caller's argument order.
        let zs_by_dim: Vec<&Vec<f64>> = dims
            .iter()
            .map(|dim| {
                fixed
                    .iter()
                    .find(|(index, _)| index == dim)
                    .map(|(_, zs)| zs)
                    .ok_or_else(|| format!("missing pinned values for dimension {}", dim))
            })
            .collect::<Result<_, String>>()?;

        for t in 0..n {
            for (i, free_dim) in free.iter().enumerate() {
                let mean: f64 = ba_inv[i]
                    .iter()
                    .zip(zs_by_dim.iter())
                    .map(|(coeff, zs)| coeff * zs[t])
                    .sum();
                let mut z = mean;
                for (j, ind_col) in independent.iter().enumerate().take(i + 1) {
                    z += chol[i][j] * ind_col[t];
                }
                out[*free_dim][t] = normal_cdf(z);
            }
        }

        Ok(out)
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
/// Public PSD projection for callers that build correlation matrices outside
/// this module (e.g. `marginal::compute_gaussian_correlation`). Symmetric
/// input is projected as-is; unit diagonals come back unit.
pub(crate) fn project_to_correlation(matrix: Vec<Vec<f64>>) -> Result<Vec<Vec<f64>>, String> {
    ensure_psd(matrix)
}

/// Nearest correlation matrix in the eigenvalue sense: symmetrize, clip
/// negative eigenvalues to a small positive floor, rescale back to a unit
/// diagonal. Repeats while negative eigenvalues remain because the diagonal
/// rescale can reintroduce small negative ones.
///
/// The old implementation forced diagonal dominance: any diagonal smaller
/// than its row's off-diagonal absolute sum was raised to that sum plus
/// eps. On strongly correlated blocks that inflates the diagonal several
/// fold (the M1 fixture stored a diagonal of 4.065 for ρ = 1.0 columns),
/// and the copula then shrank every generated correlation by roughly the
/// same factor, ρ = 1.0 sampling near 0.25 (issue #89 S2a).
fn ensure_psd(mut matrix: Vec<Vec<f64>>) -> Result<Vec<Vec<f64>>, String> {
    let dim = matrix.len();

    // Symmetrize via an upper-triangle snapshot so the write to row j does
    // not alias the read from row i.
    let mut upper: Vec<(usize, usize, f64)> = Vec::new();
    for (i, row_i) in matrix.iter().enumerate() {
        for (j, value) in row_i.iter().enumerate().skip(i + 1) {
            upper.push((i, j, *value));
        }
    }
    for (i, j, value) in upper {
        let avg = (value + matrix[j][i]) / 2.0;
        matrix[i][j] = avg;
        matrix[j][i] = avg;
    }

    for _ in 0..PSD_PROJECTION_ROUNDS {
        let (eigenvalues, eigenvectors) = jacobi_eigen_decomposition(&matrix)?;
        let min_eigenvalue = eigenvalues.iter().copied().fold(f64::INFINITY, f64::min);
        if min_eigenvalue >= -PSD_EIGENVALUE_TOLERANCE {
            break;
        }
        // Rebuild from the clipped spectrum: Σ' = V diag(max(λ, floor)) Vᵀ.
        let mut rebuilt = vec![vec![0.0f64; dim]; dim];
        for k in 0..dim {
            let lambda = eigenvalues[k].max(PSD_EIGENVALUE_FLOOR);
            for x in 0..dim {
                for y in 0..dim {
                    rebuilt[x][y] += lambda * eigenvectors[x][k] * eigenvectors[y][k];
                }
            }
        }
        // Rescale to a unit diagonal; the rescale is itself an eigenvalue
        // perturbation, hence the bounded retry loop above.
        let scales: Vec<f64> = rebuilt
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let diag = row[i];
                if diag > 0.0 {
                    diag.sqrt()
                } else {
                    1.0
                }
            })
            .collect();
        let mut projected = vec![vec![0.0f64; dim]; dim];
        for (x, scale_x) in scales.iter().enumerate() {
            for (y, scale_y) in scales.iter().enumerate() {
                projected[x][y] = rebuilt[x][y] / (scale_x * scale_y);
            }
        }
        matrix = projected;
    }

    Ok(matrix)
}

const PSD_PROJECTION_ROUNDS: usize = 10;
/// Off-diagonal convergence threshold for the Jacobi sweep loop.
const JACOBI_SWEEP_TOLERANCE: f64 = 1e-12;

/// Sweep budget for the cyclic Jacobi decomposition. Copula-scale matrices
/// converge in a handful of sweeps; 60 leaves a wide margin.
const MAX_JACOBI_SWEEPS: usize = 60;

const PSD_EIGENVALUE_TOLERANCE: f64 = 1e-12;
const PSD_EIGENVALUE_FLOOR: f64 = 1e-10;

/// Cyclic Jacobi eigenvalue decomposition of a symmetric matrix. Returns the
/// eigenvalues (unsorted) and the eigenvector matrix V with A = V Λ Vᵀ.
///
/// One *sweep* applies a rotation to every upper-triangle pair (n(n-1)/2
/// rotations), not one rotation to the largest pair. Jacobi converges
/// quadratically once the off-diagonal mass is small, so a handful of sweeps
/// drives the largest off-diagonal entry below `JACOBI_SWEEP_TOLERANCE` for
/// the copula-scale matrices this codebase builds. If the sweep budget is
/// exhausted before that holds, the diagonal is *not* a spectrum: returning
/// it would let `ensure_psd` clip a fake spectrum and silently produce a
/// matrix that is far from the nearest PSD projection. That case is an
/// error instead (`max_off_diagonal` is reported so the matrix can be
/// diagnosed).
fn jacobi_eigen_decomposition(matrix: &[Vec<f64>]) -> Result<(Vec<f64>, Vec<Vec<f64>>), String> {
    jacobi_eigen_decomposition_with(matrix, MAX_JACOBI_SWEEPS)
}

/// Injected-sweep-cap variant; the test seam that lets a Red test force
/// non-convergence deterministically instead of relying on a huge
/// adversarial matrix.
fn jacobi_eigen_decomposition_with(
    matrix: &[Vec<f64>],
    max_sweeps: usize,
) -> Result<(Vec<f64>, Vec<Vec<f64>>), String> {
    let dim = matrix.len();
    let mut a: Vec<Vec<f64>> = matrix.to_vec();
    let mut v: Vec<Vec<f64>> = vec![vec![0.0; dim]; dim];
    for (i, v_row) in v.iter_mut().enumerate() {
        v_row[i] = 1.0;
    }

    if dim > 1 {
        for _ in 0..max_sweeps {
            // Off-diagonal mass before the sweep; convergence is judged on
            // the *post-sweep* state below.
            for p in 0..dim {
                for q in (p + 1)..dim {
                    let apq = a[p][q];
                    if apq.abs() < 1e-300 {
                        continue;
                    }
                    let theta = 0.5 * ((2.0 * apq).atan2(a[q][q] - a[p][p]));
                    let (c, s) = (theta.cos(), theta.sin());
                    // Similarity transform A' = Jᵀ A J with
                    // J = [[c, s], [-s, c]] acting on coordinates p, q. The
                    // column rotation (right factor) must read the
                    // pre-rotation columns; the row rotation (left factor)
                    // then reads the intermediate A J.
                    let (col_p, col_q): (Vec<f64>, Vec<f64>) = {
                        let mut cp = Vec::with_capacity(dim);
                        let mut cq = Vec::with_capacity(dim);
                        for row in a.iter() {
                            cp.push(row[p]);
                            cq.push(row[q]);
                        }
                        (cp, cq)
                    };
                    // Right factor: A J — rotate columns p, q.
                    for ((row, col_p_k), col_q_k) in
                        a.iter_mut().zip(col_p.iter()).zip(col_q.iter())
                    {
                        row[p] = c * col_p_k - s * col_q_k;
                        row[q] = s * col_p_k + c * col_q_k;
                    }
                    // Left factor: Jᵀ (A J) — rotate rows p, q.
                    let row_p = a[p].clone();
                    let row_q = a[q].clone();
                    for (j, value_p) in row_p.iter().enumerate() {
                        let value_q = row_q[j];
                        a[p][j] = c * value_p - s * value_q;
                        a[q][j] = s * value_p + c * value_q;
                    }
                    // Accumulate the rotation: V' = V J.
                    let (v_p, v_q): (Vec<f64>, Vec<f64>) = {
                        let mut vp = Vec::with_capacity(dim);
                        let mut vq = Vec::with_capacity(dim);
                        for row in v.iter() {
                            vp.push(row[p]);
                            vq.push(row[q]);
                        }
                        (vp, vq)
                    };
                    for ((row, v_p_k), v_q_k) in v.iter_mut().zip(v_p.iter()).zip(v_q.iter()) {
                        row[p] = c * v_p_k - s * v_q_k;
                        row[q] = s * v_p_k + c * v_q_k;
                    }
                }
            }
            let max_off_diagonal = a
                .iter()
                .enumerate()
                .flat_map(|(i, row)| {
                    row.iter()
                        .enumerate()
                        .skip(i + 1)
                        .map(move |(_, value)| value.abs())
                })
                .fold(0.0f64, f64::max);
            if max_off_diagonal < JACOBI_SWEEP_TOLERANCE {
                let eigenvalues = a.iter().enumerate().map(|(i, row)| row[i]).collect();
                return Ok((eigenvalues, v));
            }
        }
        // Sweep budget exhausted without reaching the tolerance: the
        // diagonal is not a spectrum. Fail loudly instead of feeding a
        // fake spectrum to the PSD clip.
        let max_off_diagonal = a
            .iter()
            .enumerate()
            .flat_map(|(i, row)| {
                row.iter()
                    .enumerate()
                    .skip(i + 1)
                    .map(move |(_, value)| value.abs())
            })
            .fold(0.0f64, f64::max);
        return Err(format!(
            "Jacobi eigen decomposition did not converge within {max_sweeps} sweeps \
             (max off-diagonal |entry| = {max_off_diagonal:.3e}, tolerance \
             {JACOBI_SWEEP_TOLERANCE:.0e}); the correlation matrix is too far from PSD \
             to project reliably"
        ));
    }

    let eigenvalues = a.iter().enumerate().map(|(i, row)| row[i]).collect();
    Ok((eigenvalues, v))
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
        let copula = GaussianCopula::new(correlation).expect("copula must construct");
        let samples = copula.sample(100, Some(42));
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].len(), 100);
    }

    #[test]
    fn copula_handles_non_psd_matrix() {
        let correlation = vec![vec![1.0, 0.9], vec![0.9, 0.5]];
        let _copula = GaussianCopula::new(correlation).expect("copula must construct");
    }

    #[test]
    fn copula_sampling_preserves_correlation() {
        let correlation = vec![vec![1.0, 0.8], vec![0.8, 1.0]];
        let copula = GaussianCopula::new(correlation).expect("copula must construct");
        let samples = copula.sample(10000, Some(42));

        let corr = pearson_correlation(&samples[0], &samples[1]);
        assert!((corr - 0.8).abs() < 0.2);
    }

    #[test]
    fn should_preserve_correlation_structure_of_psd_matrix() {
        // Issue #89 S2a: the old ensure_psd raised any diagonal below its
        // row's off-diagonal absolute sum to that sum plus eps. For the
        // matrix below the diagonals were inflated to 1.80/1.75/1.65, so
        // the correlated pairs sampled at rho/diag ~ 0.53/0.49/0.46
        // instead of their requested strengths. A PSD input
        // must pass through the projection unchanged: the sampled uniforms
        // (Gaussian rank -> Pearson on the z-scale) must reproduce the
        // requested rho within tight tolerance, not a shrunk fraction.
        let correlation = vec![
            vec![1.0, 0.95, 0.85],
            vec![0.95, 1.0, 0.80],
            vec![0.85, 0.80, 1.0],
        ];
        let copula = GaussianCopula::new(correlation).expect("copula must construct");
        let samples = copula.sample(60_000, Some(7));

        // Uniforms from a Gaussian copula with parameter rho have Pearson
        // correlation 6*asin(rho/2)/pi on the z-scale before the CDF; after
        // the CDF the rank correlation keeps the same value in expectation.
        let expected = |rho: f64| 6.0 * (rho / 2.0).asin() / std::f64::consts::PI;
        for (i, j, rho) in [(0usize, 1usize, 0.95), (0, 2, 0.85), (1, 2, 0.80)] {
            let corr = pearson_correlation(&samples[i], &samples[j]);
            assert!(
                (corr - expected(rho)).abs() < 0.03,
                "sampled corr({i},{j}) = {corr:.4} must match {rho} (gaussian-copula \
                 expected {:.4}); diagonal inflation suspected",
                expected(rho)
            );
        }
    }

    #[test]
    fn should_project_non_psd_matrix_without_inflating_the_diagonal() {
        // [[1, 0.95], [0.95, 0.5]] has a negative eigenvalue. The projection
        // must clip eigenvalues, not raise the diagonal: the output stays a
        // correlation matrix (unit diagonal, symmetric, PSD).
        let correlation = vec![vec![1.0, 0.95], vec![0.95, 0.5]];
        let projected = ensure_psd(correlation).expect("projection must succeed");

        for (i, row) in projected.iter().enumerate() {
            assert!(
                (row[i] - 1.0).abs() < 1e-9,
                "diagonal must stay 1.0, got {}",
                row[i]
            );
            for (j, value) in row.iter().enumerate() {
                assert!(
                    (*value - projected[j][i]).abs() < 1e-12,
                    "matrix must stay symmetric"
                );
            }
        }
        // Off-diagonal must remain positive and at most 1: the old
        // diagonal-dominance hack pushed this entry to exactly 1.0 on both
        // sides and the diagonal to 1.95/1.45.
        assert!(
            projected[0][1] > 0.3 && projected[0][1] <= 1.0 + 1e-9,
            "off-diagonal must be a correlation in (0.3, 1], got {}",
            projected[0][1]
        );
        // Cholesky must succeed on the projection without the negative-diff
        // fallback.
        let _l = cholesky_decomposition(&projected);
    }

    /// Re-review bug (PR #120): the Jacobi loop performed 100 single
    /// rotations, not 100 sweeps; a 12-dimension non-PSD matrix did not
    /// converge, and `ensure_psd` then trusted the diagonal of a
    /// non-diagonalized matrix as the spectrum. A projection on a realistic
    /// (n >= 12) copula matrix must produce a genuinely PSD result: unit
    /// diagonal, symmetric, and the eigenvalues of the *output* must all be
    /// positive (which requires the decomposition itself to have converged).
    #[test]
    fn should_project_a_twelve_dimensional_non_psd_matrix_to_psd() {
        let dim = 12usize;
        // Build a rank-deficient correlation matrix: six latent factors,
        // two correlated columns each, plus one clamped beyond |1| to force a
        // negative eigenvalue on top of singularity.
        let mut correlation = vec![vec![0.0f64; dim]; dim];
        for (i, row) in correlation.iter_mut().enumerate() {
            row[i] = 1.0;
        }
        for block in 0..6 {
            let (a, b) = (block * 2, block * 2 + 1);
            let rho = 0.95 - block as f64 * 0.03;
            correlation[a][b] = rho;
            correlation[b][a] = rho;
        }
        correlation[0][dim - 1] = 1.2;
        correlation[dim - 1][0] = 1.2;

        let projected = ensure_psd(correlation).expect("projection must succeed");

        // Unit diagonal, symmetric.
        for (i, row) in projected.iter().enumerate() {
            assert!(
                (row[i] - 1.0).abs() < 1e-9,
                "diagonal must stay 1.0, got {}",
                row[i]
            );
            for (j, value) in row.iter().enumerate() {
                assert!(
                    (*value - projected[j][i]).abs() < 1e-12,
                    "matrix must stay symmetric at ({i},{j})"
                );
                assert!(
                    value.abs() <= 1.0 + 1e-9,
                    "entries must stay in [-1, 1], got {value} at ({i},{j})"
                );
            }
        }
        // The decisive check: run the (now converged) decomposition on the
        // OUTPUT. If the projection did its job the output spectrum is
        // non-negative; if Jacobi did not converge, the returned "eigen-
        // values" are the diagonal of a matrix that still has large
        // off-diagonal mass and this assertion becomes flaky-by-design.
        let (eigenvalues, _) = jacobi_eigen_decomposition(&projected).expect("must converge");
        let min_eigenvalue = eigenvalues.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(
            min_eigenvalue > -1e-8,
            "projected matrix must be PSD, min eigenvalue was {min_eigenvalue}"
        );
        // Cholesky must succeed without the silent 0.001 fallback.
        let _l = cholesky_decomposition(&projected);
    }

    /// The decomposition must actually converge: after the sweep loop the
    /// matrix must be numerically diagonal, otherwise the returned
    /// "eigenvalues" are meaningless and the PSD clip misses real negative
    /// eigenvalues.
    #[test]
    fn jacobi_decomposition_converges_on_a_twelve_by_twelve_matrix() {
        let dim = 12usize;
        let mut matrix: Vec<Vec<f64>> = (0..dim)
            .map(|i| {
                let mut row = vec![0.0f64; dim];
                row[i] = 1.0;
                row
            })
            .collect();
        let pairs: Vec<(usize, usize, f64)> = (0..dim)
            .flat_map(|i| (i + 1..dim).map(move |j| (i, j, 0.5 - (i + j) as f64 * 0.01)))
            .collect();
        for (i, j, value) in pairs {
            matrix[i][j] = value;
            matrix[j][i] = value;
        }
        let (eigenvalues, eigenvectors) =
            jacobi_eigen_decomposition(&matrix).expect("must converge");

        // Reconstruct A' = V diag V^T and compare with the input.
        let mut rebuilt = vec![vec![0.0f64; dim]; dim];
        for k in 0..dim {
            for x in 0..dim {
                for y in 0..dim {
                    rebuilt[x][y] += eigenvalues[k] * eigenvectors[x][k] * eigenvectors[y][k];
                }
            }
        }
        let mut max_error = 0.0f64;
        for x in 0..dim {
            for y in 0..dim {
                max_error = max_error.max((rebuilt[x][y] - matrix[x][y]).abs());
            }
        }
        assert!(
            max_error < 1e-9,
            "decomposition must reproduce the input, max error {max_error}"
        );
    }

    /// PR #120 review r7 bug: when the sweep budget is exhausted without
    /// reaching the off-diagonal tolerance, the function must fail instead
    /// of returning the diagonal of a still-undigonalized matrix as the
    /// spectrum (which `ensure_psd` would then clip as if it were real
    /// eigenvalues). The sweep cap is injected so the test can force
    /// non-convergence deterministically instead of relying on a huge
    /// adversarial matrix.
    #[test]
    fn should_fail_when_the_sweep_budget_is_exhausted_without_convergence() {
        let dim = 12usize;
        let mut matrix: Vec<Vec<f64>> = (0..dim)
            .map(|i| {
                let mut row = vec![0.0f64; dim];
                row[i] = 1.0;
                row
            })
            .collect();
        let pairs: Vec<(usize, usize, f64)> = (0..dim)
            .flat_map(|i| (i + 1..dim).map(move |j| (i, j, 0.5 - (i + j) as f64 * 0.01)))
            .collect();
        for (i, j, rho) in pairs {
            matrix[i][j] = rho;
            matrix[j][i] = rho;
        }
        // Zero sweeps can never converge on an off-diagonal matrix.
        let err = jacobi_eigen_decomposition_with(&matrix, 0)
            .expect_err("an exhausted sweep budget must be an error, not a fake spectrum");
        assert!(
            err.contains("converge") || err.contains("convergence"),
            "error must state the convergence failure: {err}"
        );
    }

    /// The public decomposition keeps converging on the matrices this
    /// codebase builds: the error path above must not reject honest input.
    #[test]
    fn should_still_converge_on_a_typical_correlation_matrix() {
        let dim = 12usize;
        let mut matrix: Vec<Vec<f64>> = (0..dim)
            .map(|i| {
                let mut row = vec![0.0f64; dim];
                row[i] = 1.0;
                row
            })
            .collect();
        let pairs: Vec<(usize, usize, f64)> = (0..dim)
            .flat_map(|i| (i + 1..dim).map(move |j| (i, j, 0.5 - (i + j) as f64 * 0.01)))
            .collect();
        for (i, j, rho) in pairs {
            matrix[i][j] = rho;
            matrix[j][i] = rho;
        }
        let (eigenvalues, eigenvectors) =
            jacobi_eigen_decomposition(&matrix).expect("must converge");
        let mut rebuilt = vec![vec![0.0f64; dim]; dim];
        for k in 0..dim {
            for x in 0..dim {
                for y in 0..dim {
                    rebuilt[x][y] += eigenvalues[k] * eigenvectors[x][k] * eigenvectors[y][k];
                }
            }
        }
        let max_error = rebuilt
            .iter()
            .zip(matrix.iter())
            .map(|(rb_row, m_row)| {
                rb_row
                    .iter()
                    .zip(m_row.iter())
                    .map(|(rb, m)| (rb - m).abs())
                    .fold(0.0f64, f64::max)
            })
            .fold(0.0f64, f64::max);
        assert!(
            max_error < 1e-9,
            "decomposition must reproduce the input, max error {max_error}"
        );
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
        let copula = GaussianCopula::new(vec![vec![1.0, 0.5], vec![0.5, 1.0]])
            .expect("copula must construct");
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
        let copula = GaussianCopula::new(vec![vec![1.0, 0.8], vec![0.8, 1.0]])
            .expect("copula must construct");
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
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .expect("copula must construct");
        let error = copula
            .sample_with_fixed_z(4, &[(5, 0.0)], Some(1))
            .unwrap_err();
        assert!(error.contains('5'), "error was {error}");
    }

    #[test]
    fn should_reject_duplicate_fixed_dimensions() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .expect("copula must construct");
        let error = copula
            .sample_with_fixed_z(4, &[(0, 0.0), (0, 1.0)], Some(1))
            .unwrap_err();
        assert!(error.contains("duplicate"), "error was {error}");
    }

    #[test]
    fn should_reject_fixed_uniform_outside_open_unit_interval() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .expect("copula must construct");
        assert!(copula
            .sample_with_fixed_uniforms(4, &[(0, 1.0)], Some(1))
            .is_err());
        assert!(copula
            .sample_with_fixed_uniforms(4, &[(0, 0.0)], Some(1))
            .is_err());
    }

    #[test]
    fn should_round_trip_fixed_uniform_through_inverse_normal() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.3], vec![0.3, 1.0]])
            .expect("copula must construct");
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
        let copula = GaussianCopula::new(vec![vec![1.0, 0.4], vec![0.4, 1.0]])
            .expect("copula must construct");
        let first = copula
            .sample_with_fixed_z(16, &[(0, -0.5)], Some(99))
            .unwrap();
        let second = copula
            .sample_with_fixed_z(16, &[(0, -0.5)], Some(99))
            .unwrap();
        assert_eq!(first, second);
    }

    // ─── 逐行固定 z（#68 copula_conditional 的 fixed_range 情形）──────────

    #[test]
    fn should_condition_each_row_on_its_own_pinned_z() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.8], vec![0.8, 1.0]])
            .expect("copula must construct");
        let n = 8000;
        // First half pinned at z = 1, second half at z = -1.
        let mut pins: Vec<f64> = vec![1.0; n / 2];
        pins.extend(vec![-1.0; n / 2]);

        let samples = copula
            .sample_with_fixed_z_rows(n, &[(0, pins)], Some(11))
            .unwrap();
        let z: Vec<f64> = samples[1].iter().map(|u| normal_quantile(*u)).collect();

        let head = z[..n / 2].iter().sum::<f64>() / (n / 2) as f64;
        let tail = z[n / 2..].iter().sum::<f64>() / (n / 2) as f64;
        assert!((head - 0.8).abs() < 0.06, "pinned z=1 mean was {head}");
        assert!((tail + 0.8).abs() < 0.06, "pinned z=-1 mean was {tail}");
        for (index, u) in samples[0].iter().enumerate() {
            let expected = normal_cdf(if index < n / 2 { 1.0 } else { -1.0 });
            assert!((u - expected).abs() < 1e-12);
        }
    }

    #[test]
    fn should_reject_pinned_row_list_of_the_wrong_length() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .expect("copula must construct");
        let error = copula
            .sample_with_fixed_z_rows(4, &[(0, vec![0.0, 0.0])], Some(1))
            .unwrap_err();
        assert!(error.contains('4'), "error was {error}");
    }

    #[test]
    fn should_reproduce_row_pinned_samples_for_same_seed() {
        let copula = GaussianCopula::new(vec![vec![1.0, 0.5], vec![0.5, 1.0]])
            .expect("copula must construct");
        let pins = vec![0.1, -0.2, 0.3, -0.4];
        let first = copula
            .sample_with_fixed_z_rows(4, &[(1, pins.clone())], Some(5))
            .unwrap();
        let second = copula
            .sample_with_fixed_z_rows(4, &[(1, pins)], Some(5))
            .unwrap();
        assert_eq!(first, second);
    }
}
