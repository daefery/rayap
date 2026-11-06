//! Freshness benchmark: how stale do stored copies get when the crawler can only afford a
//! fixed number of revisits, and how much does learning each page's change rate help?
//!
//! Pages change at hidden Poisson rates. A crawler with `budget` revisits per day watches
//! them for `days`. After each visit it learns only whether the page changed since the last
//! visit (as rayap does from its content fingerprint). Policies:
//! - `uniform`: every page revisited equally often (round robin);
//! - `adaptive`: rayap's planner. It estimates each page's rate from its own visit history
//!   (`revisit::estimate_rate`) and re-plans frequencies daily (`revisit::allocate`);
//! - `oracle`: the same planner given the true rates. This is the best a rate-based plan can do.
//!
//! The simulator knows when every change happened, so it measures exactly:
//! - freshness: the share of page-time the stored copy equals the live page;
//! - staleness delay: the time from a page's first unseen change until the crawler sees it;
//! - changes caught per 1,000 fetches: visits that found a change.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rayap_core::revisit::{self, Observation, Prior};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    Uniform,
    Adaptive,
    Oracle,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Outcome {
    pub policy: &'static str,
    pub fetches: u64,
    pub fetches_that_found_a_change: u64,
    pub changes_caught_per_1000_fetches: f64,
    /// Share of page-time (during the measured period) that the stored copy was current.
    pub freshness: f64,
    pub mean_staleness_hours: f64,
    pub p90_staleness_hours: f64,
    /// Changes that happened but were never seen before the end (overwritten or unvisited).
    pub pages_never_refreshed_after_a_change: u64,
}

/// A small, fast PRNG (SplitMix64) so runs are reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn exp(&mut self, rate: f64) -> f64 {
        -(1.0 - self.unit()).ln() / rate
    }
}

/// Change times for every page over [0, days), generated once and shared by all policies.
pub fn change_times(rates: &[f64], days: f64, seed: u64) -> Vec<Vec<f64>> {
    let mut rng = Rng::new(seed);
    rates
        .iter()
        .map(|&rate| {
            let mut t = 0.0;
            let mut out = Vec::new();
            if rate > 0.0 {
                loop {
                    t += rng.exp(rate);
                    if t >= days {
                        break;
                    }
                    out.push(t);
                }
            }
            out
        })
        .collect()
}

#[derive(PartialEq)]
struct Due(f64, usize);
impl Eq for Due {}
impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Due {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap on time.
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}

pub struct Setup<'a> {
    pub rates: &'a [f64],
    pub changes: &'a [Vec<f64>],
    pub days: f64,
    /// Revisits per day, over all pages.
    pub budget: f64,
    /// Results are measured from this day on (earlier days let the adaptive policy learn).
    pub warmup_days: f64,
    /// Smallest revisit frequency per page, per day.
    pub floor: f64,
    pub prior: Prior,
    pub seed: u64,
}

pub fn run(policy: Policy, s: &Setup) -> Outcome {
    let n = s.rates.len();
    let mut rng = Rng::new(s.seed ^ 0xF00D);
    let weights = vec![1.0; n];
    let uniform = s.budget / n as f64;

    // Per page: index of the next change not yet captured, time of the last visit, and the
    // visit history (adaptive only).
    let mut next_change = vec![0usize; n];
    let mut last_visit = vec![0.0f64; n];
    let mut history: Vec<Vec<Observation>> = vec![Vec::new(); n];
    // Every page starts fresh at t=0 (first crawl), with its first revisit at a random phase.
    let mut freq: Vec<f64> = match policy {
        Policy::Uniform | Policy::Adaptive => vec![uniform; n],
        Policy::Oracle => revisit::allocate(s.rates, &weights, s.budget, s.floor),
    };
    // The crawler has exactly `budget` fetch slots per day, evenly spaced, under every policy.
    // Each slot goes to the page whose next visit is most overdue (earliest due time), the way
    // a rate-limited crawler drains its frontier. A policy only decides each page's due time.
    let mut heap: BinaryHeap<Due> = (0..n)
        .map(|i| Due(rng.unit() / freq[i].max(1e-12), i))
        .collect();
    let slot = 1.0 / s.budget;
    let mut k: u64 = 1;

    let mut out = Outcome::default();
    let mut stale_time = 0.0;
    let mut delays: Vec<f64> = Vec::new();
    let mut next_replan = 1.0;

    loop {
        let t = k as f64 * slot;
        if t >= s.days {
            break;
        }
        k += 1;
        if policy == Policy::Adaptive && t >= next_replan {
            let est: Vec<f64> = history
                .iter()
                .map(|h| revisit::estimate_rate(h, s.prior))
                .collect();
            freq = revisit::allocate(&est, &weights, s.budget, s.floor);
            next_replan += 1.0;
            heap = (0..n)
                .map(|j| Due(last_visit[j] + 1.0 / freq[j], j))
                .collect();
        }
        let Some(Due(_, i)) = heap.pop() else { break };
        // Visit page i at time t.
        let ch = &s.changes[i];
        let first = ch.get(next_change[i]).copied().filter(|c| *c <= t);
        let changed = first.is_some();
        if let Some(c) = first {
            // Stale from its first unseen change until now.
            let from = c.max(s.warmup_days);
            if t > from {
                stale_time += t - from;
            }
            if c >= s.warmup_days {
                delays.push(t - c);
            }
        }
        while next_change[i] < ch.len() && ch[next_change[i]] <= t {
            next_change[i] += 1;
        }
        if t >= s.warmup_days {
            out.fetches += 1;
            if changed {
                out.fetches_that_found_a_change += 1;
            }
        }
        if policy == Policy::Adaptive {
            history[i].push(Observation {
                interval: t - last_visit[i],
                changed,
            });
        }
        last_visit[i] = t;
        heap.push(Due(t + 1.0 / freq[i], i));
    }
    // Pages that are stale at the end: count staleness until the end.
    for (ch, &next) in s.changes.iter().zip(&next_change) {
        if let Some(&c) = ch.get(next) {
            let from = c.max(s.warmup_days);
            stale_time += s.days - from;
            if c >= s.warmup_days {
                out.pages_never_refreshed_after_a_change += 1;
            }
        }
    }
    let measured = (s.days - s.warmup_days) * n as f64;
    out.policy = match policy {
        Policy::Uniform => "uniform",
        Policy::Adaptive => "adaptive",
        Policy::Oracle => "oracle",
    };
    out.freshness = 1.0 - stale_time / measured;
    out.changes_caught_per_1000_fetches =
        1000.0 * out.fetches_that_found_a_change as f64 / out.fetches.max(1) as f64;
    delays.sort_by(f64::total_cmp);
    if !delays.is_empty() {
        out.mean_staleness_hours = 24.0 * delays.iter().sum::<f64>() / delays.len() as f64;
        out.p90_staleness_hours = 24.0 * delays[(delays.len() - 1) * 9 / 10];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup<'a>(rates: &'a [f64], changes: &'a [Vec<f64>], budget: f64) -> Setup<'a> {
        Setup {
            rates,
            changes,
            days: 60.0,
            budget,
            warmup_days: 20.0,
            floor: 1.0 / 30.0,
            prior: Prior {
                rate: 0.2,
                weight: 0.5,
            },
            seed: 1,
        }
    }

    #[test]
    fn uniform_freshness_matches_the_formula() {
        // One rate for every page: measured freshness must match F(λ, f).
        let rates = vec![0.5; 4000];
        let changes = change_times(&rates, 60.0, 9);
        let s = setup(&rates, &changes, 4000.0 * 0.7);
        let o = run(Policy::Uniform, &s);
        let expected = revisit::freshness(0.5, 0.7);
        assert!(
            (o.freshness - expected).abs() < 0.01,
            "measured {} formula {expected}",
            o.freshness
        );
    }

    #[test]
    fn oracle_beats_uniform_and_adaptive_approaches_oracle() {
        let mut rng = Rng::new(5);
        let rates: Vec<f64> = (0..4000)
            .map(|_| {
                if rng.unit() < 0.3 {
                    0.0
                } else {
                    10f64.powf(-2.0 + 3.0 * rng.unit())
                }
            })
            .collect();
        let changes = change_times(&rates, 60.0, 11);
        let s = setup(&rates, &changes, 4000.0 / 5.0);
        let u = run(Policy::Uniform, &s);
        let a = run(Policy::Adaptive, &s);
        let o = run(Policy::Oracle, &s);
        assert!(
            o.freshness > u.freshness + 0.03,
            "oracle {} uniform {}",
            o.freshness,
            u.freshness
        );
        assert!(
            a.freshness > u.freshness,
            "adaptive {} uniform {}",
            a.freshness,
            u.freshness
        );
        assert!(
            a.freshness > o.freshness - 0.05,
            "adaptive {} oracle {}",
            a.freshness,
            o.freshness
        );
        let spent = |x: &Outcome| x.fetches as f64;
        assert_eq!(spent(&a), spent(&u), "same budget");
        assert_eq!(spent(&o), spent(&u), "same budget");
    }
}
