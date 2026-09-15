//! Distribution-distance statistics shared by marginal auto-selection (#66)
//! and the quality report (#67). Pure functions, no IO.

/// A reference CDF used as the null hypothesis of a KS test.
///
/// `cdf_left` is the left limit `F(x⁻)`. It only differs from `cdf` when the
/// reference carries an atom at `x` (an ECDF / quantile sketch does). Using
/// `cdf` for both against a discontinuous reference mixes the right limit of
/// the reference with the left limit of the empirical CDF and invents a
/// deviation the size of the atom.
pub trait ReferenceCdf {
    fn cdf(&self, x: f64) -> f64;

    fn cdf_left(&self, x: f64) -> f64 {
        self.cdf(x)
    }
}

impl<F: Fn(f64) -> f64> ReferenceCdf for F {
    fn cdf(&self, x: f64) -> f64 {
        self(x)
    }
}

/// Kolmogorov-Smirnov statistic between an empirical sample and a reference
/// CDF: `max |F_n(x) - F(x)|`.
///
/// Ties are handled with run-length groups: each group of equal values
/// contributes the deviation just below the value and just at/above it. The
/// input is sorted internally with `total_cmp`; non-finite samples are ignored
/// so one bad value cannot poison the statistic.
pub fn ks_statistic(samples: &[f64], reference: impl ReferenceCdf) -> f64 {
    let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(f64::total_cmp);

    let n = sorted.len() as f64;
    let mut d = 0.0f64;
    let mut i = 0usize;
    while i < sorted.len() {
        let x = sorted[i];
        let mut j = i;
        while j < sorted.len() && sorted[j] == x {
            j += 1;
        }
        let before = i as f64 / n;
        let after = j as f64 / n;
        let at = reference.cdf(x).clamp(0.0, 1.0);
        let below = reference.cdf_left(x).clamp(0.0, 1.0);
        d = d.max((after - at).abs()).max((below - before).abs());
        i = j;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_use_group_boundaries_for_step_deviations() {
        // Reference is a step function whose steps coincide with the sample
        // jumps, so the one-sided deviation equals the jump size (1/4).
        let d = ks_statistic(&[2.0, 4.0, 6.0, 8.0], |x: f64| match x {
            _ if x < 2.0 => 0.0,
            _ if x < 4.0 => 0.25,
            _ if x < 6.0 => 0.5,
            _ if x < 8.0 => 0.75,
            _ => 1.0,
        });
        assert!((d - 0.25).abs() < 1e-12, "d = {}", d);
    }

    #[test]
    fn should_measure_uniform_sample_against_uniform_cdf() {
        let samples: Vec<f64> = (1..=100).map(|i| i as f64 / 100.0).collect();
        let d = ks_statistic(&samples, |x: f64| x.clamp(0.0, 1.0));
        assert!((d - 0.01).abs() < 1e-9, "d = {}", d);
    }

    #[test]
    fn should_detect_shifted_sample_against_normal_cdf() {
        // All mass far in the right tail: F_n jumps to 1 while F(x) ~ 0.
        let samples = vec![10.0; 16];
        let d = ks_statistic(&samples, |x: f64| {
            0.5 * (1.0 + (x / std::f64::consts::SQRT_2).tanh())
        });
        assert!(d > 0.9, "d = {}", d);
    }

    #[test]
    fn should_handle_ties_by_group_not_per_duplicate() {
        // Ten zeros against an Exponential(1) CDF: F_n jumps to 1 at x=0 where
        // F(0)=0, so D = 1.
        let samples = vec![0.0; 10];
        let d = ks_statistic(&samples, |x: f64| 1.0 - (-x).exp());
        assert!((d - 1.0).abs() < 1e-12, "d = {}", d);
    }

    #[test]
    fn should_not_penalise_a_reference_whose_atom_matches_the_sample() {
        // Reference is the exact CDF of the sample: half its mass at 0, half
        // at 1. Mixing F(0) with the pre-jump empirical value would report
        // 0.5 instead of 0.
        struct HalfAtom;
        impl ReferenceCdf for HalfAtom {
            fn cdf(&self, x: f64) -> f64 {
                if x < 0.0 {
                    0.0
                } else if x < 1.0 {
                    0.5
                } else {
                    1.0
                }
            }

            fn cdf_left(&self, x: f64) -> f64 {
                if x <= 0.0 {
                    0.0
                } else if x <= 1.0 {
                    0.5
                } else {
                    1.0
                }
            }
        }

        let samples = vec![0.0, 0.0, 1.0, 1.0];
        let d = ks_statistic(&samples, HalfAtom);
        assert!(d < 1e-12, "d = {}", d);
    }

    #[test]
    fn should_ignore_non_finite_samples() {
        let samples = vec![f64::NAN, 1.0, 2.0, f64::INFINITY];
        let d = ks_statistic(&samples, |x: f64| if x < 1.5 { 0.0 } else { 1.0 });
        assert!((d - 0.5).abs() < 1e-12, "d = {}", d);
    }

    #[test]
    fn should_return_zero_for_empty_sample() {
        assert_eq!(ks_statistic(&[], |_x: f64| 0.5), 0.0);
    }
}
