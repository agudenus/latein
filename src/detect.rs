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
            if let Some(op) =
                self.evaluate(event, OpportunityKind::BinaryYesNo, Decimal::ONE, &legs)
            {
                out.push(op);
            }
        }

        if !event.neg_risk {
            return;
        }
        let n = event.markets.len();
        if n < 2 || n > self.cfg.scan.max_negrisk_outcomes {
            return;
        }

        // YES-side sweep: buy YES on every outcome, exactly one pays $1.
        if let Some(legs) = self.negrisk_legs(event, books, true) {
            if let Some(op) =
                self.evaluate(event, OpportunityKind::NegRiskYesSide, Decimal::ONE, &legs)
            {
                out.push(op);
            }
        }

        // NO-side sweep: buy NO on every outcome, exactly one loses → N−1 pay $1.
        if let Some(legs) = self.negrisk_legs(event, books, false) {
            let payout = Decimal::from(n) - Decimal::ONE;
            if let Some(op) = self.evaluate(event, OpportunityKind::NegRiskNoSide, payout, &legs) {
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
            ));
        }

        if !self.cfg.scan.report_maker_only {
            return None;
        }
        let maker = self.size_maker(&best_asks, &best_bids, payout, fee_rate, floor)?;
        Some(self.build(
            event, kind, payout, legs, &best_asks, &best_bids,
            &best_asks, // a maker never walks the ask book; taker fields are top-of-book
            maker, fee_rate, true,
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
    ) -> Opportunity {
        let conversion_required = matches!(kind, OpportunityKind::NegRiskNoSide);
        let mut flags = Vec::new();

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
            // All three constructions lock the payout at resolution inside one event and
            // one oracle. Relative value is reserved for combinatorial/cross-platform.
            label: Label::TrueArb,
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

    fn event(neg_risk: bool, category: &str, markets: Vec<TrackedMarket>) -> TrackedEvent {
        TrackedEvent {
            id: "1".into(),
            slug: "test-event".into(),
            title: "Test Event".into(),
            neg_risk,
            category: Category::new(category),
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
        // The book fixture deliberately covers only tokens 1001/1002/1004/2002, so:
        //  - the NegRisk YES sweep needs 1001 + 1003 → 1003 has no book → suppressed;
        //  - the NegRisk NO sweep (1002 + 1004) prices at 0.58 + 0.59 = 1.17 vs a $1
        //    payout for N=2 → no gap;
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
