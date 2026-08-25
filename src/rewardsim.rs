//! R1 — the liquidity-rewards farming simulator.
//!
//! ## Why this exists, and why it is not the obvious pivot
//!
//! Phase A proved our resting orders do not get hit: 5 full fills in 6 094 simulated
//! last-in-queue orders. The tempting conclusion — "quote for the rewards instead, where not
//! being filled is fine" — is **wrong**, and `research/liquidity-rewards-2026-08.md` §3.1–3.2
//! is explicit about why. Reward scoring only credits orders within `max_spread`
//! (1.5–5.5 **cents**) of the adjusted midpoint, quadratically penalising distance, so a
//! competitive reward quote sits at or within a cent of the touch. **In-band quotes do get
//! filled.** Rewards are approximately the market's price for taking that adverse selection.
//!
//! So the only number worth simulating is **net of markout**. A gross-rewards simulation
//! would repeat soak lesson #1 — a detectable number that dies on contact with the tape — in
//! new clothes. This module therefore produces two figures per market per day and never one:
//!
//! ```text
//!   gross  = pool_per_day × our share of the day's Q-score (with the $1/day/market floor)
//!   net    = gross + Σ signed markout on the fills our in-band quotes would have taken
//! ```
//!
//! Nothing is placed, nothing is signed, no wallet exists. Every input is public market data
//! that arrives whether we are here or not.
//!
//! ## The quote we simulate
//!
//! The inventory-neutral structure (`warproxxx/poly-maker`, research §1.3): **two resting
//! buys**, one on each token of the condition —
//!
//! ```text
//!   buy YES at  mid − s          buy NO at  (1 − mid) − s
//! ```
//!
//! If both fill, the pair merges back to $1 of collateral and banks `2s`, a maker-only exit
//! that never crosses a spread. That edge is not special-cased anywhere below: it falls out
//! of the markout arithmetic, because `mid_no ≡ 1 − mid_yes` makes the two signed markouts
//! sum to exactly `2s` when both legs fill and the midpoint has not moved.
//!
//! Capital per paired share is `(mid − s) + (1 − mid − s) = 1 − 2s` — about $1 per share
//! **regardless of the market's price level**, which is why there is no cheap-penny-market
//! edge and why the cap is expressed in dollars of quoting capital.
//!
//! ## What each number refuses to assume
//!
//! * **Competition is counted from the live book, both sides, in YES coordinates.** Orders
//!   resting on the NO token are mirrored into YES coordinates (`bid_no(q) ≡ ask_yes(1−q)`)
//!   and added, because ignoring them would *shrink* the competition and flatter our share.
//! * **An aggregated book level is treated as one qualifying order.** We cannot see
//!   individual orders, and assuming a level is many sub-`min_size` orders (which would score
//!   zero) is the flattering assumption; assuming it qualifies is the pessimistic one.
//! * **A sample we could not take is a lost sample, not a skipped one.** Scoring samples
//!   about once a minute, so the day's denominator is [`SAMPLES_PER_DAY`] whether or not we
//!   were up: downtime is a proportional loss of the epoch, exactly as it is live.
//! * **A window with no trade-print feed is a hole** (`no_print_feed`), never a zero markout.
//! * **The advertised pool is a configured cap, not a payout** (research §2.2). Every gross
//!   figure here inherits that overstatement, and the report says so.
//! * **Verdicts are written once.** An epoch row is inserted when the epoch closes and is
//!   never updated; a markout that had not matured by then is counted as *pending* and
//!   excluded from the net, rather than being back-filled later.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::RewardSimConfig;
use crate::types::{BookMap, OrderBook, PriceLevel, TokenId};
use crate::ws::{PrintObserver, TradePrint, TradeSide};

/// Scoring samples land about once a minute, and an epoch is one UTC day, so a full day is
/// 1 440 samples. It is the **denominator** of our share of the day: samples we did not take
/// (downtime, a missing book) score zero rather than being quietly dropped from the average.
pub const SAMPLES_PER_DAY: i64 = 1_440;

/// The `c` of the documented `Q_min` rule: a single-sided quote scores `Q/3`.
pub const ONE_SIDED_DIVISOR: Decimal = Decimal::from_parts(3, 0, 0, false, 0);

/// Below this midpoint a one-sided quote scores **nothing** — two-sided is mandatory.
pub const TWO_SIDED_BELOW: Decimal = Decimal::from_parts(10, 0, 0, false, 2); // 0.10
/// …and above this one, likewise.
pub const TWO_SIDED_ABOVE: Decimal = Decimal::from_parts(90, 0, 0, false, 2); // 0.90

/// Rewards are paid per market per day only above this; below it nothing is paid and it does
/// not roll over.
///
/// TODO(verify-live): the research is unambiguous that the floor is $1/day and single-source
/// on whether the test is **per market** (assumed here, the pessimistic reading) or per
/// wallet per day. Resolving it live moves every number in this module.
pub const PAYOUT_FLOOR_USD: Decimal = Decimal::ONE;

/// A price in cents (`3.5`) as a price in dollars (`0.035`).
///
/// This function exists so the conversion has a name. `max_spread` is published in cents
/// while every book price in this codebase is a fraction of a dollar, and a ported
/// implementation flagged the 100× confusion between them as its own bug class.
pub fn cents_to_dollars(cents: Decimal) -> Decimal {
    cents / Decimal::ONE_HUNDRED
}

/// Distance from the midpoint, in cents.
pub fn dollars_to_cents(dollars: Decimal) -> Decimal {
    dollars * Decimal::ONE_HUNDRED
}

/// One qualifying order's score: `S(v, s) = ((v − s)/v)² × shares`.
///
/// * `v` — `rewards_max_spread`, the qualifying half-band, in **cents**
/// * `s` — the order's distance from the adjusted midpoint, in **cents**
/// * `shares` — order size in **shares**, not dollars
///
/// Zero when the order is outside the band, when the band is degenerate, or when the size is
/// below the market's `min_size` (the caller applies that filter — see
/// [`book_side_score`]). The documented worked example (midpoint 0.50, `max_spread` 3¢) is
/// `((3−1)/3)²×100 + ((3−2)/3)²×200 + ((3−1)/3)²×100`, and it is a unit test below.
pub fn order_score(v_cents: Decimal, s_cents: Decimal, shares: Decimal) -> Decimal {
    if v_cents <= Decimal::ZERO || s_cents < Decimal::ZERO || s_cents > v_cents {
        return Decimal::ZERO;
    }
    if shares <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let f = (v_cents - s_cents) / v_cents;
    f * f * shares
}

/// The documented side-aggregation rule:
/// `Q_min = max( min(Q_one, Q_two), max(Q_one/3, Q_two/3) )`.
///
/// Two-sided quoting is worth 3× the score for 2× the capital, which is why the simulator
/// only ever quotes two-sided; the one-sided branch exists because *competitors* quote that
/// way and their score has to be counted the same way ours would be.
pub fn q_min(q_bid: Decimal, q_ask: Decimal) -> Decimal {
    let both = q_bid.min(q_ask);
    let single = (q_bid / ONE_SIDED_DIVISOR).max(q_ask / ONE_SIDED_DIVISOR);
    both.max(single)
}

/// True where a single-sided quote still scores (at `Q/3`): midpoint in `[0.10, 0.90]`.
/// Outside that band the venue requires two-sided quoting and a one-sided book scores zero.
pub fn single_sided_scores(mid: Decimal) -> bool {
    mid >= TWO_SIDED_BELOW && mid <= TWO_SIDED_ABOVE
}

/// [`q_min`], with the mandatory-two-sided band applied: outside `[0.10, 0.90]`, a side with
/// no qualifying size at all scores nothing rather than `Q/3`.
pub fn side_aggregate(mid: Decimal, q_bid: Decimal, q_ask: Decimal) -> Decimal {
    let one_sided = q_bid <= Decimal::ZERO || q_ask <= Decimal::ZERO;
    if one_sided && !single_sided_scores(mid) {
        return Decimal::ZERO;
    }
    q_min(q_bid, q_ask)
}

/// A condition's two books folded into one, in **YES coordinates**.
///
/// A resting buy of NO at `q` is economically a resting sell of YES at `1 − q`, so the NO
/// book's bids become ask-side competition and its asks become bid-side competition. Merging
/// them is the pessimistic choice: it can only *raise* the competing score we divide by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedBook {
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

impl MergedBook {
    /// Fold the YES book, and the mirrored NO book when we have it, into one.
    pub fn build(yes: Option<&OrderBook>, no: Option<&OrderBook>) -> Self {
        let mut bids: BTreeMap<Decimal, Decimal> = BTreeMap::new();
        let mut asks: BTreeMap<Decimal, Decimal> = BTreeMap::new();
        if let Some(book) = yes {
            for level in &book.bids {
                *bids.entry(level.price).or_default() += level.size;
            }
            for level in &book.asks {
                *asks.entry(level.price).or_default() += level.size;
            }
        }
        if let Some(book) = no {
            // bid_no(q) ≡ ask_yes(1 − q); ask_no(r) ≡ bid_yes(1 − r).
            for level in &book.bids {
                *asks.entry(Decimal::ONE - level.price).or_default() += level.size;
            }
            for level in &book.asks {
                *bids.entry(Decimal::ONE - level.price).or_default() += level.size;
            }
        }
        let mut bids: Vec<PriceLevel> = bids
            .into_iter()
            .filter(|(price, size)| *price > Decimal::ZERO && *size > Decimal::ZERO)
            .map(|(price, size)| PriceLevel::new(price, size))
            .collect();
        let mut asks: Vec<PriceLevel> = asks
            .into_iter()
            .filter(|(price, size)| *price > Decimal::ZERO && *size > Decimal::ZERO)
            .map(|(price, size)| PriceLevel::new(price, size))
            .collect();
        bids.sort_by(|a, b| b.price.cmp(&a.price));
        asks.sort_by(|a, b| a.price.cmp(&b.price));
        Self { bids, asks }
    }

    /// The **size-cutoff-adjusted** midpoint: the midpoint of the best bid and best ask
    /// *after discarding every level below `min_size`*.
    ///
    /// The cutoff is the venue's own anti-gaming rule (it stops a dust order pinning a fake
    /// midpoint), and it is load-bearing here for the same reason: the midpoint decides both
    /// where our quote sits and which competitors are in band.
    ///
    /// `None` when either side has no qualifying level — a sample we cannot take, which is
    /// counted as a lost sample rather than guessed at.
    pub fn adjusted_midpoint(&self, min_size: Decimal) -> Option<Decimal> {
        let best_bid = self
            .bids
            .iter()
            .find(|l| l.size >= min_size)
            .map(|l| l.price)?;
        let best_ask = self
            .asks
            .iter()
            .find(|l| l.size >= min_size)
            .map(|l| l.price)?;
        if best_ask <= best_bid {
            // A crossed or locked book after the size cutoff is not a midpoint we can
            // reason about; refuse rather than invent one.
            return None;
        }
        Some((best_bid + best_ask) / Decimal::TWO)
    }
}

/// Total in-band score resting on one side of a book.
///
/// Every level at or inside `v` cents of `mid` and at least `min_size` shares contributes
/// `((v − d)/v)² × size`, where `d` is its distance from the midpoint in cents.
pub fn book_side_score(
    levels: &[PriceLevel],
    mid: Decimal,
    v_cents: Decimal,
    min_size: Decimal,
) -> Decimal {
    let mut total = Decimal::ZERO;
    for level in levels {
        if level.size < min_size {
            continue;
        }
        let distance = dollars_to_cents((level.price - mid).abs());
        total += order_score(v_cents, distance, level.size);
    }
    total
}

// ---------------------------------------------------------------------------------
// Candidates, quotes and selection
// ---------------------------------------------------------------------------------

/// The reward parameters of one market, in the units the venue publishes them in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardParams {
    /// `rewards_max_spread`, the qualifying half-band, in **cents**.
    pub max_spread_cents: Decimal,
    /// `rewards_min_size`, in **shares**.
    pub min_size: Decimal,
    /// `Σ rewards_daily_rate`, USD/day. A configured cap, not a promised payout.
    pub pool_daily: Decimal,
}

/// A reward-eligible market the simulator may quote, with everything the report needs to
/// name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardCandidate {
    pub condition_id: String,
    pub event_slug: String,
    pub question: String,
    pub category: String,
    pub yes_token: TokenId,
    pub no_token: TokenId,
    pub params: RewardParams,
    /// Where the parameters came from: `sampling-markets` (the CLOB rewards endpoint) or
    /// `gamma` (the per-market fields on the discovery payload). Recorded because they can
    /// disagree, and the report should never pretend it does not know which it used.
    pub source: &'static str,
}

/// The two resting buys we simulate on one market, and what they score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotePlan {
    /// Our distance from the adjusted midpoint, in **cents**, after clamping to the band.
    pub s_cents: Decimal,
    /// Shares per side, at least the market's `min_size`.
    pub shares: Decimal,
    /// `shares × (1 − 2s)` — the capital the pair of buys commits, in USD.
    pub capital: Decimal,
    /// `Q_min` for our own two-sided quote at this size and distance.
    pub our_score: Decimal,
}

/// A candidate evaluated against the live book: what it would pay and what it would cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateScore {
    pub candidate: RewardCandidate,
    pub quote: QuotePlan,
    pub mid: Decimal,
    /// In-band competing score, both sides, in YES coordinates.
    pub competing_score: Decimal,
    /// `our_score / (our_score + competing_score)` — our share of one sample's pool.
    pub share: Decimal,
    /// `pool_daily × share`, before the $1/day floor.
    pub expected_gross: Decimal,
    /// The same after the floor: below $1/day a market pays **zero**, which is what makes
    /// concentration, not diversification, the optimal play at our size.
    pub expected_gross_paid: Decimal,
    /// Historical markout per day for this market from closed epochs, when we have any.
    /// `None` on the first day — and the report says the ranking is gross-only until then.
    pub markout_history: Option<Decimal>,
    /// `expected_gross_paid + markout_history.unwrap_or(0)`.
    pub expected_net: Decimal,
    /// `expected_net / capital`, the number the selection maximises.
    pub net_per_dollar: Decimal,
}

/// Work out where our quote sits and what it commits, given a market's parameters.
///
/// `s` is clamped to at most half the band: at `s = v` the score is exactly zero, and the
/// research's own sizing assumption is `s ≈ v/2` to `v/3`. A configured spread wider than
/// half the band would quietly buy a score of nearly nothing.
pub fn plan_quote(params: &RewardParams, cfg: &RewardSimConfig) -> Option<QuotePlan> {
    if params.max_spread_cents <= Decimal::ZERO {
        return None;
    }
    let half_band = params.max_spread_cents / Decimal::TWO;
    let s_cents = cfg.quote_spread_cents.min(half_band).max(Decimal::ZERO);
    let shares = cfg.quote_size_shares.max(params.min_size);
    if shares <= Decimal::ZERO {
        return None;
    }
    let s_dollars = cents_to_dollars(s_cents);
    // (mid − s) + (1 − mid − s) = 1 − 2s, independent of the price level.
    let capital = shares * (Decimal::ONE - Decimal::TWO * s_dollars);
    if capital <= Decimal::ZERO {
        return None;
    }
    let per_side = order_score(params.max_spread_cents, s_cents, shares);
    Some(QuotePlan {
        s_cents,
        shares,
        capital,
        // Two-sided and symmetric, so `Q_min` is simply the per-side score.
        our_score: q_min(per_side, per_side),
    })
}

/// Score one candidate against the books we hold. `None` when the book cannot be sampled.
pub fn evaluate_candidate(
    candidate: &RewardCandidate,
    books: &BookMap,
    cfg: &RewardSimConfig,
    markout_history: Option<Decimal>,
) -> Option<CandidateScore> {
    let quote = plan_quote(&candidate.params, cfg)?;
    let merged = MergedBook::build(
        books.get(&candidate.yes_token),
        books.get(&candidate.no_token),
    );
    let mid = merged.adjusted_midpoint(candidate.params.min_size)?;
    let competing = competing_score(&merged, mid, &candidate.params);
    let share = share_of_pool(quote.our_score, competing)?;
    let expected_gross = candidate.params.pool_daily * share;
    let expected_gross_paid = apply_payout_floor(expected_gross);
    let expected_net = expected_gross_paid + markout_history.unwrap_or(Decimal::ZERO);
    let net_per_dollar = expected_net.checked_div(quote.capital)?;
    Some(CandidateScore {
        candidate: candidate.clone(),
        quote,
        mid,
        competing_score: competing,
        share,
        expected_gross,
        expected_gross_paid,
        markout_history,
        expected_net,
        net_per_dollar,
    })
}

/// In-band competing score for a merged book, both sides, under the documented aggregation.
pub fn competing_score(merged: &MergedBook, mid: Decimal, params: &RewardParams) -> Decimal {
    let bid = book_side_score(
        &merged.bids,
        mid,
        params.max_spread_cents,
        params.min_size,
    );
    let ask = book_side_score(
        &merged.asks,
        mid,
        params.max_spread_cents,
        params.min_size,
    );
    side_aggregate(mid, bid, ask)
}

/// `our / (our + competition)` — our share of one sample's normalised score.
///
/// `None` when nothing scores at all (no pool share is defined), which is a lost sample.
pub fn share_of_pool(our_score: Decimal, competing_score: Decimal) -> Option<Decimal> {
    let total = our_score + competing_score;
    if total <= Decimal::ZERO {
        return None;
    }
    our_score.checked_div(total)
}

/// The $1/day/market payout floor: below it, nothing is paid and nothing rolls over.
pub fn apply_payout_floor(gross: Decimal) -> Decimal {
    if gross < PAYOUT_FLOOR_USD {
        Decimal::ZERO
    } else {
        gross
    }
}

/// Choose the day's portfolio: maximise expected net per committed dollar under the cap.
///
/// Two rules beyond the ranking, both from the research:
///
/// * a market whose expected gross does not clear **$1/day pays nothing at all**, so
///   committing capital to it is strictly worse than not quoting it (§3.3 — the floor makes
///   concentration, not diversification, optimal at our size);
/// * a market whose expected *net* is not positive is not quoted, because the whole point of
///   the exercise is that gross rewards are roughly the price of the adverse selection they
///   pay for.
pub fn select_portfolio(
    scores: &mut [CandidateScore],
    cfg: &RewardSimConfig,
) -> Vec<CandidateScore> {
    scores.sort_by(|a, b| {
        b.net_per_dollar
            .cmp(&a.net_per_dollar)
            // A deterministic tiebreak, so two runs over the same books choose the same
            // portfolio and a diff of the daily log means something.
            .then_with(|| a.candidate.condition_id.cmp(&b.candidate.condition_id))
    });
    let mut chosen = Vec::new();
    let mut committed = Decimal::ZERO;
    for score in scores.iter() {
        if chosen.len() >= cfg.max_markets {
            break;
        }
        if score.expected_gross_paid <= Decimal::ZERO || score.expected_net <= Decimal::ZERO {
            continue;
        }
        if committed + score.quote.capital > cfg.capital_cap_usd {
            continue;
        }
        committed += score.quote.capital;
        chosen.push(score.clone());
    }
    chosen
}

// ---------------------------------------------------------------------------------
// The epoch: what one market's day produced
// ---------------------------------------------------------------------------------

/// A closed epoch — one market, one UTC day, one verdict, written once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedEpoch {
    pub day: NaiveDate,
    pub condition_id: String,
    pub event_slug: String,
    pub question: String,
    pub category: String,
    pub params: RewardParams,
    /// Our quote: distance in cents, size in shares, capital committed in USD.
    pub s_cents: Decimal,
    pub shares: Decimal,
    pub capital: Decimal,
    /// Samples actually scored, and the samples a full day would have had.
    pub samples_scored: i64,
    pub samples_expected: i64,
    /// Samples attempted that produced no score — no book, no qualifying level, a crossed
    /// book after the size cutoff. A lost sample, counted so uptime is legible.
    pub samples_lost: i64,
    /// `Σ share_i` over the scored samples. Divided by `samples_expected`, this is our share
    /// of the day.
    pub share_sum: Decimal,
    pub our_score_sum: Decimal,
    pub competing_score_sum: Decimal,
    /// `pool_daily × share_sum / samples_expected`, before the floor.
    pub gross_usd: Decimal,
    /// The same after the $1/day/market floor. This is what would actually be paid.
    pub gross_paid_usd: Decimal,
    /// Simulated fills our in-band quotes would have taken, and their size.
    pub fills: i64,
    pub fill_shares: Decimal,
    /// Signed markout, in USD, at the short and long horizons. Negative is adverse selection.
    pub markout_short_usd: Decimal,
    pub markout_long_usd: Decimal,
    pub fills_marked_short: i64,
    pub fills_marked_long: i64,
    /// Fills whose short-horizon markout had not matured when the epoch closed. Excluded
    /// from `net_usd` rather than guessed at.
    pub fills_pending: i64,
    /// **The number the kill-line reads**: `gross_paid_usd + markout_short_usd`.
    pub net_usd: Decimal,
    /// The window contained time with no trade-print feed, so its markout is an absence of
    /// measurement rather than a measurement of zero adverse selection.
    pub no_print_feed: bool,
    pub opened_at: DateTime<Utc>,
    pub closed_at: DateTime<Utc>,
}

impl ClosedEpoch {
    /// True when this epoch cannot honestly contribute a *net* figure: no print feed, so the
    /// markout side of the ledger was never measured.
    pub fn markout_is_a_hole(&self) -> bool {
        self.no_print_feed
    }
}

/// One simulated fill, waiting for its markouts to mature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedFill {
    pub condition_id: String,
    pub token_id: TokenId,
    /// True when the fill is on the YES token (our `mid − s` buy); false for the NO token's
    /// `(1 − mid) − s` buy.
    pub yes_side: bool,
    /// Our quote price — the price we would have been filled *at*, not the print's price. A
    /// print through our quote fills us at our own limit, which is the pessimistic reading
    /// (we never capture the extra).
    pub price: Decimal,
    pub size: Decimal,
    /// The YES-coordinate midpoint at the moment of the fill.
    pub mid_at_fill: Decimal,
    pub filled_at: DateTime<Utc>,
    /// Signed markout per share at each horizon, `None` until it matures.
    pub markout_short: Option<Decimal>,
    pub markout_long: Option<Decimal>,
}

impl SimulatedFill {
    /// Signed markout **per share** against a later YES-coordinate midpoint.
    ///
    /// Both of our orders are buys, so the sign convention is the same for both: the value of
    /// what we bought, later, minus what we paid. A YES fill marks against `mid`; a NO fill
    /// marks against `1 − mid`, because that is what the NO token is worth in the same
    /// coordinates. Positive = the market came to us; negative = adverse selection.
    pub fn markout_against(&self, mid_yes: Decimal) -> Decimal {
        let reference = if self.yes_side {
            mid_yes
        } else {
            Decimal::ONE - mid_yes
        };
        reference - self.price
    }

    /// The same, in dollars over the whole fill.
    pub fn markout_usd(&self, mid_yes: Decimal) -> Decimal {
        self.markout_against(mid_yes) * self.size
    }
}

/// One market's in-flight epoch.
#[derive(Debug, Clone)]
struct MarketEpoch {
    day: NaiveDate,
    candidate: RewardCandidate,
    quote: QuotePlan,
    samples_scored: i64,
    samples_lost: i64,
    share_sum: Decimal,
    our_score_sum: Decimal,
    competing_score_sum: Decimal,
    fills: i64,
    fill_shares: Decimal,
    markout_short: Decimal,
    markout_long: Decimal,
    fills_marked_short: i64,
    fills_marked_long: i64,
    /// Our live quote prices, refreshed on each sample. `None` before the first scored
    /// sample: we cannot be filled at a price we never posted.
    yes_bid: Option<Decimal>,
    no_bid: Option<Decimal>,
    /// One fill per side per sample interval: a filled quote is gone until we re-post it on
    /// the next sample. It bounds the fill count to something a real quoting loop could
    /// produce rather than crediting every print in a burst.
    yes_filled_this_sample: bool,
    no_filled_this_sample: bool,
    pending: Vec<SimulatedFill>,
    no_print_feed: bool,
    opened_at: DateTime<Utc>,
}

impl MarketEpoch {
    fn close(mut self, now: DateTime<Utc>) -> (ClosedEpoch, Vec<SimulatedFill>) {
        let samples_expected = SAMPLES_PER_DAY;
        let gross = self
            .share_sum
            .checked_div(Decimal::from(samples_expected))
            .unwrap_or(Decimal::ZERO)
            * self.candidate.params.pool_daily;
        let gross_paid = apply_payout_floor(gross);
        // Anything still unmatured is a hole, not a zero: it is counted and left out.
        let fills_pending = self
            .pending
            .iter()
            .filter(|f| f.markout_short.is_none())
            .count() as i64;
        let epoch = ClosedEpoch {
            day: self.day,
            condition_id: self.candidate.condition_id.clone(),
            event_slug: self.candidate.event_slug.clone(),
            question: self.candidate.question.clone(),
            category: self.candidate.category.clone(),
            params: self.candidate.params.clone(),
            s_cents: self.quote.s_cents,
            shares: self.quote.shares,
            capital: self.quote.capital,
            samples_scored: self.samples_scored,
            samples_expected,
            samples_lost: self.samples_lost,
            share_sum: self.share_sum,
            our_score_sum: self.our_score_sum,
            competing_score_sum: self.competing_score_sum,
            gross_usd: gross,
            gross_paid_usd: gross_paid,
            fills: self.fills,
            fill_shares: self.fill_shares,
            markout_short_usd: self.markout_short,
            markout_long_usd: self.markout_long,
            fills_marked_short: self.fills_marked_short,
            fills_marked_long: self.fills_marked_long,
            fills_pending,
            net_usd: gross_paid + self.markout_short,
            no_print_feed: self.no_print_feed,
            opened_at: self.opened_at,
            closed_at: now,
        };
        (epoch, std::mem::take(&mut self.pending))
    }
}

/// Counters for the health line and `runtime_status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewardStatsSnapshot {
    pub markets_quoted: u64,
    pub samples_scored: u64,
    pub samples_lost: u64,
    pub fills: u64,
    pub epochs_closed: u64,
    pub prints_matched: u64,
    pub print_feed_live: bool,
}

#[derive(Debug, Default)]
struct Inner {
    /// The portfolio, keyed by condition id.
    epochs: HashMap<String, MarketEpoch>,
    /// token → condition ids quoting on it. Keeps a print O(work attached to that asset) on
    /// a 147 000-token feed, exactly as the maker simulator does.
    index: HashMap<TokenId, Vec<String>>,
    /// Closed epochs waiting for the daemon to persist them.
    closed: Vec<ClosedEpoch>,
    /// Matured fills waiting for the daemon to persist them.
    settled_fills: Vec<SimulatedFill>,
    /// Per-market mean daily markout from closed epochs, for the next day's ranking.
    markout_history: HashMap<String, Decimal>,
    current_day: Option<NaiveDate>,
    last_sample_at: Option<DateTime<Utc>>,
}

/// The simulator. Shared (`Arc`) between the daemon loop, which samples and drains, and the
/// stream's socket tasks, which feed it prints.
#[derive(Debug)]
pub struct RewardSimulator {
    cfg: RewardSimConfig,
    inner: Mutex<Inner>,
    print_feed_live: AtomicBool,
    markets_quoted: AtomicU64,
    samples_scored: AtomicU64,
    samples_lost: AtomicU64,
    fills: AtomicU64,
    epochs_closed: AtomicU64,
    prints_matched: AtomicU64,
}

impl RewardSimulator {
    pub fn new(cfg: &RewardSimConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            inner: Mutex::new(Inner::default()),
            // Assume no feed until a live stream says otherwise: the honest default flags the
            // measurement rather than trusting it.
            print_feed_live: AtomicBool::new(false),
            markets_quoted: AtomicU64::new(0),
            samples_scored: AtomicU64::new(0),
            samples_lost: AtomicU64::new(0),
            fills: AtomicU64::new(0),
            epochs_closed: AtomicU64::new(0),
            prints_matched: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn config(&self) -> &RewardSimConfig {
        &self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tell the simulator whether a print feed is running. Turning it **off** marks every
    /// open epoch `no_print_feed`, and the flag is sticky: once a window has a hole in it, it
    /// has one, and its markout is not evidence.
    pub fn set_print_feed(&self, live: bool) {
        let was = self.print_feed_live.swap(live, Ordering::Relaxed);
        if !live {
            let mut inner = self.lock();
            for epoch in inner.epochs.values_mut() {
                epoch.no_print_feed = true;
            }
        } else if !was {
            tracing::info!("rewards simulator: trade-print feed is live again");
        }
    }

    pub fn print_feed_live(&self) -> bool {
        self.print_feed_live.load(Ordering::Relaxed)
    }

    /// The tokens the current portfolio quotes on — the books the daemon has to fetch.
    pub fn portfolio_tokens(&self) -> Vec<TokenId> {
        let inner = self.lock();
        let mut out: Vec<TokenId> = inner.index.keys().cloned().collect();
        out.sort();
        out
    }

    /// The current portfolio's condition ids, in a stable order.
    pub fn portfolio(&self) -> Vec<String> {
        let inner = self.lock();
        let mut out: Vec<String> = inner.epochs.keys().cloned().collect();
        out.sort();
        out
    }

    pub fn portfolio_len(&self) -> usize {
        self.lock().epochs.len()
    }

    /// Per-market mean daily markout from closed epochs, for candidate ranking.
    pub fn markout_history(&self, condition_id: &str) -> Option<Decimal> {
        self.lock().markout_history.get(condition_id).copied()
    }

    /// Is a sample due? The scoring cadence is about a minute, and sampling faster would not
    /// make the estimate better — it would just weight whatever minute we happen to be in.
    pub fn sample_due(&self, now: DateTime<Utc>) -> bool {
        match self.lock().last_sample_at {
            None => true,
            Some(last) => (now - last).num_seconds() >= self.cfg.sample_interval_secs as i64,
        }
    }

    /// Install a freshly chosen portfolio, closing the epochs of any market that left it.
    ///
    /// A market that stays is left exactly as it is — its epoch keeps accumulating, because
    /// re-selection is not a new day and re-opening it would reset the day's evidence.
    pub fn set_portfolio(&self, chosen: &[CandidateScore], now: DateTime<Utc>) {
        let day = now.date_naive();
        let keep: Vec<String> = chosen
            .iter()
            .map(|s| s.candidate.condition_id.clone())
            .collect();
        let mut inner = self.lock();
        inner.current_day = Some(day);

        let dropped: Vec<String> = inner
            .epochs
            .keys()
            .filter(|id| !keep.contains(id))
            .cloned()
            .collect();
        for id in dropped {
            Self::close_one(&mut inner, &id, now);
            self.epochs_closed.fetch_add(1, Ordering::Relaxed);
        }
        for score in chosen {
            let id = score.candidate.condition_id.clone();
            if inner.epochs.contains_key(&id) {
                continue;
            }
            for token in [&score.candidate.yes_token, &score.candidate.no_token] {
                inner
                    .index
                    .entry(token.clone())
                    .or_default()
                    .push(id.clone());
            }
            inner.epochs.insert(
                id,
                MarketEpoch {
                    day,
                    candidate: score.candidate.clone(),
                    quote: score.quote,
                    samples_scored: 0,
                    samples_lost: 0,
                    share_sum: Decimal::ZERO,
                    our_score_sum: Decimal::ZERO,
                    competing_score_sum: Decimal::ZERO,
                    fills: 0,
                    fill_shares: Decimal::ZERO,
                    markout_short: Decimal::ZERO,
                    markout_long: Decimal::ZERO,
                    fills_marked_short: 0,
                    fills_marked_long: 0,
                    yes_bid: None,
                    no_bid: None,
                    yes_filled_this_sample: false,
                    no_filled_this_sample: false,
                    pending: Vec::new(),
                    no_print_feed: !self.print_feed_live(),
                    opened_at: now,
                },
            );
            self.markets_quoted.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Take one scoring sample of every market in the portfolio, and mature whatever markouts
    /// have come due.
    ///
    /// Returns the number of markets that produced a score. `books` need only cover the
    /// portfolio's tokens ([`portfolio_tokens`](Self::portfolio_tokens)).
    pub fn sample(&self, books: &BookMap, now: DateTime<Utc>) -> usize {
        let mut inner = self.lock();
        inner.last_sample_at = Some(now);
        let mut scored = 0usize;
        let short = chrono::Duration::seconds(self.cfg.markout_short_secs as i64);
        let long = chrono::Duration::seconds(self.cfg.markout_long_secs as i64);
        let mut settled: Vec<SimulatedFill> = Vec::new();

        for epoch in inner.epochs.values_mut() {
            let merged = MergedBook::build(
                books.get(&epoch.candidate.yes_token),
                books.get(&epoch.candidate.no_token),
            );
            let mid = merged.adjusted_midpoint(epoch.candidate.params.min_size);

            // Markouts mature against the midpoint of the moment they come due, so they need
            // this sample's midpoint — and only this sample's.
            if let Some(mid) = mid {
                for fill in epoch.pending.iter_mut() {
                    if fill.markout_short.is_none() && now - fill.filled_at >= short {
                        let value = fill.markout_usd(mid);
                        fill.markout_short = Some(fill.markout_against(mid));
                        epoch.markout_short += value;
                        epoch.fills_marked_short += 1;
                    }
                    if fill.markout_long.is_none() && now - fill.filled_at >= long {
                        let value = fill.markout_usd(mid);
                        fill.markout_long = Some(fill.markout_against(mid));
                        epoch.markout_long += value;
                        epoch.fills_marked_long += 1;
                    }
                }
                // A fill with both horizons marked is finished evidence: hand it to the
                // daemon to persist and stop carrying it.
                let (done, still_pending): (Vec<_>, Vec<_>) = std::mem::take(&mut epoch.pending)
                    .into_iter()
                    .partition(|f| f.markout_short.is_some() && f.markout_long.is_some());
                settled.extend(done);
                epoch.pending = still_pending;
            }

            match mid.and_then(|mid| {
                let competing = competing_score(&merged, mid, &epoch.candidate.params);
                share_of_pool(epoch.quote.our_score, competing).map(|share| (mid, competing, share))
            }) {
                Some((mid, competing, share)) => {
                    epoch.samples_scored += 1;
                    epoch.share_sum += share;
                    epoch.our_score_sum += epoch.quote.our_score;
                    epoch.competing_score_sum += competing;
                    // Re-post both quotes around the fresh midpoint.
                    let s = cents_to_dollars(epoch.quote.s_cents);
                    epoch.yes_bid = Some(mid - s);
                    epoch.no_bid = Some(Decimal::ONE - mid - s);
                    epoch.yes_filled_this_sample = false;
                    epoch.no_filled_this_sample = false;
                    scored += 1;
                    self.samples_scored.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    epoch.samples_lost += 1;
                    // No midpoint, no quote: we cannot be filled at a price we could not have
                    // computed.
                    epoch.yes_bid = None;
                    epoch.no_bid = None;
                    self.samples_lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        inner.settled_fills.extend(settled);
        scored
    }

    /// Apply one trade print: does it cross a quote we would have had resting?
    ///
    /// Only a **SELL** print can fill either of our orders — both are resting buys, one on
    /// each token — and only at or through our price. This is the M8 rule in reverse: there
    /// we asked whether the queue ahead of us had traded out, here we accept the fill,
    /// because that is precisely what the reward subsidy is paying us to accept.
    pub fn observe_at(&self, print: &TradePrint, now: DateTime<Utc>) {
        let Some((token, price, size)) = print.usable() else {
            return;
        };
        if size <= Decimal::ZERO || print.side != TradeSide::Sell {
            return;
        }
        let mut inner = self.lock();
        let Some(ids) = inner.index.get(token).cloned() else {
            return;
        };
        let mut matched = 0u64;
        let mut fills = 0u64;
        for id in ids {
            let Some(epoch) = inner.epochs.get_mut(&id) else {
                continue;
            };
            let yes_side = epoch.candidate.yes_token == *token;
            let (quote, already_filled) = if yes_side {
                (epoch.yes_bid, epoch.yes_filled_this_sample)
            } else {
                (epoch.no_bid, epoch.no_filled_this_sample)
            };
            let Some(quote) = quote else { continue };
            matched += 1;
            if already_filled || price > quote {
                continue;
            }
            // Filled at *our* limit, never at the print's better price, and never for more
            // than we quoted.
            let filled = size.min(epoch.quote.shares);
            let mid_at_fill = if yes_side {
                quote + cents_to_dollars(epoch.quote.s_cents)
            } else {
                Decimal::ONE - (quote + cents_to_dollars(epoch.quote.s_cents))
            };
            epoch.pending.push(SimulatedFill {
                condition_id: id.clone(),
                token_id: token.clone(),
                yes_side,
                price: quote,
                size: filled,
                mid_at_fill,
                filled_at: now,
                markout_short: None,
                markout_long: None,
            });
            epoch.fills += 1;
            epoch.fill_shares += filled;
            if yes_side {
                epoch.yes_filled_this_sample = true;
            } else {
                epoch.no_filled_this_sample = true;
            }
            fills += 1;
        }
        if matched > 0 {
            self.prints_matched.fetch_add(matched, Ordering::Relaxed);
        }
        if fills > 0 {
            self.fills.fetch_add(fills, Ordering::Relaxed);
        }
    }

    /// Close every epoch whose UTC day is behind `now` — the 00:00 UTC epoch boundary.
    ///
    /// Returns true when anything rolled, which is the daemon's signal to re-select the
    /// portfolio for the new day.
    pub fn roll_epoch(&self, now: DateTime<Utc>) -> bool {
        let today = now.date_naive();
        let mut inner = self.lock();
        let stale: Vec<String> = inner
            .epochs
            .iter()
            .filter(|(_, epoch)| epoch.day < today)
            .map(|(id, _)| id.clone())
            .collect();
        if stale.is_empty() {
            inner.current_day = Some(today);
            return false;
        }
        for id in stale {
            Self::close_one(&mut inner, &id, now);
            self.epochs_closed.fetch_add(1, Ordering::Relaxed);
        }
        inner.current_day = Some(today);
        true
    }

    /// Close everything — used at shutdown, so no epoch is lost silently.
    pub fn close_all(&self, now: DateTime<Utc>) {
        let mut inner = self.lock();
        let ids: Vec<String> = inner.epochs.keys().cloned().collect();
        for id in ids {
            Self::close_one(&mut inner, &id, now);
            self.epochs_closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn close_one(inner: &mut Inner, id: &str, now: DateTime<Utc>) {
        let Some(epoch) = inner.epochs.remove(id) else {
            return;
        };
        for token in [
            epoch.candidate.yes_token.clone(),
            epoch.candidate.no_token.clone(),
        ] {
            if let Some(ids) = inner.index.get_mut(&token) {
                ids.retain(|other| other != id);
                if ids.is_empty() {
                    inner.index.remove(&token);
                }
            }
        }
        let (closed, pending) = epoch.close(now);
        // The next day's ranking learns from this one: a market that cost us more in markout
        // than it paid in rewards is ranked on that fact, not on its advertised pool.
        inner
            .markout_history
            .insert(closed.condition_id.clone(), closed.markout_short_usd);
        // Fills that matured are evidence and are persisted; the rest are a counted hole in
        // the closed epoch (`fills_pending`) and are dropped rather than back-filled later.
        inner
            .settled_fills
            .extend(pending.into_iter().filter(|f| f.markout_short.is_some()));
        inner.closed.push(closed);
    }

    /// Take the closed epochs waiting to be persisted.
    pub fn drain_closed(&self) -> Vec<ClosedEpoch> {
        std::mem::take(&mut self.lock().closed)
    }

    /// Take the matured fills waiting to be persisted.
    pub fn drain_fills(&self) -> Vec<SimulatedFill> {
        std::mem::take(&mut self.lock().settled_fills)
    }

    pub fn stats(&self) -> RewardStatsSnapshot {
        RewardStatsSnapshot {
            markets_quoted: self.markets_quoted.load(Ordering::Relaxed),
            samples_scored: self.samples_scored.load(Ordering::Relaxed),
            samples_lost: self.samples_lost.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
            epochs_closed: self.epochs_closed.load(Ordering::Relaxed),
            prints_matched: self.prints_matched.load(Ordering::Relaxed),
            print_feed_live: self.print_feed_live(),
        }
    }
}

/// Prints reach the simulator through the same `BookStore` hook the maker simulator uses,
/// behind a [`crate::ws::PrintFanout`].
impl PrintObserver for RewardSimulator {
    fn observe(&self, print: &TradePrint, _received: Instant) {
        self.observe_at(print, Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn params(v: Decimal, min_size: Decimal, pool: Decimal) -> RewardParams {
        RewardParams {
            max_spread_cents: v,
            min_size,
            pool_daily: pool,
        }
    }

    fn cfg() -> RewardSimConfig {
        RewardSimConfig {
            enabled: true,
            capital_cap_usd: dec!(2000),
            quote_spread_cents: dec!(1),
            quote_size_shares: dec!(100),
            sample_interval_secs: 60,
            max_markets: 20,
            max_candidates: 150,
            max_candidate_pages: 5,
            markout_short_secs: 300,
            markout_long_secs: 3_600,
            kill_line_net_usd_per_day: dec!(1),
            kill_line_window_days: 14,
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_787_000_000 + secs, 0).expect("timestamp")
    }

    /// The documented worked example, verbatim from the research doc §1.2: adjusted midpoint
    /// 0.50, `max_spread` 3¢, three orders at 1¢/2¢/1¢ for 100/200/100 shares.
    ///
    /// `((3−1)/3)²×100 + ((3−2)/3)²×200 + ((3−1)/3)²×100`
    ///  = 0.444444…×100 + 0.111111…×200 + 0.444444…×100
    ///  = 44.4444… + 22.2222… + 44.4444… = 111.1111…
    #[test]
    fn the_documented_worked_example_reproduces_by_hand() {
        let v = dec!(3);
        let a = order_score(v, dec!(1), dec!(100));
        let b = order_score(v, dec!(2), dec!(200));
        let c = order_score(v, dec!(1), dec!(100));
        // (2/3)² = 4/9 exactly; Decimal division truncates, so compare within a cent.
        assert!((a - dec!(44.4444444)).abs() < dec!(0.001), "got {a}");
        assert!((b - dec!(22.2222222)).abs() < dec!(0.001), "got {b}");
        assert!((a + b + c - dec!(111.111111)).abs() < dec!(0.001));

        // The band edges, hand-checked: at the edge the score is zero, outside it is zero,
        // and at the midpoint itself it is the full size.
        assert_eq!(order_score(v, dec!(3), dec!(100)), Decimal::ZERO);
        assert_eq!(order_score(v, dec!(3.5), dec!(100)), Decimal::ZERO);
        assert_eq!(order_score(v, dec!(0), dec!(100)), dec!(100));
        // A degenerate band cannot score.
        assert_eq!(order_score(dec!(0), dec!(0), dec!(100)), Decimal::ZERO);
    }

    /// The both-sides rule, `Q_min = max(min(a,b), max(a/3, b/3))`, at every branch.
    #[test]
    fn the_both_sides_rule_prefers_the_smaller_side_and_penalises_one_sidedness() {
        // Balanced: the min *is* the answer, and it beats either third.
        assert_eq!(q_min(dec!(90), dec!(90)), dec!(90));
        // Lopsided: 30 vs 300 → min 30, thirds 10 and 100 → 100 wins.
        assert_eq!(q_min(dec!(30), dec!(300)), dec!(100));
        // One-sided: min is 0, so the third of the live side carries it.
        assert_eq!(q_min(dec!(60), Decimal::ZERO), dec!(20));

        // …but outside [0.10, 0.90] a one-sided book scores nothing at all.
        assert_eq!(side_aggregate(dec!(0.5), dec!(60), Decimal::ZERO), dec!(20));
        assert_eq!(
            side_aggregate(dec!(0.05), dec!(60), Decimal::ZERO),
            Decimal::ZERO,
            "below 0.10 two-sided quoting is mandatory"
        );
        assert_eq!(
            side_aggregate(dec!(0.95), Decimal::ZERO, dec!(60)),
            Decimal::ZERO
        );
        // Exactly on the boundary the single-sided allowance still applies.
        assert!(single_sided_scores(dec!(0.10)));
        assert!(single_sided_scores(dec!(0.90)));
        assert!(!single_sided_scores(dec!(0.0999)));
        // Two-sided is unaffected by the band.
        assert_eq!(side_aggregate(dec!(0.02), dec!(60), dec!(60)), dec!(60));
    }

    /// Cents vs dollars, the 100× bug class, pinned in both directions.
    #[test]
    fn the_cents_to_dollars_conversion_is_named_and_exact() {
        assert_eq!(cents_to_dollars(dec!(3.5)), dec!(0.035));
        assert_eq!(dollars_to_cents(dec!(0.035)), dec!(3.5));
        assert_eq!(cents_to_dollars(dec!(1)), dec!(0.01));
    }

    /// Two-sided score per paired share is `((v−s)/v)²` and the capital is `1 − 2s` per
    /// share, independent of the market's price level (research §1.3).
    #[test]
    fn a_quote_plan_commits_about_a_dollar_per_paired_share() {
        let cfg = cfg();
        let plan = plan_quote(&params(dec!(4), dec!(50), dec!(30)), &cfg).expect("plan");
        assert_eq!(plan.s_cents, dec!(1), "1¢ is inside half of a 4¢ band");
        assert_eq!(plan.shares, dec!(100));
        // (1 − 2×0.01) × 100 = 98
        assert_eq!(plan.capital, dec!(98.00));
        // ((4−1)/4)² × 100 = 0.5625 × 100 = 56.25, and both sides are equal so Q_min is it.
        assert_eq!(plan.our_score, dec!(56.25));

        // A tight band clamps our distance to half of it rather than scoring ~nothing.
        let tight = plan_quote(&params(dec!(1.5), dec!(200), dec!(50)), &cfg).expect("plan");
        assert_eq!(tight.s_cents, dec!(0.75));
        // min_size wins over the configured size when it is larger.
        assert_eq!(tight.shares, dec!(200));
        assert_eq!(tight.capital, dec!(197.000));

        // A market with no band is not quotable at all.
        assert!(plan_quote(&params(dec!(0), dec!(50), dec!(30)), &cfg).is_none());
    }

    fn book(token: &str, bids: &[(Decimal, Decimal)], asks: &[(Decimal, Decimal)]) -> OrderBook {
        OrderBook::new(
            TokenId::new(token),
            bids.iter().map(|(p, s)| PriceLevel::new(*p, *s)).collect(),
            asks.iter().map(|(p, s)| PriceLevel::new(*p, *s)).collect(),
        )
        .normalized()
    }

    /// The NO book is mirrored into YES coordinates and *added* — never ignored, because
    /// ignoring it would shrink the competition and flatter our share.
    #[test]
    fn the_no_book_is_mirrored_into_yes_coordinates() {
        let yes = book("y", &[(dec!(0.49), dec!(100))], &[(dec!(0.51), dec!(100))]);
        // A NO bid at 0.48 is a YES ask at 0.52; a NO ask at 0.52 is a YES bid at 0.48.
        let no = book("n", &[(dec!(0.48), dec!(300))], &[(dec!(0.52), dec!(200))]);
        let merged = MergedBook::build(Some(&yes), Some(&no));
        assert_eq!(
            merged.bids,
            vec![
                PriceLevel::new(dec!(0.49), dec!(100)),
                PriceLevel::new(dec!(0.48), dec!(200)),
            ]
        );
        assert_eq!(
            merged.asks,
            vec![
                PriceLevel::new(dec!(0.51), dec!(100)),
                PriceLevel::new(dec!(0.52), dec!(300)),
            ]
        );
        assert_eq!(merged.adjusted_midpoint(dec!(50)), Some(dec!(0.50)));
    }

    /// The size cutoff decides the midpoint, exactly as the venue's anti-dust rule does.
    #[test]
    fn the_adjusted_midpoint_discards_levels_below_min_size() {
        let yes = book(
            "y",
            &[(dec!(0.60), dec!(5)), (dec!(0.50), dec!(500))],
            &[(dec!(0.61), dec!(5)), (dec!(0.70), dec!(500))],
        );
        let merged = MergedBook::build(Some(&yes), None);
        // With no cutoff the dust pins 0.605; with a 50-share cutoff the real midpoint is 0.60.
        assert_eq!(merged.adjusted_midpoint(dec!(1)), Some(dec!(0.605)));
        assert_eq!(merged.adjusted_midpoint(dec!(50)), Some(dec!(0.60)));
        // Nothing qualifying on a side = no sample, not a guessed midpoint.
        assert_eq!(merged.adjusted_midpoint(dec!(1000)), None);
        assert_eq!(MergedBook::default().adjusted_midpoint(dec!(1)), None);
    }

    /// Pool share and the $1/day floor, hand-computed.
    #[test]
    fn pool_share_and_the_one_dollar_floor() {
        // Our 56.25 against 168.75 of competition = exactly a quarter of the pool.
        let share = share_of_pool(dec!(56.25), dec!(168.75)).expect("share");
        assert_eq!(share, dec!(0.25));
        // A $30/day pool at a quarter share is $7.50/day: comfortably over the floor.
        assert_eq!(apply_payout_floor(dec!(30) * share), dec!(7.50));
        // A $5/day pool at a 10% share is $0.50 — and $0.50 is paid as **nothing**.
        assert_eq!(apply_payout_floor(dec!(0.50)), Decimal::ZERO);
        // Exactly $1.00 clears it.
        assert_eq!(apply_payout_floor(dec!(1.00)), dec!(1.00));
        assert_eq!(apply_payout_floor(dec!(0.999999)), Decimal::ZERO);
        // No score anywhere at all is a lost sample, not a 100% share.
        assert_eq!(share_of_pool(Decimal::ZERO, Decimal::ZERO), None);
    }

    /// Markout sign convention: both of our orders are buys, and a NO fill marks against
    /// `1 − mid`. A filled pair with an unmoved midpoint banks exactly `2s`.
    #[test]
    fn markout_signs_and_the_paired_merge_edge() {
        let yes_fill = SimulatedFill {
            condition_id: "c".into(),
            token_id: TokenId::new("y"),
            yes_side: true,
            price: dec!(0.49), // mid 0.50 − 1¢
            size: dec!(100),
            mid_at_fill: dec!(0.50),
            filled_at: t(0),
            markout_short: None,
            markout_long: None,
        };
        let no_fill = SimulatedFill {
            token_id: TokenId::new("n"),
            yes_side: false,
            price: dec!(0.49), // (1 − 0.50) − 1¢
            ..yes_fill.clone()
        };

        // Midpoint unmoved: each leg is +1¢/share, and the pair banks 2s = 2¢/share = $2.
        assert_eq!(yes_fill.markout_against(dec!(0.50)), dec!(0.01));
        assert_eq!(no_fill.markout_against(dec!(0.50)), dec!(0.01));
        assert_eq!(
            yes_fill.markout_usd(dec!(0.50)) + no_fill.markout_usd(dec!(0.50)),
            dec!(2.00)
        );

        // The market runs away from the YES buy: mid 0.50 → 0.44 is −5¢/share on that leg
        // (bought at 0.49, now worth 0.44) and +5¢ on the NO leg (worth 0.56, paid 0.49).
        assert_eq!(yes_fill.markout_against(dec!(0.44)), dec!(-0.05));
        assert_eq!(no_fill.markout_against(dec!(0.44)), dec!(0.07));
        // The one-sided case is the loss case: only the YES leg filled.
        assert_eq!(yes_fill.markout_usd(dec!(0.44)), dec!(-5.00));
    }

    fn candidate(id: &str, yes: &str, no: &str, p: RewardParams) -> RewardCandidate {
        RewardCandidate {
            condition_id: id.into(),
            event_slug: format!("event-{id}"),
            question: format!("Question {id}?"),
            category: "politics".into(),
            yes_token: TokenId::new(yes),
            no_token: TokenId::new(no),
            params: p,
            source: "sampling-markets",
        }
    }

    fn books_at(mid_bid: Decimal, mid_ask: Decimal, size: Decimal) -> BookMap {
        let mut map = BookMap::new();
        map.insert(
            TokenId::new("y1"),
            book("y1", &[(mid_bid, size)], &[(mid_ask, size)]),
        );
        map
    }

    /// Selection maximises expected net per committed dollar, refuses anything under the
    /// $1/day floor, and stops at the capital cap.
    #[test]
    fn selection_ranks_by_net_per_dollar_and_respects_the_cap() {
        let mut cfg = cfg();
        cfg.capital_cap_usd = dec!(150); // room for exactly one 98-dollar quote
        let books = books_at(dec!(0.49), dec!(0.51), dec!(500));

        let rich = candidate("rich", "y1", "n1", params(dec!(4), dec!(50), dec!(300)));
        let thin = candidate("thin", "y1", "n1", params(dec!(4), dec!(50), dec!(2)));
        let mut scores: Vec<CandidateScore> = [&rich, &thin]
            .into_iter()
            .filter_map(|c| evaluate_candidate(c, &books, &cfg, None))
            .collect();
        assert_eq!(scores.len(), 2);

        let chosen = select_portfolio(&mut scores, &cfg);
        assert_eq!(chosen.len(), 1, "the cap admits one market");
        assert_eq!(chosen[0].candidate.condition_id, "rich");
        assert!(chosen[0].expected_gross_paid > Decimal::ZERO);

        // The thin pool's share is real but under the $1/day floor, so it pays nothing and
        // is never selected — the floor is what makes concentration optimal at our size.
        let thin_score = scores
            .iter()
            .find(|s| s.candidate.condition_id == "thin")
            .expect("scored");
        assert!(thin_score.expected_gross > Decimal::ZERO);
        assert_eq!(thin_score.expected_gross_paid, Decimal::ZERO);
        assert_eq!(thin_score.expected_net, Decimal::ZERO);

        // A measured markout history is subtracted before ranking, and can veto a market.
        let with_history = evaluate_candidate(&rich, &books, &cfg, Some(dec!(-500))).expect("score");
        assert!(with_history.expected_net < Decimal::ZERO);
        let mut only_bad = vec![with_history];
        assert!(
            select_portfolio(&mut only_bad, &cfg).is_empty(),
            "a market that costs more in markout than it pays is not quoted"
        );
    }

    fn sell(token: &str, price: Decimal, size: Decimal) -> TradePrint {
        TradePrint {
            asset_id: Some(TokenId::new(token)),
            price: Some(price),
            size: Some(size),
            side: TradeSide::Sell,
            server_ts: None,
            transaction_hash: None,
        }
    }

    fn simulator() -> RewardSimulator {
        let sim = RewardSimulator::new(&cfg());
        sim.set_print_feed(true);
        sim
    }

    fn portfolio_of(sim: &RewardSimulator, books: &BookMap, now: DateTime<Utc>) -> usize {
        let c = candidate("c1", "y1", "n1", params(dec!(4), dec!(50), dec!(300)));
        let cfg = cfg();
        let mut scores: Vec<CandidateScore> = [c]
            .iter()
            .filter_map(|c| evaluate_candidate(c, books, &cfg, None))
            .collect();
        let chosen = select_portfolio(&mut scores, &cfg);
        sim.set_portfolio(&chosen, now);
        chosen.len()
    }

    /// End to end over one market: sample, get crossed, mature the markout, close the epoch.
    #[test]
    fn a_sampled_market_accumulates_score_takes_a_fill_and_closes_with_a_net() {
        let sim = simulator();
        let mut books = books_at(dec!(0.49), dec!(0.51), dec!(500));
        books.insert(
            TokenId::new("n1"),
            book("n1", &[(dec!(0.49), dec!(500))], &[(dec!(0.51), dec!(500))]),
        );
        assert_eq!(portfolio_of(&sim, &books, t(0)), 1);
        assert_eq!(sim.portfolio_tokens().len(), 2);

        // One sample: mid 0.50, our quotes go out at 0.49 on each token.
        assert_eq!(sim.sample(&books, t(0)), 1);
        assert!(sim.sample_due(t(60)));
        assert!(!sim.sample_due(t(30)));

        // A SELL through our YES bid fills us at our own price, once.
        sim.observe_at(&sell("y1", dec!(0.48), dec!(40)), t(10));
        sim.observe_at(&sell("y1", dec!(0.47), dec!(999)), t(20));
        // A print above our bid never reaches us.
        sim.observe_at(&sell("n1", dec!(0.60), dec!(999)), t(21));
        assert_eq!(sim.stats().fills, 1, "one fill per side per sample interval");

        // The short horizon matures five minutes after the fill (t=10), not five minutes
        // after the sample: at t=320 it is due, and the midpoint has not moved, so the
        // markout is +1¢ × 40 shares = $0.40.
        sim.sample(&books, t(320));
        sim.close_all(t(400));
        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        let epoch = &closed[0];
        assert_eq!(epoch.fills, 1);
        assert_eq!(epoch.fill_shares, dec!(40));
        assert_eq!(epoch.fills_marked_short, 1);
        assert_eq!(epoch.markout_short_usd, dec!(0.40));
        assert_eq!(epoch.samples_scored, 2);
        // Two samples of a 1 440-sample day at a share of 56.25/(56.25+competition).
        assert!(epoch.gross_usd > Decimal::ZERO);
        // …and two samples cannot clear $1/day, so the paid figure is zero and the net is
        // the markout alone. That is the floor doing exactly what it does live.
        assert_eq!(epoch.gross_paid_usd, Decimal::ZERO);
        assert_eq!(epoch.net_usd, dec!(0.40));
        assert!(!epoch.no_print_feed);
        assert_eq!(sim.portfolio_len(), 0);
    }

    /// A window with no print feed is a hole: the flag is sticky and survives the feed
    /// coming back, exactly as it does for the maker simulator.
    #[test]
    fn a_window_without_a_print_feed_is_flagged_not_zeroed() {
        let sim = RewardSimulator::new(&cfg()); // never marked live
        let books = books_at(dec!(0.49), dec!(0.51), dec!(500));
        portfolio_of(&sim, &books, t(0));
        sim.close_all(t(10));
        assert!(sim.drain_closed()[0].no_print_feed);

        let sim = simulator();
        portfolio_of(&sim, &books, t(0));
        sim.set_print_feed(false);
        sim.set_print_feed(true);
        sim.close_all(t(10));
        assert!(
            sim.drain_closed()[0].markout_is_a_hole(),
            "a gap in the feed cannot be read as an absence of adverse selection"
        );
    }

    /// A lost sample is counted, and it costs us the day's share — it is never dropped from
    /// the average, because live downtime is a proportional loss of the epoch.
    #[test]
    fn a_sample_with_no_usable_book_is_lost_not_skipped() {
        let sim = simulator();
        let books = books_at(dec!(0.49), dec!(0.51), dec!(500));
        portfolio_of(&sim, &books, t(0));

        sim.sample(&books, t(0));
        sim.sample(&BookMap::new(), t(60)); // the books went away
        sim.close_all(t(120));
        let epoch = &sim.drain_closed()[0];
        assert_eq!(epoch.samples_scored, 1);
        assert_eq!(epoch.samples_lost, 1);
        assert_eq!(epoch.samples_expected, SAMPLES_PER_DAY);

        // With no midpoint there is no quote, so a print in that window cannot fill us.
        assert_eq!(sim.stats().fills, 0);
    }

    /// The epoch rolls at the UTC boundary and never mid-day, and a closed epoch's markout
    /// becomes the next day's ranking input.
    #[test]
    fn epochs_roll_at_the_utc_boundary_and_feed_the_next_days_ranking() {
        let sim = simulator();
        let books = books_at(dec!(0.49), dec!(0.51), dec!(500));
        let midday = DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
            .expect("ts")
            .with_timezone(&Utc);
        portfolio_of(&sim, &books, midday);

        assert!(!sim.roll_epoch(midday + chrono::Duration::hours(6)));
        assert_eq!(sim.portfolio_len(), 1);
        assert!(sim.roll_epoch(midday + chrono::Duration::hours(13)));
        assert_eq!(sim.portfolio_len(), 0, "the day's epoch closed");
        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].day, midday.date_naive());
        assert_eq!(sim.markout_history("c1"), Some(Decimal::ZERO));
    }

    /// Only a SELL print can fill a resting buy, and only at or through our price.
    #[test]
    fn buy_prints_and_prints_above_our_quote_never_fill_us() {
        let sim = simulator();
        let books = books_at(dec!(0.49), dec!(0.51), dec!(500));
        portfolio_of(&sim, &books, t(0));
        sim.sample(&books, t(0));

        sim.observe_at(&sell("y1", dec!(0.495), dec!(100)), t(1)); // above our 0.49 bid
        sim.observe_at(
            &TradePrint {
                side: TradeSide::Buy,
                ..sell("y1", dec!(0.40), dec!(100))
            },
            t(2),
        );
        sim.observe_at(&sell("nobody", dec!(0.10), dec!(100)), t(3));
        assert_eq!(sim.stats().fills, 0);
        // The two prints on a token we quote are still counted as matched work.
        assert_eq!(sim.stats().prints_matched, 1);
    }
}
