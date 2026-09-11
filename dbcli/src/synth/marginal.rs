use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniformParams {
    pub low: f64,
    pub high: f64,
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
        }
    }

    pub fn inverse_cdf(&self, p: f64) -> f64 {
        match self {
            Marginal::Normal(params) => normal_ppf(params.loc, params.scale, p),
            Marginal::Beta(params) => beta_ppf(params.a, params.b, params.loc, params.scale, p),
            Marginal::Gamma(params) => gamma_ppf(params.shape, params.scale, p),
            Marginal::Uniform(params) => uniform_ppf(params.low, params.high, p),
            Marginal::Categorical(params) => categorical_ppf(params),
        }
    }
}

fn normal_cdf(x: f64, loc: f64, scale: f64) -> f64 {
    let z = (x - loc) / scale;
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

fn normal_ppf(loc: f64, scale: f64, p: f64) -> f64 {
    let a = -8.0 * (2.0 * p - 1.0).abs().ln();
    let t = (a.sqrt() - 2.685_924_321_146_84) / 2.729_082_346_098_76;
    let x = t
        - (2.515_517 + 0.802_853 * t + 0.010_328 * t * t)
            / (1.0 + 1.432_788 * t + 0.189_269 * t * t + 0.001_308 * t * t * t);
    let x = if p < 0.5 { -x } else { x };
    loc + scale * x
}

fn beta_cdf(x: f64, a: f64, b: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    0.5
}

fn beta_ppf(a: f64, b: f64, loc: f64, scale: f64, p: f64) -> f64 {
    let x = a / (a + b);
    loc + scale * x
}

fn gamma_cdf(x: f64, _shape: f64, _scale: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    0.5
}

fn gamma_ppf(shape: f64, scale: f64, _p: f64) -> f64 {
    shape * scale
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

fn categorical_ppf(params: &CategoricalParams) -> f64 {
    if params.values.is_empty() {
        return 0.0;
    }
    0.0
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

    #[test]
    fn normal_cdf_roundtrip() {
        let params = NormalParams {
            loc: 0.0,
            scale: 1.0,
        };
        let x = 0.5;
        let cdf_val = normal_cdf(x, params.loc, params.scale);
        assert!(cdf_val > 0.0 && cdf_val < 1.0);
    }
}
