//! Revisit planning: estimate how often each page changes, then split a fetch budget across
//! pages so the stored copies stay as fresh as possible.
//!
//! The model is the standard one from the crawling literature: each page changes as a
//! Poisson process with its own rate λ, and a crawler that revisits a page at fixed
//! intervals of 1/f keeps it fresh for the expected fraction
//! F(λ, f) = (f/λ)(1 − e^(−λ/f)) of the time (Cho & Garcia-Molina, "Effective page refresh
//! policies for Web crawlers", ACM TODS 2003). Maximising Σ wᵢ·F(λᵢ, fᵢ) under Σ fᵢ = budget
//! gives every page the same marginal value wᵢ·∂F/∂fᵢ, and some pages get nothing: those
//! that change too often to be worth chasing.
//!
//! Units are the caller's: if rates are per day, frequencies and the budget are per day.

use std::sync::OnceLock;

/// What one revisit showed: the time since the previous visit and whether the page had changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub interval: f64,
    pub changed: bool,
}

/// A weak prior on a page's change rate, so pages with few or no observed changes still get a
/// finite, positive estimate. `weight` is the prior's strength in pseudo-changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prior {
    pub rate: f64,
    pub weight: f64,
}

/// Most likely change rate (posterior mode under a Gamma prior with mode `prior.rate`).
///
/// A revisit only shows *whether* the page changed, not how many times, so the likelihood
/// of a changed interval Δ is 1 − e^(−λΔ) and of an unchanged one e^(−λΔ). The naive
/// estimate "changes ÷ time" undercounts fast-changing pages; this one does not.
pub fn estimate_rate(observations: &[Observation], prior: Prior) -> f64 {
    let a = prior.weight.max(1e-9);
    let b = a / prior.rate.max(1e-12);
    let unchanged_time: f64 = observations
        .iter()
        .filter(|o| !o.changed)
        .map(|o| o.interval)
        .sum();
    // Derivative of the log posterior; strictly decreasing in λ, so the root is unique.
    let slope = |rate: f64| {
        let changed: f64 = observations
            .iter()
            .filter(|o| o.changed && o.interval > 0.0)
            .map(|o| o.interval / (rate * o.interval).exp_m1())
            .sum();
        a / rate - b - unchanged_time + changed
    };
    let (mut lo, mut hi) = (1e-12f64.ln(), 1e9f64.ln());
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if slope(mid.exp()) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)).exp()
}

/// Expected fraction of time a page with change rate `rate` is fresh when revisited every
/// 1/`frequency`.
pub fn freshness(rate: f64, frequency: f64) -> f64 {
    if rate <= 0.0 {
        return 1.0;
    }
    if frequency <= 0.0 {
        return 0.0;
    }
    let r = rate / frequency;
    -(-r).exp_m1() / r
}

/// g(r) = 1 − e^(−r)(1 + r): λ·∂F/∂f expressed in r = λ/f. Increasing from 0 to 1.
fn g(r: f64) -> f64 {
    -(-r).exp_m1() - r * (-r).exp()
}

const R_MIN: f64 = 1e-6;
const R_MAX: f64 = 60.0;
const TABLE: usize = 4096;

/// g sampled on a log grid of r, for fast inversion.
fn g_table() -> &'static [(f64, f64)] {
    static T: OnceLock<Vec<(f64, f64)>> = OnceLock::new();
    T.get_or_init(|| {
        (0..TABLE)
            .map(|i| {
                let r = (R_MIN.ln() + (R_MAX / R_MIN).ln() * i as f64 / (TABLE - 1) as f64).exp();
                (g(r), r)
            })
            .collect()
    })
}

/// r such that g(r) = c, for 0 < c < 1.
fn g_inverse(c: f64) -> f64 {
    let t = g_table();
    if c <= t[0].0 {
        return (2.0 * c).sqrt(); // g(r) ≈ r²/2 near 0
    }
    if c >= t[TABLE - 1].0 {
        return R_MAX;
    }
    let i = t.partition_point(|(gv, _)| *gv < c);
    let ((g0, r0), (g1, r1)) = (t[i - 1], t[i]);
    let x = (c - g0) / (g1 - g0);
    (r0.ln() + x * (r1.ln() - r0.ln())).exp()
}

/// Splits `budget` (visits per time unit, summed over pages) across pages to maximise the
/// weighted expected freshness, giving every page at least `floor` (so pages believed never
/// to change are still checked now and then). `rates` and `weights` are per page.
///
/// If the budget cannot cover the floor, every page gets an equal share.
pub fn allocate(rates: &[f64], weights: &[f64], budget: f64, floor: f64) -> Vec<f64> {
    assert_eq!(rates.len(), weights.len());
    let n = rates.len();
    if n == 0 {
        return Vec::new();
    }
    if budget <= floor * n as f64 {
        return vec![budget / n as f64; n];
    }
    // Frequency each page would get at marginal value μ.
    let at = |mu: f64, i: usize| -> f64 {
        let (rate, w) = (rates[i], weights[i]);
        if rate <= 0.0 || w <= 0.0 {
            return floor;
        }
        let c = mu * rate / w;
        if c >= 1.0 {
            return floor;
        }
        (rate / g_inverse(c)).max(floor)
    };
    let total = |mu: f64| (0..n).map(|i| at(mu, i)).sum::<f64>();
    let max_mu = (0..n)
        .filter(|&i| rates[i] > 0.0)
        .map(|i| weights[i] / rates[i])
        .fold(0.0f64, f64::max);
    if max_mu <= 0.0 {
        return vec![budget / n as f64; n];
    }
    // total(μ) decreases from +∞ (μ → 0) to n·floor (μ ≥ max_mu): bisect on log μ.
    let (mut lo, mut hi) = ((max_mu * 1e-15).ln(), max_mu.ln());
    for _ in 0..70 {
        let mid = 0.5 * (lo + hi);
        if total(mid.exp()) > budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mu = hi.exp();
    let mut f: Vec<f64> = (0..n).map(|i| at(mu, i)).collect();
    // Hand the small bisection remainder to the floor-only pages, keeping Σf = budget exactly.
    let spent: f64 = f.iter().sum();
    let scale = budget / spent;
    for x in &mut f {
        *x *= scale;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> f64 {
        let mut s = seed;
        move || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn freshness_limits() {
        assert_eq!(freshness(0.0, 1.0), 1.0);
        assert_eq!(freshness(1.0, 0.0), 0.0);
        assert!(
            (freshness(1e-9, 1.0) - 1.0).abs() < 1e-6,
            "rarely changing: almost always fresh"
        );
        assert!(
            freshness(100.0, 1.0) < 0.011,
            "changes far faster than visits: almost never fresh"
        );
        assert!((freshness(1.0, 1.0) - (1.0 - (-1.0f64).exp())).abs() < 1e-12);
    }

    #[test]
    fn g_inverse_round_trips() {
        for c in [1e-14, 1e-9, 1e-4, 0.01, 0.3, 0.5, 0.9, 0.999, 0.999_999] {
            let r = g_inverse(c);
            assert!((g(r) - c).abs() / c < 2e-3, "c={c} r={r} g={}", g(r));
        }
    }

    #[test]
    fn estimate_recovers_known_rates_from_revisits() {
        let mut u = rng(7);
        for (rate, interval) in [(0.1, 1.0), (1.0, 1.0), (5.0, 1.0), (0.5, 3.0)] {
            let obs: Vec<Observation> = (0..2000)
                .map(|_| Observation {
                    interval,
                    changed: u() < 1.0 - (-rate * interval).exp(),
                })
                .collect();
            let est = estimate_rate(
                &obs,
                Prior {
                    rate: 0.2,
                    weight: 0.5,
                },
            );
            assert!(
                (est - rate).abs() / rate < 0.12,
                "rate {rate} interval {interval}: estimated {est}"
            );
            // The naive ratio is biased low when changes are frequent relative to visits.
            let naive = obs.iter().filter(|o| o.changed).count() as f64 / (2000.0 * interval);
            if rate * interval > 2.0 {
                assert!(naive < 0.5 * rate, "naive {naive} should undercount {rate}");
            }
        }
    }

    #[test]
    fn no_observed_changes_gives_a_small_positive_rate() {
        let obs = vec![
            Observation {
                interval: 7.0,
                changed: false
            };
            10
        ];
        let est = estimate_rate(
            &obs,
            Prior {
                rate: 0.1,
                weight: 0.5,
            },
        );
        assert!(est > 0.0 && est < 0.01, "{est}");
        assert!(
            (estimate_rate(
                &[],
                Prior {
                    rate: 0.1,
                    weight: 0.5
                }
            ) - 0.1)
                .abs()
                < 1e-6,
            "no data: the prior"
        );
    }

    #[test]
    fn allocation_spends_the_budget_and_equalises_marginal_value() {
        let rates = [0.0, 0.01, 0.1, 0.5, 1.0, 3.0, 50.0];
        let w = [1.0; 7];
        let budget = 2.0;
        let floor = 0.01;
        let f = allocate(&rates, &w, budget, floor);
        assert!((f.iter().sum::<f64>() - budget).abs() < 1e-9);
        assert!(f.iter().all(|x| *x >= floor * 0.999));
        assert!(
            (f[0] - floor).abs() < 1e-3,
            "never-changing page gets the floor: {f:?}"
        );
        assert!(
            (f[6] - floor).abs() < 1e-3,
            "page changing 50x per unit is not worth chasing: {f:?}"
        );
        // Interior pages share one marginal value.
        let marginal = |i: usize| {
            let r = rates[i] / f[i];
            g(r) / rates[i]
        };
        let interior: Vec<f64> = (1..6)
            .filter(|&i| f[i] > floor * 1.5)
            .map(marginal)
            .collect();
        let (lo, hi) = interior
            .iter()
            .fold((f64::MAX, 0.0f64), |(a, b), x| (a.min(*x), b.max(*x)));
        assert!(hi / lo < 1.02, "marginal values {interior:?}");
    }

    #[test]
    fn optimal_allocation_beats_uniform_in_expectation() {
        let mut u = rng(3);
        let rates: Vec<f64> = (0..5000)
            .map(|_| {
                if u() < 0.25 {
                    0.0
                } else {
                    10f64.powf(-2.5 + 3.5 * u())
                }
            })
            .collect();
        let w = vec![1.0; rates.len()];
        let budget = rates.len() as f64 / 7.0;
        let f = allocate(&rates, &w, budget, 1.0 / 60.0);
        let mean = |fs: &[f64]| {
            rates
                .iter()
                .zip(fs)
                .map(|(l, x)| freshness(*l, *x))
                .sum::<f64>()
                / rates.len() as f64
        };
        let uniform = vec![budget / rates.len() as f64; rates.len()];
        assert!(
            mean(&f) > mean(&uniform) + 0.05,
            "optimal {} uniform {}",
            mean(&f),
            mean(&uniform)
        );
    }

    #[test]
    fn tiny_budget_is_shared_equally() {
        let f = allocate(&[1.0, 2.0], &[1.0, 1.0], 0.01, 0.1);
        assert_eq!(f, vec![0.005, 0.005]);
    }
}
