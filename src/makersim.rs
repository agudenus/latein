//! M8 — the maker-fill simulator: the hypothetical maker P&L turned into a lower bound.
//!
//! ## Why this exists
//!
//! The daily summary reports two maker numbers today, and only one of them is a
//! measurement. `net_maker` is what a construction pays *if every resting leg is crossed*,
//! and the soak's first weeks put that hypothetical at ~$633/day against $11 of taker P&L.
//! That gap is the entire Phase B question, and the hypothetical cannot answer it: we never
//! rest an order, so we never learn whether anyone would have traded with us.
//!
//! This module answers a strictly weaker question that we *can* answer from public data:
//! **if we had posted at the bid, and we were the very last order in that queue, would the
//! market have traded through us?** Every answer it gives is pessimistic, so the number it
//! produces is a floor:
//!
//! ```text
//!   simulated lower bound  ≤  the truth  ≤  the "if always filled" hypothetical
//! ```
//!
//! No order is placed, no wallet exists, nothing is signed. This is arithmetic over trade
//! prints that arrive whether we are here or not.
//!
//! ## The rule (ported from `docs/design/crypto-engine.md` §8.2)
//!
//! When a maker-only opportunity is detected we record, per leg, the price we would have
//! rested at — **the current best bid**, which is exactly the price
//! `net_maker = payout − Σ best_bid` is computed from (`src/costs.rs`) and exactly the price
//! the lifecycle tracker already re-checks for maker-only rows (`fills_still_available`).
//! Alongside it we record `Q0`, the size already resting at that price at that instant.
//!
//! From then on a leg accumulates `V`, the volume of subsequent **SELL** prints at our price
//! *or through it* (a lower price: our level can only trade after everything above it has
//! gone). The leg fills when
//!
//! ```text
//!   V  ≥  Q0 + our_size
//! ```
//!
//! — the level fully traded through, *plus* our own size, because we are behind all of it.
//! Three refusals make this a floor rather than an estimate:
//!
//! * **Cancels ahead of us are never credited.** `Q0` never shrinks. In a real book, queue
//!   position improves every time someone ahead cancels; we cannot see cancels in public
//!   data, and a simulator that assumed them would be flattering itself.
//! * **A book improving past our level is not a fill.** Only prints count. A bid that
//!   evaporates and re-forms lower tells us nothing about whether *we* traded.
//! * **An unreadable print is not evidence.** A print with no size, no price, or a side we
//!   cannot read is counted and discarded, never guessed into a fill.
//!
//! ## What this still cannot see
//!
//! Stated here because the number is only worth what its caveats are worth:
//!
//! * **Our own order would have changed the queue.** Posting size at the bid is information;
//!   it can attract flow, deter it, or cause someone to step in front. We model none of it.
//! * **Partial fills do not exist here.** A leg is filled or it is not, because the P&L we
//!   credit is the whole construction's `net_maker_total`. A leg that traded 90% of the way
//!   through our order counts as unfilled — pessimistic, and one more reason this is a bound.
//! * **Queue position is the worst case, not the real one.** In practice we would sometimes
//!   be first at a new price level. Assuming last is the point.
//! * **No print feed, no evidence.** In REST-only fallback there are no prints at all, so
//!   every sim in that window closes `maker_unfilled` carrying `no_print_feed` — a hole in
//!   the measurement, reported as such rather than as a zero fill rate.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::MakerSimConfig;
use crate::types::{BookMap, Opportunity, Side, TokenId};
use crate::ws::{PrintObserver, TradePrint, TradeSide};

/// How a simulation ended. Written once, at close, and never revised — the same discipline
/// the opportunity lifecycle follows, and for the same reason: a verdict that can improve
/// with hindsight is not a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimStatus {
    /// Every leg's queue traded through within the window. The only status that credits P&L.
    MakerFilled,
    /// Some legs filled, some did not. Credits **nothing**, and records what the filled legs
    /// would have cost us — that is the legging exposure the structure actually carries.
    MakerPartial,
    /// No leg filled. The expected outcome of a pessimistic rule, and not a failure.
    MakerUnfilled,
    /// Evicted at `maker_sim.max_concurrent` before its window closed. No verdict was
    /// reached, so none is reported: it is counted apart from `maker_unfilled` so a capacity
    /// limit can never masquerade as evidence that quotes do not fill.
    MakerUntracked,
}

impl SimStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MakerFilled => "maker_filled",
            Self::MakerPartial => "maker_partial",
            Self::MakerUnfilled => "maker_unfilled",
            Self::MakerUntracked => "maker_untracked",
        }
    }
}

impl std::fmt::Display for SimStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One leg's phantom resting order: what we would have posted, what was ahead of us, and
/// what the tape did about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedLeg {
    pub token_id: TokenId,
    /// The price we would rest at: the best bid at detection. Not a chosen price — it is the
    /// one the maker economics were computed from.
    pub price: Decimal,
    /// `Q0`: size already resting at `price` when the phantom order was placed. Never
    /// reduced, because cancels are invisible to us.
    pub visible_size: Decimal,
    /// Shares we would have posted (the opportunity's executable size).
    pub our_size: Decimal,
    /// SELL volume printed at `price` or through it since placement.
    pub volume_through: Decimal,
    /// Prints that touched this level, whether or not they filled us. A leg with zero here
    /// was never traded at, which is a different fact from "the queue was too long".
    pub prints_observed: u64,
    pub filled_at: Option<DateTime<Utc>>,
    /// Milliseconds from placement to this leg's fill.
    pub fill_ms: Option<i64>,
}

impl PlacedLeg {
    /// The volume that has to print through this level before our own order is completely
    /// consumed: everyone ahead of us, then us.
    ///
    /// The comparison is `>=`, and the choice is deliberate: at exactly `Q0 + our_size` the
    /// last share of our order is the last share traded, so it is filled. One tick of size
    /// less and it is not. The strict-inequality reading (`>`) differs only in that single
    /// boundary and would refuse a fill that the arithmetic says happened.
    pub fn threshold(&self) -> Decimal {
        self.visible_size + self.our_size
    }

    pub fn is_filled(&self) -> bool {
        self.filled_at.is_some()
    }

    /// What this leg would have cost us if it filled: `price × size`. Used for the legging
    /// exposure of a partial simulation.
    pub fn cost(&self) -> Decimal {
        self.price * self.our_size
    }
}

/// A simulation in flight.
#[derive(Debug, Clone)]
struct Sim {
    opportunity_id: i64,
    event_slug: String,
    kind: String,
    category: String,
    legs: Vec<PlacedLeg>,
    /// The construction's `net_maker_total` — credited in full if, and only if, every leg
    /// fills. Never pro-rated: a partial pair is not a fraction of an arbitrage.
    net_maker_total: Decimal,
    opened_at: DateTime<Utc>,
    /// True when this simulation spent any part of its window without a live print feed, so
    /// an unfilled verdict says nothing about the market.
    no_print_feed: bool,
}

impl Sim {
    fn all_filled(&self) -> bool {
        self.legs.iter().all(PlacedLeg::is_filled)
    }

    fn filled_legs(&self) -> usize {
        self.legs.iter().filter(|l| l.is_filled()).count()
    }

    fn close(self, now: DateTime<Utc>, forced: Option<SimStatus>) -> ClosedSim {
        let filled = self.filled_legs();
        let status = forced.unwrap_or(if filled == self.legs.len() {
            SimStatus::MakerFilled
        } else if filled > 0 {
            SimStatus::MakerPartial
        } else {
            SimStatus::MakerUnfilled
        });
        // P&L is credited on exactly one status, and legging exposure on exactly one other.
        // Anything else reports the counts and no money at all.
        let pnl_lower_bound = (status == SimStatus::MakerFilled).then_some(self.net_maker_total);
        let legging_exposure = (status == SimStatus::MakerPartial)
            .then(|| self.legs.iter().filter(|l| l.is_filled()).map(PlacedLeg::cost).sum());
        let time_to_fill_ms = (status == SimStatus::MakerFilled)
            .then(|| self.legs.iter().filter_map(|l| l.fill_ms).max())
            .flatten();
        ClosedSim {
            opportunity_id: self.opportunity_id,
            event_slug: self.event_slug,
            kind: self.kind,
            category: self.category,
            prints_observed: self.legs.iter().map(|l| l.prints_observed).sum(),
            legs_total: self.legs.len(),
            legs_filled: filled,
            legs: self.legs,
            net_maker_total: self.net_maker_total,
            status,
            pnl_lower_bound,
            legging_exposure,
            time_to_fill_ms,
            no_print_feed: self.no_print_feed,
            opened_at: self.opened_at,
            closed_at: now,
        }
    }
}

/// A finished simulation, ready to be persisted. Immutable by construction: the only way to
/// make one is to consume the in-flight [`Sim`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedSim {
    pub opportunity_id: i64,
    pub event_slug: String,
    pub kind: String,
    pub category: String,
    pub legs: Vec<PlacedLeg>,
    pub legs_total: usize,
    pub legs_filled: usize,
    /// What the construction would have paid at full fill. Carried even when nothing is
    /// credited, so a partial's forgone edge is legible.
    pub net_maker_total: Decimal,
    pub status: SimStatus,
    /// `Some` only for [`SimStatus::MakerFilled`]. This is the lower-bound P&L.
    pub pnl_lower_bound: Option<Decimal>,
    /// `Some` only for [`SimStatus::MakerPartial`]: what the filled legs cost, i.e. the
    /// capital left sitting in an incomplete position.
    pub legging_exposure: Option<Decimal>,
    pub prints_observed: u64,
    pub time_to_fill_ms: Option<i64>,
    pub no_print_feed: bool,
    pub opened_at: DateTime<Utc>,
    pub closed_at: DateTime<Utc>,
}

/// Why an opportunity did not get a simulation. Counted, never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotOpened {
    /// Not a maker-only construction: its taker economics already clear the floor, and the
    /// maker question is not the one being asked of it.
    NotMakerOnly,
    /// No maker net at all (a leg had no bid), so there is no price to rest at.
    NoMakerSide,
    /// The book we detected on does not show a level at the price we would post at. Rather
    /// than assume an empty queue — the single most flattering assumption available — no
    /// simulation is opened.
    NoVisibleLevel,
    /// `maker_sim.enabled = false`.
    Disabled,
}

/// The result of asking for a simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Tracking started. Carries the simulation evicted to make room, if any.
    Yes {
        legs: usize,
        evicted: Option<Box<ClosedSim>>,
    },
    No(NotOpened),
}

/// Counters over the simulator's whole life, for the health line and `runtime_status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimStatsSnapshot {
    pub opened: u64,
    pub filled: u64,
    pub partial: u64,
    pub unfilled: u64,
    pub untracked: u64,
    /// Prints that matched a tracked level (and so moved a queue).
    pub prints_matched: u64,
    /// Prints offered to us for an asset we were tracking but which could not count — the
    /// wrong side, or a price above our level.
    pub prints_ignored: u64,
    pub open_now: u64,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    /// Open simulations, ordered by id — which is insertion order, which is what
    /// "oldest first" eviction needs.
    sims: BTreeMap<u64, Sim>,
    /// asset → the open simulations with a leg on it. The reason a print costs O(work on
    /// that asset) rather than O(open sims): at live scale prints arrive for 147 000 tokens
    /// and all but a handful of them touch nothing we are tracking.
    index: HashMap<TokenId, Vec<u64>>,
    /// Closed simulations waiting for the daemon to persist them. Prints arrive on the
    /// socket task; SQLite writes belong to the daemon loop, so the two are separated by
    /// this queue rather than by a lock held across an await.
    closed: Vec<ClosedSim>,
}

/// The simulator. Shared (`Arc`) between the daemon loop, which opens and drains, and the
/// stream's socket tasks, which feed it prints.
#[derive(Debug)]
pub struct MakerSimulator {
    enabled: bool,
    window: chrono::Duration,
    max_concurrent: usize,
    inner: Mutex<Inner>,
    /// False while detection is running on REST alone: there is no print feed, so nothing
    /// can ever fill and every verdict in that window must say so.
    print_feed_live: AtomicBool,
    opened: AtomicU64,
    filled: AtomicU64,
    partial: AtomicU64,
    unfilled: AtomicU64,
    untracked: AtomicU64,
    prints_matched: AtomicU64,
    prints_ignored: AtomicU64,
}

impl MakerSimulator {
    pub fn new(cfg: &MakerSimConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            window: chrono::Duration::seconds(cfg.window_secs.max(1) as i64),
            max_concurrent: cfg.max_concurrent.max(1),
            inner: Mutex::new(Inner::default()),
            // Assume no feed until a live stream says otherwise: the honest default is the
            // one that flags the measurement rather than the one that trusts it.
            print_feed_live: AtomicBool::new(false),
            opened: AtomicU64::new(0),
            filled: AtomicU64::new(0),
            partial: AtomicU64::new(0),
            unfilled: AtomicU64::new(0),
            untracked: AtomicU64::new(0),
            prints_matched: AtomicU64::new(0),
            prints_ignored: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn window_secs(&self) -> u64 {
        self.window.num_seconds().max(0) as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Same reasoning as the book store: a panic elsewhere must not blind the simulator.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tell the simulator whether a print feed is running.
    ///
    /// Turning it **off** marks every open simulation `no_print_feed`, because from that
    /// moment their windows contain time in which no fill could possibly have been observed.
    /// The flag is sticky per simulation: once a window has a hole in it, it has one.
    pub fn set_print_feed(&self, live: bool) {
        let was = self.print_feed_live.swap(live, Ordering::Relaxed);
        if !live {
            let mut inner = self.lock();
            for sim in inner.sims.values_mut() {
                sim.no_print_feed = true;
            }
        } else if !was {
            tracing::info!("maker-fill simulator: trade-print feed is live again");
        }
    }

    pub fn print_feed_live(&self) -> bool {
        self.print_feed_live.load(Ordering::Relaxed)
    }

    /// Start simulating the resting orders a maker-only opportunity implies.
    ///
    /// `books` must be the books detection ran on: `visible_size` is only meaningful as a
    /// snapshot of the same instant the price came from.
    pub fn open(
        &self,
        opportunity_id: i64,
        op: &Opportunity,
        books: &BookMap,
        now: DateTime<Utc>,
    ) -> Opened {
        if !self.enabled {
            return Opened::No(NotOpened::Disabled);
        }
        // The maker question is asked of maker-only constructions: those are the ones whose
        // reported edge exists *only* if a resting order is crossed.
        if !op.maker_only {
            return Opened::No(NotOpened::NotMakerOnly);
        }
        let Some(net_maker_total) = op.net_maker_total else {
            return Opened::No(NotOpened::NoMakerSide);
        };

        let mut legs = Vec::with_capacity(op.legs.len());
        for leg in &op.legs {
            let Some(price) = leg.best_bid else {
                return Opened::No(NotOpened::NoMakerSide);
            };
            // The size resting at exactly our price. A level we cannot see is not an empty
            // one — see `NotOpened::NoVisibleLevel`.
            let Some(visible_size) = books.get(&leg.token_id).and_then(|book| {
                book.levels(Side::Bid)
                    .iter()
                    .find(|level| level.price == price)
                    .map(|level| level.size)
            }) else {
                return Opened::No(NotOpened::NoVisibleLevel);
            };
            legs.push(PlacedLeg {
                token_id: leg.token_id.clone(),
                price,
                visible_size,
                our_size: leg.size,
                volume_through: Decimal::ZERO,
                prints_observed: 0,
                filled_at: None,
                fill_ms: None,
            });
        }
        if legs.is_empty() {
            return Opened::No(NotOpened::NoMakerSide);
        }

        let no_print_feed = !self.print_feed_live();
        let sim = Sim {
            opportunity_id,
            event_slug: op.event_slug.clone(),
            kind: op.kind.as_str().to_string(),
            category: op.category.as_str().to_string(),
            legs,
            net_maker_total,
            opened_at: now,
            no_print_feed,
        };
        let leg_count = sim.legs.len();

        let mut inner = self.lock();
        // Capacity is enforced before admission, oldest first: the newest detection is the
        // one we know most about, and a queue that only ever refuses new work would measure
        // the first 200 opportunities of the soak and nothing else.
        let mut evicted = None;
        while inner.sims.len() >= self.max_concurrent {
            let Some(oldest) = inner.sims.keys().next().copied() else {
                break;
            };
            if let Some(closed) = Self::remove(&mut inner, oldest, now, Some(SimStatus::MakerUntracked))
            {
                self.untracked.fetch_add(1, Ordering::Relaxed);
                evicted = Some(Box::new(closed));
            }
        }

        let id = inner.next_id;
        inner.next_id += 1;
        for leg in &sim.legs {
            inner.index.entry(leg.token_id.clone()).or_default().push(id);
        }
        inner.sims.insert(id, sim);
        drop(inner);
        self.opened.fetch_add(1, Ordering::Relaxed);
        Opened::Yes {
            legs: leg_count,
            evicted,
        }
    }

    /// Remove one simulation, unindex it, close it and queue it for persistence.
    fn remove(
        inner: &mut Inner,
        id: u64,
        now: DateTime<Utc>,
        forced: Option<SimStatus>,
    ) -> Option<ClosedSim> {
        let sim = inner.sims.remove(&id)?;
        for leg in &sim.legs {
            if let Some(ids) = inner.index.get_mut(&leg.token_id) {
                ids.retain(|other| *other != id);
                if ids.is_empty() {
                    inner.index.remove(&leg.token_id);
                }
            }
        }
        let closed = sim.close(now, forced);
        inner.closed.push(closed.clone());
        Some(closed)
    }

    /// Apply one trade print. This is the whole fill rule.
    ///
    /// Called on the socket read task, so it does exactly one hash lookup for the ~99.9 % of
    /// prints that touch nothing we track.
    pub fn observe_at(&self, print: &TradePrint, now: DateTime<Utc>) {
        let Some((token, price, size)) = print.usable() else {
            return;
        };
        if size <= Decimal::ZERO {
            return;
        }
        let mut inner = self.lock();
        let Some(ids) = inner.index.get(token).cloned() else {
            return;
        };
        // A print that consumed the ask side (someone bought) says nothing about the queue in
        // front of a resting buy of ours, and a side we could not read says nothing at all.
        if print.side != TradeSide::Sell {
            self.prints_ignored
                .fetch_add(ids.len() as u64, Ordering::Relaxed);
            return;
        }

        let mut newly_complete: Vec<u64> = Vec::new();
        for id in ids {
            let Some(sim) = inner.sims.get_mut(&id) else {
                continue;
            };
            let opened_at = sim.opened_at;
            let mut touched = false;
            for leg in sim.legs.iter_mut() {
                if leg.token_id != *token || leg.is_filled() {
                    continue;
                }
                // "At or through": a sell printing *below* our bid can only have happened
                // after our level was consumed, so it counts. A sell printing above it hit a
                // better bid and never reached us.
                if price > leg.price {
                    continue;
                }
                touched = true;
                leg.prints_observed += 1;
                leg.volume_through += size;
                if leg.volume_through >= leg.threshold() {
                    leg.filled_at = Some(now);
                    leg.fill_ms = Some((now - opened_at).num_milliseconds().max(0));
                }
            }
            if touched {
                self.prints_matched.fetch_add(1, Ordering::Relaxed);
            } else {
                self.prints_ignored.fetch_add(1, Ordering::Relaxed);
            }
            if sim.all_filled() {
                newly_complete.push(id);
            }
        }
        // A completed pair closes the moment its last leg fills: waiting out the window would
        // only give the verdict a chance to change, and it must not.
        for id in newly_complete {
            if Self::remove(&mut inner, id, now, None).is_some() {
                self.filled.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Close every simulation whose window has elapsed.
    pub fn expire(&self, now: DateTime<Utc>) {
        let mut inner = self.lock();
        let due: Vec<u64> = inner
            .sims
            .iter()
            .filter(|(_, sim)| now - sim.opened_at >= self.window)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            if let Some(closed) = Self::remove(&mut inner, id, now, None) {
                self.count_close(closed.status);
            }
        }
    }

    /// Close, as they stand, every simulation with a leg on a token that is no longer in the
    /// universe.
    ///
    /// This is the "the event disappeared mid-window" case. Its verdict is whatever the tape
    /// had established by that moment — usually `maker_unfilled`, sometimes `maker_partial`
    /// — because there is no honest way to keep waiting for prints on a book that is gone.
    pub fn retain_tokens(&self, live: &HashSet<TokenId>, now: DateTime<Utc>) {
        let mut inner = self.lock();
        let gone: Vec<u64> = inner
            .sims
            .iter()
            .filter(|(_, sim)| sim.legs.iter().any(|leg| !live.contains(&leg.token_id)))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(closed) = Self::remove(&mut inner, id, now, None) {
                self.count_close(closed.status);
            }
        }
    }

    /// Close everything still open — used at shutdown, so no simulation is lost silently.
    pub fn close_all(&self, now: DateTime<Utc>) {
        let mut inner = self.lock();
        let ids: Vec<u64> = inner.sims.keys().copied().collect();
        for id in ids {
            if let Some(closed) = Self::remove(&mut inner, id, now, None) {
                self.count_close(closed.status);
            }
        }
    }

    fn count_close(&self, status: SimStatus) {
        let counter = match status {
            SimStatus::MakerFilled => &self.filled,
            SimStatus::MakerPartial => &self.partial,
            SimStatus::MakerUnfilled => &self.unfilled,
            SimStatus::MakerUntracked => &self.untracked,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Take the closed simulations waiting to be persisted.
    pub fn drain_closed(&self) -> Vec<ClosedSim> {
        std::mem::take(&mut self.lock().closed)
    }

    pub fn open_count(&self) -> usize {
        self.lock().sims.len()
    }

    pub fn stats(&self) -> SimStatsSnapshot {
        SimStatsSnapshot {
            opened: self.opened.load(Ordering::Relaxed),
            filled: self.filled.load(Ordering::Relaxed),
            partial: self.partial.load(Ordering::Relaxed),
            unfilled: self.unfilled.load(Ordering::Relaxed),
            untracked: self.untracked.load(Ordering::Relaxed),
            prints_matched: self.prints_matched.load(Ordering::Relaxed),
            prints_ignored: self.prints_ignored.load(Ordering::Relaxed),
            open_now: self.open_count() as u64,
        }
    }
}

/// The stream hands prints straight to the simulator. Wall-clock `now` is read here rather
/// than passed through the frame, so the core rule stays a pure function of
/// (print, time) and every test can supply both.
impl PrintObserver for MakerSimulator {
    fn observe(&self, print: &TradePrint, _received: Instant) {
        self.observe_at(print, Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Category, Label, Leg, OpportunityKind, OrderBook, PriceLevel, TokenId as Tok,
    };
    use rust_decimal_macros::dec;

    fn cfg(window_secs: u64, max_concurrent: usize) -> MakerSimConfig {
        MakerSimConfig {
            enabled: true,
            window_secs,
            max_concurrent,
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + secs, 0).expect("timestamp")
    }

    /// A two-leg maker-only opportunity resting at 0.47 and 0.46 for 100 shares each.
    ///
    /// `net_maker = 1 − (0.47 + 0.46) = 0.07` per share, so `net_maker_total = $7.00` at 100
    /// shares — hand-computed, and the exact figure a filled simulation must credit.
    fn maker_only_op() -> Opportunity {
        let leg = |token: &str, bid: Decimal| Leg {
            token_id: Tok::new(token),
            condition_id: "0xaaa".into(),
            question: "Q?".into(),
            outcome: "Yes".into(),
            best_ask: bid + dec!(0.02),
            best_bid: Some(bid),
            vwap: bid + dec!(0.02),
            size: dec!(100),
            ask_depth: dec!(500),
            fee_rate: dec!(0.04),
        };
        Opportunity {
            kind: OpportunityKind::BinaryYesNo,
            label: Label::TrueArb,
            event_slug: "an-event".into(),
            event_title: "An event".into(),
            category: Category::new("politics"),
            fee_rate: dec!(0.04),
            payout: dec!(1),
            legs: vec![leg("1001", dec!(0.47)), leg("1002", dec!(0.46))],
            gross_gap: dec!(0.01),
            slippage_cost: Decimal::ZERO,
            spread_cost: Some(dec!(0.06)),
            fee_taker: dec!(0.02),
            net_taker: dec!(-0.01),
            net_maker: Some(dec!(0.07)),
            executable_size: dec!(100),
            capital_required: dec!(93),
            net_taker_total: dec!(-1),
            net_maker_total: Some(dec!(7.00)),
            partial_coverage: None,
            resolution_flags: Vec::new(),
            conversion_required: false,
            maker_only: true,
        }
    }

    /// Books whose bid levels carry the sizes we are queueing behind.
    fn books(visible_a: Decimal, visible_b: Decimal) -> BookMap {
        let book = |token: &str, bid: Decimal, size: Decimal| {
            OrderBook::new(
                Tok::new(token),
                vec![
                    PriceLevel::new(bid, size),
                    PriceLevel::new(bid - dec!(0.01), dec!(999)),
                ],
                vec![PriceLevel::new(bid + dec!(0.02), dec!(500))],
            )
            .normalized()
        };
        let mut map = BookMap::new();
        map.insert(Tok::new("1001"), book("1001", dec!(0.47), visible_a));
        map.insert(Tok::new("1002"), book("1002", dec!(0.46), visible_b));
        map
    }

    fn sell(token: &str, price: Decimal, size: Decimal) -> TradePrint {
        TradePrint {
            asset_id: Some(Tok::new(token)),
            price: Some(price),
            size: Some(size),
            side: TradeSide::Sell,
            server_ts: None,
            transaction_hash: None,
        }
    }

    fn live(sim: &MakerSimulator) -> &MakerSimulator {
        sim.set_print_feed(true);
        sim
    }

    /// (a) The queue threshold, hand-computed on both sides of the boundary.
    ///
    /// Leg 1001: `Q0 = 250` visible at 0.47, our order 100 shares → 350 must print through
    /// before we are done. 349.99 is not a fill; the next cent of size is.
    #[test]
    fn a_fill_needs_the_level_traded_through_plus_our_own_size() {
        let sim = MakerSimulator::new(&cfg(3_600, 200));
        live(&sim);
        let op = maker_only_op();
        assert!(matches!(
            sim.open(1, &op, &books(dec!(250), dec!(80)), t(0)),
            Opened::Yes { legs: 2, .. }
        ));

        // 349.99 = 250 + 100 − 0.01.
        sim.observe_at(&sell("1001", dec!(0.47), dec!(349.99)), t(10));
        assert_eq!(sim.open_count(), 1, "one cent short is not a fill");
        assert!(sim.drain_closed().is_empty());

        // …and the cent that completes it.
        sim.observe_at(&sell("1001", dec!(0.47), dec!(0.01)), t(11));
        // Leg 1002 is untouched, so the simulation is still open — but leg 1001 is done, and
        // the window's expiry will report a partial.
        sim.expire(t(3_601));
        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        let closed = &closed[0];
        assert_eq!(closed.status, SimStatus::MakerPartial);
        assert_eq!(closed.legs_filled, 1);
        assert_eq!(closed.legs[0].volume_through, dec!(350.00));
        assert_eq!(closed.legs[0].threshold(), dec!(350));
        assert_eq!(closed.legs[0].fill_ms, Some(11_000));
    }

    /// (b) "Through" means at our price or better for the seller — that is, lower. A print
    /// above our bid hit someone else's better quote and never reached our level.
    #[test]
    fn prints_through_the_level_count_and_prints_above_it_do_not() {
        let sim = MakerSimulator::new(&cfg(3_600, 200));
        live(&sim);
        let op = maker_only_op();
        sim.open(1, &op, &books(dec!(10), dec!(10)), t(0));

        // 0.48 > our 0.47: a seller took a better bid than ours.
        sim.observe_at(&sell("1001", dec!(0.48), dec!(1_000)), t(1));
        // 0.46 < 0.47: the market traded through our level entirely.
        sim.observe_at(&sell("1001", dec!(0.46), dec!(110)), t(2));
        // A BUY print is somebody lifting an ask; it says nothing about our queue.
        sim.observe_at(
            &TradePrint {
                side: TradeSide::Buy,
                ..sell("1002", dec!(0.46), dec!(1_000))
            },
            t(3),
        );

        sim.expire(t(3_601));
        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].status, SimStatus::MakerPartial);
        assert_eq!(
            closed[0].legs[0].volume_through,
            dec!(110),
            "only the print at or through our price counted"
        );
        assert_eq!(
            closed[0].legs[1].volume_through,
            Decimal::ZERO,
            "a BUY print must never fill a resting buy"
        );
    }

    /// (c) One leg filled, the window expires: nothing is credited, and the exposure the
    /// structure actually carries is recorded.
    ///
    /// Filled leg is 1001 at 0.47 × 100 shares = **$47.00** of capital sitting in half a
    /// position whose other half never came.
    #[test]
    fn a_partial_credits_nothing_and_records_its_legging_exposure() {
        let sim = MakerSimulator::new(&cfg(600, 200));
        live(&sim);
        sim.open(7, &maker_only_op(), &books(dec!(0), dec!(50)), t(0));
        sim.observe_at(&sell("1001", dec!(0.47), dec!(100)), t(5));

        sim.expire(t(601));
        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        let c = &closed[0];
        assert_eq!(c.status, SimStatus::MakerPartial);
        assert_eq!(c.legs_filled, 1);
        assert_eq!(c.legs_total, 2);
        assert_eq!(c.pnl_lower_bound, None, "a half-built pair pays nothing");
        assert_eq!(c.legging_exposure, Some(dec!(47.00)));
        assert_eq!(c.time_to_fill_ms, None);
        assert_eq!(sim.stats().partial, 1);
        assert_eq!(sim.stats().filled, 0);
    }

    /// (d) Every leg fills: the credited number is the opportunity's own `net_maker_total`,
    /// $7.00 — not a re-derivation, and not pro-rated.
    #[test]
    fn every_leg_filled_credits_the_constructions_net_maker_total() {
        let sim = MakerSimulator::new(&cfg(3_600, 200));
        live(&sim);
        sim.open(9, &maker_only_op(), &books(dec!(20), dec!(5)), t(0));

        // 1001: 20 + 100 = 120 needed. 1002: 5 + 100 = 105 needed.
        sim.observe_at(&sell("1001", dec!(0.47), dec!(120)), t(2));
        sim.observe_at(&sell("1002", dec!(0.45), dec!(105)), t(4));

        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1, "a completed pair closes on its last fill");
        let c = &closed[0];
        assert_eq!(c.status, SimStatus::MakerFilled);
        assert_eq!(c.pnl_lower_bound, Some(dec!(7.00)));
        assert_eq!(c.legging_exposure, None);
        assert_eq!(c.time_to_fill_ms, Some(4_000), "the slowest leg decides");
        assert_eq!(c.prints_observed, 2);
        assert_eq!(sim.stats().filled, 1);
        assert_eq!(sim.open_count(), 0);
    }

    /// (e) At capacity the oldest simulation is evicted and reported `maker_untracked` — a
    /// missing measurement, never a zero one.
    #[test]
    fn eviction_at_capacity_is_untracked_not_unfilled() {
        let sim = MakerSimulator::new(&cfg(3_600, 2));
        live(&sim);
        let op = maker_only_op();
        for id in 1..=2 {
            sim.open(id, &op, &books(dec!(10), dec!(10)), t(id));
        }
        assert_eq!(sim.open_count(), 2);

        let opened = sim.open(3, &op, &books(dec!(10), dec!(10)), t(3));
        let Opened::Yes { evicted, .. } = opened else {
            panic!("the newest detection must be admitted");
        };
        let evicted = *evicted.expect("the oldest simulation is evicted to make room");
        assert_eq!(evicted.opportunity_id, 1);
        assert_eq!(evicted.status, SimStatus::MakerUntracked);
        assert_eq!(evicted.pnl_lower_bound, None);
        assert_eq!(sim.open_count(), 2);
        assert_eq!(sim.stats().untracked, 1);
        assert_eq!(sim.stats().unfilled, 0);
    }

    /// (f) No print feed (REST-only): the verdict is `maker_unfilled` **and** carries the
    /// flag that says the window contained no evidence at all.
    #[test]
    fn without_a_print_feed_a_sim_closes_unfilled_and_says_why() {
        let sim = MakerSimulator::new(&cfg(60, 200));
        // Deliberately never marked live.
        sim.open(1, &maker_only_op(), &books(dec!(10), dec!(10)), t(0));
        sim.expire(t(61));
        let closed = sim.drain_closed();
        assert_eq!(closed[0].status, SimStatus::MakerUnfilled);
        assert!(closed[0].no_print_feed);

        // A feed that dies mid-window taints that window too: the hole is what matters, not
        // the state at either end.
        let sim = MakerSimulator::new(&cfg(60, 200));
        live(&sim);
        sim.open(2, &maker_only_op(), &books(dec!(10), dec!(10)), t(0));
        sim.set_print_feed(false);
        sim.set_print_feed(true);
        sim.expire(t(61));
        let closed = sim.drain_closed();
        assert!(
            closed[0].no_print_feed,
            "a window with a gap in the feed cannot be evidence of a non-fill"
        );
    }

    /// (j) A verdict, once written, is never revised — including by prints that arrive after
    /// the close, and including by a later expiry sweep.
    #[test]
    fn a_verdict_is_never_revised() {
        let sim = MakerSimulator::new(&cfg(60, 200));
        live(&sim);
        sim.open(1, &maker_only_op(), &books(dec!(10), dec!(10)), t(0));
        sim.expire(t(61));
        let first = sim.drain_closed();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].status, SimStatus::MakerUnfilled);

        // Everything the market does afterwards falls on a simulation that no longer exists.
        sim.observe_at(&sell("1001", dec!(0.47), dec!(10_000)), t(62));
        sim.observe_at(&sell("1002", dec!(0.46), dec!(10_000)), t(63));
        sim.expire(t(10_000));
        assert!(
            sim.drain_closed().is_empty(),
            "a closed simulation must never produce a second verdict"
        );
        assert_eq!(sim.stats().unfilled, 1);
        assert_eq!(sim.stats().filled, 0);
    }

    /// Only maker-only constructions are simulated, and a book that cannot show us the level
    /// we would join is a refusal rather than an empty queue (the flattering assumption).
    #[test]
    fn what_is_refused_and_why() {
        let sim = MakerSimulator::new(&cfg(60, 200));
        live(&sim);

        let mut taker = maker_only_op();
        taker.maker_only = false;
        assert_eq!(
            sim.open(1, &taker, &books(dec!(10), dec!(10)), t(0)),
            Opened::No(NotOpened::NotMakerOnly)
        );

        let mut no_bid = maker_only_op();
        no_bid.legs[1].best_bid = None;
        assert_eq!(
            sim.open(2, &no_bid, &books(dec!(10), dec!(10)), t(0)),
            Opened::No(NotOpened::NoMakerSide)
        );

        // A book whose best bid is at a different price than the leg recorded: the level we
        // would have joined is not in evidence.
        let mut moved = books(dec!(10), dec!(10));
        moved.insert(
            Tok::new("1002"),
            OrderBook::new(
                Tok::new("1002"),
                vec![PriceLevel::new(dec!(0.44), dec!(30))],
                vec![PriceLevel::new(dec!(0.48), dec!(30))],
            )
            .normalized(),
        );
        assert_eq!(
            sim.open(3, &maker_only_op(), &moved, t(0)),
            Opened::No(NotOpened::NoVisibleLevel)
        );

        let off = MakerSimulator::new(&MakerSimConfig {
            enabled: false,
            ..cfg(60, 200)
        });
        assert_eq!(
            off.open(4, &maker_only_op(), &books(dec!(10), dec!(10)), t(0)),
            Opened::No(NotOpened::Disabled)
        );
        assert_eq!(off.open_count(), 0);
    }

    /// A vanished event closes its simulations where they stand, rather than waiting out a
    /// window against a book that no longer exists.
    #[test]
    fn a_vanished_book_closes_its_simulation_at_that_moment() {
        let sim = MakerSimulator::new(&cfg(3_600, 200));
        live(&sim);
        sim.open(1, &maker_only_op(), &books(dec!(10), dec!(10)), t(0));
        sim.observe_at(&sell("1001", dec!(0.47), dec!(110)), t(5));

        let mut live_tokens = HashSet::new();
        live_tokens.insert(Tok::new("1001"));
        sim.retain_tokens(&live_tokens, t(10));

        let closed = sim.drain_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].status, SimStatus::MakerPartial);
        assert_eq!(closed[0].closed_at, t(10));
        assert_eq!(sim.open_count(), 0);
    }

    /// The index is what keeps a print cheap: a token nobody is queued on must not be
    /// walked, and a closed simulation must leave no entry behind.
    #[test]
    fn the_asset_index_is_emptied_when_a_simulation_closes() {
        let sim = MakerSimulator::new(&cfg(60, 200));
        live(&sim);
        sim.open(1, &maker_only_op(), &books(dec!(10), dec!(10)), t(0));
        assert_eq!(sim.lock().index.len(), 2);
        sim.expire(t(61));
        assert!(
            sim.lock().index.is_empty(),
            "an index entry outliving its simulation is a leak on a 147k-token feed"
        );

        // A print for a token we track nothing on is one hash lookup and no work.
        sim.observe_at(&sell("9999", dec!(0.50), dec!(1)), t(62));
        assert_eq!(sim.stats().prints_matched, 0);
    }
}
