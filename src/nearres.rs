//! R2 — the near-resolution observation study.
//!
//! ## The question
//!
//! Buying a near-certain outcome at 96–99¢ shortly before it resolves is the second strategy
//! `research/retail-strategy-survey-2026-08.md` says survives fees at our size. It has no
//! latency requirement, no informational edge, and no quoting: it is a capital-velocity
//! business. Its two ways of dying are also known in advance — a *loss rate* worse than the
//! price implies (the 3¢ you collect is wiped out by one 97¢ position in forty going to
//! zero), and *time-to-payout* long enough that the annualised yield is worse than leaving
//! the money alone.
//!
//! So this module measures exactly those two numbers, on other people's money, before a
//! dollar is committed:
//!
//! ```text
//!   loss rate      = resolutions that did not pay ÷ resolutions observed
//!   annual yield   = (Σ payout − Σ ask − Σ fee) ÷ Σ ask, annualised at the observed
//!                    time-to-payout plus a recycling overhead
//! ```
//!
//! **Nothing here simulates our own orders.** There is no quote, no queue position and no
//! fill model — only the executable ask that was on the book when the market qualified, and
//! what that market actually did afterwards.
//!
//! ## What it refuses to do
//!
//! * **Qualify on anything but the executable ask.** Mid and last-trade both flatter the
//!   entry; the ask is what a buyer would have paid.
//! * **Guess a resolution.** A market whose outcome the API does not state is
//!   [`ResolutionOutcome::Undetermined`] — counted, reported, and excluded from the loss
//!   rate. It is a hole in the measurement, not a win.
//! * **Guess a resolution time.** A market whose event has no end date never qualifies.
//! * **Revise a verdict.** A qualification is inserted once, when the ask first lands in the
//!   band, and a resolution is inserted once, when it is first observed. Neither is updated,
//!   and the observation keeps the *entry* prices it qualified at even if the market later
//!   trades better.
//! * **Claim a kill-line verdict on a handful of resolutions.** Below
//!   `nearres.min_resolutions_for_verdict` the report prints the count, a Wilson interval and
//!   a warning instead of a verdict: distinguishing a 1-in-40 loss rate from a 1-in-10 one
//!   needs tens of resolutions, and pretending otherwise is how a coin flip becomes a
//!   strategy.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::NearResConfig;
use crate::costs::{taker_fee_per_share, FeeModel};
use crate::gamma::RawMarket;
use crate::types::{BookMap, Side, TokenId, Universe};

/// Hours in a year, for annualising an observed holding period.
const HOURS_PER_YEAR: i64 = 8_760;

/// One market qualifying for the study: the executable entry that was really there, and the
/// metadata needed to follow it to resolution. Written once, never updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qualification {
    pub token_id: TokenId,
    pub condition_id: String,
    pub event_slug: String,
    pub question: String,
    pub outcome: String,
    /// Which of the market's two outcome tokens this is (0 or 1) — the index into the
    /// resolved `outcomePrices` vector.
    pub outcome_index: usize,
    pub category: String,
    /// The **executable** best ask at qualification. Never a mid, never a last trade.
    pub ask: Decimal,
    /// Size resting at that ask, and the whole ask side. Together they say whether the entry
    /// existed at any size worth having.
    pub ask_size: Decimal,
    pub ask_depth: Decimal,
    pub best_bid: Option<Decimal>,
    /// The taker fee rate this market would be charged at, from the API where it states one.
    pub fee_rate: Decimal,
    /// `rate × p × (1 − p)` at the entry ask — tiny at these prices, and favourable, which
    /// is half of why the strategy is interesting at all.
    pub fee_per_share: Decimal,
    pub end_date: DateTime<Utc>,
    pub hours_to_resolution: Decimal,
    pub qualified_at: DateTime<Utc>,
}

impl Qualification {
    /// What one share would net if it pays out in full: `1 − ask − fee`.
    pub fn net_if_won(&self) -> Decimal {
        Decimal::ONE - self.ask - self.fee_per_share
    }
}

/// How a tracked market ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// Paid $1.00 — the outcome we would have bought won.
    Won,
    /// Paid $0.00.
    Lost,
    /// Paid something in between (a split or partially-invalid resolution). Counted with the
    /// losses in the loss rate — it is not a win — and carried at its real payout in the
    /// yield arithmetic.
    Split,
    /// The API did not state a usable outcome. **A hole, not a result**: excluded from the
    /// loss rate and from the yield, and reported as its own number.
    Undetermined,
}

impl ResolutionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Won => "won",
            Self::Lost => "lost",
            Self::Split => "split",
            Self::Undetermined => "undetermined",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        match raw {
            "won" => Self::Won,
            "lost" => Self::Lost,
            "split" => Self::Split,
            _ => Self::Undetermined,
        }
    }
}

/// The verdict for one observation. Written once, when resolution is first observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub token_id: TokenId,
    pub condition_id: String,
    pub outcome: ResolutionOutcome,
    /// Realised payout per share (`1`, `0`, or the stated split). `None` when undetermined.
    pub payout: Option<Decimal>,
    /// From qualification to the moment we could see it resolved. Measured at the resolution
    /// poll's granularity, so it is an *upper* bound on the true time-to-payout by at most
    /// one poll interval — stated rather than smoothed away.
    pub hours_to_payout: Decimal,
    /// `umaResolutionStatus` verbatim, when the API states one.
    pub uma_status: Option<String>,
    /// True only when the API positively indicates a dispute. Absent status = `false` here
    /// and "not visible" in the report — never "undisputed".
    pub disputed: bool,
    pub resolved_at: DateTime<Utc>,
}

/// What a `/markets` lookup said about one condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionFacts {
    pub condition_id: String,
    /// Payout per outcome index, when the API states a usable vector.
    pub payouts: Option<Vec<Decimal>>,
    pub uma_status: Option<String>,
    pub disputed: bool,
    pub closed: bool,
}

/// Read the resolution facts off a raw Gamma market object.
///
/// The payout vector is only accepted when it has one entry per outcome token and the
/// entries sum to $1.00 (within a cent). Anything else — an empty array, a market still
/// trading, a vector that does not add up — yields `payouts: None`, which becomes
/// [`ResolutionOutcome::Undetermined`] rather than a guessed winner.
pub fn resolution_facts(m: &RawMarket) -> Option<ResolutionFacts> {
    let condition_id = m.condition_id.clone().filter(|c| !c.trim().is_empty())?;
    let payouts = m.outcome_prices.as_ref().and_then(|raw| {
        let parsed: Vec<Decimal> = raw
            .iter()
            .filter_map(|p| p.trim().parse::<Decimal>().ok())
            .collect();
        if parsed.len() != raw.len() || parsed.len() < 2 {
            return None;
        }
        let sum: Decimal = parsed.iter().copied().sum();
        ((sum - Decimal::ONE).abs() <= Decimal::new(1, 2)).then_some(parsed)
    });
    let uma_status = m
        .uma_resolution_status
        .as_ref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let disputed = uma_status
        .as_deref()
        .is_some_and(|s| s.contains("dispute") || s.contains("challenge"));
    Some(ResolutionFacts {
        condition_id,
        payouts,
        uma_status,
        disputed,
        closed: m.closed.unwrap_or(false),
    })
}

/// Classify one observation against the facts. A market that is still open, or whose payout
/// vector we could not read, is [`ResolutionOutcome::Undetermined`].
pub fn classify(
    qualification: &Qualification,
    facts: &ResolutionFacts,
) -> (ResolutionOutcome, Option<Decimal>) {
    let Some(payouts) = facts.payouts.as_ref() else {
        return (ResolutionOutcome::Undetermined, None);
    };
    let Some(payout) = payouts.get(qualification.outcome_index).copied() else {
        return (ResolutionOutcome::Undetermined, None);
    };
    let outcome = if payout >= Decimal::ONE {
        ResolutionOutcome::Won
    } else if payout <= Decimal::ZERO {
        ResolutionOutcome::Lost
    } else {
        ResolutionOutcome::Split
    };
    (outcome, Some(payout))
}

// ---------------------------------------------------------------------------------
// The observer
// ---------------------------------------------------------------------------------

/// Why a market did not qualify. Counted, never silent — "we saw nothing" and "we looked at
/// nothing" are different findings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualifyCounters {
    pub tokens_examined: u64,
    pub no_book: u64,
    pub outside_price_band: u64,
    pub no_end_date: u64,
    pub too_far_out: u64,
    pub already_tracked: u64,
    pub at_capacity: u64,
    pub qualified: u64,
}

/// Tracks which markets have qualified and which are waiting for a resolution.
#[derive(Debug, Default)]
struct ObserverState {
    /// Every token that has ever qualified in this process (plus whatever the database knew
    /// at startup): a qualification happens once, at the *first* moment the ask is in band.
    seen: HashSet<TokenId>,
    /// Qualifications with no resolution yet, keyed by token.
    open: HashMap<TokenId, Qualification>,
}

/// R2's state machine. No IO: the daemon fetches, this decides.
#[derive(Debug)]
pub struct NearResObserver {
    cfg: NearResConfig,
    state: Mutex<ObserverState>,
}

impl NearResObserver {
    pub fn new(cfg: &NearResConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            state: Mutex::new(ObserverState::default()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ObserverState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Re-seed from the database at startup, so a restart does not re-qualify markets that
    /// already have an observation row (which the insert-only schema would refuse anyway).
    pub fn reseed(&self, resolved: Vec<TokenId>, open: Vec<Qualification>) {
        let mut state = self.lock();
        state.seen.extend(resolved);
        for q in open {
            state.seen.insert(q.token_id.clone());
            state.open.insert(q.token_id.clone(), q);
        }
    }

    pub fn open_count(&self) -> usize {
        self.lock().open.len()
    }

    /// Is `ask` inside the qualifying band? Inclusive at **both** ends: 0.96 and 0.99 are in
    /// the study, and the boundary is not a matter of taste — it is the pre-committed band.
    pub fn in_band(&self, ask: Decimal) -> bool {
        ask >= self.cfg.min_ask && ask <= self.cfg.max_ask
    }

    /// Scan the universe for markets that qualify **now**, and return the new observations.
    ///
    /// One per outcome token, ever. The books must be the ones just fetched: the ask recorded
    /// here is the entry the study is about.
    pub fn qualify(
        &self,
        universe: &Universe,
        books: &BookMap,
        fees: &FeeModel,
        now: DateTime<Utc>,
    ) -> (Vec<Qualification>, QualifyCounters) {
        let mut counters = QualifyCounters::default();
        let mut out = Vec::new();
        if !self.cfg.enabled {
            return (out, counters);
        }
        let horizon = Duration::hours(self.cfg.max_hours_to_resolution);
        let mut state = self.lock();

        for event in &universe.events {
            let Some(end_date) = event.end_date else {
                counters.no_end_date += event.markets.len() as u64 * 2;
                continue;
            };
            if end_date <= now {
                // Already past its stated close: there is no holding period left to measure.
                counters.too_far_out += event.markets.len() as u64 * 2;
                continue;
            }
            if end_date - now > horizon {
                counters.too_far_out += event.markets.len() as u64 * 2;
                continue;
            }
            for market in &event.markets {
                let fee = fees.resolve(&event.category, &market.fees);
                for (index, token) in market.token_ids.iter().enumerate() {
                    counters.tokens_examined += 1;
                    if state.seen.contains(token) {
                        counters.already_tracked += 1;
                        continue;
                    }
                    let Some(book) = books.get(token) else {
                        counters.no_book += 1;
                        continue;
                    };
                    let Some(ask) = book.best_ask() else {
                        counters.no_book += 1;
                        continue;
                    };
                    if !self.in_band(ask) {
                        counters.outside_price_band += 1;
                        continue;
                    }
                    if state.open.len() >= self.cfg.max_tracked {
                        counters.at_capacity += 1;
                        continue;
                    }
                    let hours = (Decimal::from((end_date - now).num_minutes()) / Decimal::from(60))
                        .round_dp(2);
                    let qualification = Qualification {
                        token_id: token.clone(),
                        condition_id: market.condition_id.clone(),
                        event_slug: event.slug.clone(),
                        question: market.question.clone(),
                        outcome: market.outcomes[index].clone(),
                        outcome_index: index,
                        category: event.category.as_str().to_string(),
                        ask,
                        ask_size: book
                            .levels(Side::Ask)
                            .first()
                            .map(|l| l.size)
                            .unwrap_or(Decimal::ZERO),
                        ask_depth: book.depth(Side::Ask),
                        best_bid: book.best_bid(),
                        fee_rate: fee.rate,
                        fee_per_share: taker_fee_per_share(fee.rate, ask),
                        end_date,
                        hours_to_resolution: hours,
                        qualified_at: now,
                    };
                    state.seen.insert(token.clone());
                    state.open.insert(token.clone(), qualification.clone());
                    counters.qualified += 1;
                    out.push(qualification);
                }
            }
        }
        (out, counters)
    }

    /// Condition ids whose stated end date has passed and which are still unresolved — the
    /// batch the daemon should look up.
    pub fn due_for_lookup(&self, now: DateTime<Utc>, limit: usize) -> Vec<String> {
        let state = self.lock();
        let mut ids: Vec<String> = state
            .open
            .values()
            .filter(|q| q.end_date <= now)
            .map(|q| q.condition_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids.truncate(limit);
        ids
    }

    /// Apply a batch of lookups. Returns the verdicts to persist — one per observation that
    /// the facts could settle. An observation the facts leave open stays open.
    ///
    /// A market the API reports as **closed** but whose payout vector we cannot read settles
    /// as `undetermined`: it is finished, we cannot see how, and pretending otherwise would
    /// put a guess in the loss rate.
    pub fn apply_lookups(
        &self,
        facts: &[ResolutionFacts],
        now: DateTime<Utc>,
    ) -> Vec<(Qualification, Resolution)> {
        let mut out = Vec::new();
        let mut state = self.lock();
        for fact in facts {
            let tokens: Vec<TokenId> = state
                .open
                .iter()
                .filter(|(_, q)| q.condition_id == fact.condition_id)
                .map(|(token, _)| token.clone())
                .collect();
            for token in tokens {
                let Some(q) = state.open.get(&token).cloned() else {
                    continue;
                };
                let (outcome, payout) = classify(&q, fact);
                if outcome == ResolutionOutcome::Undetermined && !fact.closed {
                    // Still trading: nothing has happened yet, so nothing is written.
                    continue;
                }
                let hours = Decimal::from((now - q.qualified_at).num_minutes()) / Decimal::from(60);
                let resolution = Resolution {
                    token_id: token.clone(),
                    condition_id: fact.condition_id.clone(),
                    outcome,
                    payout,
                    hours_to_payout: hours.round_dp(2),
                    uma_status: fact.uma_status.clone(),
                    disputed: fact.disputed,
                    resolved_at: now,
                };
                state.open.remove(&token);
                out.push((q, resolution));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------

/// One resolved observation, as the report needs it. Comes from the database join, so the
/// summary arithmetic is testable without any of the machinery above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedObservation {
    pub token_id: String,
    pub event_slug: String,
    pub category: String,
    pub ask: Decimal,
    pub fee_per_share: Decimal,
    pub outcome: ResolutionOutcome,
    pub payout: Option<Decimal>,
    pub hours_to_payout: Decimal,
    pub disputed: bool,
}

/// The pre-committed kill-line's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillLine {
    /// Both tests passed at a sample size big enough to mean something.
    Pass,
    /// A test failed. Carries the reason, in the report's own words.
    Fail(String),
    /// Not enough resolutions for the loss-rate test to distinguish anything. **Not a pass.**
    NotEnoughData { resolutions: usize, needed: u32 },
}

impl KillLine {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail(_) => "FAIL",
            Self::NotEnoughData { .. } => "NO VERDICT",
        }
    }
}

/// The R2 summary: counts first, then the two kill-line numbers, then the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearResSummary {
    pub enabled: bool,
    /// Observations that qualified in the reported window.
    pub qualified: usize,
    /// Observations still waiting for a resolution.
    pub open: usize,
    pub won: usize,
    pub lost: usize,
    pub split: usize,
    pub undetermined: usize,
    pub disputed: usize,
    /// `(lost + split) / (won + lost + split)`. `None` when nothing has resolved.
    pub loss_rate: Option<Decimal>,
    /// 95% Wilson interval on that rate, as a percentage pair.
    pub loss_rate_ci_pct: Option<(Decimal, Decimal)>,
    pub resolutions: usize,
    /// True while `resolutions` is below the configured minimum — printed loudly, because a
    /// 1-in-40 kill-line cannot be judged on a handful of outcomes.
    pub sample_too_small: bool,
    /// Σ ask over resolved observations — the capital one share each would have committed.
    pub capital: Decimal,
    pub payout: Decimal,
    pub fees: Decimal,
    /// `payout − capital − fees`.
    pub net: Decimal,
    pub mean_ask: Option<Decimal>,
    pub mean_hours_to_payout: Option<Decimal>,
    /// The holding period the annualisation actually uses: observed time-to-payout plus the
    /// configured recycling overhead.
    pub cycle_hours: Option<Decimal>,
    pub annual_yield_pct: Option<Decimal>,
    /// The pre-committed thresholds, carried so the report states the rule it applied rather
    /// than re-reading a config that may have been edited since.
    pub kill_line_loss_one_in: u32,
    pub kill_line_annual_yield_pct: Decimal,
    pub verdict: KillLine,
}

/// Aggregate resolved observations into the summary. Pure, so every number is testable.
pub fn summarize(
    enabled: bool,
    qualified: usize,
    open: usize,
    resolved: &[ResolvedObservation],
    cfg: &NearResConfig,
) -> NearResSummary {
    let mut summary = NearResSummary {
        enabled,
        qualified,
        open,
        won: 0,
        lost: 0,
        split: 0,
        undetermined: 0,
        disputed: 0,
        loss_rate: None,
        loss_rate_ci_pct: None,
        resolutions: 0,
        sample_too_small: true,
        capital: Decimal::ZERO,
        payout: Decimal::ZERO,
        fees: Decimal::ZERO,
        net: Decimal::ZERO,
        mean_ask: None,
        mean_hours_to_payout: None,
        cycle_hours: None,
        annual_yield_pct: None,
        kill_line_loss_one_in: cfg.kill_line_loss_one_in,
        kill_line_annual_yield_pct: cfg.kill_line_annual_yield_pct,
        verdict: KillLine::NotEnoughData {
            resolutions: 0,
            needed: cfg.min_resolutions_for_verdict,
        },
    };

    let mut hours = Decimal::ZERO;
    for obs in resolved {
        if obs.disputed {
            summary.disputed += 1;
        }
        match obs.outcome {
            ResolutionOutcome::Won => summary.won += 1,
            ResolutionOutcome::Lost => summary.lost += 1,
            ResolutionOutcome::Split => summary.split += 1,
            // Excluded from every number below: a hole is not a data point.
            ResolutionOutcome::Undetermined => {
                summary.undetermined += 1;
                continue;
            }
        }
        summary.capital += obs.ask;
        summary.fees += obs.fee_per_share;
        summary.payout += obs.payout.unwrap_or(Decimal::ZERO);
        hours += obs.hours_to_payout;
    }

    summary.resolutions = summary.won + summary.lost + summary.split;
    summary.net = summary.payout - summary.capital - summary.fees;
    if summary.resolutions == 0 {
        return summary;
    }
    let n = Decimal::from(summary.resolutions);
    let losses = Decimal::from(summary.lost + summary.split);
    summary.loss_rate = Some((losses / n).round_dp(4));
    summary.loss_rate_ci_pct =
        wilson_interval_pct(summary.lost + summary.split, summary.resolutions);
    summary.mean_ask = Some((summary.capital / n).round_dp(4));
    let mean_hours = (hours / n).round_dp(2);
    summary.mean_hours_to_payout = Some(mean_hours);
    let cycle = mean_hours + Decimal::from(cfg.recycle_overhead_hours);
    summary.cycle_hours = Some(cycle);
    summary.sample_too_small = (summary.resolutions as u32) < cfg.min_resolutions_for_verdict;

    if summary.capital > Decimal::ZERO && cycle > Decimal::ZERO {
        let per_cycle = summary.net / summary.capital;
        let cycles_per_year = Decimal::from(HOURS_PER_YEAR) / cycle;
        summary.annual_yield_pct =
            Some((per_cycle * cycles_per_year * Decimal::ONE_HUNDRED).round_dp(2));
    }

    summary.verdict = verdict(&summary, cfg);
    summary
}

/// The pre-committed kill-line, applied.
fn verdict(summary: &NearResSummary, cfg: &NearResConfig) -> KillLine {
    if summary.sample_too_small {
        return KillLine::NotEnoughData {
            resolutions: summary.resolutions,
            needed: cfg.min_resolutions_for_verdict,
        };
    }
    let threshold = Decimal::ONE / Decimal::from(cfg.kill_line_loss_one_in);
    if let Some(rate) = summary.loss_rate {
        if rate > threshold {
            return KillLine::Fail(format!(
                "realised loss rate {:.2}% is worse than the pre-committed 1-in-{} ({:.2}%)",
                rate * Decimal::ONE_HUNDRED,
                cfg.kill_line_loss_one_in,
                threshold * Decimal::ONE_HUNDRED,
            ));
        }
    }
    match summary.annual_yield_pct {
        Some(yield_pct) if yield_pct < cfg.kill_line_annual_yield_pct => KillLine::Fail(format!(
            "annualised net yield {:.2}%/yr is below the pre-committed {}%/yr at the observed \
             {} h recycling period",
            yield_pct,
            cfg.kill_line_annual_yield_pct,
            summary
                .cycle_hours
                .map(|h| h.to_string())
                .unwrap_or_else(|| "?".into()),
        )),
        Some(_) => KillLine::Pass,
        None => KillLine::NotEnoughData {
            resolutions: summary.resolutions,
            needed: cfg.min_resolutions_for_verdict,
        },
    }
}

/// 95% Wilson score interval for a proportion, as a percentage pair.
///
/// `f64` here, and **only** here: this is a statistic about a count, not a money value, and
/// nothing downstream prices from it. Every dollar figure in this module is `Decimal`. The
/// Wilson form is used rather than the normal approximation precisely because the interesting
/// case is a small `n` with zero or one failures, where the normal interval collapses to a
/// point and would flatter the strategy.
pub fn wilson_interval_pct(failures: usize, n: usize) -> Option<(Decimal, Decimal)> {
    if n == 0 {
        return None;
    }
    const Z: f64 = 1.959_963_984_540_054;
    let n_f = n as f64;
    let p = failures as f64 / n_f;
    let z2 = Z * Z;
    let denominator = 1.0 + z2 / n_f;
    let centre = p + z2 / (2.0 * n_f);
    let spread = Z * ((p * (1.0 - p) / n_f) + z2 / (4.0 * n_f * n_f)).sqrt();
    let low = ((centre - spread) / denominator).clamp(0.0, 1.0) * 100.0;
    let high = ((centre + spread) / denominator).clamp(0.0, 1.0) * 100.0;
    Some((
        Decimal::from_f64_retain(low)?.round_dp(2),
        Decimal::from_f64_retain(high)?.round_dp(2),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Category, MarketActivity, MarketFees, MarketTrading, OrderBook, PriceLevel, TrackedEvent,
        TrackedMarket,
    };
    use rust_decimal_macros::dec;

    fn cfg() -> NearResConfig {
        NearResConfig::default()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .expect("ts")
            .with_timezone(&Utc)
    }

    fn fee_model() -> FeeModel {
        FeeModel::new(
            [
                ("politics".to_string(), dec!(0.04)),
                ("other".to_string(), dec!(0.05)),
            ]
            .into_iter()
            .collect(),
        )
    }

    fn universe(end_in_hours: i64) -> Universe {
        let market = TrackedMarket {
            condition_id: "0xcond".into(),
            question: "Will it happen?".into(),
            outcomes: ["Yes".into(), "No".into()],
            token_ids: [TokenId::new("y"), TokenId::new("n")],
            fees: MarketFees::default(),
            activity: MarketActivity::default(),
            trading: MarketTrading::default(),
        };
        Universe {
            events: vec![TrackedEvent {
                id: "1".into(),
                slug: "an-event".into(),
                title: "An event".into(),
                neg_risk: false,
                category: Category::new("politics"),
                markets: vec![market],
                total_outcomes: 1,
                end_date: Some(now() + Duration::hours(end_in_hours)),
            }],
        }
    }

    fn books(yes_ask: Decimal, no_ask: Decimal) -> BookMap {
        let mut map = BookMap::new();
        for (token, ask) in [("y", yes_ask), ("n", no_ask)] {
            map.insert(
                TokenId::new(token),
                OrderBook::new(
                    TokenId::new(token),
                    vec![PriceLevel::new(ask - dec!(0.01), dec!(500))],
                    vec![
                        PriceLevel::new(ask, dec!(300)),
                        PriceLevel::new(ask + dec!(0.005), dec!(200)),
                    ],
                )
                .normalized(),
            );
        }
        map
    }

    /// The band is inclusive at both ends — exactly 96¢ and exactly 99¢ qualify, 95.9¢ and
    /// 99.1¢ do not. This is the pre-committed band, so the boundary is a fact, not taste.
    #[test]
    fn the_price_band_is_inclusive_at_exactly_96_and_99_cents() {
        let observer = NearResObserver::new(&cfg());
        assert!(observer.in_band(dec!(0.96)));
        assert!(observer.in_band(dec!(0.99)));
        assert!(observer.in_band(dec!(0.975)));
        assert!(!observer.in_band(dec!(0.9599)));
        assert!(!observer.in_band(dec!(0.9901)));
        assert!(!observer.in_band(dec!(1.00)));

        // …and end to end through the qualifier, at both boundaries at once.
        let (qualified, counters) = observer.qualify(
            &universe(24),
            &books(dec!(0.96), dec!(0.99)),
            &fee_model(),
            now(),
        );
        assert_eq!(qualified.len(), 2);
        assert_eq!(counters.qualified, 2);
        assert_eq!(counters.outside_price_band, 0);

        let observer = NearResObserver::new(&cfg());
        let (qualified, counters) = observer.qualify(
            &universe(24),
            &books(dec!(0.9599), dec!(0.9901)),
            &fee_model(),
            now(),
        );
        assert!(qualified.is_empty());
        assert_eq!(counters.outside_price_band, 2);
    }

    /// A qualification records the *executable* entry and happens exactly once per token.
    #[test]
    fn a_qualification_records_the_executable_entry_and_never_repeats() {
        let observer = NearResObserver::new(&cfg());
        let (first, _) = observer.qualify(
            &universe(48),
            &books(dec!(0.97), dec!(0.50)),
            &fee_model(),
            now(),
        );
        assert_eq!(first.len(), 1, "only the 97¢ side is in band");
        let q = &first[0];
        assert_eq!(q.ask, dec!(0.97));
        assert_eq!(q.ask_size, dec!(300));
        assert_eq!(q.ask_depth, dec!(500));
        assert_eq!(q.best_bid, Some(dec!(0.96)));
        assert_eq!(q.outcome_index, 0);
        assert_eq!(q.outcome, "Yes");
        assert_eq!(q.hours_to_resolution, dec!(48));
        // Politics 0.04 at 0.97: 0.04 × 0.97 × 0.03 = 0.001164/share.
        assert_eq!(q.fee_per_share, dec!(0.001164));
        assert_eq!(q.net_if_won(), dec!(0.028836));

        // A second pass over a better book must not produce a second observation, and must
        // not revise the first.
        let (second, counters) = observer.qualify(
            &universe(48),
            &books(dec!(0.98), dec!(0.50)),
            &fee_model(),
            now(),
        );
        assert!(second.is_empty());
        assert_eq!(counters.already_tracked, 1);
        assert_eq!(observer.open_count(), 1);
    }

    /// An unknown or distant end date never qualifies: we do not guess a resolution time.
    #[test]
    fn qualification_needs_a_known_end_date_inside_the_horizon() {
        let observer = NearResObserver::new(&cfg());
        let mut far = universe(200);
        let (q, counters) =
            observer.qualify(&far, &books(dec!(0.97), dec!(0.97)), &fee_model(), now());
        assert!(q.is_empty());
        assert_eq!(counters.too_far_out, 2);

        far.events[0].end_date = None;
        let (q, counters) =
            observer.qualify(&far, &books(dec!(0.97), dec!(0.97)), &fee_model(), now());
        assert!(q.is_empty());
        assert_eq!(counters.no_end_date, 2);
    }

    fn raw_market(prices: Option<&str>, closed: bool, uma: Option<&str>) -> RawMarket {
        let body = format!(
            r#"{{"conditionId":"0xcond","closed":{},"outcomePrices":{},"umaResolutionStatus":{}}}"#,
            closed,
            prices
                .map(|p| format!("\"{}\"", p.replace('"', "\\\"")))
                .unwrap_or_else(|| "null".into()),
            uma.map(|u| format!("\"{u}\""))
                .unwrap_or_else(|| "null".into()),
        );
        serde_json::from_str(&body).expect("raw market")
    }

    /// Resolution facts are read only when the payout vector is complete and adds to $1.
    #[test]
    fn a_payout_vector_that_does_not_add_up_is_not_a_resolution() {
        let good = resolution_facts(&raw_market(Some(r#"["1","0"]"#), true, Some("resolved")))
            .expect("facts");
        assert_eq!(good.payouts, Some(vec![dec!(1), dec!(0)]));
        assert!(good.closed);
        assert!(!good.disputed);

        let disputed = resolution_facts(&raw_market(Some(r#"["0","1"]"#), true, Some("Disputed")))
            .expect("facts");
        assert!(disputed.disputed);
        assert_eq!(disputed.uma_status.as_deref(), Some("disputed"));

        // Still trading: prices are a market quote, not a payout — but they sum to 1, so the
        // `closed` flag is what stops them being read as a resolution.
        let open =
            resolution_facts(&raw_market(Some(r#"["0.97","0.03"]"#), false, None)).expect("facts");
        assert!(!open.closed);

        // Nonsense vectors are refused outright.
        assert_eq!(
            resolution_facts(&raw_market(Some(r#"["1","1"]"#), true, None))
                .expect("facts")
                .payouts,
            None
        );
        assert_eq!(
            resolution_facts(&raw_market(None, true, None))
                .expect("facts")
                .payouts,
            None
        );
    }

    /// A closed market we cannot read settles `undetermined`; an open one stays open.
    #[test]
    fn an_unreadable_resolution_is_a_hole_and_an_open_market_stays_open() {
        let observer = NearResObserver::new(&cfg());
        observer.qualify(
            &universe(1),
            &books(dec!(0.97), dec!(0.50)),
            &fee_model(),
            now(),
        );
        let later = now() + Duration::hours(6);

        // Open, unreadable → nothing written, still tracked.
        let facts = resolution_facts(&raw_market(None, false, None)).expect("facts");
        assert!(observer.apply_lookups(&[facts], later).is_empty());
        assert_eq!(observer.open_count(), 1);

        // Closed, unreadable → an undetermined verdict, written once, and the observation is
        // no longer open.
        let facts = resolution_facts(&raw_market(None, true, None)).expect("facts");
        let settled = observer.apply_lookups(&[facts], later);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].1.outcome, ResolutionOutcome::Undetermined);
        assert_eq!(settled[0].1.payout, None);
        assert_eq!(settled[0].1.hours_to_payout, dec!(6));
        assert_eq!(observer.open_count(), 0);
    }

    /// A won market pays $1 to the index we bought.
    #[test]
    fn the_outcome_index_decides_who_won() {
        let observer = NearResObserver::new(&cfg());
        let (q, _) = observer.qualify(
            &universe(1),
            &books(dec!(0.97), dec!(0.50)),
            &fee_model(),
            now(),
        );
        let facts = resolution_facts(&raw_market(r#"["1","0"]"#.into(), true, Some("resolved")))
            .expect("facts");
        let (outcome, payout) = classify(&q[0], &facts);
        assert_eq!(outcome, ResolutionOutcome::Won);
        assert_eq!(payout, Some(dec!(1)));

        // The same market, had we been on the other token.
        let mut other = q[0].clone();
        other.outcome_index = 1;
        assert_eq!(classify(&other, &facts).0, ResolutionOutcome::Lost);

        // A split resolution is not a win.
        let split =
            resolution_facts(&raw_market(r#"["0.5","0.5"]"#.into(), true, None)).expect("facts");
        assert_eq!(classify(&q[0], &split).0, ResolutionOutcome::Split);
    }

    fn observation(
        ask: Decimal,
        outcome: ResolutionOutcome,
        hours: Decimal,
    ) -> ResolvedObservation {
        ResolvedObservation {
            token_id: "t".into(),
            event_slug: "e".into(),
            category: "politics".into(),
            ask,
            fee_per_share: taker_fee_per_share(dec!(0.04), ask),
            payout: match outcome {
                ResolutionOutcome::Won => Some(Decimal::ONE),
                ResolutionOutcome::Lost => Some(Decimal::ZERO),
                ResolutionOutcome::Split => Some(dec!(0.5)),
                ResolutionOutcome::Undetermined => None,
            },
            outcome,
            hours_to_payout: hours,
            disputed: false,
        }
    }

    /// The yield arithmetic, hand-computed: forty 97¢ wins, one 97¢ loss.
    #[test]
    fn the_annualised_yield_is_computed_at_the_observed_recycling_speed() {
        let mut cfg = cfg();
        cfg.min_resolutions_for_verdict = 10;
        cfg.recycle_overhead_hours = 0;
        let mut rows: Vec<ResolvedObservation> = (0..40)
            .map(|_| observation(dec!(0.97), ResolutionOutcome::Won, dec!(24)))
            .collect();
        rows.push(observation(dec!(0.97), ResolutionOutcome::Lost, dec!(24)));

        let s = summarize(true, 41, 0, &rows, &cfg);
        assert_eq!(s.resolutions, 41);
        assert_eq!(s.won, 40);
        assert_eq!(s.lost, 1);
        // 41 × 0.97 = 39.77 committed; 40 × $1 = $40 back; fees 41 × 0.04×0.97×0.03.
        assert_eq!(s.capital, dec!(39.77));
        assert_eq!(s.payout, dec!(40));
        assert_eq!(s.fees, dec!(0.047724));
        assert_eq!(s.net, dec!(0.182276));
        // Loss rate 1/41 = 2.44%, just *worse* than the pre-committed 1-in-40 (2.5%)? No —
        // 1/41 = 2.4390%, which is better, so this passes the loss-rate test.
        assert_eq!(s.loss_rate, Some(dec!(0.0244)));
        // 24 h cycles: 365 per year on a 0.4583% per-cycle return ≈ 167%/yr.
        assert_eq!(s.mean_hours_to_payout, Some(dec!(24)));
        assert_eq!(s.cycle_hours, Some(dec!(24)));
        let annual = s.annual_yield_pct.expect("yield");
        assert!(
            (annual - dec!(167.28)).abs() < dec!(0.5),
            "annualised yield was {annual}"
        );
        assert_eq!(s.verdict, KillLine::Pass);
    }

    /// Both kill-line branches, and the small-sample refusal that outranks them.
    #[test]
    fn the_kill_line_fails_on_loss_rate_and_on_yield_and_refuses_a_small_sample() {
        let mut cfg = cfg();
        cfg.min_resolutions_for_verdict = 10;
        cfg.recycle_overhead_hours = 0;

        // Two losses in twelve is far worse than 1-in-40.
        let mut rows: Vec<ResolvedObservation> = (0..10)
            .map(|_| observation(dec!(0.97), ResolutionOutcome::Won, dec!(24)))
            .collect();
        rows.extend((0..2).map(|_| observation(dec!(0.97), ResolutionOutcome::Lost, dec!(24))));
        let s = summarize(true, 12, 0, &rows, &cfg);
        assert!(matches!(s.verdict, KillLine::Fail(ref why) if why.contains("loss rate")));

        // A clean loss rate, but the money is locked up for a year: the yield test kills it.
        let slow: Vec<ResolvedObservation> = (0..40)
            .map(|_| observation(dec!(0.97), ResolutionOutcome::Won, dec!(8760)))
            .collect();
        let s = summarize(true, 40, 0, &slow, &cfg);
        assert_eq!(s.loss_rate, Some(Decimal::ZERO));
        assert!(matches!(s.verdict, KillLine::Fail(ref why) if why.contains("yield")));

        // Three resolutions cannot decide anything, whatever they say.
        let tiny: Vec<ResolvedObservation> = (0..3)
            .map(|_| observation(dec!(0.97), ResolutionOutcome::Won, dec!(24)))
            .collect();
        let s = summarize(true, 3, 4, &tiny, &cfg);
        assert!(s.sample_too_small);
        assert_eq!(
            s.verdict,
            KillLine::NotEnoughData {
                resolutions: 3,
                needed: 10
            }
        );
        assert_eq!(s.verdict.label(), "NO VERDICT");
    }

    /// Undetermined resolutions are excluded from every number, and counted on their own.
    #[test]
    fn undetermined_resolutions_are_excluded_from_the_arithmetic() {
        let cfg = cfg();
        let rows = vec![
            observation(dec!(0.97), ResolutionOutcome::Won, dec!(24)),
            observation(dec!(0.97), ResolutionOutcome::Undetermined, dec!(24)),
        ];
        let s = summarize(true, 2, 0, &rows, &cfg);
        assert_eq!(s.resolutions, 1);
        assert_eq!(s.undetermined, 1);
        assert_eq!(s.capital, dec!(0.97), "the hole commits no capital");
        assert_eq!(s.loss_rate, Some(Decimal::ZERO));
    }

    /// The Wilson interval is wide where the normal approximation would lie: zero failures in
    /// five is *not* evidence of a zero loss rate.
    #[test]
    fn the_wilson_interval_is_honest_about_small_samples() {
        let (low, high) = wilson_interval_pct(0, 5).expect("interval");
        assert_eq!(low, Decimal::ZERO);
        assert!(
            high > dec!(40),
            "0/5 must still admit a loss rate above 40%, got {high}"
        );
        let (low, high) = wilson_interval_pct(1, 400).expect("interval");
        assert!(low > Decimal::ZERO && high < dec!(2), "got {low}..{high}");
        assert_eq!(wilson_interval_pct(0, 0), None);
    }
}
