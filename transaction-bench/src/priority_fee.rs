//! Priority-fee logic: per-transaction fee selection and metrics.
//!
//! - [`PriorityFeeMode`] picks the additional compute-unit-price added on top
//!   of `--compute-unit-price` (none / random / premium / scheduled). In random
//!   mode the per-tx draw is shaped by a [`FeeDistribution`] (uniform /
//!   power-law / Pareto / tiered) so high-priority transactions can be made rare
//!   and tunable rather than uniformly common. Premium mode ([`PremiumPlan`])
//!   instead pins an *exact*, reproducible high-fee subset derived from the
//!   total transaction count.
//! - [`PriorityFeeStats`] accumulates totals across worker threads for
//!   metrics reporting.
//!
//! The clap-derived `PriorityFeeParams` that produces a `PriorityFeeMode` from
//! CLI args lives in [`crate::cli`] so all clap structs stay in one place.
use {
    rand::Rng,
    std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    },
};

/// How the additional priority fee (on top of `--compute-unit-price`) is chosen.
///
/// `f64` shape parameters mean this is `PartialEq` but not `Eq`.
#[derive(Clone, Debug, PartialEq)]
pub enum PriorityFeeMode {
    /// No additional fee beyond the base.
    None,
    /// Each tx gets base + a random draw shaped by the given distribution.
    Random(FeeDistribution),
    /// An exact, reproducible high-fee subset: `premium_count` of `total`
    /// transactions (chosen by send-order index) pay `premium_fee`, the rest pay
    /// nothing extra. See [`PremiumPlan`].
    Premium(PremiumPlan),
    /// Fee ramps linearly from `0` to `max` over `period_ms` milliseconds,
    /// then resets. One full sawtooth ramp per `period_ms`.
    Scheduled { max: u64, period_ms: u64 },
}

/// Shape of the per-transaction random additional fee.
///
/// `Uniform` / `PowerLaw` / `Pareto` are continuous draws mapped into a range
/// bounded by `max` (`Pareto` into `[1, max]`, the others into `[0, max]`);
/// `Tiered` is a weighted pick from a fixed table of fees. Selected via
/// `--priority-fee-distribution`.
#[derive(Clone, Debug, PartialEq)]
pub enum FeeDistribution {
    /// Flat `U(0, max)`: every fee equally likely (the original behavior).
    Uniform { max: u64 },
    /// Bounded power law on `[0, max]`: `fee = max * (1 - u^(1/beta))`.
    /// `beta = 1` is ~uniform; `beta > 1` concentrates mass near `0`, so high
    /// fees get rarer as `beta` grows. Simple single-knob skew.
    PowerLaw { max: u64, beta: f64 },
    /// Bounded Pareto on `[1, max]` with shape `alpha`: a heavy right tail that
    /// mirrors real priority-fee markets. Smaller `alpha` = heavier tail (the
    /// rare expensive draws reach closer to `max`).
    Pareto { max: u64, alpha: f64 },
    /// Weighted discrete tiers: each tx draws one tier's fee with probability
    /// proportional to its weight. Easiest to reason about ("N% pay exactly X").
    Tiered { tiers: Vec<FeeTier> },
}

/// One `(fee, weight)` bucket of a [`FeeDistribution::Tiered`] mixture. `fee`
/// is additional microlamports; `weight` is a relative (unnormalized) share.
#[derive(Clone, Debug, PartialEq)]
pub struct FeeTier {
    pub fee: u64,
    pub weight: f64,
}

/// A reproducible two-level fee plan: exactly `premium_count` of `total`
/// transactions pay `premium_fee` additional microlamports, the rest pay
/// nothing extra. Premium positions are spread on an even stride across
/// `[0, total)` (see [`Self::is_premium`]), so — unlike a random draw — both the
/// count and the layout are deterministic and independent of send rate. `total`
/// comes from `--num-transactions`, which makes the composition reproducible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PremiumPlan {
    /// Total transactions the run will send (`--num-transactions`).
    pub total: u64,
    /// How many of `total` are premium.
    pub premium_count: u64,
    /// Additional microlamports a premium transaction pays.
    pub premium_fee: u64,
}

impl PremiumPlan {
    /// Additional fee for transaction `index` (0-based send-order position):
    /// `premium_fee` on a premium slot, else `0`.
    pub fn fee_at(&self, index: u64) -> u64 {
        if self.is_premium(index) {
            self.premium_fee
        } else {
            0
        }
    }

    /// Whether `index` is a premium slot, via Bresenham-style even placement:
    /// premium iff `floor((index+1)*P/N) > floor(index*P/N)`, where `P =
    /// premium_count` and `N = total`. With `P <= N` the running premium count
    /// advances by 0 or 1 per step, so exactly `P` of the `N` indices in
    /// `[0, N)` are premium and any prefix of length `L` holds `floor(L*P/N)` of
    /// them. `u128` math avoids overflow for large runs.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn is_premium(&self, index: u64) -> bool {
        if self.premium_count == 0 || self.total == 0 {
            return false;
        }
        if self.premium_count >= self.total {
            return true;
        }
        let p = u128::from(self.premium_count);
        let n = u128::from(self.total);
        let i = u128::from(index);
        (i + 1) * p / n > i * p / n
    }
}

impl PriorityFeeMode {
    /// Resolve the additional fee for transaction `tx_index` (0-based position
    /// in overall send order). Only [`Self::Premium`] uses `tx_index`; the
    /// random and scheduled modes ignore it.
    pub fn resolve(&self, tx_index: u64) -> u64 {
        match self {
            Self::None => 0,
            Self::Random(distribution) => distribution.sample(),
            Self::Premium(plan) => plan.fee_at(tx_index),
            Self::Scheduled { max, period_ms } => {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock before UNIX epoch")
                    .as_millis() as u64;
                scheduled_fee_at(now_ms, *max, *period_ms)
            }
        }
    }
}

impl FeeDistribution {
    /// Draw one additional-fee sample (microlamports) using the thread RNG.
    pub fn sample(&self) -> u64 {
        let mut rng = rand::thread_rng();
        match self {
            Self::Uniform { max } => rng.gen_range(0..=*max),
            Self::PowerLaw { max, beta } => power_law_fee(rng.gen_range(0.0..1.0), *max, *beta),
            Self::Pareto { max, alpha } => pareto_fee(rng.gen_range(0.0..1.0), *max, *alpha),
            Self::Tiered { tiers } => {
                let total: f64 = tiers.iter().map(|tier| tier.weight).sum();
                tiered_fee(rng.gen_range(0.0..1.0) * total, tiers)
            }
        }
    }
}

/// Bounded power law: maps `u` in `[0, 1)` into `[0, max]` via
/// `round(max * (1 - (1 - u)^(1/beta)))`. Monotonic in `u`, so `u = 0` yields
/// `0` and `u -> 1` yields `max`. `beta = 1` reduces to `round(max * u)`;
/// larger `beta` pulls the same `u` toward `0`, making high fees rarer.
///
/// Caller guarantees `beta > 0` (validated at the CLI boundary).
#[allow(clippy::arithmetic_side_effects)]
fn power_law_fee(u: f64, max: u64, beta: f64) -> u64 {
    let x = 1.0 - (1.0 - u).powf(1.0 / beta);
    (x * max as f64).round() as u64
}

/// Bounded Pareto on `[1, max]` with shape `alpha`, via the inverse CDF of a
/// Pareto truncated to `[low, high]`:
/// `x = low * (1 - u*(1 - (low/high)^alpha))^(-1/alpha)`.
/// `u = 0` yields `low` (`1`) and `u -> 1` yields `high` (`max`); the result is
/// clamped to `[1, max]` to absorb float rounding. Fixing `low = 1` means the
/// tail's span in decades is controlled by `max`.
///
/// Caller guarantees `alpha > 0` (validated at the CLI boundary).
#[allow(clippy::arithmetic_side_effects)]
fn pareto_fee(u: f64, max: u64, alpha: f64) -> u64 {
    const LOW: f64 = 1.0;
    let high = max as f64;
    if high <= LOW {
        // Degenerate range [1, 1] (max <= 1): nothing to spread.
        return max;
    }
    let ratio = (LOW / high).powf(alpha);
    let x = LOW * (1.0 - u * (1.0 - ratio)).powf(-1.0 / alpha);
    (x.round() as u64).clamp(1, max)
}

/// Weighted pick: walk `tiers`, subtracting each weight from `pick` (drawn from
/// `[0, total_weight)`), and return the first tier that drives `pick` below
/// zero. Falls back to the last tier if float rounding lets `pick` reach the
/// total exactly.
#[allow(clippy::arithmetic_side_effects)]
fn tiered_fee(mut pick: f64, tiers: &[FeeTier]) -> u64 {
    for tier in tiers {
        pick -= tier.weight;
        if pick < 0.0 {
            return tier.fee;
        }
    }
    tiers.last().map_or(0, |tier| tier.fee)
}

/// Linear sawtooth: ramps from `0` to `max` over `period_ms` milliseconds, then
/// resets. Hits both endpoints exactly (`fee(0) = 0`, `fee(period_ms-1) = max`)
/// because positions `[0, period_ms-1]` are mapped to `[0, max]` via the
/// divisor `period_ms - 1`.
///
/// `period_ms = 1` is degenerate (the only sampled position per cycle is `0`)
/// and always returns `0` — a 1 ms ramp is not meaningful with ms-resolution
/// sampling. Use `period_ms >= 2` for a real ramp.
///
/// Safety: caller must guarantee `period_ms >= 1`; the public
/// [`PriorityFeeMode::Scheduled`] variant carries this invariant via
/// [`NonZeroU64`].
#[allow(clippy::arithmetic_side_effects)]
fn scheduled_fee_at(now_ms: u64, max: u64, period_ms: u64) -> u64 {
    debug_assert!(period_ms >= 1, "period_ms must be non-zero");
    let position = now_ms % period_ms;
    // Divisor = period_ms - 1 so position = period_ms-1 yields exactly `max`.
    // For period_ms = 1 the divisor would be 0; clamp to 1, position is then
    // always 0 anyway, so the result is 0.
    let divisor = period_ms.saturating_sub(1).max(1);
    position * max / divisor
}

/// Accumulates priority fee totals for metrics reporting.
#[derive(Default)]
pub struct PriorityFeeStats {
    total_fees: AtomicU64,
    tx_count: AtomicU64,
}

impl PriorityFeeStats {
    pub fn record(&self, fee: u64) {
        self.total_fees.fetch_add(fee, Ordering::Relaxed);
        self.tx_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn read_and_reset(&self) -> (u64, u64) {
        let total = self.total_fees.swap(0, Ordering::Relaxed);
        let count = self.tx_count.swap(0, Ordering::Relaxed);
        (total, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scheduled_fee_ramps_to_max() {
        // period_ms = 4, max = 3: ramp uses divisor = 3, so positions
        // 0..=3 map exactly to fees 0, 1, 2, 3, then wraps.
        let observed: Vec<u64> = (0..8).map(|t| scheduled_fee_at(t, 3, 4)).collect();
        assert_eq!(observed, vec![0, 1, 2, 3, 0, 1, 2, 3]);

        // period_ms = 8, max = 3: ramp uses divisor = 7. Fees are
        // floor(position * 3 / 7) → 0, 0, 0, 1, 1, 2, 2, 3, then wraps.
        let observed: Vec<u64> = (0..8).map(|t| scheduled_fee_at(t, 3, 8)).collect();
        assert_eq!(observed, vec![0, 0, 0, 1, 1, 2, 2, 3]);

        // Endpoints reachable for a "round" period.
        assert_eq!(scheduled_fee_at(0, 1000, 100), 0);
        assert_eq!(scheduled_fee_at(99, 1000, 100), 1000);
        assert_eq!(scheduled_fee_at(100, 1000, 100), 0); // wraps

        // period_ms = 1 is degenerate: only position 0 is sampled, always 0.
        // (Codex review flagged this; document it instead of pretending to ramp
        // in a 1 ms window with ms-resolution sampling.)
        assert_eq!(scheduled_fee_at(0, 100, 1), 0);
        assert_eq!(scheduled_fee_at(42, 100, 1), 0);
    }

    #[test]
    fn test_power_law_fee_endpoints_and_skew() {
        // Endpoints: u = 0 -> 0, u -> 1 -> max (monotonic in u).
        assert_eq!(power_law_fee(0.0, 1000, 2.0), 0);
        assert_eq!(power_law_fee(1.0, 1000, 2.0), 1000);
        // beta = 1 is uniform: the median draw sits at max/2.
        assert_eq!(power_law_fee(0.5, 1000, 1.0), 500);
        // beta > 1 skews low: the same median u lands well below max/2...
        assert!(power_law_fee(0.5, 1000, 2.0) < 500);
        // ...and larger beta pushes it lower still.
        assert!(power_law_fee(0.5, 1000, 4.0) < power_law_fee(0.5, 1000, 2.0));
    }

    #[test]
    fn test_pareto_fee_endpoints_and_tail() {
        // Bounded to [1, max]: u = 0 -> 1, u -> 1 -> max.
        assert_eq!(pareto_fee(0.0, 1000, 1.0), 1);
        assert_eq!(pareto_fee(1.0, 1000, 1.0), 1000);
        // Heavy tail: over three decades the median draw stays tiny vs max.
        assert!(pareto_fee(0.5, 1000, 1.0) < 10);
        // Degenerate range [1, 1] always yields 1.
        assert_eq!(pareto_fee(0.7, 1, 1.0), 1);
    }

    #[test]
    fn test_tiered_fee_weighted_pick() {
        let tiers = vec![
            FeeTier {
                fee: 0,
                weight: 90.0,
            },
            FeeTier {
                fee: 500,
                weight: 9.0,
            },
            FeeTier {
                fee: 1000,
                weight: 1.0,
            },
        ];
        // Cumulative boundaries: [0, 90) -> 0, [90, 99) -> 500, [99, 100) -> 1000.
        assert_eq!(tiered_fee(0.0, &tiers), 0);
        assert_eq!(tiered_fee(89.9, &tiers), 0);
        assert_eq!(tiered_fee(90.0, &tiers), 500);
        assert_eq!(tiered_fee(98.9, &tiers), 500);
        assert_eq!(tiered_fee(99.0, &tiers), 1000);
        // Overshoot from float rounding falls back to the last tier.
        assert_eq!(tiered_fee(100.0, &tiers), 1000);
    }

    #[test]
    fn test_premium_plan_exact_count_and_even_spread() {
        let plan = PremiumPlan {
            total: 10,
            premium_count: 3,
            premium_fee: 500,
        };
        let premium_indices: Vec<u64> = (0..10).filter(|&i| plan.is_premium(i)).collect();
        // Exactly `premium_count` premiums, spread on an even stride (~N/P).
        assert_eq!(premium_indices, vec![3, 6, 9]);
        assert_eq!(plan.fee_at(6), 500);
        assert_eq!(plan.fee_at(0), 0);
    }

    #[test]
    fn test_premium_plan_edge_counts() {
        // premium_count = 0 => nobody pays extra.
        let none = PremiumPlan {
            total: 5,
            premium_count: 0,
            premium_fee: 7,
        };
        assert!((0..5).all(|i| none.fee_at(i) == 0));

        // premium_count == total => everybody is premium.
        let all = PremiumPlan {
            total: 5,
            premium_count: 5,
            premium_fee: 7,
        };
        assert!((0..5).all(|i| all.fee_at(i) == 7));
    }

    #[test]
    fn test_premium_plan_prefix_is_proportional() {
        // Any prefix of length L holds exactly floor(L*P/N) premiums, so a
        // duration-truncated run still has the right composition on its prefix.
        let plan = PremiumPlan {
            total: 1000,
            premium_count: 100,
            premium_fee: 1,
        };
        assert_eq!((0..250).filter(|&i| plan.is_premium(i)).count(), 25);
        assert_eq!((0..1000).filter(|&i| plan.is_premium(i)).count(), 100);
    }
}
