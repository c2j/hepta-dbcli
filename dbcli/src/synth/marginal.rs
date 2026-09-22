use crate::synth::model::{ColumnModel, LogicalType};
use crate::synth::stats::ks_statistic;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name")]
pub enum Marginal {
    #[serde(rename = "norm")]
    Normal(NormalParams),
    #[serde(rename = "beta")]
    Beta(BetaParams),
    #[serde(rename = "gamma")]
    Gamma(GammaParams),
    #[serde(rename = "categorical")]
    Categorical(CategoricalParams),
    #[serde(rename = "uniform")]
    Uniform(UniformParams),
    /// Empirical (piecewise-linear quantile) marginal. Knots are the sorted
    /// training values sampled at a uniform probability grid, so the
    /// probabilities are implicit (`i / (len - 1)`) and the serialized model
    /// only carries the value axis.
    #[serde(rename = "ecdf")]
    Ecdf(EcdfParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalParams {
    pub loc: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaParams {
    pub a: f64,
    pub b: f64,
    pub loc: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaParams {
    pub shape: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoricalParams {
    pub values: Vec<String>,
    pub weights: Vec<f64>,
}

impl CategoricalParams {
    pub fn sample_index(&self, u: f64) -> usize {
        let total: f64 = self.weights.iter().sum();
        if self.values.is_empty() || total <= 0.0 {
            return 0;
        }
        let target = u.clamp(0.0, 1.0) * total;
        let mut acc = 0.0;
        for (i, &w) in self.weights.iter().enumerate() {
            acc += w;
            if target < acc {
                return i;
            }
        }
        self.values.len() - 1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniformParams {
    pub low: f64,
    pub high: f64,
}

/// Knot values of the empirical quantile function at uniformly spaced
/// probabilities. `knots` is non-decreasing; `knots[0]` / `knots[last]` are the
/// observed sample min / max, so an ECDF marginal can never generate outside
/// the training range (no min/max clipping needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcdfParams {
    pub knots: Vec<f64>,
}

impl Marginal {
    pub fn cdf(&self, x: f64) -> f64 {
        match self {
            Marginal::Normal(p) => normal_cdf(x, p.loc, p.scale),
            Marginal::Beta(p) => {
                let t = (x - p.loc) / p.scale;
                beta_cdf(t, p.a, p.b)
            }
            Marginal::Gamma(p) => gamma_cdf(x, p.shape, p.scale),
            Marginal::Uniform(p) => uniform_cdf(x, p.low, p.high),
            Marginal::Categorical(_) => 0.0,
            Marginal::Ecdf(p) => p.cdf(x),
        }
    }

    pub fn inverse_cdf(&self, p: f64) -> f64 {
        match self {
            Marginal::Normal(params) => normal_ppf(params.loc, params.scale, p),
            Marginal::Beta(params) => beta_ppf(params.a, params.b, params.loc, params.scale, p),
            Marginal::Gamma(params) => gamma_ppf(params.shape, params.scale, p),
            Marginal::Uniform(params) => uniform_ppf(params.low, params.high, p),
            Marginal::Categorical(params) => params.sample_index(p) as f64,
            Marginal::Ecdf(params) => params.inverse_cdf(p),
        }
    }

    /// Left limit `F(x⁻)`. Only the ECDF carries atoms; every parametric
    /// family is continuous, so its left limit equals `cdf`.
    pub fn cdf_left(&self, x: f64) -> f64 {
        match self {
            Marginal::Ecdf(params) => params.cdf_left(x),
            other => other.cdf(x),
        }
    }
}

impl crate::synth::stats::ReferenceCdf for &EcdfParams {
    fn cdf(&self, x: f64) -> f64 {
        EcdfParams::cdf(self, x)
    }

    fn cdf_left(&self, x: f64) -> f64 {
        EcdfParams::cdf_left(self, x)
    }
}

impl crate::synth::stats::ReferenceCdf for &Marginal {
    fn cdf(&self, x: f64) -> f64 {
        Marginal::cdf(self, x)
    }

    fn cdf_left(&self, x: f64) -> f64 {
        Marginal::cdf_left(self, x)
    }
}

impl EcdfParams {
    /// Largest knot index `i` with `knots[i] <= x`, or `None` when no knot is
    /// at or below `x`.
    fn last_knot_at_or_below(&self, x: f64) -> Option<usize> {
        let count = self.knots.partition_point(|&v| v <= x);
        count.checked_sub(1)
    }

    /// Right-continuous empirical CDF: at a mass point the value is the *end*
    /// of the flat quantile region, so a zero-inflated sample reports
    /// `F(0) = P(X <= 0)` rather than 0.
    pub fn cdf(&self, x: f64) -> f64 {
        let n = self.knots.len();
        if n == 0 {
            return 0.0;
        }
        if n == 1 {
            return if x >= self.knots[0] { 1.0 } else { 0.0 };
        }
        if x < self.knots[0] {
            return 0.0;
        }
        if x >= self.knots[n - 1] {
            return 1.0;
        }
        let last = n - 1;
        match self.last_knot_at_or_below(x) {
            None => 0.0,
            Some(i) => {
                if self.knots[i] == x {
                    return i as f64 / last as f64;
                }
                // knots[i] < x < knots[i + 1] because i is the last index at or
                // below x and x < knots[last].
                let lo = self.knots[i];
                let hi = self.knots[i + 1];
                let frac = (x - lo) / (hi - lo);
                (i as f64 + frac) / last as f64
            }
        }
    }

    pub fn inverse_cdf(&self, p: f64) -> f64 {
        let n = self.knots.len();
        if n == 0 {
            return 0.0;
        }
        if n == 1 {
            return self.knots[0];
        }
        let last = (n - 1) as f64;
        let t = p.clamp(0.0, 1.0) * last;
        let i = (t.floor() as usize).min(n - 2);
        let frac = t - i as f64;
        self.knots[i] + frac * (self.knots[i + 1] - self.knots[i])
    }

    /// Left limit of the CDF: at an atom it is the *start* of the flat
    /// quantile region, mirroring `cdf`'s use of the end.
    pub fn cdf_left(&self, x: f64) -> f64 {
        let n = self.knots.len();
        if n == 0 {
            return 0.0;
        }
        if n == 1 {
            return if x > self.knots[0] { 1.0 } else { 0.0 };
        }
        let last = n - 1;
        if x <= self.knots[0] {
            return 0.0;
        }
        if x > self.knots[last] {
            return 1.0;
        }
        let below = self.knots.partition_point(|&v| v < x);
        if self.knots.get(below) == Some(&x) {
            // `below` is the first knot equal to x: the atom's left edge.
            return below as f64 / last as f64;
        }
        let lo = self.knots[below - 1];
        let hi = self.knots[below];
        let frac = (x - lo) / (hi - lo);
        (below as f64 - 1.0 + frac) / last as f64
    }
}

fn normal_cdf(x: f64, loc: f64, scale: f64) -> f64 {
    let z = (x - loc) / scale;
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// Inverse normal CDF via binary search on the CDF.
/// Guarantees correct roundtrip; fast enough for synth workloads.
fn normal_ppf(loc: f64, scale: f64, p: f64) -> f64 {
    const EPS: f64 = 1e-15;
    let p = p.clamp(EPS, 1.0 - EPS);

    // Quick return for p=0.5
    if (p - 0.5).abs() < 1e-15 {
        return loc;
    }

    // Binary search bounds: normal is effectively bounded at ±10 σ
    let mut lo = loc - 10.0 * scale;
    let mut hi = loc + 10.0 * scale;

    for _ in 0..50 {
        let mid = (lo + hi) * 0.5;
        let cdf = normal_cdf(mid, loc, scale);
        if cdf < p {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi - lo < 1e-12 {
            break;
        }
    }
    (lo + hi) * 0.5
}

fn beta_cdf(x: f64, a: f64, b: f64) -> f64 {
    beta_reg(a, b, x)
}

fn beta_ppf(a: f64, b: f64, loc: f64, scale: f64, p: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 || scale <= 0.0 {
        return loc + scale * a / (a + b);
    }
    let target = p.clamp(0.0, 1.0);
    if target <= 0.0 {
        return loc;
    }
    if target >= 1.0 {
        return loc + scale;
    }
    bisect(
        |x| beta_reg(a, b, (x - loc) / scale),
        target,
        loc,
        loc + scale,
    )
}

fn gamma_cdf(x: f64, shape: f64, scale: f64) -> f64 {
    if shape <= 0.0 || scale <= 0.0 {
        return 0.0;
    }
    gamma_p(shape, x / scale)
}

fn gamma_ppf(shape: f64, scale: f64, p: f64) -> f64 {
    if shape <= 0.0 || scale <= 0.0 {
        return shape * scale;
    }
    let target = p.clamp(0.0, 1.0);
    if target <= 0.0 {
        return 0.0;
    }
    if target >= 1.0 {
        return f64::INFINITY;
    }
    let mut hi = shape * scale;
    while gamma_cdf(hi, shape, scale) < target {
        hi *= 2.0;
        if hi > 1e300 {
            break;
        }
    }
    scale * bisect(|u| gamma_p(shape, u), target, 0.0, hi / scale)
}

fn bisect(f: impl Fn(f64) -> f64, target: f64, mut lo: f64, mut hi: f64) -> f64 {
    debug_assert!(lo <= hi);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if mid <= lo || mid >= hi {
            break;
        }
        if f(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const COEFS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        let ln_sin_pi_x = (std::f64::consts::PI * x).sin().abs().ln();
        std::f64::consts::PI.ln() - ln_sin_pi_x - ln_gamma(1.0 - x)
    } else {
        let x = x - 1.0;
        let mut a = COEFS[0];
        let t = x + G + 0.5;
        for (i, &c) in COEFS.iter().enumerate().skip(1) {
            a += c / (x + i as f64);
        }
        0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
    }
}

fn gamma_p(a: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x < a + 1.0 {
        let mut ap = a;
        let mut sum = 1.0 / a;
        let mut del = sum;
        for _ in 0..1000 {
            ap += 1.0;
            del *= x / ap;
            sum += del;
            if del.abs() < sum.abs() * 1e-15 {
                break;
            }
        }
        sum * (-x + a * x.ln() - ln_gamma(a)).exp()
    } else {
        1.0 - gamma_q_cf(a, x)
    }
}

fn gamma_q_cf(a: f64, x: f64) -> f64 {
    let tiny = 1e-300;
    let mut b = x + 1.0 - a;
    let mut c = 1.0 / tiny;
    let mut d = 1.0 / b;
    if b.abs() < tiny {
        b = tiny;
    }
    let mut h = d;
    for i in 1..=1000 {
        let an = -(i as f64) * (i as f64 - a);
        b += 2.0;
        d = an * d + b;
        if d.abs() < tiny {
            d = tiny;
        }
        c = b + an / c;
        if c.abs() < tiny {
            c = tiny;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < 1e-15 {
            break;
        }
    }
    (-x + a * x.ln() - ln_gamma(a)).exp() * h
}

fn beta_reg(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let ln_front = ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln();
    let front = ln_front.exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * betacf(a, b, x) / a
    } else {
        1.0 - front * betacf(b, a, 1.0 - x) / b
    }
}

fn betacf(a: f64, b: f64, x: f64) -> f64 {
    let tiny = 1e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < tiny {
        d = tiny;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=1000 {
        let mf = m as f64;
        let m2 = 2.0 * mf;
        let aa = mf * (b - mf) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < tiny {
            d = tiny;
        }
        c = 1.0 + aa / c;
        if c.abs() < tiny {
            c = tiny;
        }
        d = 1.0 / d;
        h *= d * c;
        let aa = -(a + mf) * (qab + mf) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < tiny {
            d = tiny;
        }
        c = 1.0 + aa / c;
        if c.abs() < tiny {
            c = tiny;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < 1e-15 {
            break;
        }
    }
    h
}

fn uniform_cdf(x: f64, low: f64, high: f64) -> f64 {
    if x <= low {
        return 0.0;
    }
    if x >= high {
        return 1.0;
    }
    (x - low) / (high - low)
}

fn uniform_ppf(low: f64, high: f64, p: f64) -> f64 {
    low + (high - low) * p
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

pub trait MarginalFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String>;
}

pub struct NormalFitter;

impl MarginalFitter for NormalFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        if samples.is_empty() {
            return Err("empty samples".to_string());
        }
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();

        Ok(Marginal::Normal(NormalParams {
            loc: mean,
            scale: std_dev,
        }))
    }
}

pub struct BetaFitter;

impl MarginalFitter for BetaFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        if samples.is_empty() {
            return Err("empty samples".to_string());
        }
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;

        let common_factor = mean * (1.0 - mean) / variance - 1.0;
        let a = mean * common_factor;
        let b = (1.0 - mean) * common_factor;

        Ok(Marginal::Beta(BetaParams {
            a,
            b,
            loc: 0.0,
            scale: 1.0,
        }))
    }
}

pub struct GammaFitter;

impl MarginalFitter for GammaFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        if samples.is_empty() {
            return Err("empty samples".to_string());
        }
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;

        let shape = mean * mean / variance;
        let scale = variance / mean;

        Ok(Marginal::Gamma(GammaParams { shape, scale }))
    }
}

pub struct CategoricalFitter;

impl CategoricalFitter {
    pub fn fit_strings(&self, samples: &[String]) -> Result<Marginal, String> {
        if samples.is_empty() {
            return Err("empty samples".to_string());
        }

        let mut counts = std::collections::HashMap::new();
        for s in samples {
            *counts.entry(s.clone()).or_insert(0) += 1;
        }

        let total = samples.len() as f64;
        let mut values: Vec<String> = counts.keys().cloned().collect();
        values.sort();
        let weights: Vec<f64> = values.iter().map(|v| counts[v] as f64 / total).collect();

        Ok(Marginal::Categorical(CategoricalParams { values, weights }))
    }
}

pub struct UniformFitter;

impl MarginalFitter for UniformFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        if samples.is_empty() {
            return Err("empty samples".to_string());
        }
        let low = samples.iter().cloned().fold(f64::INFINITY, f64::min);
        let high = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        Ok(Marginal::Uniform(UniformParams { low, high }))
    }
}

/// Upper bound on ECDF knots per column. Knots carry one `f64` each, so 512
/// keeps the pretty-printed JSON increment under ~16KB per column while still
/// resolving the shapes (skew, multi-mode, zero inflation) that parametric
/// marginals flatten.
pub const ECDF_MAX_KNOTS: usize = 512;

pub struct EcdfFitter;

impl MarginalFitter for EcdfFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
        if sorted.is_empty() {
            return Err("empty samples".to_string());
        }
        // Stable, order-independent: sorting removes any dependence on the
        // sampling order, so the same multiset always yields the same knots.
        sorted.sort_by(f64::total_cmp);

        let m = sorted.len().min(ECDF_MAX_KNOTS);
        let knots: Vec<f64> = if m == 1 {
            vec![sorted[0]]
        } else {
            let last = (sorted.len() - 1) as f64;
            (0..m)
                .map(|i| {
                    let t = i as f64 / (m - 1) as f64 * last;
                    let lo = t.floor() as usize;
                    let hi = (lo + 1).min(sorted.len() - 1);
                    sorted[lo] + (t - lo as f64) * (sorted[hi] - sorted[lo])
                })
                .collect()
        };

        Ok(Marginal::Ecdf(EcdfParams { knots }))
    }
}

/// Parametric families considered by automatic selection, in tie-break order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericFamily {
    Normal,
    Beta,
    Gamma,
    Uniform,
}

impl NumericFamily {
    /// Fixed order: on an exact KS tie the earlier family wins, so selection
    /// never depends on iteration order.
    pub const ORDER: [NumericFamily; 4] = [Self::Normal, Self::Beta, Self::Gamma, Self::Uniform];

    /// Stable name used by the `marginal:` rules override and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Beta => "beta",
            Self::Gamma => "gamma",
            Self::Uniform => "uniform",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|f| f.name() == name)
    }

    pub fn fit(self, samples: &[f64]) -> Result<Marginal, String> {
        match self {
            Self::Normal => NormalFitter.fit(samples),
            Self::Beta => BetaFitter.fit(samples),
            Self::Gamma => GammaFitter.fit(samples),
            Self::Uniform => UniformFitter.fit(samples),
        }
    }

    /// Support constraints of the fitted family. Beta is fitted on the unit
    /// interval (`loc = 0`, `scale = 1`) and Gamma on the positive half-line,
    /// so out-of-support samples are excluded rather than fitted into a
    /// nonsensical shape.
    pub(crate) fn applicable(self, samples: &[f64]) -> bool {
        match self {
            Self::Normal | Self::Uniform => true,
            Self::Beta => samples.iter().all(|v| (0.0..=1.0).contains(v)),
            Self::Gamma => samples.iter().all(|v| *v > 0.0),
        }
    }

    /// Whether the fitted parameters are usable. Moment matching does not
    /// validate its inputs: Gamma on a sample that straddles zero yields a
    /// negative scale, and Beta outside the unit interval can yield negative
    /// shape parameters. A forced family whose fit fails this check falls back
    /// to auto-selection instead of producing a broken marginal.
    pub(crate) fn parameters_are_usable(self, marginal: &Marginal) -> bool {
        match (self, marginal) {
            (Self::Normal, Marginal::Normal(p)) => {
                p.loc.is_finite() && p.scale.is_finite() && p.scale >= 0.0
            }
            (Self::Beta, Marginal::Beta(p)) => {
                p.a.is_finite()
                    && p.b.is_finite()
                    && p.a > 0.0
                    && p.b > 0.0
                    && p.loc.is_finite()
                    && p.scale > 0.0
            }
            (Self::Gamma, Marginal::Gamma(p)) => {
                p.shape.is_finite() && p.scale.is_finite() && p.shape > 0.0 && p.scale > 0.0
            }
            (Self::Uniform, Marginal::Uniform(p)) => {
                p.low.is_finite() && p.high.is_finite() && p.high >= p.low
            }
            _ => false,
        }
    }
}

/// Below this sample count a holdout is too small for KS to be meaningful, so
/// selection stays parametric. Numerical columns only reach selection when
/// their cardinality exceeds the `top_values` cap, so this is a safety net.
const MIN_SELECTION_SPLIT: usize = 40;
/// One in N samples is held out for scoring. The split runs over the sorted
/// sample, so it is deterministic and independent of the input row order.
const SELECTION_HOLDOUT_STRIDE: usize = 10;
/// ECDF must beat the best parametric KS by this relative margin...
const ECDF_RELATIVE_MARGIN: f64 = 0.2;
/// ...or at least by this absolute margin, before its model size is justified.
const ECDF_ABSOLUTE_MARGIN: f64 = 0.005;

/// Choose a marginal for a high-cardinality numerical column.
///
/// Candidates are fitted on the training split and scored by KS on a held-out
/// split. Scoring on the samples the ECDF interpolates would make it win by
/// construction and turn "auto-select" into "always ECDF"; the holdout plus
/// the relative margin keeps a well-fitting parametric family in place while
/// still catching shapes no parametric family can express.
pub fn fit_auto_numeric_marginal(samples: &[f64]) -> Result<Marginal, String> {
    let mut all: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    if all.is_empty() {
        return Err("no finite samples to fit a marginal".to_string());
    }
    all.sort_by(f64::total_cmp);

    // Constant column: keep the legacy point-mass Normal so downstream
    // rounding / scale handling is unchanged.
    if all[0] == all[all.len() - 1] {
        return NormalFitter.fit(&all);
    }

    let (train, holdout): (Vec<f64>, Vec<f64>) = if all.len() >= MIN_SELECTION_SPLIT {
        let mut train = Vec::with_capacity(all.len());
        let mut holdout = Vec::with_capacity(all.len() / SELECTION_HOLDOUT_STRIDE + 1);
        for (i, value) in all.iter().enumerate() {
            if i % SELECTION_HOLDOUT_STRIDE == 0 {
                holdout.push(*value);
            } else {
                train.push(*value);
            }
        }
        (train, holdout)
    } else {
        (all.clone(), all.clone())
    };

    let mut best: Option<(NumericFamily, f64)> = None;
    for family in NumericFamily::ORDER {
        if !family.applicable(&all) {
            continue;
        }
        let Ok(candidate) = family.fit(&train) else {
            continue;
        };
        let ks = ks_statistic(&holdout, &candidate);
        let better = match best {
            None => true,
            Some((_, incumbent)) => ks < incumbent,
        };
        if better {
            best = Some((family, ks));
        }
    }
    let (family, ks_param) =
        best.ok_or_else(|| "no candidate marginal fit the samples".to_string())?;

    if all.len() >= MIN_SELECTION_SPLIT {
        let ecdf_train = EcdfFitter.fit(&train)?;
        let ks_ecdf = ks_statistic(&holdout, &ecdf_train);
        let margin = (ECDF_RELATIVE_MARGIN * ks_param).max(ECDF_ABSOLUTE_MARGIN);
        if ks_ecdf + margin < ks_param {
            return EcdfFitter.fit(&all);
        }
    }

    // Refit the winning family on everything: the split exists to judge the
    // family, not to throw away fitting data.
    family.fit(&all)
}

/// Compute Gaussian-space correlation from training rows via PIT then Pearson.
///
/// For each column: marginal CDF (PIT) → Φ⁻¹ → Pearson. Categorical columns
/// use SDV-style UniformEncoder (cumulative-frequency mid-points).
///
/// Missing values are handled with **pairwise deletion**: each column pair
/// keeps only rows where both sides are non-NULL, and the Pearson denominator
/// is that pair's complete count. Filling NULLs with `loc` / `0.5` (the
/// previous behaviour) treats missingness as a typical observation and can
/// invent or cancel correlation; pairwise-complete avoids that distortion.
/// The trade-off is that pairs no longer share a common sample size, and a
/// pair with fewer than two complete rows falls back to 0.
pub fn compute_gaussian_correlation(
    rows: &[Vec<serde_json::Value>],
    column_order: &[String],
    columns: &HashMap<String, ColumnModel>,
) -> Vec<Vec<f64>> {
    let n_cols = column_order.len();
    let n_rows = rows.len();
    if n_rows < 2 || n_cols == 0 {
        return (0..n_cols)
            .map(|i| {
                (0..n_cols)
                    .map(|j| if i == j { 1.0 } else { 0.0 })
                    .collect()
            })
            .collect();
    }

    let mut gaussian_data = vec![vec![None; n_rows]; n_cols];

    for (col_idx, col_name) in column_order.iter().enumerate() {
        let col_model = columns.get(col_name);
        for (row_idx, row) in rows.iter().enumerate() {
            let val = row.get(col_idx).cloned().unwrap_or(serde_json::Value::Null);
            gaussian_data[col_idx][row_idx] = pit_to_gaussian(&val, col_model);
        }
    }

    let mut corr = vec![vec![0.0f64; n_cols]; n_cols];
    for i in 0..n_cols {
        corr[i][i] = 1.0;
        for j in (i + 1)..n_cols {
            let r = pearson_pairwise(&gaussian_data[i], &gaussian_data[j]);
            corr[i][j] = r;
            corr[j][i] = r;
        }
    }

    // PSD projection without touching the diagonal (issue #89 S2a). The old
    // diagonal-dominance hack stored diag > 1 in the model whenever columns
    // correlate strongly, and the copula then shrank every generated
    // correlation by that factor. The projection clips eigenvalues instead
    // and leaves an honest correlation matrix.
    crate::synth::copula::project_to_correlation(corr)
}

/// DECIMAL/NUMBER values are often serialized as JSON strings by drivers;
/// fall back to parsing the text form. Returns `None` when the value is
/// neither a JSON number nor a numeric string, so the row pair is dropped
/// from the pairwise-complete correlation instead of being filled in.
fn numeric_value(val: &serde_json::Value) -> Option<f64> {
    val.as_f64()
        .or_else(|| val.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

/// Non-NULL numeric samples of one column, used to fit (and score) the
/// candidate marginals of a numerical column.
pub(crate) fn column_numeric_samples(rows: &[Vec<serde_json::Value>], col_idx: usize) -> Vec<f64> {
    rows.iter()
        .filter_map(|row| row.get(col_idx).and_then(numeric_value))
        .collect()
}

/// Value on the axis the column's marginal is expressed in. Datetime columns
/// are modelled in epoch seconds, so text samples are converted first;
/// integer-encoded datetimes (compact `YYYYMMDD`) stay numeric.
pub(crate) fn numeric_axis_value(val: &serde_json::Value, model: &ColumnModel) -> Option<f64> {
    if matches!(model.logical_type, LogicalType::Datetime) {
        match model.datetime_format.as_deref() {
            Some(fmt) => crate::synth::datetime::parse_to_epoch(val, Some(fmt)),
            None => numeric_value(val),
        }
    } else {
        numeric_value(val)
    }
}

fn pit_to_gaussian(val: &serde_json::Value, col_model: Option<&ColumnModel>) -> Option<f64> {
    if val.is_null() {
        return None;
    }
    let u = if let Some(model) = col_model {
        match &model.marginal {
            Marginal::Categorical(p) => {
                // SDV UniformEncoder: map category to mid-point of its cumulative interval.
                // top_values keys are stringified (numeric levels → "1"), so numeric
                // row values must be matched through their string form too.
                let key = val
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| val.to_string());
                if let Some(idx) = p.values.iter().position(|v| v == &key) {
                    let total: f64 = p.weights.iter().sum();
                    let cum_before: f64 = p.weights[..idx].iter().sum();
                    (cum_before + p.weights[idx] / 2.0) / total
                } else {
                    0.5
                }
            }
            marginal => {
                // Every numeric marginal (Normal / Beta / Gamma / Uniform /
                // Ecdf) routes through its own CDF, so the copula's Gaussian
                // space always reflects the fitted shape.
                let x = numeric_axis_value(val, model)?;
                marginal.cdf(x).clamp(1e-12, 1.0 - 1e-12)
            }
        }
    } else {
        0.5
    };
    Some(normal_ppf(0.0, 1.0, u))
}

fn pearson_pairwise(xi: &[Option<f64>], xj: &[Option<f64>]) -> f64 {
    let mut n = 0.0;
    let mut sum_i = 0.0;
    let mut sum_j = 0.0;
    for (a, b) in xi.iter().zip(xj.iter()) {
        if let (Some(a), Some(b)) = (*a, *b) {
            n += 1.0;
            sum_i += a;
            sum_j += b;
        }
    }
    if n < 2.0 {
        return 0.0;
    }
    let mean_i = sum_i / n;
    let mean_j = sum_j / n;
    let mut cov = 0.0;
    let mut var_i = 0.0;
    let mut var_j = 0.0;
    for (a, b) in xi.iter().zip(xj.iter()) {
        if let (Some(a), Some(b)) = (*a, *b) {
            let di = a - mean_i;
            let dj = b - mean_j;
            cov += di * dj;
            var_i += di * di;
            var_j += dj * dj;
        }
    }
    if var_i > 0.0 && var_j > 0.0 {
        cov / (var_i * var_j).sqrt()
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_fitter_converges() {
        let samples: Vec<f64> = vec![100.0; 1000];
        let fitter = NormalFitter;
        let marginal = fitter.fit(&samples).unwrap();
        if let Marginal::Normal(p) = marginal {
            assert!((p.loc - 100.0).abs() < 0.01);
            assert!((p.scale).abs() < 0.01);
        } else {
            panic!("expected Normal");
        }
    }

    #[test]
    fn categorical_fitter_weights_sum_to_one() {
        let samples = vec![
            "a".to_string(),
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        let fitter = CategoricalFitter;
        let marginal = fitter.fit_strings(&samples).unwrap();
        if let Marginal::Categorical(p) = marginal {
            let total: f64 = p.weights.iter().sum();
            assert!((total - 1.0).abs() < 1e-10);
            assert_eq!(p.values.len(), 3);
        } else {
            panic!("expected Categorical");
        }
    }

    #[test]
    fn uniform_fitter_bounds() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let fitter = UniformFitter;
        let marginal = fitter.fit(&samples).unwrap();
        if let Marginal::Uniform(p) = marginal {
            assert!((p.low - 1.0).abs() < 0.01);
            assert!((p.high - 5.0).abs() < 0.01);
        } else {
            panic!("expected Uniform");
        }
    }

    // shape=1, scale=1 的 Gamma 即指数分布：CDF = 1 - e^(-x)
    #[test]
    fn gamma_cdf_exponential_known_values() {
        let m = Marginal::Gamma(GammaParams {
            shape: 1.0,
            scale: 1.0,
        });
        let cdf2 = m.cdf(2.0);
        let expected = 1.0 - (-2.0f64).exp();
        assert!(
            (cdf2 - expected).abs() < 1e-9,
            "gamma(1,1).cdf(2) = {} want {}",
            cdf2,
            expected
        );
    }

    // shape=0.5, scale=1：CDF = erf(sqrt(x))
    // erf 为 A&S 5.1.26 多项式（误差 ~1.5e-7），两独立实现对比到该精度
    #[test]
    fn gamma_cdf_half_shape_matches_erf() {
        let m = Marginal::Gamma(GammaParams {
            shape: 0.5,
            scale: 1.0,
        });
        let cdf1 = m.cdf(1.0);
        let expected = erf(1.0);
        assert!(
            (cdf1 - expected).abs() < 2e-7,
            "gamma(0.5,1).cdf(1) = {} want erf(1) = {}",
            cdf1,
            expected
        );
    }

    #[test]
    fn gamma_cdf_monotone_and_bounded() {
        let m = Marginal::Gamma(GammaParams {
            shape: 2.5,
            scale: 1.3,
        });
        let mut prev = 0.0;
        for i in 0..50 {
            let x = i as f64 * 0.5;
            let c = m.cdf(x);
            assert!(c >= prev, "gamma cdf not monotone at x={}", x);
            assert!((0.0..=1.0).contains(&c));
            prev = c;
        }
        assert!((m.cdf(0.0) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn gamma_ppf_roundtrip() {
        let m = Marginal::Gamma(GammaParams {
            shape: 2.0,
            scale: 3.0,
        });
        for &p in &[0.05, 0.25, 0.5, 0.75, 0.95] {
            let x = m.inverse_cdf(p);
            let back = m.cdf(x);
            assert!(
                (back - p).abs() < 1e-6,
                "gamma roundtrip p={} cdf(ppf(p))={}",
                p,
                back
            );
        }
    }

    #[test]
    fn beta_cdf_uniform_when_ab_equal_one() {
        let m = Marginal::Beta(BetaParams {
            a: 1.0,
            b: 1.0,
            loc: 0.0,
            scale: 1.0,
        });
        assert!((m.cdf(0.3) - 0.3).abs() < 1e-12);
    }

    // Beta(1,b)：CDF = 1-(1-x)^b
    #[test]
    fn beta_cdf_known_closed_form_b2() {
        let m = Marginal::Beta(BetaParams {
            a: 1.0,
            b: 2.0,
            loc: 0.0,
            scale: 1.0,
        });
        let expected = 1.0 - 0.7_f64.powi(2);
        assert!((m.cdf(0.3) - expected).abs() < 1e-12);
    }

    // Beta(a,1)：CDF = x^a
    #[test]
    fn beta_cdf_known_closed_form_a3() {
        let m = Marginal::Beta(BetaParams {
            a: 3.0,
            b: 1.0,
            loc: 0.0,
            scale: 1.0,
        });
        let expected = 0.3_f64.powi(3);
        assert!((m.cdf(0.3) - expected).abs() < 1e-12);
    }

    #[test]
    fn beta_cdf_support_boundaries() {
        let m = Marginal::Beta(BetaParams {
            a: 2.0,
            b: 5.0,
            loc: 10.0,
            scale: 20.0,
        });
        assert_eq!(m.cdf(9.999), 0.0);
        assert_eq!(m.cdf(30.001), 1.0);
    }

    #[test]
    fn beta_ppf_roundtrip() {
        let m = Marginal::Beta(BetaParams {
            a: 2.0,
            b: 5.0,
            loc: 0.0,
            scale: 1.0,
        });
        for &p in &[0.05, 0.25, 0.5, 0.75, 0.95] {
            let x = m.inverse_cdf(p);
            let back = m.cdf(x);
            assert!(
                (back - p).abs() < 1e-6,
                "beta roundtrip p={} cdf(ppf(p))={}",
                p,
                back
            );
        }
    }

    #[test]
    fn categorical_sample_index_follows_cumulative_weights() {
        let p = CategoricalParams {
            values: vec!["a".into(), "b".into(), "c".into()],
            weights: vec![0.5, 0.25, 0.25],
        };
        assert_eq!(p.sample_index(0.0), 0);
        assert_eq!(p.sample_index(0.49), 0);
        assert_eq!(p.sample_index(0.5), 1);
        assert_eq!(p.sample_index(0.74), 1);
        assert_eq!(p.sample_index(0.75), 2);
        assert_eq!(p.sample_index(0.999), 2);
    }

    #[test]
    fn categorical_sample_index_clamps_degenerate_weights() {
        let p = CategoricalParams {
            values: vec!["only".into()],
            weights: vec![1.0],
        };
        assert_eq!(p.sample_index(0.999999), 0);
    }

    #[test]
    fn normal_ppf_roundtrip_over_two_sigma() {
        let mut x = -2.0;
        while x <= 2.0 {
            let p = normal_cdf(x, 0.0, 1.0);
            let back = normal_ppf(0.0, 1.0, p);
            assert!(
                (back - x).abs() < 1e-8,
                "ppf(cdf({})) = {}, drift too large",
                x,
                back
            );
            x += 0.1;
        }
    }

    #[test]
    fn normal_ppf_median_is_loc() {
        assert!((normal_ppf(5.0, 2.0, 0.5) - 5.0).abs() < 1e-12);
    }

    fn normal_column_model(loc: f64, scale: f64) -> ColumnModel {
        ColumnModel {
            logical_type: crate::synth::model::LogicalType::Numerical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal: Marginal::Normal(NormalParams { loc, scale }),
            pii: None,
        }
    }

    #[test]
    fn correlation_detects_strong_linear_relationship() {
        let rows: Vec<Vec<serde_json::Value>> = (0..50)
            .map(|i| vec![serde_json::Value::from(i), serde_json::Value::from(3 * i)])
            .collect();
        let order = vec!["a".to_string(), "b".to_string()];
        // 边际参数须与数据分布一致（真实 train 由数据拟合），否则 PIT 饱和
        let columns = std::collections::HashMap::from([
            ("a".to_string(), normal_column_model(24.5, 15.0)),
            ("b".to_string(), normal_column_model(73.5, 45.0)),
        ]);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            corr[0][1] > 0.99,
            "linear columns must correlate strongly, got {}",
            corr[0][1]
        );
        assert_eq!(corr[0][0], 1.0);
        assert_eq!(corr[1][1], 1.0);
        assert!((corr[1][0] - corr[0][1]).abs() < 1e-12);
    }

    #[test]
    fn correlation_encodes_numeric_categorical_levels() {
        // 低基数值列按 Categorical 拟合后，top_values 键是数字的字符串形式
        // （"1"/"2"/"3"）；训练行里仍是 JSON 数值。相关矩阵不得因
        // as_str() 匹配失败而整列退化为 0.5 中点、相关系数静默归零。
        fn categorical_int_model() -> ColumnModel {
            ColumnModel {
                logical_type: crate::synth::model::LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                min: None,
                max: None,
                null_rate: None,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: vec!["1".to_string(), "2".to_string(), "3".to_string()],
                    weights: vec![1.0 / 3.0; 3],
                }),
                pii: None,
            }
        }

        let rows: Vec<Vec<serde_json::Value>> = (0..60)
            .map(|i| {
                let level = (i % 3 + 1) as i64;
                vec![
                    serde_json::Value::from(level),
                    serde_json::Value::from(level as f64 * 10.0),
                ]
            })
            .collect();
        let order = vec!["lvl".to_string(), "amt".to_string()];
        let columns = std::collections::HashMap::from([
            ("lvl".to_string(), categorical_int_model()),
            ("amt".to_string(), normal_column_model(20.0, 8.2)),
        ]);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            corr[0][1] > 0.5,
            "numeric-categorical column must correlate with its numeric partner, got {}",
            corr[0][1]
        );
    }

    #[test]
    fn correlation_handles_numeric_strings_from_drivers() {
        // DECIMAL/NUMBER 常被驱动序列化为 JSON 字符串；相关性不得因此静默归零
        let rows: Vec<Vec<serde_json::Value>> = (0..50)
            .map(|i| {
                vec![
                    serde_json::Value::from(i),
                    serde_json::Value::from(format!("{:.2}", 3.0 * i as f64)),
                ]
            })
            .collect();
        let order = vec!["a".to_string(), "b".to_string()];
        let columns = std::collections::HashMap::from([
            ("a".to_string(), normal_column_model(24.5, 15.0)),
            ("b".to_string(), normal_column_model(73.5, 45.0)),
        ]);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            corr[0][1] > 0.99,
            "numeric strings must still correlate, got {}",
            corr[0][1]
        );
    }

    #[test]
    fn correlation_near_zero_for_uncorrelated_columns() {
        let rows: Vec<Vec<serde_json::Value>> = (0..50)
            .map(|i| {
                vec![
                    serde_json::Value::from(i),
                    serde_json::Value::from((i % 2) * 100),
                ]
            })
            .collect();
        let order = vec!["a".to_string(), "b".to_string()];
        let columns = std::collections::HashMap::from([
            ("a".to_string(), normal_column_model(24.5, 15.0)),
            ("b".to_string(), normal_column_model(50.0, 1.0)),
        ]);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            corr[0][1].abs() < 0.2,
            "alternating column must be ~uncorrelated, got {}",
            corr[0][1]
        );
    }

    #[test]
    fn correlation_matrix_keeps_unit_diagonal_for_strongly_correlated_columns() {
        // Issue #89 S2a: the old PSD "fix" raised each diagonal entry to
        // sum(|off-diagonal|) + eps, so two perfectly correlated columns
        // (corr[0][1] ~ 1) produced diag = 2 + eps. That matrix was stored in
        // the model and inflated the copula's sampling variance 2x, shrinking
        // every generated correlation by half. The correlation matrix is a
        // correlation matrix: its diagonal must stay exactly 1.0 no matter
        // how strong the off-diagonal structure is.
        let rows: Vec<Vec<serde_json::Value>> = (0..50)
            .map(|i| {
                vec![
                    serde_json::Value::from(i),
                    serde_json::Value::from(3 * i),
                    serde_json::Value::from(7 * i),
                ]
            })
            .collect();
        let order = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let columns = std::collections::HashMap::from([
            ("a".to_string(), normal_column_model(24.5, 15.0)),
            ("b".to_string(), normal_column_model(73.5, 45.0)),
            ("c".to_string(), normal_column_model(171.5, 105.0)),
        ]);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        for (i, row) in corr.iter().enumerate() {
            assert!(
                (row[i] - 1.0).abs() < 1e-9,
                "diagonal must be exactly 1.0, got corr[{i}][{i}] = {}",
                row[i]
            );
        }
        assert!(
            corr[0][1] > 0.99 && corr[0][2] > 0.99 && corr[1][2] > 0.99,
            "linear columns must keep their strong correlation, got {}",
            corr[0][1]
        );
    }

    fn pit_standard_normal(x: f64, loc: f64, scale: f64) -> f64 {
        let marginal = Marginal::Normal(NormalParams { loc, scale });
        let standard = Marginal::Normal(NormalParams {
            loc: 0.0,
            scale: 1.0,
        });
        let u = marginal.cdf(x).clamp(1e-12, 1.0 - 1e-12);
        standard.inverse_cdf(u)
    }

    fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
        assert_eq!(xs.len(), ys.len());
        let n = xs.len() as f64;
        let mean_x = xs.iter().sum::<f64>() / n;
        let mean_y = ys.iter().sum::<f64>() / n;
        let mut cov = 0.0;
        let mut var_x = 0.0;
        let mut var_y = 0.0;
        for i in 0..xs.len() {
            let dx = xs[i] - mean_x;
            let dy = ys[i] - mean_y;
            cov += dx * dy;
            var_x += dx * dx;
            var_y += dy * dy;
        }
        cov / (var_x * var_y).sqrt()
    }

    #[test]
    fn should_use_pairwise_complete_correlation() {
        // Complete pairs are perfectly linear (b = 2a). Three trailing A
        // NULLs sit opposite large B values; filling those with loc would
        // fabricate a strong negative pull that pairwise deletion ignores.
        let rows: Vec<Vec<serde_json::Value>> = vec![
            vec![serde_json::Value::from(0.0), serde_json::Value::from(0.0)],
            vec![serde_json::Value::from(1.0), serde_json::Value::from(2.0)],
            vec![serde_json::Value::from(2.0), serde_json::Value::from(4.0)],
            vec![serde_json::Value::from(3.0), serde_json::Value::from(6.0)],
            vec![serde_json::Value::from(4.0), serde_json::Value::from(8.0)],
            vec![serde_json::Value::Null, serde_json::Value::from(100.0)],
            vec![serde_json::Value::Null, serde_json::Value::from(100.0)],
            vec![serde_json::Value::Null, serde_json::Value::from(100.0)],
        ];
        let loc_a = 2.0;
        let scale_a = 1.5;
        let loc_b = 4.0;
        let scale_b = 3.0;
        let order = vec!["a".to_string(), "b".to_string()];
        let columns = std::collections::HashMap::from([
            ("a".to_string(), normal_column_model(loc_a, scale_a)),
            ("b".to_string(), normal_column_model(loc_b, scale_b)),
        ]);

        let complete_a: Vec<f64> = (0..5)
            .map(|i| pit_standard_normal(i as f64, loc_a, scale_a))
            .collect();
        let complete_b: Vec<f64> = (0..5)
            .map(|i| pit_standard_normal(2.0 * i as f64, loc_b, scale_b))
            .collect();
        let pairwise_hand = pearson(&complete_a, &complete_b);

        let filled_a: Vec<f64> = (0..8)
            .map(|i| {
                let x = if i < 5 { i as f64 } else { loc_a };
                pit_standard_normal(x, loc_a, scale_a)
            })
            .collect();
        let filled_b: Vec<f64> = [0.0, 2.0, 4.0, 6.0, 8.0, 100.0, 100.0, 100.0]
            .into_iter()
            .map(|x| pit_standard_normal(x, loc_b, scale_b))
            .collect();
        let fill_hand = pearson(&filled_a, &filled_b);

        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            (corr[0][1] - pairwise_hand).abs() < 1e-6,
            "expected pairwise-complete Pearson {pairwise_hand}, got {}",
            corr[0][1]
        );
        assert!(
            (pairwise_hand - fill_hand).abs() > 0.2,
            "fixture must separate pairwise ({pairwise_hand}) from fill-with-loc ({fill_hand})"
        );
        assert!(
            (corr[0][1] - fill_hand).abs() > 0.2,
            "result must not match the old fill-with-loc Pearson {fill_hand}, got {}",
            corr[0][1]
        );
    }

    // ─── ECDF marginal (#66) ─────────────────────────────────────────────

    /// Deterministic uniform source for fixture generation; avoids depending
    /// on the RNG crates in unit tests.
    struct Lcg(u64);

    impl Lcg {
        fn next_u01(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }

        fn uniform(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next_u01()
        }
    }

    fn sorted_uniform_1_to(n: usize) -> Vec<f64> {
        (1..=n).map(|i| i as f64).collect()
    }

    #[test]
    fn ecdf_fitter_caps_knots_on_large_samples() {
        let samples: Vec<f64> = (1..=10_000).map(|i| i as f64).collect();
        let Marginal::Ecdf(p) = EcdfFitter.fit(&samples).unwrap() else {
            panic!("expected Ecdf");
        };
        assert!(
            p.knots.len() <= ECDF_MAX_KNOTS,
            "knots = {} > cap {}",
            p.knots.len(),
            ECDF_MAX_KNOTS
        );
        assert!(p.knots.len() > 1);
        assert!((p.knots[0] - 1.0).abs() < 1e-9);
        assert!((p.knots[p.knots.len() - 1] - 10_000.0).abs() < 1e-9);
    }

    #[test]
    fn ecdf_fitter_uses_uniform_probability_grid() {
        // Below the knot cap every sample becomes a knot, so the grid is the
        // exact empirical quantile function.
        let samples = sorted_uniform_1_to(100);
        let Marginal::Ecdf(p) = EcdfFitter.fit(&samples).unwrap() else {
            panic!("expected Ecdf");
        };
        assert_eq!(p.knots.len(), 100);
        assert!((p.knots[0] - 1.0).abs() < 1e-9);
        assert!(
            (p.knots[50] - 51.0).abs() < 1e-9,
            "median knot = {}",
            p.knots[50]
        );
        assert!((p.knots[99] - 100.0).abs() < 1e-9);
    }

    #[test]
    fn ecdf_cdf_and_inverse_are_mutually_consistent() {
        let samples = sorted_uniform_1_to(1000);
        let Marginal::Ecdf(p) = EcdfFitter.fit(&samples).unwrap() else {
            panic!("expected Ecdf");
        };
        for &u in &[0.05, 0.25, 0.5, 0.75, 0.95] {
            let x = p.inverse_cdf(u);
            let back = p.cdf(x);
            assert!(
                (back - u).abs() < 1e-9,
                "u = {u} -> x = {x} -> cdf = {back}"
            );
        }
    }

    #[test]
    fn ecdf_captures_zero_inflation_without_negative_values() {
        // 70% zeros, 30% Uniform(10, 100).
        let mut rng = Lcg(42);
        let samples: Vec<f64> = (0..10_000)
            .map(|i| {
                if i < 7_000 {
                    0.0
                } else {
                    rng.uniform(10.0, 100.0)
                }
            })
            .collect();
        let Marginal::Ecdf(p) = EcdfFitter.fit(&samples).unwrap() else {
            panic!("expected Ecdf");
        };

        let mass_at_zero = p.cdf(0.0);
        assert!(
            (0.65..=0.75).contains(&mass_at_zero),
            "F(0) = {mass_at_zero}"
        );
        assert_eq!(p.inverse_cdf(0.3), 0.0, "u below the mass point must be 0");
        assert!(
            p.inverse_cdf(0.85) > 10.0,
            "upper quantile = {}",
            p.inverse_cdf(0.85)
        );

        // Every generated value stays inside the observed range: no negatives
        // and no clipping fallback needed.
        for i in 0..=100 {
            let x = p.inverse_cdf(i as f64 / 100.0);
            assert!(x >= 0.0, "negative value {x} at u = {}", i as f64 / 100.0);
        }
    }

    #[test]
    fn ecdf_fit_is_deterministic() {
        let mut rng = Lcg(7);
        let samples: Vec<f64> = (0..5_000).map(|_| rng.uniform(-3.0, 9.0)).collect();
        let a = EcdfFitter.fit(&samples).unwrap();
        let mut shuffled = samples.clone();
        shuffled.reverse();
        let b = EcdfFitter.fit(&shuffled).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "fitting must not depend on input order"
        );
    }

    #[test]
    fn ecdf_serde_roundtrip_preserves_cdf() {
        let samples = sorted_uniform_1_to(200);
        let marginal = EcdfFitter.fit(&samples).unwrap();
        let json = serde_json::to_string(&marginal).unwrap();
        let loaded: Marginal = serde_json::from_str(&json).unwrap();
        for x in [1.0, 50.5, 137.25, 200.0, 250.0] {
            assert!((loaded.cdf(x) - marginal.cdf(x)).abs() < 1e-12);
        }
    }

    #[test]
    fn ecdf_degenerate_constant_sample_is_flat() {
        let samples = vec![5.0; 100];
        let Marginal::Ecdf(p) = EcdfFitter.fit(&samples).unwrap() else {
            panic!("expected Ecdf");
        };
        assert_eq!(p.cdf(4.0), 0.0);
        assert_eq!(p.cdf(5.0), 1.0);
        assert_eq!(p.inverse_cdf(0.5), 5.0);
    }

    // ─── PIT must cover every marginal variant (#66) ─────────────────────

    // ─── automatic marginal selection (#66) ──────────────────────────────

    /// Gamma(2, 1) is the sum of two unit exponentials, so `-ln u1 - ln u2`
    /// draws it exactly without a PPF round trip.
    fn gamma2_samples(seed: u64, n: usize) -> Vec<f64> {
        let mut rng = Lcg(seed);
        (0..n)
            .map(|_| {
                let u1 = rng.next_u01().max(1e-12);
                let u2 = rng.next_u01().max(1e-12);
                -u1.ln() - u2.ln()
            })
            .collect()
    }

    fn skewness(xs: &[f64]) -> f64 {
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let m2 = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let m3 = xs.iter().map(|x| (x - mean).powi(3)).sum::<f64>() / n;
        m3 / m2.powf(1.5)
    }

    /// Exact `P(X <= x)` of Gamma(2, 1) = `1 - e^-x (1 + x)`.
    fn gamma2_cdf(x: f64) -> f64 {
        1.0 - (-x).exp() * (1.0 + x)
    }

    /// Exact Gamma(2, 1) quantile by bisection on the closed-form CDF, so the
    /// shape assertions are not polluted by sampling noise.
    fn gamma2_quantile(p: f64) -> f64 {
        let (mut lo, mut hi) = (0.0f64, 50.0f64);
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if gamma2_cdf(mid) < p {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    #[test]
    fn auto_selection_recovers_skewed_shape_and_beats_normal_baseline() {
        let train = gamma2_samples(2024, 10_000);
        let chosen = fit_auto_numeric_marginal(&train).unwrap();
        assert!(
            matches!(chosen, Marginal::Gamma(_) | Marginal::Ecdf(_)),
            "skewed samples must not fall back to Normal, got {chosen:?}"
        );

        // Quantiles are compared against the analytic distribution, so the
        // 5% budget covers model error only, not Monte-Carlo noise.
        for p in [0.1, 0.5, 0.9] {
            let expected = gamma2_quantile(p);
            let got = chosen.inverse_cdf(p);
            let err = (got - expected).abs() / expected;
            assert!(
                err < 0.05,
                "p{p} relative error {err} (got {got}, want {expected})"
            );
        }

        // Generated skewness matches the analytic 2/sqrt(2) = 1.414.
        let mut rng = Lcg(99);
        let generated: Vec<f64> = (0..2_000)
            .map(|_| chosen.inverse_cdf(rng.next_u01()))
            .collect();
        let skew_gap = (std::f64::consts::SQRT_2 - skewness(&generated)).abs();
        assert!(skew_gap < 0.2, "skewness gap {skew_gap}");

        // The Normal baseline misses the tail the chosen marginal holds.
        // (Note: p90 is a poor discriminator here — the Normal p90 error is
        // ~2% — so the tail check uses p99, where it is ~20%.)
        let normal = NormalFitter.fit(&train).unwrap();
        let p99 = gamma2_quantile(0.99);
        let normal_err = (normal.inverse_cdf(0.99) - p99).abs() / p99;
        assert!(
            normal_err > 0.15,
            "fixture must expose the Normal p99 miss, got {normal_err}"
        );
        let chosen_err = (chosen.inverse_cdf(0.99) - p99).abs() / p99;
        assert!(chosen_err < 0.05, "chosen p99 relative error {chosen_err}");
    }

    #[test]
    fn auto_selection_picks_ecdf_for_zero_inflated_mixture() {
        let mut rng = Lcg(7);
        let train: Vec<f64> = (0..10_000)
            .map(|i| {
                if i < 7_000 {
                    0.0
                } else {
                    rng.uniform(10.0, 100.0)
                }
            })
            .collect();
        let chosen = fit_auto_numeric_marginal(&train).unwrap();
        assert!(
            matches!(chosen, Marginal::Ecdf(_)),
            "zero-inflated mixture must select Ecdf, got {chosen:?}"
        );

        let mut rng = Lcg(11);
        let generated: Vec<f64> = (0..10_000)
            .map(|_| chosen.inverse_cdf(rng.next_u01()))
            .collect();
        let zero_ratio =
            generated.iter().filter(|v| **v == 0.0).count() as f64 / generated.len() as f64;
        assert!(
            (0.65..=0.75).contains(&zero_ratio),
            "zero ratio {zero_ratio}"
        );
        assert!(
            generated.iter().all(|v| *v >= 0.0),
            "ECDF must not generate negative values without clipping"
        );
    }

    #[test]
    fn auto_selection_keeps_parametric_when_shape_matches() {
        // A near-perfect Normal sample must not be replaced by ECDF just
        // because ECDF wins on the training points by construction.
        let mut rng = Lcg(5);
        let standard = Marginal::Normal(NormalParams {
            loc: 0.0,
            scale: 1.0,
        });
        let train: Vec<f64> = (0..10_000)
            .map(|_| standard.inverse_cdf(rng.next_u01()))
            .collect();
        let chosen = fit_auto_numeric_marginal(&train).unwrap();
        assert!(
            matches!(chosen, Marginal::Normal(_)),
            "well-fitting Normal must stay parametric, got {chosen:?}"
        );
    }

    #[test]
    fn auto_selection_is_deterministic_and_order_independent() {
        let train = gamma2_samples(31, 3_000);
        let a = fit_auto_numeric_marginal(&train).unwrap();
        let mut reversed = train.clone();
        reversed.reverse();
        let b = fit_auto_numeric_marginal(&reversed).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn auto_selection_keeps_normal_for_constant_column() {
        let chosen = fit_auto_numeric_marginal(&[7.5; 500]).unwrap();
        assert!(matches!(chosen, Marginal::Normal(_)), "got {chosen:?}");
        assert_eq!(chosen.inverse_cdf(0.5), 7.5);
    }

    #[test]
    fn auto_selection_rejects_empty_samples() {
        assert!(fit_auto_numeric_marginal(&[]).is_err());
        assert!(fit_auto_numeric_marginal(&[f64::NAN, f64::INFINITY]).is_err());
    }

    fn column_model(marginal: Marginal) -> ColumnModel {
        ColumnModel {
            logical_type: crate::synth::model::LogicalType::Numerical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal,
            pii: None,
        }
    }

    /// A strictly monotone pair whose marginals are each fitted from their own
    /// column must come out strongly correlated: PIT must not degrade to a
    /// constant that reports r = 0.
    fn assert_monotone_pair_correlates(a: &[f64], b: &[f64], fit: impl Fn(&[f64]) -> Marginal) {
        assert_eq!(a.len(), b.len());
        let columns = HashMap::from([
            ("a".to_string(), column_model(fit(a))),
            ("b".to_string(), column_model(fit(b))),
        ]);
        let rows: Vec<Vec<serde_json::Value>> = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| vec![serde_json::Value::from(x), serde_json::Value::from(y)])
            .collect();
        let order = vec!["a".to_string(), "b".to_string()];
        let corr = compute_gaussian_correlation(&rows, &order, &columns);
        assert!(
            corr[0][1] > 0.99,
            "monotone pair must correlate, got {}",
            corr[0][1]
        );
    }

    #[test]
    fn pit_correlates_gamma_marginals() {
        let samples: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        let doubled: Vec<f64> = samples.iter().map(|&x| 2.0 * x).collect();
        assert_monotone_pair_correlates(&samples, &doubled, |s| GammaFitter.fit(s).unwrap());
    }

    #[test]
    fn pit_correlates_ecdf_marginals() {
        let samples: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        let doubled: Vec<f64> = samples.iter().map(|&x| 2.0 * x).collect();
        assert_monotone_pair_correlates(&samples, &doubled, |s| EcdfFitter.fit(s).unwrap());
    }

    #[test]
    fn pit_correlates_uniform_marginals() {
        let samples: Vec<f64> = (1..=60).map(|i| i as f64).collect();
        let doubled: Vec<f64> = samples.iter().map(|&x| 2.0 * x).collect();
        assert_monotone_pair_correlates(&samples, &doubled, |s| UniformFitter.fit(s).unwrap());
    }

    #[test]
    fn pit_correlates_beta_marginals() {
        // Stay inside the unit interval. Scaled Beta has no closed-form Beta
        // fit, so this pair is identical (rank correlation 1) which still
        // fails the old constant-0.5 fallback (zero variance -> r = 0).
        let samples: Vec<f64> = (1..=60).map(|i| i as f64 / 120.0).collect();
        assert_monotone_pair_correlates(&samples, &samples, |s| BetaFitter.fit(s).unwrap());
    }
}
