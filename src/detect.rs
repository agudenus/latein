//! Arbitrage detectors.
//!
//! Three structural forms, all intra-Polymarket (single oracle, so no cross-venue
//! resolution risk):
//!
//! 1. **Binary YES+NO** — one condition, `ask(YES) + ask(NO) < $1`.
//! 2. **NegRisk YES-side** — mutually exclusive event, `Σ ask(YESᵢ) < $1`.
//! 3. **NegRisk NO-side** — `Σ ask(NOᵢ) < $(N−1)`; exactly one outcome wins, so exactly
//!    one NO loses and the other `N−1` pay $1 each.
//!
//! Every candidate is then depth-walked: we binary-search the largest size at which
//! *every* leg's VWAP still leaves net-per-share above the configured floor, with the
//! per-trade capital cap enforced inside the search.
//!
//! ## Full-coverage guard (why both NegRisk forms check `total_outcomes`)
//!
//! Both sweeps are only risk-free when they span **every** outcome of the event. Discovery
//! drops markets that are closed, inactive, order-book-disabled or unparseable, so
//! `event.markets` is a *subset* of the event's real outcome set. Sweeping that subset
//! leaves the dropped outcomes uncovered: if one of them wins, every leg we bought pays
//! zero. A two-outcome sweep of a three-outcome election summing to $0.968 looks like a
//! 3.2¢ lock and is not one.
//!
//! Therefore:
//!
//! * `tracked == total` → the sweep is a genuine lock → `true-arb`.
//! * `tracked < total` → suppressed by default; with `scan.report_partial_negrisk = true`
//!   the **YES-side** may be surfaced as `relative-value` carrying a `partial_coverage`
//!   flag, never as arbitrage.
//! * `tracked < total` on the **NO-side** is always suppressed: its `$(N−1)` payout is
//!   derived from the full outcome count, and with a partial sweep the number of NOs that
//!   pay is not `tracked − 1` at all. There is no honest way to report it.

use rust_decimal::{Decimal, RoundingStrategy};

use crate::config::Config;
use crate::costs::{breakdown, CostBreakdown, FeeModel, LegPrices};
use crate::risk::RiskLimits;
use crate::types::{
    BookMap, Label, Leg, Opportunity, OpportunityKind, OrderBook, Side, TrackedEvent,
    TrackedMarket, Universe,
};

/// Everything the detectors need, assembled once per scan.
pub struct Detector<'a> {
    cfg: &'a Config,
    fees: &'a FeeModel,
    risk: RiskLimits,
}

/// One prospective buy leg, resolved to a book.
struct CandidateLeg<'a> {
    market: &'a TrackedMarket,
    outcome: String,
    book: &'a OrderBook,
}

/// How much of the event's outcome set a construction covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// Either not an event-wide sweep (a single binary condition, whose YES and NO always
    /// sum to $1 on their own) or a sweep spanning every outcome Gamma listed.
    Complete,
    /// Only `tracked` of the event's `total` outcomes are priced. NOT risk-free.
    Partial { tracked: usize, total: usize },
}

impl<'a> Detector<'a> {
    pub fn new(cfg: &'a Config, fees: &'a FeeModel) -> Self {
        Self {
            cfg,
            fees,
            risk: RiskLimits::from_config(&cfg.risk),
        }
    }

    /// Run all three detectors across the universe. Results are sorted by dollar profit,
    /// best first.
    pub fn scan(&self, universe: &Universe, books: &BookMap) -> Vec<Opportunity> {
        let mut out = Vec::new();
        for event in &universe.events {
            if !self.cfg.category_included(&event.category) {
                continue;
            }
            self.scan_event(event, books, &mut out);
        }
        out.sort_by(|a, b| {
            b.net_taker_total
                .cmp(&a.net_taker_total)
                .then_with(|| {
                    b.net_maker_total
                        .unwrap_or(Decimal::ZERO)
                        .cmp(&a.net_maker_total.unwrap_or(Decimal::ZERO))
                })
                .then_with(|| a.event_slug.cmp(&b.event_slug))
        });
        out
    }

    fn scan_event(&self, event: &TrackedEvent, books: &BookMap, out: &mut Vec<Opportunity>) {
        // Single-condition YES/NO applies to every binary market, including the
        // constituents of a NegRisk event — each is its own condition.
        for market in &event.markets {
            let legs = [
                CandidateLeg {
                    market,
                    outcome: market.yes_label().to_string(),
                    book: match books.get(market.yes_token()) {
                        Some(b) => b,
                        None => continue,
                    },
                },
                CandidateLeg {
                    market,
                    outcome: market.no_label().to_string(),
                    book: match books.get(market.no_token()) {
                        Some(b) => b,
                        None => continue,
                    },
                },
            ];
            if let Some(op) = self.evaluate(
                event,
                OpportunityKind::BinaryYesNo,
                Decimal::ONE,
                &legs,
                // A single condition is self-contained: YES + NO of the same market sum to
                // $1 at resolution no matter what else the event lists.
                Coverage::Complete,
            ) {
                out.push(op);
            }
        }

        if !event.neg_risk {
            return;
        }
        let tracked = event.markets.len();
        let total = event.total_outcomes;
        if tracked < 2 || tracked > self.cfg.scan.max_negrisk_outcomes {
            return;
        }

        // The full-coverage guard. `total` is Gamma's pre-filter outcome count, so
        // `tracked < total` means discovery dropped outcomes and the sweep is incomplete.
        let coverage = if event.coverage_complete() {
            Coverage::Complete
        } else {
            if !self.cfg.scan.report_partial_negrisk {
                tracing::debug!(
                    event = %event.slug,
                    tracked,
                    total,
                    "suppressing NegRisk sweep: outcome coverage is incomplete"
                );
                return;
            }
            Coverage::Partial { tracked, total }
        };

        // YES-side sweep: buy YES on every outcome, exactly one pays $1.
        if let Some(legs) = self.negrisk_legs(event, books, true) {
            if let Some(op) = self.evaluate(
                event,
                OpportunityKind::NegRiskYesSide,
                Decimal::ONE,
                &legs,
                coverage,
            ) {
                out.push(op);
            }
        }

        // NO-side sweep: buy NO on every outcome, exactly one loses → N−1 pay $1.
        //
        // `N` is the event's *full* outcome count. That payout is only defensible when we
        // hold a NO on every outcome; over a partial set the guaranteed payout is a
        // different number entirely, so quoting `total − 1` would overstate the lock and
        // quoting `tracked − 1` would silently turn this into a different construction
        // (one whose NegRisk conversion path assumes the complete set). Neither is
        // honest, so a partial NO-side sweep is suppressed outright — not even as
        // relative value.
        if coverage != Coverage::Complete {
            tracing::debug!(
                event = %event.slug,
                tracked,
                total,
                "suppressing NegRisk NO-side sweep: the $(N−1) payout requires full coverage"
            );
            return;
        }
        if let Some(legs) = self.negrisk_legs(event, books, false) {
            let payout = Decimal::from(total) - Decimal::ONE;
            if let Some(op) = self.evaluate(
                event,
                OpportunityKind::NegRiskNoSide,
                payout,
                &legs,
                coverage,
            ) {
                out.push(op);
            }
        }
    }

    fn negrisk_legs<'b>(
        &self,
        event: &'b TrackedEvent,
        books: &'b BookMap,
        yes_side: bool,
    ) -> Option<Vec<CandidateLeg<'b>>> {
        let mut legs = Vec::with_capacity(event.markets.len());
        for market in &event.markets {
            let (token, outcome) = if yes_side {
                (market.yes_token(), market.yes_label().to_string())
            } else {
                (market.no_token(), format!("NOT {}", market.yes_label()))
            };
            // A missing book means we cannot price the sweep at all — never partially.
            let book = books.get(token)?;
            legs.push(CandidateLeg {
                market,
                outcome,
                book,
            });
        }
        Some(legs)
    }

    /// Gate on the executable-side gap, then size it. `None` when there is no reportable
    /// opportunity.
    fn evaluate(
        &self,
        event: &TrackedEvent,
        kind: OpportunityKind,
        payout: Decimal,
        legs: &[CandidateLeg<'_>],
        coverage: Coverage,
    ) -> Option<Opportunity> {
        let books: Vec<&OrderBook> = legs.iter().map(|l| l.book).collect();
        let best_asks: Option<Vec<Decimal>> = books.iter().map(|b| b.best_ask()).collect();
        let best_asks = best_asks?;
        let sum_ask: Decimal = best_asks.iter().copied().sum();

        // Gate: the gap must exist on the side we can actually buy. Never mid/last.
        if sum_ask >= payout {
            return None;
        }

        let best_bids: Vec<Option<Decimal>> = books.iter().map(|b| b.best_bid()).collect();
        let fee_rate = self.fees.rate_for(&event.category);
        let floor = self.cfg.net_floor_for(&event.category);

        if let Some(sized) =
            self.size_taker(&books, &best_asks, &best_bids, payout, fee_rate, floor)
        {
            return Some(self.build(
                event,
                kind,
                payout,
                legs,
                &best_asks,
                &best_bids,
                &sized.vwaps,
                sized.cost,
                fee_rate,
                false,
                coverage,
            ));
        }

        if !self.cfg.scan.report_maker_only {
            return None;
        }
        let maker = self.size_maker(&best_asks, &best_bids, payout, fee_rate, floor)?;
        Some(self.build(
            event, kind, payout, legs, &best_asks, &best_bids,
            &best_asks, // a maker never walks the ask book; taker fields are top-of-book
            maker, fee_rate, true, coverage,
        ))
    }

    /// Binary-search the largest joint size whose depth-walked net-per-share clears the
    /// floor and whose capital stays inside the per-trade cap.
    ///
    /// Both predicates are monotone in size (VWAP is non-decreasing as we walk deeper,
    /// and `d net / d vwap = −1 − rate·(1−2·vwap) < 0` for every rate ≤ 0.07), so the
    /// feasible set is a prefix and the search is exact to `size_search_tolerance`.
    fn size_taker(
        &self,
        books: &[&OrderBook],
        best_asks: &[Decimal],
        best_bids: &[Option<Decimal>],
        payout: Decimal,
        fee_rate: Decimal,
        floor: Decimal,
    ) -> Option<SizedTaker> {
        let min_size = self.cfg.scan.min_size_shares;
        let max_size = books
            .iter()
            .map(|b| b.depth(Side::Ask))
            .min()
            .unwrap_or(Decimal::ZERO);
        if max_size < min_size {
            return None;
        }

        let eval = |size: Decimal| -> Option<SizedTaker> {
            let vwaps: Option<Vec<Decimal>> = books
                .iter()
                .map(|b| b.vwap_for_size(Side::Ask, size))
                .collect();
            let vwaps = vwaps?;
            let leg_prices = leg_prices(best_asks, best_bids, &vwaps);
            let cost = breakdown(payout, &leg_prices, fee_rate, size);
            Some(SizedTaker { vwaps, cost })
        };
        let feasible = |size: Decimal| -> Option<SizedTaker> {
            let sized = eval(size)?;
            (sized.cost.net_taker > floor && self.risk.allows_capital(sized.cost.capital_required))
                .then_some(sized)
        };

        // Not viable even at the smallest size we would bother trading.
        feasible(min_size)?;
        if let Some(sized) = feasible(max_size) {
            return Some(sized);
        }

        let tol = self.cfg.scan.size_search_tolerance;
        let two = Decimal::from(2u32);
        let (mut lo, mut hi) = (min_size, max_size);
        for _ in 0..128 {
            if hi - lo <= tol {
                break;
            }
            let mid = ((lo + hi) / two).round_dp(4);
            if mid <= lo || mid >= hi {
                break;
            }
            if feasible(mid).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        feasible(lo)
    }

    /// Fallback for gaps that die to fees/slippage as a taker but survive as a maker.
    /// Makers pay no fee and cross no spread, but must actually get filled on every leg.
    fn size_maker(
        &self,
        best_asks: &[Decimal],
        best_bids: &[Option<Decimal>],
        payout: Decimal,
        fee_rate: Decimal,
        floor: Decimal,
    ) -> Option<CostBreakdown> {
        let bids: Option<Vec<Decimal>> = best_bids.iter().copied().collect();
        let sum_bid: Decimal = bids?.iter().copied().sum();
        if payout - sum_bid <= floor {
            return None;
        }
        // Truncate, never round up: rounding up would push capital past the per-trade cap.
        let size = self
            .risk
            .max_shares_at(sum_bid)?
            .round_dp_with_strategy(4, RoundingStrategy::ToZero);
        if size < self.cfg.scan.min_size_shares {
            return None;
        }
        let leg_prices = leg_prices(best_asks, best_bids, best_asks);
        let mut cost = breakdown(payout, &leg_prices, fee_rate, size);
        // A maker posts at the bid, so the capital actually committed is bid-based.
        cost.capital_required = sum_bid * size;
        Some(cost)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        event: &TrackedEvent,
        kind: OpportunityKind,
        payout: Decimal,
        legs: &[CandidateLeg<'_>],
        best_asks: &[Decimal],
        best_bids: &[Option<Decimal>],
        vwaps: &[Decimal],
        cost: CostBreakdown,
        fee_rate: Decimal,
        maker_only: bool,
        coverage: Coverage,
    ) -> Opportunity {
        let conversion_required = matches!(kind, OpportunityKind::NegRiskNoSide);
        let mut flags = Vec::new();

        // A sweep that does not span every outcome is relative value, full stop. The
        // label is what downstream consumers key on, so it is decided here and nowhere
        // else.
        let (label, partial_coverage) = match coverage {
            Coverage::Complete => (Label::TrueArb, None),
            Coverage::Partial { tracked, total } => {
                flags.push(format!(
                    "partial_coverage: NOT risk-free — {tracked} of {total} outcomes covered. \
                     Discovery dropped {} outcome(s) of this event (closed, inactive, no order \
                     book, or unparseable), so the untracked outcome(s) can win and pay these \
                     legs nothing. Relative value, not arbitrage.",
                    total.saturating_sub(tracked)
                ));
                (Label::RelativeValue, Some((tracked, total)))
            }
        };

        flags.push(match kind {
            OpportunityKind::BinaryYesNo => {
                "single condition: YES and NO of the same market always sum to $1 at \
                 resolution — no cross-market resolution risk"
                    .to_string()
            }
            OpportunityKind::NegRiskYesSide | OpportunityKind::NegRiskNoSide => {
                "assumes the event's listed outcomes are mutually exclusive AND exhaustive; \
                 an unlisted 'other' outcome breaks the lock"
                    .to_string()
            }
        });
        if conversion_required {
            flags.push(
                "NO-side sweep pays $(N−1) held to resolution; the capital-efficient exit \
                 uses the NegRisk adapter NO→YES conversion, which is not implemented and \
                 must be verified before trading"
                    .to_string(),
            );
        }
        if cost.net_maker.is_some() {
            flags.push(
                "maker net assumes resting at the current best bid on every leg and being \
                 crossed; fills are not guaranteed (legging risk)"
                    .to_string(),
            );
        }
        if maker_only {
            flags.push(
                "maker-only: taker net is below the floor, so the taker columns are \
                 top-of-book and were not depth-sized"
                    .to_string(),
            );
        }
        if event.category.as_str() == "geopolitics" {
            flags.push(
                "geopolitics is fee-free but carries the highest oracle/resolution \
                 ambiguity risk"
                    .to_string(),
            );
        }

        let out_legs = legs
            .iter()
            .enumerate()
            .map(|(i, l)| Leg {
                token_id: l.book.asset_id.clone(),
                condition_id: l.market.condition_id.clone(),
                question: l.market.question.clone(),
                outcome: l.outcome.clone(),
                best_ask: best_asks[i],
                best_bid: best_bids[i],
                vwap: vwaps[i],
                size: cost.size,
                ask_depth: l.book.depth(Side::Ask),
            })
            .collect();

        Opportunity {
            kind,
            // `true-arb` only when the construction locks the payout at resolution: a
            // single condition, or a sweep covering every outcome of one event under one
            // oracle. A partial sweep is downgraded to relative value above.
            label,
            event_slug: event.slug.clone(),
            event_title: event.title.clone(),
            category: event.category.clone(),
            fee_rate,
            payout,
            legs: out_legs,
            gross_gap: cost.gross_gap,
            slippage_cost: cost.slippage_cost,
            spread_cost: cost.spread_cost,
            fee_taker: cost.fee_taker,
            net_taker: cost.net_taker,
            net_maker: cost.net_maker,
            executable_size: cost.size,
            capital_required: cost.capital_required,
            net_taker_total: cost.net_taker_total,
            net_maker_total: cost.net_maker_total,
            partial_coverage,
            resolution_flags: flags,
            conversion_required,
            maker_only,
        }
    }
}

struct SizedTaker {
    vwaps: Vec<Decimal>,
    cost: CostBreakdown,
}

fn leg_prices(
    best_asks: &[Decimal],
    best_bids: &[Option<Decimal>],
    vwaps: &[Decimal],
) -> Vec<LegPrices> {
    best_asks
        .iter()
        .zip(best_bids)
        .zip(vwaps)
        .map(|((ask, bid), vwap)| LegPrices {
            best_ask: *ask,
            best_bid: *bid,
            vwap: *vwap,
        })
        .collect()
}

/// Convenience wrapper used by the CLI.
pub fn scan(cfg: &Config, universe: &Universe, books: &BookMap) -> Vec<Opportunity> {
    let fees = FeeModel::new(cfg.fees.clone());
    Detector::new(cfg, &fees).scan(universe, books)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, PriceLevel, TokenId};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    /// The depth walker is exact only to `size_search_tolerance` (plus the 4-dp rounding
    /// of each probe), and it always lands *below* the true boundary — never above.
    fn assert_size_near(actual: Decimal, boundary: Decimal, tol: Decimal) {
        assert!(
            actual <= boundary,
            "size {actual} exceeded the feasible boundary {boundary}"
        );
        assert!(
            boundary - actual <= tol + dec!(0.0001),
            "size {actual} is more than {tol} short of the boundary {boundary}"
        );
    }

    fn book(id: &str, bids: &[(Decimal, Decimal)], asks: &[(Decimal, Decimal)]) -> OrderBook {
        OrderBook::new(
            TokenId::new(id),
            bids.iter().map(|(p, s)| PriceLevel::new(*p, *s)).collect(),
            asks.iter().map(|(p, s)| PriceLevel::new(*p, *s)).collect(),
        )
        .normalized()
    }

    fn market(idx: usize, question: &str) -> TrackedMarket {
        TrackedMarket {
            condition_id: format!("0xcond{idx}"),
            question: question.to_string(),
            outcomes: ["Yes".into(), "No".into()],
            token_ids: [
                TokenId::new(format!("{idx}-yes")),
                TokenId::new(format!("{idx}-no")),
            ],
        }
    }

    /// Fully covered event: every outcome Gamma listed is tracked.
    fn event(neg_risk: bool, category: &str, markets: Vec<TrackedMarket>) -> TrackedEvent {
        let total = markets.len();
        event_with_total(neg_risk, category, markets, total)
    }

    /// Event where Gamma listed `total` outcomes but only `markets` survived discovery.
    fn event_with_total(
        neg_risk: bool,
        category: &str,
        markets: Vec<TrackedMarket>,
        total: usize,
    ) -> TrackedEvent {
        TrackedEvent {
            id: "1".into(),
            slug: "test-event".into(),
            title: "Test Event".into(),
            neg_risk,
            category: Category::new(category),
            total_outcomes: total,
            markets,
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn run(cfg: &Config, event: TrackedEvent, books: Vec<OrderBook>) -> Vec<Opportunity> {
        let universe = Universe {
            events: vec![event],
        };
        let map: BookMap = books
            .into_iter()
            .map(|b| (b.asset_id.clone(), b))
            .collect::<HashMap<_, _>>();
        scan(cfg, &universe, &map)
    }

    // --- binary YES/NO -------------------------------------------------------------

    #[test]
    fn binary_detects_and_sizes_a_geopolitics_arb() {
        // Fee-free category so the arithmetic is purely gap − slippage.
        // YES asks: 200 @ 0.48, 300 @ 0.50 ; NO asks: 500 @ 0.49
        // At 200 shares: vwap 0.48 + 0.49 = 0.97 → net 0.03/share.
        // At 500 shares: YES vwap = (200*0.48 + 300*0.50)/500 = (96+150)/500 = 0.492
        //                Σ = 0.982 → net 0.018/share, capital 0.982*500 = $491 > $50 cap.
        // Cap binds first: 50 / 0.982 = 50.916... shares, but VWAP below 200 shares is
        // 0.48, so at s ≤ 200 the cost is 0.97/share → cap size = 50/0.97 = 51.546...
        let books = vec![
            book(
                "0-yes",
                &[(dec!(0.46), dec!(400))],
                &[(dec!(0.48), dec!(200)), (dec!(0.50), dec!(300))],
            ),
            book(
                "0-no",
                &[(dec!(0.47), dec!(400))],
                &[(dec!(0.49), dec!(500))],
            ),
        ];
        let ops = run(
            &cfg(),
            event(false, "geopolitics", vec![market(0, "Q?")]),
            books,
        );
        assert_eq!(ops.len(), 1);
        let op = &ops[0];
        assert_eq!(op.kind, OpportunityKind::BinaryYesNo);
        assert!(!op.maker_only);
        assert_eq!(op.fee_rate, dec!(0));
        assert_eq!(op.gross_gap, dec!(0.03)); // 1 − (0.48 + 0.49)
        assert_eq!(op.fee_taker, dec!(0));
        assert_eq!(op.net_taker, dec!(0.03)); // no slippage inside the capped size
        assert_eq!(op.spread_cost, Some(dec!(0.04))); // (0.48−0.46) + (0.49−0.47)
        assert_eq!(op.net_maker, Some(dec!(0.07))); // 1 − (0.46 + 0.47)
                                                    // Size is bounded by the $50 per-trade cap at $0.97/share, not by depth:
                                                    // 50 / 0.97 = 51.5463917... shares.
        assert_size_near(
            op.executable_size,
            dec!(50) / dec!(0.97),
            cfg().scan.size_search_tolerance,
        );
        assert!(op.capital_required <= dec!(50));
        assert!(op.capital_required > dec!(49.9));
    }

    #[test]
    fn binary_depth_walk_shrinks_size_when_the_second_level_kills_the_edge() {
        // Cap raised so depth, not capital, is the binding constraint.
        // YES asks: 100 @ 0.40, 900 @ 0.60 ; NO asks: 1000 @ 0.55 (fee-free geopolitics,
        // net floor 0.003).
        // At 100 shares: 0.40 + 0.55 = 0.95 → net 0.05.
        // At 200 shares: YES vwap = (40 + 60)/200 = 0.50 → Σ 1.05 → net −0.05.
        // Break-even against the floor: Σvwap = 0.997 → YES vwap = 0.447
        //   (100*0.40 + (s−100)*0.60)/s = 0.447  ⇒  0.6s − 20 = 0.447s
        //   0.153s = 20  ⇒  s = 130.7189542...
        let mut c = cfg();
        c.risk.per_trade_cap_usd = dec!(100000);
        let books = vec![
            book(
                "0-yes",
                &[(dec!(0.38), dec!(400))],
                &[(dec!(0.40), dec!(100)), (dec!(0.60), dec!(900))],
            ),
            book(
                "0-no",
                &[(dec!(0.53), dec!(400))],
                &[(dec!(0.55), dec!(1000))],
            ),
        ];
        let ops = run(
            &c,
            event(false, "geopolitics", vec![market(0, "Q?")]),
            books,
        );
        assert_eq!(ops.len(), 1);
        assert_size_near(
            ops[0].executable_size,
            dec!(20) / dec!(0.153),
            c.scan.size_search_tolerance,
        );
        // Net per share at that size is still above the floor, but barely — the second
        // ask level has eaten almost all of the 5¢ top-of-book edge.
        assert!(ops[0].net_taker > dec!(0.003));
        assert!(ops[0].net_taker < dec!(0.0031));
        assert!(ops[0].slippage_cost > dec!(0.046));
    }

    #[test]
    fn binary_gap_that_dies_to_the_politics_fee_is_reported_maker_only() {
        // Σask = 0.98 → gross 0.02; fee at 0.49 = 2 * 0.04 * 0.49 * 0.51 = 0.019992
        // net_taker = 0.000008 < 0.005 floor → not takeable.
        // Σbid = 0.93 → net_maker = 0.07 > floor → reported maker-only.
        let books = vec![
            book(
                "0-yes",
                &[(dec!(0.47), dec!(500))],
                &[(dec!(0.49), dec!(500))],
            ),
            book(
                "0-no",
                &[(dec!(0.46), dec!(500))],
                &[(dec!(0.49), dec!(500))],
            ),
        ];
        let ops = run(
            &cfg(),
            event(false, "politics", vec![market(0, "Q?")]),
            books,
        );
        assert_eq!(ops.len(), 1);
        let op = &ops[0];
        assert!(op.maker_only);
        assert_eq!(op.fee_rate, dec!(0.04));
        assert_eq!(op.fee_taker, dec!(0.019992));
        assert_eq!(op.net_taker, dec!(0.000008));
        assert_eq!(op.net_maker, Some(dec!(0.07)));
        // Maker size / capital are bid-based: 50 / 0.93 = 53.7634... shares.
        assert!(op.executable_size > dec!(53.76) && op.executable_size < dec!(53.77));
        assert!(op.capital_required <= dec!(50));
        assert!(op.resolution_flags.iter().any(|f| f.contains("maker-only")));
    }

    #[test]
    fn no_gap_on_the_executable_side_is_not_an_opportunity() {
        // Mid-price sum is 0.99 (0.495 + 0.495) but the asks sum to 1.01 — a mid-based
        // detector would fire here and a taker would lose money.
        let books = vec![
            book(
                "0-yes",
                &[(dec!(0.48), dec!(500))],
                &[(dec!(0.51), dec!(500))],
            ),
            book(
                "0-no",
                &[(dec!(0.48), dec!(500))],
                &[(dec!(0.50), dec!(500))],
            ),
        ];
        let ops = run(
            &cfg(),
            event(false, "politics", vec![market(0, "Q?")]),
            books,
        );
        assert!(ops.is_empty());
    }

    #[test]
    fn insufficient_depth_for_the_minimum_size_is_dropped() {
        // 3 shares available < min_size 5, and the spread is zero so there is no maker
        // fallback either.
        let books = vec![
            book("0-yes", &[(dec!(0.48), dec!(3))], &[(dec!(0.48), dec!(3))]),
            book("0-no", &[(dec!(0.49), dec!(3))], &[(dec!(0.49), dec!(3))]),
        ];
        let mut c = cfg();
        c.scan.report_maker_only = false;
        let ops = run(
            &c,
            event(false, "geopolitics", vec![market(0, "Q?")]),
            books,
        );
        assert!(ops.is_empty());
    }

    // --- NegRisk -------------------------------------------------------------------

    #[test]
    fn negrisk_yes_side_sweep_hand_computed() {
        // 3 mutually exclusive outcomes, geopolitics (fee-free) to isolate the sum math.
        // YES asks 0.30 / 0.31 / 0.33 = 0.94 → gross gap 0.06/share.
        // Depth 1000 each, cap 50 → size = 50 / 0.94 = 53.1914... shares.
        let mut markets = vec![];
        let mut books = vec![];
        for (i, (ask, bid)) in [
            (dec!(0.30), dec!(0.29)),
            (dec!(0.31), dec!(0.30)),
            (dec!(0.33), dec!(0.32)),
        ]
        .into_iter()
        .enumerate()
        {
            markets.push(market(i, &format!("Outcome {i}?")));
            books.push(book(
                &format!("{i}-yes"),
                &[(bid, dec!(1000))],
                &[(ask, dec!(1000))],
            ));
            // NO books priced so no NO-side or binary opportunity exists.
            books.push(book(
                &format!("{i}-no"),
                &[(Decimal::ONE - ask - dec!(0.02), dec!(1000))],
                &[(Decimal::ONE - bid, dec!(1000))],
            ));
        }
        let ops = run(&cfg(), event(true, "geopolitics", markets), books);
        let yes = ops
            .iter()
            .find(|o| o.kind == OpportunityKind::NegRiskYesSide)
            .expect("YES-side sweep must be detected");
        assert_eq!(yes.legs.len(), 3);
        assert_eq!(yes.payout, dec!(1));
        assert_eq!(yes.gross_gap, dec!(0.06));
        assert_eq!(yes.net_taker, dec!(0.06));
        assert_eq!(yes.spread_cost, Some(dec!(0.03)));
        assert_eq!(yes.net_maker, Some(dec!(0.09))); // 1 − (0.29+0.30+0.32)
        assert!(!yes.conversion_required);
        // All 3 of 3 listed outcomes are tracked, so this really is a lock.
        assert_eq!(yes.label, Label::TrueArb);
        assert_eq!(yes.partial_coverage, None);
        // Capital cap binds: 50 / 0.94 = 53.1914893... shares.
        assert_size_near(
            yes.executable_size,
            dec!(50) / dec!(0.94),
            cfg().scan.size_search_tolerance,
        );
        assert!(yes.capital_required <= dec!(50));
    }

    #[test]
    fn negrisk_no_side_sweep_uses_n_minus_one_payout_and_flags_conversion() {
        // 4 outcomes, politics (fee 0.04). NO asks all 0.72 → Σ 2.88 vs payout 3.00.
        // gross gap 0.12 ; fee = 4 * 0.04 * 0.72 * 0.28 = 0.032256
        // net_taker = 0.087744 ; cap 50 / 2.88 = 17.3611... shares
        let mut markets = vec![];
        let mut books = vec![];
        for i in 0..4 {
            markets.push(market(i, &format!("Outcome {i}?")));
            // YES side deliberately expensive: Σ YES asks = 4 * 0.30 = 1.20 → no YES arb,
            // and each binary pair sums to 0.30 + 0.72 = 1.02 → no binary arb.
            books.push(book(
                &format!("{i}-yes"),
                &[(dec!(0.28), dec!(1000))],
                &[(dec!(0.30), dec!(1000))],
            ));
            books.push(book(
                &format!("{i}-no"),
                &[(dec!(0.70), dec!(1000))],
                &[(dec!(0.72), dec!(1000))],
            ));
        }
        let ops = run(&cfg(), event(true, "politics", markets), books);
        assert_eq!(
            ops.len(),
            1,
            "only the NO-side sweep should fire; got {:?}",
            ops.iter().map(|o| o.kind).collect::<Vec<_>>()
        );
        let no = &ops[0];
        assert_eq!(no.kind, OpportunityKind::NegRiskNoSide);
        assert_eq!(no.payout, dec!(3));
        assert_eq!(no.legs.len(), 4);
        assert_eq!(no.gross_gap, dec!(0.12));
        assert_eq!(no.fee_taker, dec!(0.032256));
        assert_eq!(no.net_taker, dec!(0.087744));
        assert_eq!(no.spread_cost, Some(dec!(0.08)));
        assert_eq!(no.net_maker, Some(dec!(0.20)));
        assert!(no.conversion_required);
        // Capital cap binds: 50 / 2.88 = 17.3611111... share-sets (4 legs each).
        assert_size_near(
            no.executable_size,
            dec!(50) / dec!(2.88),
            cfg().scan.size_search_tolerance,
        );
        assert!(no.capital_required <= dec!(50));
        assert!(no
            .resolution_flags
            .iter()
            .any(|f| f.contains("NegRisk adapter")));
        assert_eq!(no.label, Label::TrueArb);
    }

    // --- NegRisk full-coverage guard -------------------------------------------------
    //
    // Reproduces the live false positive: a US Senate event whose Gamma outcome list has
    // three markets, one of which discovery dropped. Sweeping the surviving two sums to
    // $0.968 and looks like a 3.2¢ lock — it is not, because the dropped outcome can win.

    /// Two YES books priced like the observed "Tennessee Senate" sweep, plus NO books
    /// deliberately priced so neither the binary nor the NO-side detector can fire.
    fn senate_like_books() -> Vec<OrderBook> {
        vec![
            // YES A: ask 0.026 / bid 0.024      NO A: ask 0.980 → binary 1.006, no gap.
            book("0-yes", &[(dec!(0.024), dec!(1000))], &[(dec!(0.026), dec!(1000))]),
            book("0-no", &[(dec!(0.970), dec!(1000))], &[(dec!(0.980), dec!(1000))]),
            // YES B: ask 0.942 / bid 0.940      NO B: ask 0.062 → binary 1.004, no gap.
            book("1-yes", &[(dec!(0.940), dec!(1000))], &[(dec!(0.942), dec!(1000))]),
            book("1-no", &[(dec!(0.055), dec!(1000))], &[(dec!(0.062), dec!(1000))]),
        ]
    }

    fn senate_like_markets() -> Vec<TrackedMarket> {
        vec![market(0, "Will the Democrat win?"), market(1, "Will the Republican win?")]
    }

    #[test]
    fn partial_negrisk_coverage_is_suppressed_by_default() {
        // Gamma listed 3 outcomes; discovery kept 2. Σ YES asks = 0.968 → a 3.2¢ "gap"
        // that is not a lock, so with the default config nothing is reported at all.
        let ops = run(
            &cfg(),
            event_with_total(true, "politics", senate_like_markets(), 3),
            senate_like_books(),
        );
        assert!(
            ops.is_empty(),
            "an incomplete NegRisk sweep must not be reported by default; got {:?}",
            ops.iter().map(|o| o.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn partial_negrisk_coverage_is_relative_value_never_true_arb() {
        // Same event, with the opt-in flag set.
        //   Σ YES asks = 0.026 + 0.942 = 0.968 → gross gap 0.032
        //   fee (politics 0.04) = 0.04*0.026*0.974 + 0.04*0.942*0.058
        //                       = 0.00101296 + 0.00218544 = 0.0031984
        //   net_taker = 0.032 − 0 − 0.0031984 = 0.0288016
        //   spread    = (0.026−0.024) + (0.942−0.940) = 0.004
        //   net_maker = 1 − (0.024 + 0.940) = 0.036  (= gross + spread)
        //   size: capital cap 50 / 0.968 = 51.6528925... shares (depth 1000 is ample)
        let mut c = cfg();
        c.scan.report_partial_negrisk = true;
        let ops = run(
            &c,
            event_with_total(true, "politics", senate_like_markets(), 3),
            senate_like_books(),
        );

        assert_eq!(ops.len(), 1, "unexpected: {ops:#?}");
        let op = &ops[0];
        assert_eq!(op.kind, OpportunityKind::NegRiskYesSide);
        assert_eq!(
            op.label,
            Label::RelativeValue,
            "a partial sweep is never risk-free"
        );
        assert_eq!(op.partial_coverage, Some((2, 3)));
        assert_eq!(op.gross_gap, dec!(0.032));
        assert_eq!(op.fee_taker, dec!(0.0031984));
        assert_eq!(op.net_taker, dec!(0.0288016));
        assert_eq!(op.spread_cost, Some(dec!(0.004)));
        assert_eq!(op.net_maker, Some(dec!(0.036)));
        assert!(!op.maker_only);
        assert_size_near(
            op.executable_size,
            dec!(50) / dec!(0.968),
            c.scan.size_search_tolerance,
        );
        assert!(op.capital_required <= dec!(50));

        // The caveat has to be legible to a human and greppable in the JSONL/DB.
        let flag = op
            .resolution_flags
            .iter()
            .find(|f| f.starts_with("partial_coverage:"))
            .expect("a partial sweep must carry the partial_coverage flag");
        assert!(flag.contains("NOT risk-free"), "got: {flag}");
        assert!(flag.contains("2 of 3 outcomes covered"), "got: {flag}");

        // And the alert rendering must repeat it — the label alone is easy to skim past.
        let text = crate::alert::format_opportunity(op);
        assert!(text.contains("relative-value"), "got: {text}");
        assert!(text.contains("NOT risk-free: 2 of 3 outcomes covered"), "got: {text}");
    }

    #[test]
    fn complete_coverage_of_the_same_shape_still_fires_as_true_arb() {
        // Identical books, but Gamma really does list only the two outcomes. Then the
        // sweep *is* exhaustive and the 3.2¢ gap is a genuine lock.
        let ops = run(
            &cfg(),
            event_with_total(true, "politics", senate_like_markets(), 2),
            senate_like_books(),
        );
        assert_eq!(ops.len(), 1, "unexpected: {ops:#?}");
        assert_eq!(ops[0].kind, OpportunityKind::NegRiskYesSide);
        assert_eq!(ops[0].label, Label::TrueArb);
        assert_eq!(ops[0].partial_coverage, None);
        assert_eq!(ops[0].net_taker, dec!(0.0288016));
        assert!(!ops[0]
            .resolution_flags
            .iter()
            .any(|f| f.starts_with("partial_coverage:")));
    }

    /// NO books that would fire an unguarded NO-side sweep, with YES asks set so neither
    /// the binary nor the YES-side detector can fire:
    ///   binary A 0.62 + 0.40 = 1.02 ; binary B 0.55 + 0.50 = 1.05 ; Σ YES asks = 1.17.
    ///   Σ NO asks = 0.90 → against a $1 payout that is a 10¢ gap.
    fn no_side_books() -> Vec<OrderBook> {
        vec![
            book("0-yes", &[(dec!(0.60), dec!(1000))], &[(dec!(0.62), dec!(1000))]),
            book("0-no", &[(dec!(0.38), dec!(1000))], &[(dec!(0.40), dec!(1000))]),
            book("1-yes", &[(dec!(0.53), dec!(1000))], &[(dec!(0.55), dec!(1000))]),
            book("1-no", &[(dec!(0.48), dec!(1000))], &[(dec!(0.50), dec!(1000))]),
        ]
    }

    #[test]
    fn partial_negrisk_no_side_is_never_reported_even_when_partials_are_enabled() {
        // The permissive setting, which still must not let a NO-side sweep through: its
        // $(N−1) payout is only defined over the complete outcome set.
        let mut c = cfg();
        c.scan.report_partial_negrisk = true;
        let ops = run(
            &c,
            event_with_total(true, "geopolitics", senate_like_markets(), 3),
            no_side_books(),
        );
        assert!(
            !ops.iter()
                .any(|o| o.kind == OpportunityKind::NegRiskNoSide),
            "a partial NO-side sweep must never be reported; got {:?}",
            ops.iter().map(|o| o.kind).collect::<Vec<_>>()
        );
        assert!(ops.is_empty(), "unexpected: {ops:#?}");
    }

    #[test]
    fn complete_no_side_coverage_still_fires_as_true_arb() {
        // Proves the books above really are NO-side arb shaped, so the previous test is
        // measuring the guard and not an accidentally dead scenario.
        //   payout = N − 1 = 1 ; Σ NO asks 0.90 → gross 0.10 ; geopolitics fee 0
        //   spread = 0.02 + 0.02 = 0.04 ; net_maker = 1 − (0.38 + 0.48) = 0.14
        //   size: 50 / 0.90 = 55.5555... shares
        let ops = run(
            &cfg(),
            event_with_total(true, "geopolitics", senate_like_markets(), 2),
            no_side_books(),
        );
        assert_eq!(ops.len(), 1, "unexpected: {ops:#?}");
        let op = &ops[0];
        assert_eq!(op.kind, OpportunityKind::NegRiskNoSide);
        assert_eq!(op.label, Label::TrueArb);
        assert_eq!(op.partial_coverage, None);
        assert_eq!(op.payout, dec!(1));
        assert_eq!(op.gross_gap, dec!(0.10));
        assert_eq!(op.fee_taker, dec!(0));
        assert_eq!(op.net_taker, dec!(0.10));
        assert_eq!(op.spread_cost, Some(dec!(0.04)));
        assert_eq!(op.net_maker, Some(dec!(0.14)));
        assert_size_near(
            op.executable_size,
            dec!(50) / dec!(0.90),
            cfg().scan.size_search_tolerance,
        );
    }

    #[test]
    fn an_unknown_outcome_count_is_treated_as_incomplete() {
        // `total_outcomes == 0` means discovery never populated the count. Assuming full
        // coverage there would re-open exactly the hole this guard closes.
        let ops = run(
            &cfg(),
            event_with_total(true, "politics", senate_like_markets(), 0),
            senate_like_books(),
        );
        assert!(ops.is_empty(), "unexpected: {ops:#?}");
    }

    #[test]
    fn negrisk_detectors_do_not_run_on_non_negrisk_events() {
        let mut markets = vec![];
        let mut books = vec![];
        for i in 0..3 {
            markets.push(market(i, "Q?"));
            books.push(book(
                &format!("{i}-yes"),
                &[(dec!(0.29), dec!(1000))],
                &[(dec!(0.30), dec!(1000))],
            ));
            books.push(book(
                &format!("{i}-no"),
                &[(dec!(0.68), dec!(1000))],
                &[(dec!(0.69), dec!(1000))],
            ));
        }
        // Σ YES asks = 0.90 < 1 would be a sweep, but the event is not NegRisk so the
        // outcomes are not mutually exclusive and the "arb" does not exist.
        let ops = run(&cfg(), event(false, "geopolitics", markets), books);
        assert!(ops.iter().all(|o| o.kind == OpportunityKind::BinaryYesNo));
    }

    #[test]
    fn negrisk_events_wider_than_the_cap_are_skipped() {
        let mut c = cfg();
        c.scan.max_negrisk_outcomes = 2;
        let mut markets = vec![];
        let mut books = vec![];
        for i in 0..3 {
            markets.push(market(i, "Q?"));
            books.push(book(
                &format!("{i}-yes"),
                &[(dec!(0.29), dec!(1000))],
                &[(dec!(0.30), dec!(1000))],
            ));
            books.push(book(
                &format!("{i}-no"),
                &[(dec!(0.68), dec!(1000))],
                &[(dec!(0.69), dec!(1000))],
            ));
        }
        let ops = run(&c, event(true, "geopolitics", markets), books);
        assert!(ops.iter().all(|o| o.kind == OpportunityKind::BinaryYesNo));
    }

    #[test]
    fn missing_book_for_one_leg_suppresses_the_whole_sweep() {
        let markets = (0..3).map(|i| market(i, "Q?")).collect::<Vec<_>>();
        let mut books = vec![];
        for i in 0..3 {
            if i != 2 {
                books.push(book(
                    &format!("{i}-yes"),
                    &[(dec!(0.29), dec!(1000))],
                    &[(dec!(0.30), dec!(1000))],
                ));
            }
            books.push(book(
                &format!("{i}-no"),
                &[(dec!(0.68), dec!(1000))],
                &[(dec!(0.69), dec!(1000))],
            ));
        }
        let ops = run(&cfg(), event(true, "geopolitics", markets), books);
        assert!(
            !ops.iter()
                .any(|o| o.kind == OpportunityKind::NegRiskYesSide),
            "a sweep must never be priced from a partial set of legs"
        );
    }

    // --- end to end over the recorded fixtures ---------------------------------------

    #[test]
    fn end_to_end_from_gamma_and_clob_fixtures() {
        // Gamma fixture → universe; CLOB fixture → books; detectors → opportunities.
        // The Gamma fixture's NegRisk event lists 3 outcomes and one is closed, so
        // coverage is 2 of 3 and *both* sweeps are suppressed by the full-coverage guard
        // before books are even considered. Independently, the book fixture covers only
        // tokens 1001/1002/1004/2002, so:
        //  - the NegRisk YES sweep would also need 1003, which has no book;
        //  - the NegRisk NO sweep (1002 + 1004) would price at 0.58 + 0.59 = 1.17, with
        //    no gap against any payout it could claim;
        //  - the sports market (2001/2002) is missing a leg → skipped;
        //  - only market 1001/1002 is fully priced: 0.41 + 0.58 = 0.99 → gross gap 0.01.
        let raw = crate::gamma::parse_events_page(
            "fixture",
            include_str!("../tests/fixtures/gamma_events.json"),
        )
        .expect("gamma fixture");
        let (universe, _) = crate::gamma::build_universe(&raw);
        let books: BookMap =
            crate::clob::parse_books("fixture", include_str!("../tests/fixtures/clob_books.json"))
                .expect("clob fixture")
                .into_iter()
                .map(|b| (b.asset_id.clone(), b))
                .collect();

        let ops = scan(&cfg(), &universe, &books);
        assert_eq!(ops.len(), 1, "unexpected: {ops:#?}");
        let op = &ops[0];
        assert_eq!(op.kind, OpportunityKind::BinaryYesNo);
        assert_eq!(op.category.as_str(), "politics");
        assert_eq!(op.fee_rate, dec!(0.04));
        assert_eq!(op.gross_gap, dec!(0.01)); // 1 − (0.41 + 0.58)
                                              // 0.04*0.41*0.59 = 0.009676 ; 0.04*0.58*0.42 = 0.009744 → 0.019420
        assert_eq!(op.fee_taker, dec!(0.019420));
        assert_eq!(op.net_taker, dec!(-0.009420));
        assert!(
            op.maker_only,
            "the 1¢ gap does not survive the 0.04 fee tier"
        );
        assert_eq!(op.spread_cost, Some(dec!(0.02)));
        assert_eq!(op.net_maker, Some(dec!(0.03))); // 1 − (0.40 + 0.57)
                                                    // Maker size is capital-capped at the bid notional: 50 / 0.97, truncated to 4 dp.
        assert_eq!(op.executable_size, dec!(51.5463));
        assert_eq!(op.capital_required, dec!(49.999911));
        assert!(op.capital_required <= dec!(50));
    }

    #[test]
    fn category_filter_excludes_events() {
        let mut c = cfg();
        c.scan.include_categories = vec!["politics".into()];
        let books = vec![
            book(
                "0-yes",
                &[(dec!(0.46), dec!(500))],
                &[(dec!(0.48), dec!(500))],
            ),
            book(
                "0-no",
                &[(dec!(0.47), dec!(500))],
                &[(dec!(0.49), dec!(500))],
            ),
        ];
        let ops = run(
            &c,
            event(false, "geopolitics", vec![market(0, "Q?")]),
            books,
        );
        assert!(ops.is_empty());
    }
}
