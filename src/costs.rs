//! Execution-cost model.
//!
//! Everything here follows `research/execution-costs-and-arb-filtering.md`: gaps are
//! computed on the **executable** side of the book (asks for buy legs, never mid/last),
//! the taker fee curve `rate × p × (1 − p)` is applied per leg, and every opportunity is
//! reported both as a taker and as a maker.
//!
//! Decomposition used throughout (all per share of the position set):
//!
//! ```text
//!   gross_gap     = payout − Σ best_ask          (top-of-book, executable side)
//!   slippage_cost = Σ vwap  − Σ best_ask         (cost of walking the book to size)
//!   fee_taker     = Σ rate · vwapᵢ · (1 − vwapᵢ)
//!   net_taker     = gross_gap − slippage_cost − fee_taker
//!
//!   spread_cost   = Σ (best_ask − best_bid)      (what a taker crosses)
//!   net_maker     = payout − Σ best_bid = gross_gap + spread_cost
//! ```
//!
//! Note `spread_cost` is *already embedded* in the ask-based `gross_gap`; it is reported
//! as the amount a maker recovers by resting instead of crossing. Subtracting it from
//! `net_taker` again would double-count.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::types::Category;

/// Category → taker fee rate, with `other` as the documented fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeModel {
    rates: BTreeMap<String, Decimal>,
}

impl FeeModel {
    pub fn new(rates: BTreeMap<String, Decimal>) -> Self {
        Self { rates }
    }

    /// Rate for a category. Unknown categories fall back to `other` (0.05), which is the
    /// conservative choice: it never under-states fees.
    pub fn rate_for(&self, category: &Category) -> Decimal {
        self.rates
            .get(category.as_str())
            .or_else(|| self.rates.get(Category::OTHER))
            .copied()
            .unwrap_or_else(|| Decimal::new(5, 2))
    }
}

/// Taker fee for one leg, per share: `rate × p × (1 − p)`. Peaks at p = 0.50 — exactly
/// where arb gaps cluster.
pub fn taker_fee_per_share(rate: Decimal, price: Decimal) -> Decimal {
    rate * price * (Decimal::ONE - price)
}

/// Taker fee for one leg in dollars: `shares × rate × p × (1 − p)`.
pub fn taker_fee(shares: Decimal, rate: Decimal, price: Decimal) -> Decimal {
    shares * taker_fee_per_share(rate, price)
}

/// Prices for one buy leg at a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegPrices {
    pub best_ask: Decimal,
    pub best_bid: Option<Decimal>,
    /// Depth-walked average fill price for the size being evaluated.
    pub vwap: Decimal,
}

/// Full per-share and sized cost breakdown for a multi-leg position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostBreakdown {
    pub gross_gap: Decimal,
    pub slippage_cost: Decimal,
    pub spread_cost: Option<Decimal>,
    pub fee_taker: Decimal,
    pub net_taker: Decimal,
    pub net_maker: Option<Decimal>,
    pub size: Decimal,
    pub capital_required: Decimal,
    pub fee_taker_total: Decimal,
    pub net_taker_total: Decimal,
    pub net_maker_total: Option<Decimal>,
}

/// Cost the position of buying one share of every leg, at `size` shares each.
///
/// `payout` is the guaranteed dollar payout per share-set at resolution: `$1` for a
/// binary YES+NO or a NegRisk YES-side sweep, `$(N−1)` for the NegRisk NO-side sweep.
pub fn breakdown(
    payout: Decimal,
    legs: &[LegPrices],
    fee_rate: Decimal,
    size: Decimal,
) -> CostBreakdown {
    let sum_ask: Decimal = legs.iter().map(|l| l.best_ask).sum();
    let sum_vwap: Decimal = legs.iter().map(|l| l.vwap).sum();
    let fee_taker: Decimal = legs
        .iter()
        .map(|l| taker_fee_per_share(fee_rate, l.vwap))
        .sum();

    let gross_gap = payout - sum_ask;
    let slippage_cost = sum_vwap - sum_ask;
    let net_taker = gross_gap - slippage_cost - fee_taker;

    // Maker economics require a bid on every leg to rest against.
    let sum_bid: Option<Decimal> = legs
        .iter()
        .map(|l| l.best_bid)
        .try_fold(Decimal::ZERO, |acc, bid| bid.map(|b| acc + b));
    let spread_cost = sum_bid.map(|b| sum_ask - b);
    let net_maker = sum_bid.map(|b| payout - b);

    CostBreakdown {
        gross_gap,
        slippage_cost,
        spread_cost,
        fee_taker,
        net_taker,
        net_maker,
        size,
        capital_required: sum_vwap * size,
        fee_taker_total: legs.iter().map(|l| taker_fee(size, fee_rate, l.vwap)).sum(),
        net_taker_total: net_taker * size,
        net_maker_total: net_maker.map(|n| n * size),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn model() -> FeeModel {
        FeeModel::new(crate::config::Config::default().fees)
    }

    #[test]
    fn verified_fee_rates_are_wired_through() {
        let m = model();
        assert_eq!(m.rate_for(&Category::new("geopolitics")), dec!(0));
        assert_eq!(m.rate_for(&Category::new("politics")), dec!(0.04));
        assert_eq!(m.rate_for(&Category::new("finance")), dec!(0.04));
        assert_eq!(m.rate_for(&Category::new("tech")), dec!(0.04));
        assert_eq!(m.rate_for(&Category::new("mentions")), dec!(0.04));
        assert_eq!(m.rate_for(&Category::new("sports")), dec!(0.05));
        assert_eq!(m.rate_for(&Category::new("economics")), dec!(0.05));
        assert_eq!(m.rate_for(&Category::new("culture")), dec!(0.05));
        assert_eq!(m.rate_for(&Category::new("weather")), dec!(0.05));
        assert_eq!(m.rate_for(&Category::new("crypto")), dec!(0.07));
        // Unknown → "other" = 0.05, the conservative fallback.
        assert_eq!(m.rate_for(&Category::new("no-such-category")), dec!(0.05));
    }

    #[test]
    fn fee_curve_matches_hand_computed_values() {
        // 100 shares of crypto at p = 0.50: 100 * 0.07 * 0.5 * 0.5 = $1.75 (docs table).
        assert_eq!(taker_fee(dec!(100), dec!(0.07), dec!(0.50)), dec!(1.75));
        // 100 shares politics at p = 0.50: 100 * 0.04 * 0.25 = $1.00 (docs table).
        assert_eq!(taker_fee(dec!(100), dec!(0.04), dec!(0.50)), dec!(1.00));
        // 100 shares sports at p = 0.50: 100 * 0.05 * 0.25 = $1.25 (docs table).
        assert_eq!(taker_fee(dec!(100), dec!(0.05), dec!(0.50)), dec!(1.25));
        // Geopolitics is fee-free at any price.
        assert_eq!(taker_fee(dec!(100), dec!(0), dec!(0.50)), dec!(0));
        // Curve peaks at 0.50 and decays symmetrically: 0.04*0.2*0.8 = 0.0064/share.
        assert_eq!(taker_fee_per_share(dec!(0.04), dec!(0.20)), dec!(0.0064));
        assert_eq!(taker_fee_per_share(dec!(0.04), dec!(0.80)), dec!(0.0064));
        assert!(
            taker_fee_per_share(dec!(0.04), dec!(0.50))
                > taker_fee_per_share(dec!(0.04), dec!(0.20))
        );
    }

    #[test]
    fn breakdown_binary_politics_hand_computed() {
        // YES ask 0.49 (bid 0.47), NO ask 0.49 (bid 0.46); no slippage (vwap = ask).
        // Σask = 0.98, gross_gap = 1 − 0.98 = 0.02
        // fee   = 0.04*0.49*0.51 * 2 = 0.009996 * 2 = 0.0199920... let's be exact:
        //         0.04*0.49 = 0.0196; 0.0196*0.51 = 0.009996 per leg; ×2 = 0.019992
        // net_taker = 0.02 − 0 − 0.019992 = 0.000008  (the fee curve nearly eats it)
        // Σbid = 0.93 → spread_cost = 0.98 − 0.93 = 0.05; net_maker = 1 − 0.93 = 0.07
        let legs = [
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: Some(dec!(0.47)),
                vwap: dec!(0.49),
            },
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: Some(dec!(0.46)),
                vwap: dec!(0.49),
            },
        ];
        let b = breakdown(dec!(1), &legs, dec!(0.04), dec!(200));
        assert_eq!(b.gross_gap, dec!(0.02));
        assert_eq!(b.slippage_cost, dec!(0));
        assert_eq!(b.fee_taker, dec!(0.019992));
        assert_eq!(b.net_taker, dec!(0.000008));
        assert_eq!(b.spread_cost, Some(dec!(0.05)));
        assert_eq!(b.net_maker, Some(dec!(0.07)));
        // Identity: net_maker == gross_gap + spread_cost (no double counting).
        assert_eq!(b.net_maker, Some(b.gross_gap + b.spread_cost.unwrap()));
        assert_eq!(b.capital_required, dec!(196.00)); // 0.98 * 200
        assert_eq!(b.net_taker_total, dec!(0.001600)); // 0.000008 * 200
        assert_eq!(b.net_maker_total, Some(dec!(14.00)));
    }

    #[test]
    fn breakdown_charges_slippage_when_vwap_exceeds_top_of_book() {
        // Same book, but size forces vwap 0.50 on leg 1.
        // Σask = 0.98 → gross_gap 0.02; Σvwap = 0.99 → slippage 0.01
        // fee = 0.04*0.50*0.50 + 0.04*0.49*0.51 = 0.01 + 0.009996 = 0.019996
        // net_taker = 0.02 − 0.01 − 0.019996 = −0.009996  (dead)
        let legs = [
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: Some(dec!(0.47)),
                vwap: dec!(0.50),
            },
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: Some(dec!(0.46)),
                vwap: dec!(0.49),
            },
        ];
        let b = breakdown(dec!(1), &legs, dec!(0.04), dec!(100));
        assert_eq!(b.slippage_cost, dec!(0.01));
        assert_eq!(b.fee_taker, dec!(0.019996));
        assert_eq!(b.net_taker, dec!(-0.009996));
    }

    #[test]
    fn geopolitics_is_fee_free_so_thin_gaps_survive() {
        // Σask = 0.996 → gross_gap 0.004, which dies at 0.04/0.05 fee rates but lives here.
        let legs = [
            LegPrices {
                best_ask: dec!(0.498),
                best_bid: Some(dec!(0.49)),
                vwap: dec!(0.498),
            },
            LegPrices {
                best_ask: dec!(0.498),
                best_bid: Some(dec!(0.49)),
                vwap: dec!(0.498),
            },
        ];
        let free = breakdown(dec!(1), &legs, dec!(0), dec!(100));
        assert_eq!(free.fee_taker, dec!(0));
        assert_eq!(free.net_taker, dec!(0.004));

        let politics = breakdown(dec!(1), &legs, dec!(0.04), dec!(100));
        // 0.498 * 0.502 = 0.249996; × 0.04 = 0.00999984 per leg → 0.01999968 for the pair,
        // which is 5× the 0.004 gap.
        assert_eq!(politics.fee_taker, dec!(0.01999968));
        assert!(politics.net_taker < Decimal::ZERO);
    }

    #[test]
    fn negrisk_no_side_uses_n_minus_one_payout() {
        // 4 outcomes, buy NO on each at 0.72 → Σ = 2.88 vs payout 3.00 → gap 0.12.
        // fee = 4 * 0.04 * 0.72 * 0.28 = 4 * 0.008064 = 0.032256
        // net_taker = 0.12 − 0 − 0.032256 = 0.087744
        let legs: Vec<LegPrices> = (0..4)
            .map(|_| LegPrices {
                best_ask: dec!(0.72),
                best_bid: Some(dec!(0.70)),
                vwap: dec!(0.72),
            })
            .collect();
        let b = breakdown(dec!(3), &legs, dec!(0.04), dec!(10));
        assert_eq!(b.gross_gap, dec!(0.12));
        assert_eq!(b.fee_taker, dec!(0.032256));
        assert_eq!(b.net_taker, dec!(0.087744));
        assert_eq!(b.spread_cost, Some(dec!(0.08))); // 4 * 0.02
        assert_eq!(b.net_maker, Some(dec!(0.20))); // 3 − 2.80
        assert_eq!(b.capital_required, dec!(28.80));
    }

    #[test]
    fn missing_bid_disables_maker_numbers_rather_than_guessing() {
        let legs = [
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: None,
                vwap: dec!(0.49),
            },
            LegPrices {
                best_ask: dec!(0.49),
                best_bid: Some(dec!(0.46)),
                vwap: dec!(0.49),
            },
        ];
        let b = breakdown(dec!(1), &legs, dec!(0.04), dec!(10));
        assert_eq!(b.spread_cost, None);
        assert_eq!(b.net_maker, None);
        assert_eq!(b.net_maker_total, None);
    }
}
