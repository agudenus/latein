//! M6 — the CLOB WebSocket market channel: order books pushed instead of polled.
//!
//! The REST scanner re-fetches every book on a fixed cadence, so the *floor* on detection
//! latency is the scan interval (5 s by default) no matter how fast the detectors are.
//! This module replaces that floor with the network: a pool of WebSocket connections
//! subscribes to every tracked outcome token, maintains the books locally from the pushed
//! frames, and wakes the detector only for the events whose books actually moved.
//!
//! ## What is verified and what is guessed
//!
//! This container cannot reach any Polymarket host, so **every wire detail below is an
//! assumption** and is marked `TODO(verify-live)` at its definition:
//!
//! * the endpoint (`stream.url`), the subscribe frame shape, and whether one connection
//!   has a subscription limit at all;
//! * the `book` / `price_change` event names and their field names (`asset_id`,
//!   `changes[].side`, `timestamp`, `hash`);
//! * whether `timestamp` is a millisecond or second epoch (both are accepted);
//! * what `hash` is over — we store it and use it only as a *change* signal, never as a
//!   checksum we claim to verify.
//!
//! Everything is therefore parsed defensively: an unknown event type, an unparseable
//! frame, or a field of the wrong type is counted and skipped, never fatal. The REST path
//! stays authoritative — [`BookStore::apply_rest`] overwrites whatever the stream built,
//! and the daemon re-fetches on a slow sweep specifically so a silently wrong local book
//! cannot survive.
//!
//! ## Integrity, staleness and resync
//!
//! * A reconnect marks every book on that shard **stale**: until a fresh snapshot arrives
//!   our copy is unverified, and a stale book is re-fetched over REST.
//! * A `price_change` older than the last frame applied to that token is *not* applied
//!   (it would corrupt the level set); the book is marked stale and resynced instead.
//! * A `book` snapshot whose `hash` repeats but whose content differs is counted and the
//!   book is marked stale — we cannot verify the hash, but a contradiction in it is still
//!   evidence that our view and the server's have diverged.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rand::seq::SliceRandom;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::types::{BookMap, OrderBook, PriceLevel, Side, TokenId};

/// How many token touches may queue between the sockets and the debouncer before the read
/// loop is back-pressured. Sized for a full universe moving at once.
const TOUCH_CAPACITY: usize = 8_192;
/// Debounced batches waiting for the detector.
const BATCH_CAPACITY: usize = 64;
/// A server timestamp further than this from our clock is not usable as a latency
/// reference (clock skew, or a unit we guessed wrong); we fall back to frame receipt.
const MAX_PLAUSIBLE_LATENCY_MS: i64 = 60_000;
/// How many books one cross-check sweep compares against REST. Comparing every book on
/// every sweep is pointless work: drift is systematic, so a sample finds it.
pub const DIVERGENCE_SAMPLE: usize = 64;
const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;
/// Keepalive cadence. TODO(verify-live): the public docs mention a client keepalive on the
/// market channel; we send a WebSocket ping, which any compliant server answers.
const PING_INTERVAL_SECS: u64 = 10;

// ---------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame is not JSON: {source}")]
    Json {
        #[source]
        source: serde_json::Error,
    },
}

/// Wire metadata common to every market frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameMeta {
    /// TODO(verify-live): `timestamp`, assumed to be the server's send time as an epoch
    /// **string in milliseconds**. Seconds are accepted too (see [`parse_server_ts`]).
    pub server_ts: Option<DateTime<Utc>>,
    /// TODO(verify-live): `hash`. Opaque to us — we never claim to verify it, we only
    /// notice when it contradicts itself.
    pub hash: Option<String>,
}

/// One aggregated level update from a `price_change` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelChange {
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
}

/// A parsed market-channel frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketFrame {
    /// Full book snapshot — replaces our copy wholesale.
    Book {
        asset_id: TokenId,
        bids: Vec<PriceLevel>,
        asks: Vec<PriceLevel>,
        meta: FrameMeta,
    },
    /// Level-wise delta — applied on top of the current copy.
    PriceChange {
        asset_id: TokenId,
        changes: Vec<LevelChange>,
        meta: FrameMeta,
    },
    /// A frame we understand the shape of but not the type (e.g. `tick_size_change`).
    /// Counted, never fatal.
    Unknown { event_type: String },
    /// A frame of a known type that we could not use (missing `asset_id`, sides of the
    /// wrong JSON type, …). Counted separately from `Unknown` because it means our shape
    /// assumption is wrong, not that the API grew a feature.
    Malformed {
        event_type: String,
        reason: &'static str,
    },
}

/// Wire form. Every field is optional: the parser decides what a frame is, not serde.
#[derive(Debug, Deserialize)]
struct RawFrame {
    /// TODO(verify-live): `event_type` on the market channel; `type` is accepted as well.
    #[serde(default, rename = "event_type", alias = "type")]
    event_type: Option<String>,
    /// TODO(verify-live): `asset_id` (the batch REST endpoint uses the same spelling;
    /// `assetId` has been seen on the singular one).
    #[serde(default, alias = "assetId")]
    asset_id: Option<String>,
    #[serde(default)]
    bids: Option<Vec<PriceLevel>>,
    #[serde(default)]
    asks: Option<Vec<PriceLevel>>,
    /// TODO(verify-live): some samples name the snapshot sides `buys`/`sells`.
    #[serde(default)]
    buys: Option<Vec<PriceLevel>>,
    #[serde(default)]
    sells: Option<Vec<PriceLevel>>,
    #[serde(default)]
    changes: Option<Vec<RawChange>>,
    /// TODO(verify-live): the single-change form of `price_change` (price/size/side at the
    /// top level instead of a `changes` array).
    #[serde(default)]
    price: Option<Value>,
    #[serde(default)]
    size: Option<Value>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChange {
    #[serde(deserialize_with = "crate::types::de_decimal")]
    price: Decimal,
    #[serde(deserialize_with = "crate::types::de_decimal")]
    size: Decimal,
    #[serde(default)]
    side: Option<String>,
}

/// TODO(verify-live): the market channel labels the *taker* side of a level: `BUY` is the
/// bid side of the book, `SELL` the ask side. Both the order-side and the book-side
/// vocabularies are accepted.
fn parse_side(raw: &str) -> Option<Side> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "buy" | "bid" | "bids" => Some(Side::Bid),
        "sell" | "ask" | "asks" => Some(Side::Ask),
        _ => None,
    }
}

/// TODO(verify-live): epoch milliseconds as a string in every sample we have. A value that
/// is too small to be milliseconds is read as seconds rather than silently producing a
/// timestamp in 1970.
fn parse_server_ts(raw: &Value) -> Option<DateTime<Utc>> {
    let n: i64 = match raw {
        Value::String(s) => s.trim().parse().ok()?,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    if n <= 0 {
        return None;
    }
    if n >= 1_000_000_000_000 {
        DateTime::from_timestamp_millis(n)
    } else {
        DateTime::from_timestamp(n, 0)
    }
}

fn as_decimal(raw: &Value) -> Option<Decimal> {
    match raw {
        Value::String(s) => s.trim().parse::<Decimal>().ok(),
        Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
        _ => None,
    }
}

/// Parse one WebSocket text payload into zero or more frames.
///
/// The channel sends either a single JSON object or an array of them; both are accepted.
/// A non-JSON keepalive (`PING`/`PONG`) is not an error and yields no frames.
pub fn parse_frames(text: &str) -> Result<Vec<MarketFrame>, FrameError> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("ping")
        || trimmed.eq_ignore_ascii_case("pong")
    {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|source| FrameError::Json { source })?;
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    Ok(items.into_iter().map(classify).collect())
}

fn classify(item: Value) -> MarketFrame {
    let raw: RawFrame = match serde_json::from_value(item) {
        Ok(raw) => raw,
        // The element is JSON, but not a shape we can read at all.
        Err(_) => {
            return MarketFrame::Malformed {
                event_type: "?".to_string(),
                reason: "frame fields have unexpected types",
            }
        }
    };
    let event_type = raw.event_type.clone().unwrap_or_default();
    let meta = FrameMeta {
        server_ts: raw.timestamp.as_ref().and_then(parse_server_ts),
        hash: raw.hash.clone().filter(|h| !h.trim().is_empty()),
    };
    let asset_id = raw
        .asset_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(TokenId::new);

    match event_type.as_str() {
        "book" => {
            let Some(asset_id) = asset_id else {
                return MarketFrame::Malformed {
                    event_type,
                    reason: "book snapshot without an asset id",
                };
            };
            MarketFrame::Book {
                asset_id,
                bids: raw.bids.or(raw.buys).unwrap_or_default(),
                asks: raw.asks.or(raw.sells).unwrap_or_default(),
                meta,
            }
        }
        "price_change" => {
            let Some(asset_id) = asset_id else {
                return MarketFrame::Malformed {
                    event_type,
                    reason: "price change without an asset id",
                };
            };
            let mut changes = Vec::new();
            for change in raw.changes.into_iter().flatten() {
                match change.side.as_deref().and_then(parse_side) {
                    Some(side) => changes.push(LevelChange {
                        side,
                        price: change.price,
                        size: change.size,
                    }),
                    // A level we cannot place on a side is worse than useless: applying it
                    // to a guessed side would corrupt the book.
                    None => {
                        return MarketFrame::Malformed {
                            event_type,
                            reason: "price change level with an unreadable side",
                        }
                    }
                }
            }
            if changes.is_empty() {
                // Single-change form.
                match (
                    raw.price.as_ref().and_then(as_decimal),
                    raw.size.as_ref().and_then(as_decimal),
                    raw.side.as_deref().and_then(parse_side),
                ) {
                    (Some(price), Some(size), Some(side)) => {
                        changes.push(LevelChange { side, price, size })
                    }
                    _ => {
                        return MarketFrame::Malformed {
                            event_type,
                            reason: "price change carried no readable level",
                        }
                    }
                }
            }
            MarketFrame::PriceChange {
                asset_id,
                changes,
                meta,
            }
        }
        _ => MarketFrame::Unknown {
            event_type: if event_type.is_empty() {
                "(none)".to_string()
            } else {
                event_type
            },
        },
    }
}

// ---------------------------------------------------------------------------------
// Live book state
// ---------------------------------------------------------------------------------

/// One locally maintained book plus the bookkeeping that lets us distrust it.
#[derive(Debug, Clone)]
struct LiveBook {
    book: OrderBook,
    /// Monotonically increasing, local only. Never derived from the wire.
    revision: u64,
    last_update: Instant,
    last_server_ts: Option<DateTime<Utc>>,
    last_hash: Option<String>,
    /// When we last *asked* REST about this token, whether or not it answered. A token the
    /// API has no book for must not turn into a request on every single tick.
    last_rest_attempt: Option<Instant>,
    /// True when this copy is not trustworthy: never snapshotted, or invalidated by a
    /// reconnect / out-of-order frame / hash contradiction.
    stale: bool,
}

impl LiveBook {
    fn placeholder(asset_id: TokenId, now: Instant) -> Self {
        Self {
            book: OrderBook::new(asset_id, Vec::new(), Vec::new()),
            revision: 0,
            last_update: now,
            last_server_ts: None,
            last_hash: None,
            last_rest_attempt: None,
            stale: true,
        }
    }

    fn bump(&mut self, now: Instant, server_ts: Option<DateTime<Utc>>) {
        self.revision = self.revision.saturating_add(1);
        self.last_update = now;
        if server_ts.is_some() {
            self.last_server_ts = server_ts;
        }
    }
}

/// Counters over everything the stream did. All best-effort telemetry — nothing here
/// gates a trade.
#[derive(Debug, Default)]
pub struct StreamStats {
    pub snapshots: AtomicU64,
    pub deltas: AtomicU64,
    pub unknown_frames: AtomicU64,
    pub malformed_frames: AtomicU64,
    /// Deltas for a token we have never snapshotted — cannot be applied.
    pub orphan_deltas: AtomicU64,
    pub out_of_order: AtomicU64,
    pub hash_contradictions: AtomicU64,
    pub reconnects: AtomicU64,
    pub connect_failures: AtomicU64,
    /// Books re-fetched over REST because they were stale or missing.
    pub resynced: AtomicU64,
    /// Sampled books whose top of book disagreed with REST.
    pub divergences: AtomicU64,
}

impl StreamStats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StreamStatsSnapshot {
        StreamStatsSnapshot {
            snapshots: self.snapshots.load(Ordering::Relaxed),
            deltas: self.deltas.load(Ordering::Relaxed),
            unknown_frames: self.unknown_frames.load(Ordering::Relaxed),
            malformed_frames: self.malformed_frames.load(Ordering::Relaxed),
            orphan_deltas: self.orphan_deltas.load(Ordering::Relaxed),
            out_of_order: self.out_of_order.load(Ordering::Relaxed),
            hash_contradictions: self.hash_contradictions.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            resynced: self.resynced.load(Ordering::Relaxed),
            divergences: self.divergences.load(Ordering::Relaxed),
        }
    }
}

/// A plain-old-data copy of [`StreamStats`], for logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamStatsSnapshot {
    pub snapshots: u64,
    pub deltas: u64,
    pub unknown_frames: u64,
    pub malformed_frames: u64,
    pub orphan_deltas: u64,
    pub out_of_order: u64,
    pub hash_contradictions: u64,
    pub reconnects: u64,
    pub connect_failures: u64,
    pub resynced: u64,
    pub divergences: u64,
}

/// The locally maintained order books, shared by the sockets, the resync sweep and the
/// detector.
#[derive(Debug, Default)]
pub struct BookStore {
    books: Mutex<HashMap<TokenId, LiveBook>>,
    pub stats: StreamStats,
}

impl BookStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TokenId, LiveBook>> {
        // A poisoned lock means a task panicked mid-update; the map is still structurally
        // sound and going blind is worse than carrying on with a book we will resync.
        self.books
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Apply one frame. Returns the token whose book changed, if any — that is the signal
    /// the detector debounces on.
    pub fn apply_frame(&self, frame: &MarketFrame, received: Instant) -> Option<TokenId> {
        match frame {
            MarketFrame::Book {
                asset_id,
                bids,
                asks,
                meta,
            } => {
                let mut books = self.lock();
                let entry = books
                    .entry(asset_id.clone())
                    .or_insert_with(|| LiveBook::placeholder(asset_id.clone(), received));
                let fresh =
                    OrderBook::new(asset_id.clone(), bids.clone(), asks.clone()).normalized();
                // The hash is opaque to us, so the only claim we can check is its own
                // consistency: the same hash must not describe two different books.
                if let (Some(previous), Some(current)) = (&entry.last_hash, &meta.hash) {
                    if previous == current && entry.book != fresh {
                        StreamStats::bump(&self.stats.hash_contradictions);
                        tracing::warn!(
                            token = %asset_id,
                            "book snapshot repeated its hash with different contents — \
                             forcing a REST resync (TODO(verify-live): hash semantics)"
                        );
                        entry.stale = true;
                    }
                }
                entry.book = fresh;
                entry.last_hash = meta.hash.clone();
                entry.stale = false;
                entry.bump(received, meta.server_ts);
                StreamStats::bump(&self.stats.snapshots);
                Some(asset_id.clone())
            }
            MarketFrame::PriceChange {
                asset_id,
                changes,
                meta,
            } => {
                let mut books = self.lock();
                let Some(entry) = books.get_mut(asset_id) else {
                    // No snapshot yet: a delta on an unknown book cannot be applied, and
                    // guessing would invent liquidity. Remember it as stale so the resync
                    // sweep fetches it over REST.
                    books.insert(
                        asset_id.clone(),
                        LiveBook::placeholder(asset_id.clone(), received),
                    );
                    StreamStats::bump(&self.stats.orphan_deltas);
                    return None;
                };
                // Out of order: applying a level from before the last frame would
                // overwrite newer state with older state.
                if let (Some(frame_ts), Some(last)) = (meta.server_ts, entry.last_server_ts) {
                    if frame_ts < last {
                        entry.stale = true;
                        StreamStats::bump(&self.stats.out_of_order);
                        return None;
                    }
                }
                for change in changes {
                    entry
                        .book
                        .apply_level(change.side, change.price, change.size);
                }
                entry.last_hash = meta.hash.clone();
                entry.bump(received, meta.server_ts);
                StreamStats::bump(&self.stats.deltas);
                Some(asset_id.clone())
            }
            MarketFrame::Unknown { .. } => {
                StreamStats::bump(&self.stats.unknown_frames);
                None
            }
            MarketFrame::Malformed { .. } => {
                StreamStats::bump(&self.stats.malformed_frames);
                None
            }
        }
    }

    /// Adopt a REST fetch. REST is authoritative: a returned book replaces ours and clears
    /// its staleness.
    ///
    /// `requested` is everything we asked about, so a token the API did **not** return is
    /// remembered as *asked about* rather than *never seen*. Without that, a token with no
    /// order book at all would qualify as "missing" on every tick and be re-requested
    /// forever — turning the cheap targeted resync back into a full polling loop.
    pub fn apply_rest(&self, requested: &[TokenId], books: &BookMap, now: Instant) {
        let mut live = self.lock();
        for token in requested {
            live.entry(token.clone())
                .or_insert_with(|| LiveBook::placeholder(token.clone(), now))
                .last_rest_attempt = Some(now);
        }
        for (token, book) in books {
            let entry = live
                .entry(token.clone())
                .or_insert_with(|| LiveBook::placeholder(token.clone(), now));
            entry.book = book.clone();
            entry.stale = false;
            entry.last_hash = None;
            entry.last_rest_attempt = Some(now);
            entry.bump(now, None);
        }
    }

    /// The books for `tokens`, in the shape the detectors take. Stale books are included:
    /// a stale book is *old*, not *invented*, and suppressing it would silently disable
    /// detection for its whole event. The resync sweep is what bounds how old it can get.
    pub fn snapshot_of(&self, tokens: &[TokenId]) -> BookMap {
        let live = self.lock();
        tokens
            .iter()
            .filter_map(|t| live.get(t).map(|b| (t.clone(), b.book.clone())))
            .collect()
    }

    /// Local revision counter for a token. Diagnostic only — nothing keys on it, and
    /// deliberately so: it is *our* count of applied frames, not the venue's sequence.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn revision(&self, token: &TokenId) -> Option<u64> {
        self.lock().get(token).map(|b| b.revision)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_stale(&self, token: &TokenId) -> Option<bool> {
        self.lock().get(token).map(|b| b.stale)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Mark books unverified — used on every reconnect, because we cannot know what we
    /// missed while the socket was down.
    pub fn mark_stale(&self, tokens: &[TokenId]) {
        let mut live = self.lock();
        for token in tokens {
            if let Some(entry) = live.get_mut(token) {
                entry.stale = true;
            }
        }
    }

    /// Tokens that need a targeted REST re-fetch: never seen, explicitly stale, or not
    /// updated inside `stale_after` — and not already asked about inside the same window.
    ///
    /// That last clause is the rate limit on this path. Some tokens simply have no book,
    /// and a book that never arrives would otherwise be requested on every tick for as
    /// long as the daemon runs.
    pub fn missing_or_stale(
        &self,
        tokens: &[TokenId],
        now: Instant,
        stale_after: Duration,
    ) -> Vec<TokenId> {
        let live = self.lock();
        tokens
            .iter()
            .filter(|t| match live.get(*t) {
                None => true,
                Some(entry) => {
                    let due = entry.stale
                        || now.saturating_duration_since(entry.last_update) >= stale_after;
                    let cooled = entry.last_rest_attempt.is_none_or(|attempted| {
                        now.saturating_duration_since(attempted) >= stale_after
                    });
                    due && cooled
                }
            })
            .cloned()
            .collect()
    }

    /// Drop everything outside `keep` (tokens that left the universe).
    pub fn retain(&self, keep: &[TokenId]) {
        let keep: HashSet<&TokenId> = keep.iter().collect();
        self.lock().retain(|token, _| keep.contains(token));
    }

    /// How many of a random sample of `rest`'s books disagree with our local copy at the
    /// top of book — the number that matters, since that is what the detectors price.
    ///
    /// Call this *before* [`apply_rest`], which overwrites the evidence.
    pub fn count_divergence(&self, rest: &BookMap, sample: usize) -> usize {
        let mut tokens: Vec<&TokenId> = rest.keys().collect();
        if tokens.len() > sample {
            tokens.shuffle(&mut rand::thread_rng());
            tokens.truncate(sample);
        }
        let live = self.lock();
        let mut diverged = 0usize;
        for token in tokens {
            let Some(entry) = live.get(token) else {
                continue;
            };
            // A book we already know is stale is not evidence of drift.
            if entry.stale {
                continue;
            }
            let Some(theirs) = rest.get(token) else {
                continue;
            };
            let top = |b: &OrderBook| (b.bids.first().cloned(), b.asks.first().cloned());
            if top(&entry.book) != top(theirs) {
                diverged += 1;
            }
        }
        if diverged > 0 {
            self.stats
                .divergences
                .fetch_add(diverged as u64, Ordering::Relaxed);
        }
        diverged
    }
}

// ---------------------------------------------------------------------------------
// Debounce
// ---------------------------------------------------------------------------------

/// One token whose book changed, with everything needed to date the change.
#[derive(Debug, Clone)]
pub struct TokenTouch {
    pub token: TokenId,
    /// The triggering frame's server timestamp, when it carried a usable one.
    pub server_ts: Option<DateTime<Utc>>,
    /// When we read the frame off the socket.
    pub received: Instant,
}

/// A debounced set of touched tokens: one detection pass, however many frames caused it.
#[derive(Debug, Clone, Default)]
pub struct DirtyBatch {
    triggers: HashMap<TokenId, TokenTouch>,
}

impl DirtyBatch {
    pub fn tokens(&self) -> impl Iterator<Item = &TokenId> {
        self.triggers.keys()
    }

    pub fn len(&self) -> usize {
        self.triggers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }

    fn insert(&mut self, touch: TokenTouch) {
        self.triggers.insert(touch.token.clone(), touch);
    }

    /// Detection latency for something built out of `tokens`: the age of the *oldest*
    /// triggering frame among them, because that is the one whose information we were
    /// slowest to act on.
    ///
    /// Uses the server timestamp when it is present and plausible (see
    /// [`MAX_PLAUSIBLE_LATENCY_MS`]), otherwise the time since we read the frame.
    /// `None` when none of `tokens` was in this batch.
    pub fn latency_ms<'a>(
        &self,
        tokens: impl IntoIterator<Item = &'a TokenId>,
        now: Instant,
        wall_now: DateTime<Utc>,
    ) -> Option<i64> {
        let mut worst: Option<i64> = None;
        for token in tokens {
            let Some(touch) = self.triggers.get(token) else {
                continue;
            };
            let from_receipt =
                i64::try_from(now.saturating_duration_since(touch.received).as_millis())
                    .unwrap_or(i64::MAX);
            let ms = match touch.server_ts {
                Some(ts) => {
                    let delta = (wall_now - ts).num_milliseconds();
                    if (0..=MAX_PLAUSIBLE_LATENCY_MS).contains(&delta) {
                        delta
                    } else {
                        from_receipt
                    }
                }
                None => from_receipt,
            };
            worst = Some(worst.map_or(ms, |w: i64| w.max(ms)));
        }
        worst
    }
}

/// Collect touches into batches, emitting one batch `debounce` after the last touch.
///
/// A burst of deltas across an event's legs therefore produces a single detection pass
/// over the settled book, not one per frame.
async fn debounce_worker(
    mut touches: mpsc::Receiver<TokenTouch>,
    batches: mpsc::Sender<DirtyBatch>,
    debounce: Duration,
) {
    let mut pending = DirtyBatch::default();
    loop {
        if pending.is_empty() {
            match touches.recv().await {
                Some(touch) => pending.insert(touch),
                None => return,
            }
            continue;
        }
        match tokio::time::timeout(debounce, touches.recv()).await {
            // Another touch inside the window: the window restarts.
            Ok(Some(touch)) => pending.insert(touch),
            Ok(None) => {
                let _ = batches.send(std::mem::take(&mut pending)).await;
                return;
            }
            Err(_) => {
                if batches.send(std::mem::take(&mut pending)).await.is_err() {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// Sharding
// ---------------------------------------------------------------------------------

/// The token assignment for every connection, plus which of them changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardPlan {
    pub shards: Vec<Vec<TokenId>>,
    /// Indices whose token set differs from the plan they replace. Only these connections
    /// are disturbed; every other shard keeps its socket and its books.
    pub changed: Vec<usize>,
}

/// Split a fresh universe across connections.
pub fn shard_tokens(tokens: &[TokenId], cap: usize) -> Vec<Vec<TokenId>> {
    tokens.chunks(cap.max(1)).map(<[TokenId]>::to_vec).collect()
}

/// Re-plan `existing` shards for a new token set, moving as little as possible.
///
/// Tokens that left the universe are dropped from whichever shard held them; tokens that
/// joined go into the first shard with spare capacity, and only then into new shards. A
/// shard nobody added to or removed from is left untouched, so a universe refresh does not
/// resubscribe the whole pool (and does not make every book stale).
pub fn replan(existing: &[Vec<TokenId>], desired: &[TokenId], cap: usize) -> ShardPlan {
    let cap = cap.max(1);
    let wanted: HashSet<&TokenId> = desired.iter().collect();

    let mut shards: Vec<Vec<TokenId>> = existing
        .iter()
        .map(|shard| {
            shard
                .iter()
                .filter(|t| wanted.contains(*t))
                .cloned()
                .collect()
        })
        .collect();

    let held: HashSet<TokenId> = shards.iter().flatten().cloned().collect();
    let mut added = desired
        .iter()
        .filter(|t| !held.contains(*t))
        .cloned()
        .peekable();

    for shard in shards.iter_mut() {
        while shard.len() < cap {
            match added.next() {
                Some(token) => shard.push(token),
                None => break,
            }
        }
    }
    while added.peek().is_some() {
        let mut shard = Vec::new();
        while shard.len() < cap {
            match added.next() {
                Some(token) => shard.push(token),
                None => break,
            }
        }
        shards.push(shard);
    }

    let changed = shards
        .iter()
        .enumerate()
        .filter(|(i, tokens)| existing.get(*i) != Some(*tokens))
        .map(|(i, _)| i)
        .collect();
    ShardPlan { shards, changed }
}

// ---------------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------------

/// Connection health, and the one-way switch to REST-only polling.
#[derive(Debug)]
pub struct StreamHealth {
    consecutive_failures: AtomicU32,
    fallback_after: u32,
    fallen_back: AtomicBool,
    connected: AtomicU32,
}

impl StreamHealth {
    fn new(fallback_after: u32) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            fallback_after: fallback_after.max(1),
            fallen_back: AtomicBool::new(false),
            connected: AtomicU32::new(0),
        }
    }

    fn record_connected(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.connected.fetch_add(1, Ordering::Relaxed);
    }

    fn record_disconnected(&self) {
        // saturating: a task that exits after fallback must not wrap this to u32::MAX.
        let _ = self
            .connected
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }

    /// Returns true when this failure tripped the permanent fallback.
    fn record_connect_failure(&self) -> bool {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.fallback_after && !self.fallen_back.swap(true, Ordering::SeqCst) {
            return true;
        }
        false
    }

    /// True once the stream has been given up on for the life of the process.
    pub fn fallen_back(&self) -> bool {
        self.fallen_back.load(Ordering::SeqCst)
    }

    pub fn live_connections(&self) -> u32 {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Exponential backoff with full jitter on the increment (mirrors [`crate::http`]).
fn backoff(attempt: u32) -> Duration {
    let exp = BASE_BACKOFF_MS
        .saturating_mul(1u64 << attempt.min(7))
        .min(MAX_BACKOFF_MS);
    let jitter = {
        use rand::Rng;
        rand::thread_rng().gen_range(0..=exp / 2)
    };
    Duration::from_millis(exp + jitter)
}

// ---------------------------------------------------------------------------------
// Connection pool
// ---------------------------------------------------------------------------------

/// TODO(verify-live): the subscribe frame. The public docs show
/// `{"assets_ids": [...], "type": "market"}` — note the irregular `assets_ids` plural.
/// There is no documented *unsubscribe*, so a shard whose token set changes reconnects
/// with the new list instead (see [`StreamManager::update_universe`]).
pub fn subscribe_message(tokens: &[TokenId]) -> String {
    serde_json::json!({
        "type": "market",
        "assets_ids": tokens.iter().map(TokenId::as_str).collect::<Vec<_>>(),
    })
    .to_string()
}

struct Shard {
    tokens: Vec<TokenId>,
    tx: watch::Sender<Arc<Vec<TokenId>>>,
}

/// The connection pool: one task per shard, a debouncer, and the shared book state.
pub struct StreamManager {
    url: String,
    max_subs: usize,
    books: Arc<BookStore>,
    health: Arc<StreamHealth>,
    touch_tx: mpsc::Sender<TokenTouch>,
    batches: mpsc::Receiver<DirtyBatch>,
    shards: Vec<Shard>,
    tasks: JoinSet<()>,
    shutdown: watch::Receiver<bool>,
}

impl StreamManager {
    /// Start the pool for `tokens`. Connections are established in the background: a start
    /// never blocks the daemon, and an endpoint that is simply down turns into the
    /// permanent REST fallback after `stream.fallback_after_failures` attempts.
    pub fn start(cfg: &Config, tokens: &[TokenId], shutdown: watch::Receiver<bool>) -> Self {
        let stream = &cfg.stream;
        let books = Arc::new(BookStore::new());
        let health = Arc::new(StreamHealth::new(stream.fallback_after_failures));
        let (touch_tx, touch_rx) = mpsc::channel(TOUCH_CAPACITY);
        let (batch_tx, batches) = mpsc::channel(BATCH_CAPACITY);
        let mut tasks = JoinSet::new();
        tasks.spawn(debounce_worker(
            touch_rx,
            batch_tx,
            Duration::from_millis(stream.debounce_ms),
        ));

        let mut manager = Self {
            url: stream.url.clone(),
            max_subs: stream.max_subs_per_connection.max(1),
            books,
            health,
            touch_tx,
            batches,
            shards: Vec::new(),
            tasks,
            shutdown,
        };
        let shards = shard_tokens(tokens, manager.max_subs);
        tracing::info!(
            url = %manager.url,
            tokens = tokens.len(),
            shards = shards.len(),
            max_subs_per_connection = manager.max_subs,
            "starting the CLOB market-data stream"
        );
        manager.apply(ShardPlan {
            changed: (0..shards.len()).collect(),
            shards,
        });
        manager
    }

    pub fn books(&self) -> &Arc<BookStore> {
        &self.books
    }

    pub fn health(&self) -> &Arc<StreamHealth> {
        &self.health
    }

    pub fn shard_tokens(&self) -> Vec<Vec<TokenId>> {
        self.shards.iter().map(|s| s.tokens.clone()).collect()
    }

    /// Next debounced batch of touched tokens, or `None` once the pool has stopped.
    pub async fn recv(&mut self) -> Option<DirtyBatch> {
        self.batches.recv().await
    }

    /// Re-shard for a refreshed universe. Only the shards whose membership changed are
    /// resubscribed; the rest keep their sockets and their books.
    pub fn update_universe(&mut self, tokens: &[TokenId]) {
        let existing = self.shard_tokens();
        let plan = replan(&existing, tokens, self.max_subs);
        if plan.changed.is_empty() {
            return;
        }
        tracing::info!(
            tokens = tokens.len(),
            shards = plan.shards.len(),
            resubscribing = plan.changed.len(),
            "universe changed — resubscribing the affected stream shards"
        );
        self.books.retain(tokens);
        self.apply(plan);
    }

    fn apply(&mut self, plan: ShardPlan) {
        for (index, tokens) in plan.shards.into_iter().enumerate() {
            match self.shards.get_mut(index) {
                Some(shard) => {
                    if shard.tokens != tokens {
                        shard.tokens = tokens.clone();
                        // The shard task reconnects with the new list; a failed send only
                        // means the task is gone (shutdown or fallback).
                        let _ = shard.tx.send(Arc::new(tokens));
                    }
                }
                None => {
                    let (tx, rx) = watch::channel(Arc::new(tokens.clone()));
                    self.tasks.spawn(shard_task(
                        index,
                        self.url.clone(),
                        rx,
                        self.books.clone(),
                        self.touch_tx.clone(),
                        self.health.clone(),
                        self.shutdown.clone(),
                    ));
                    self.shards.push(Shard { tokens, tx });
                }
            }
        }
    }

    /// Stop every connection and wait briefly for the tasks to notice.
    pub async fn stop(mut self) {
        self.shards.clear();
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn shard_task(
    id: usize,
    url: String,
    mut tokens_rx: watch::Receiver<Arc<Vec<TokenId>>>,
    books: Arc<BookStore>,
    touches: mpsc::Sender<TokenTouch>,
    health: Arc<StreamHealth>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut attempt = 0u32;
    loop {
        if *shutdown.borrow() || health.fallen_back() {
            return;
        }
        let tokens: Vec<TokenId> = tokens_rx.borrow_and_update().as_ref().clone();
        if tokens.is_empty() {
            // Emptied by a universe refresh: idle until this shard is given work again.
            tokio::select! {
                _ = shutdown.changed() => return,
                changed = tokens_rx.changed() => {
                    if changed.is_err() { return; }
                }
            }
            continue;
        }

        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((socket, _response)) => {
                health.record_connected();
                attempt = 0;
                tracing::info!(shard = id, tokens = tokens.len(), "market stream connected");
                run_connection(
                    id,
                    socket,
                    &tokens,
                    &books,
                    &touches,
                    &mut tokens_rx,
                    &mut shutdown,
                )
                .await;
                health.record_disconnected();
                // Whatever we missed while the socket was down is unknowable, so every
                // book on this shard is unverified until a fresh snapshot lands.
                books.mark_stale(&tokens);
                StreamStats::bump(&books.stats.reconnects);
                if *shutdown.borrow() {
                    return;
                }
            }
            Err(err) => {
                StreamStats::bump(&books.stats.connect_failures);
                let tripped = health.record_connect_failure();
                if tripped {
                    tracing::error!(
                        shard = id,
                        %url,
                        %err,
                        "market stream unreachable — giving up on streaming for the life of \
                         this process and falling back to REST polling. Detection latency \
                         returns to the scan interval; check stream.url and connectivity."
                    );
                    return;
                }
                tracing::warn!(shard = id, %url, %err, attempt, "market stream connect failed");
                attempt = attempt.saturating_add(1);
            }
        }

        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(backoff(attempt)) => {}
        }
    }
}

/// Drive one connected socket until it closes, the token set changes, or we shut down.
async fn run_connection<S>(
    id: usize,
    mut socket: S,
    tokens: &[TokenId],
    books: &Arc<BookStore>,
    touches: &mpsc::Sender<TokenTouch>,
    tokens_rx: &mut watch::Receiver<Arc<Vec<TokenId>>>,
    shutdown: &mut watch::Receiver<bool>,
) where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    if let Err(err) = socket.send(Message::Text(subscribe_message(tokens))).await {
        tracing::warn!(shard = id, %err, "could not send the market subscribe frame");
        return;
    }

    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick is immediate

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            changed = tokens_rx.changed() => {
                if changed.is_ok() {
                    tracing::info!(shard = id, "shard membership changed — resubscribing");
                }
                return;
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    return;
                }
            }
            message = socket.next() => {
                let received = Instant::now();
                match message {
                    None => return,
                    Some(Err(err)) => {
                        tracing::warn!(shard = id, %err, "market stream read failed");
                        return;
                    }
                    Some(Ok(Message::Close(_))) => return,
                    Some(Ok(Message::Text(text))) => {
                        if !handle_payload(id, &text, books, touches, received).await {
                            return;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        match String::from_utf8(bytes) {
                            Ok(text) => {
                                if !handle_payload(id, &text, books, touches, received).await {
                                    return;
                                }
                            }
                            Err(_) => {
                                StreamStats::bump(&books.stats.malformed_frames);
                            }
                        }
                    }
                    // Ping/Pong/Frame: tungstenite answers pings itself.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Parse and apply one payload. Returns false when the consumer is gone.
async fn handle_payload(
    id: usize,
    text: &str,
    books: &Arc<BookStore>,
    touches: &mpsc::Sender<TokenTouch>,
    received: Instant,
) -> bool {
    let frames = match parse_frames(text) {
        Ok(frames) => frames,
        Err(err) => {
            StreamStats::bump(&books.stats.malformed_frames);
            tracing::debug!(shard = id, %err, "unparseable market frame ignored");
            return true;
        }
    };
    for frame in &frames {
        if let MarketFrame::Unknown { event_type } = frame {
            tracing::debug!(shard = id, event_type, "unknown market frame type ignored");
        }
        if let MarketFrame::Malformed { event_type, reason } = frame {
            tracing::debug!(
                shard = id,
                event_type,
                reason,
                "malformed market frame ignored"
            );
        }
        let server_ts = match frame {
            MarketFrame::Book { meta, .. } | MarketFrame::PriceChange { meta, .. } => {
                meta.server_ts
            }
            _ => None,
        };
        if let Some(token) = books.apply_frame(frame, received) {
            if touches
                .send(TokenTouch {
                    token,
                    server_ts,
                    received,
                })
                .await
                .is_err()
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tok(n: usize) -> TokenId {
        TokenId::new(format!("t{n}"))
    }

    fn tokens(n: usize) -> Vec<TokenId> {
        (0..n).map(tok).collect()
    }

    // -- frame parsing ----------------------------------------------------------------

    const SNAPSHOT: &str = r#"{
        "event_type": "book",
        "asset_id": "1001",
        "market": "0xabc",
        "timestamp": "1785000000000",
        "hash": "h1",
        "bids": [{"price": "0.39", "size": "500"}, {"price": "0.38", "size": "100"}],
        "asks": [{"price": "0.41", "size": "300"}]
    }"#;

    #[test]
    fn parses_a_book_snapshot() {
        let frames = parse_frames(SNAPSHOT).expect("snapshot must parse");
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            MarketFrame::Book {
                asset_id,
                bids,
                asks,
                meta,
            } => {
                assert_eq!(asset_id.as_str(), "1001");
                assert_eq!(bids.len(), 2);
                assert_eq!(bids[0].price, dec!(0.39));
                assert_eq!(asks[0].size, dec!(300));
                assert_eq!(meta.hash.as_deref(), Some("h1"));
                assert_eq!(
                    meta.server_ts,
                    DateTime::from_timestamp_millis(1_785_000_000_000)
                );
            }
            other => panic!("expected a book snapshot, got {other:?}"),
        }
    }

    #[test]
    fn parses_price_change_deltas_in_both_shapes() {
        let batched = r#"[{
            "event_type": "price_change",
            "asset_id": "1001",
            "timestamp": 1785000000001,
            "changes": [
                {"price": "0.41", "size": "0", "side": "SELL"},
                {"price": "0.40", "size": "250", "side": "BUY"}
            ]
        }]"#;
        let frames = parse_frames(batched).expect("delta must parse");
        match &frames[0] {
            MarketFrame::PriceChange {
                asset_id, changes, ..
            } => {
                assert_eq!(asset_id.as_str(), "1001");
                assert_eq!(
                    changes[0],
                    LevelChange {
                        side: Side::Ask,
                        price: dec!(0.41),
                        size: dec!(0)
                    }
                );
                assert_eq!(changes[1].side, Side::Bid);
                assert_eq!(changes[1].size, dec!(250));
            }
            other => panic!("expected a price change, got {other:?}"),
        }

        // The single-level form, with the fields at the top level.
        let single =
            r#"{"event_type":"price_change","asset_id":"7","price":0.5,"size":"12","side":"buy"}"#;
        match &parse_frames(single).expect("single form")[0] {
            MarketFrame::PriceChange { changes, .. } => {
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].price, dec!(0.5));
                assert_eq!(changes[0].size, dec!(12));
                assert_eq!(changes[0].side, Side::Bid);
            }
            other => panic!("expected a price change, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_malformed_frames_are_classified_never_fatal() {
        // A type we do not model.
        let frames = parse_frames(r#"{"event_type":"tick_size_change","asset_id":"1"}"#).unwrap();
        assert_eq!(
            frames[0],
            MarketFrame::Unknown {
                event_type: "tick_size_change".into()
            }
        );

        // No type at all.
        let frames = parse_frames(r#"{"foo":"bar"}"#).unwrap();
        assert!(matches!(frames[0], MarketFrame::Unknown { .. }));

        // Known type, unusable content.
        for (body, why) in [
            (r#"{"event_type":"book","bids":[]}"#, "asset id"),
            (
                r#"{"event_type":"price_change","asset_id":"1","changes":[{"price":"0.4","size":"1","side":"sideways"}]}"#,
                "side",
            ),
            (
                r#"{"event_type":"price_change","asset_id":"1"}"#,
                "no level",
            ),
            // Right type, wrong JSON type for a field we need.
            (
                r#"{"event_type":"book","asset_id":"1","bids":"nope"}"#,
                "types",
            ),
        ] {
            let frames = parse_frames(body).unwrap_or_else(|e| panic!("{why}: {e}"));
            assert!(
                matches!(frames[0], MarketFrame::Malformed { .. }),
                "{why}: expected malformed, got {:?}",
                frames[0]
            );
        }
    }

    #[test]
    fn malformed_json_and_keepalives_do_not_crash_the_reader() {
        assert!(parse_frames("{not json").is_err());
        assert!(parse_frames("PONG").expect("keepalive").is_empty());
        assert!(parse_frames("   ").expect("empty").is_empty());
        // An array with one good and one unusable element keeps the good one.
        let frames = parse_frames(&format!("[{SNAPSHOT},{{\"event_type\":\"book\"}}]")).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0], MarketFrame::Book { .. }));
        assert!(matches!(frames[1], MarketFrame::Malformed { .. }));
    }

    #[test]
    fn timestamps_are_read_as_milliseconds_or_seconds() {
        assert_eq!(
            parse_server_ts(&Value::String("1785000000000".into())),
            DateTime::from_timestamp_millis(1_785_000_000_000)
        );
        assert_eq!(
            parse_server_ts(&Value::from(1_785_000_000i64)),
            DateTime::from_timestamp(1_785_000_000, 0)
        );
        assert_eq!(parse_server_ts(&Value::String("nope".into())), None);
        assert_eq!(parse_server_ts(&Value::from(0)), None);
    }

    // -- book application -------------------------------------------------------------

    fn frames_of(body: &str) -> Vec<MarketFrame> {
        parse_frames(body).expect("fixture frames")
    }

    #[test]
    fn a_snapshot_then_deltas_produce_the_hand_built_book() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            assert_eq!(
                store.apply_frame(&frame, now).as_ref(),
                Some(&tok_id("1001"))
            );
        }
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.best_bid(), Some(dec!(0.39)));
        assert_eq!(book.best_ask(), Some(dec!(0.41)));
        assert_eq!(store.revision(&tok_id("1001")), Some(1));

        // A new best ask, a resized bid, and a removal (size 0) in one delta.
        let delta = r#"{"event_type":"price_change","asset_id":"1001","timestamp":"1785000000001",
            "changes":[
                {"price":"0.40","size":"120","side":"SELL"},
                {"price":"0.39","size":"250","side":"BUY"},
                {"price":"0.38","size":"0","side":"BUY"}
            ]}"#;
        for frame in frames_of(delta) {
            store.apply_frame(&frame, now);
        }
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.best_ask(), Some(dec!(0.40)), "the new ask sorts first");
        assert_eq!(book.asks.len(), 2);
        assert_eq!(book.asks[1].price, dec!(0.41));
        assert_eq!(book.bids.len(), 1, "the zero-size level was removed");
        assert_eq!(book.bids[0], PriceLevel::new(dec!(0.39), dec!(250)));
        assert_eq!(store.revision(&tok_id("1001")), Some(2));
        assert_eq!(store.stats.snapshot().deltas, 1);
        assert_eq!(store.stats.snapshot().snapshots, 1);
    }

    fn tok_id(s: &str) -> TokenId {
        TokenId::new(s)
    }

    #[test]
    fn a_delta_for_an_unseen_book_is_not_invented() {
        let store = BookStore::new();
        let delta = r#"{"event_type":"price_change","asset_id":"9","changes":[{"price":"0.4","size":"10","side":"BUY"}]}"#;
        for frame in frames_of(delta) {
            assert_eq!(store.apply_frame(&frame, Instant::now()), None);
        }
        assert_eq!(store.stats.snapshot().orphan_deltas, 1);
        // It is remembered as stale so the resync sweep fetches it over REST.
        assert_eq!(store.is_stale(&tok_id("9")), Some(true));
        assert!(store.snapshot_of(&[tok_id("9")])[&tok_id("9")]
            .best_bid()
            .is_none());
    }

    #[test]
    fn an_out_of_order_delta_is_skipped_and_forces_a_resync() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, now);
        }
        let stale_delta = r#"{"event_type":"price_change","asset_id":"1001","timestamp":"1784999999999",
            "changes":[{"price":"0.39","size":"1","side":"BUY"}]}"#;
        for frame in frames_of(stale_delta) {
            assert_eq!(store.apply_frame(&frame, now), None);
        }
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(
            book.bids[0].size,
            dec!(500),
            "the old level was not applied"
        );
        assert_eq!(store.stats.snapshot().out_of_order, 1);
        assert_eq!(store.is_stale(&tok_id("1001")), Some(true));
    }

    #[test]
    fn a_hash_that_contradicts_itself_marks_the_book_stale() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, now);
        }
        assert_eq!(store.is_stale(&tok_id("1001")), Some(false));
        // Same hash, different contents.
        let contradiction = SNAPSHOT.replace("\"size\": \"300\"", "\"size\": \"301\"");
        for frame in frames_of(&contradiction) {
            store.apply_frame(&frame, now);
        }
        assert_eq!(store.stats.snapshot().hash_contradictions, 1);
        // The snapshot itself is still applied (it is the newer view); the flag is what
        // sends the resync sweep to check.
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.asks[0].size, dec!(301));
    }

    #[test]
    fn a_reconnect_marks_the_shards_books_stale_until_a_fresh_snapshot() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, now);
        }
        assert_eq!(store.is_stale(&tok_id("1001")), Some(false));

        store.mark_stale(&[tok_id("1001")]);
        assert_eq!(store.is_stale(&tok_id("1001")), Some(true));
        assert_eq!(
            store.missing_or_stale(&[tok_id("1001")], now, Duration::from_secs(60)),
            vec![tok_id("1001")],
            "a stale book is resynced regardless of its age"
        );

        // A fresh snapshot clears it.
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, now);
        }
        assert_eq!(store.is_stale(&tok_id("1001")), Some(false));
        assert!(store
            .missing_or_stale(&[tok_id("1001")], now, Duration::from_secs(60))
            .is_empty());
    }

    #[test]
    fn staleness_covers_never_seen_and_too_old_books() {
        let store = BookStore::new();
        let t0 = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, t0);
        }
        let watched = vec![tok_id("1001"), tok_id("2002")];
        // 2002 was never seen at all.
        assert_eq!(
            store.missing_or_stale(&watched, t0, Duration::from_secs(60)),
            vec![tok_id("2002")]
        );
        // 61 s later 1001 is stale too.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(
            store.missing_or_stale(&watched, later, Duration::from_secs(60)),
            watched
        );
    }

    #[test]
    fn rest_is_authoritative_and_divergence_is_counted_before_it_lands() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, now);
        }
        let mut rest = BookMap::new();
        rest.insert(
            tok_id("1001"),
            OrderBook::new(
                tok_id("1001"),
                vec![PriceLevel::new(dec!(0.39), dec!(500))],
                vec![PriceLevel::new(dec!(0.42), dec!(300))], // we think 0.41
            )
            .normalized(),
        );
        assert_eq!(store.count_divergence(&rest, DIVERGENCE_SAMPLE), 1);
        store.apply_rest(&[tok_id("1001")], &rest, now);
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.best_ask(), Some(dec!(0.42)));
        assert_eq!(store.count_divergence(&rest, DIVERGENCE_SAMPLE), 0);
        assert_eq!(store.stats.snapshot().divergences, 1);
    }

    /// A token the API has no book for must not become a request on every tick: after we
    /// have asked once, it waits out the stale window like everything else.
    #[test]
    fn a_token_the_api_never_returns_is_not_re_requested_every_tick() {
        let store = BookStore::new();
        let t0 = Instant::now();
        let watched = vec![tok_id("1001"), tok_id("ghost")];
        let window = Duration::from_secs(60);

        // Nothing known yet: both are due.
        assert_eq!(store.missing_or_stale(&watched, t0, window), watched);

        // We asked about both; only 1001 came back.
        let mut returned = BookMap::new();
        returned.insert(
            tok_id("1001"),
            OrderBook::new(
                tok_id("1001"),
                vec![PriceLevel::new(dec!(0.39), dec!(500))],
                vec![],
            ),
        );
        store.apply_rest(&watched, &returned, t0);

        assert!(
            store
                .missing_or_stale(&watched, t0 + Duration::from_secs(5), window)
                .is_empty(),
            "neither may be re-requested five seconds later"
        );
        // Once the window has passed, the ghost is retried — we do not give up on it.
        assert_eq!(
            store.missing_or_stale(&watched, t0 + Duration::from_secs(61), window),
            watched
        );

        // A reconnect still forces a prompt resync of a book we do have, because the last
        // REST attempt for it is old by then.
        store.mark_stale(&[tok_id("1001")]);
        assert_eq!(
            store.missing_or_stale(&watched, t0 + Duration::from_secs(61), window),
            watched
        );
    }

    #[test]
    fn retain_drops_books_that_left_the_universe() {
        let store = BookStore::new();
        store.apply_rest(
            &[tok_id("a"), tok_id("b")],
            &[
                (tok_id("a"), OrderBook::new(tok_id("a"), vec![], vec![])),
                (tok_id("b"), OrderBook::new(tok_id("b"), vec![], vec![])),
            ]
            .into_iter()
            .collect(),
            Instant::now(),
        );
        assert_eq!(store.len(), 2);
        store.retain(&[tok_id("a")]);
        assert_eq!(store.len(), 1);
        assert!(store.snapshot_of(&[tok_id("b")]).is_empty());
        store.retain(&[]);
        assert!(store.is_empty());
    }

    // -- sharding ---------------------------------------------------------------------

    #[test]
    fn twelve_hundred_tokens_shard_across_three_capped_connections() {
        let all = tokens(1_200);
        let shards = shard_tokens(&all, 500);
        assert_eq!(shards.len(), 3);
        assert_eq!(
            shards.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![500, 500, 200]
        );
        // Every token is subscribed exactly once.
        let flat: Vec<&TokenId> = shards.iter().flatten().collect();
        assert_eq!(flat.len(), 1_200);
        assert_eq!(
            flat.iter().collect::<HashSet<_>>().len(),
            1_200,
            "no token may be subscribed twice"
        );
        assert_eq!(shard_tokens(&[], 500).len(), 0);
    }

    #[test]
    fn a_universe_diff_only_disturbs_the_shards_it_touches() {
        let all = tokens(1_200);
        let existing = shard_tokens(&all, 500);

        // Drop 100 tokens out of shard 0 and add 50 brand new ones.
        let mut desired: Vec<TokenId> = all.iter().skip(100).cloned().collect();
        desired.extend((5_000..5_050).map(tok));

        let plan = replan(&existing, &desired, 500);
        assert_eq!(plan.changed, vec![0], "only shard 0 changed");
        assert_eq!(plan.shards.len(), 3);
        assert_eq!(plan.shards[0].len(), 450, "400 kept + 50 new");
        assert_eq!(plan.shards[1], existing[1]);
        assert_eq!(plan.shards[2], existing[2]);
        assert!(plan.shards[0].starts_with(&[tok(100), tok(101)]));
        assert!(plan.shards[0].ends_with(&[tok(5_048), tok(5_049)]));

        // Everything desired is still subscribed exactly once.
        let flat: HashSet<&TokenId> = plan.shards.iter().flatten().collect();
        assert_eq!(flat.len(), desired.len());

        // An unchanged universe is a no-op.
        let same = replan(&plan.shards, &desired, 500);
        assert!(same.changed.is_empty());
        assert_eq!(same.shards, plan.shards);
    }

    #[test]
    fn growth_past_the_cap_opens_a_new_connection_and_shrinkage_empties_one() {
        let existing = shard_tokens(&tokens(4), 2); // [t0,t1] [t2,t3]
        let grown = replan(&existing, &tokens(6), 2);
        assert_eq!(grown.shards.len(), 3);
        assert_eq!(grown.changed, vec![2]);
        assert_eq!(grown.shards[2], vec![tok(4), tok(5)]);

        // Now shrink to nothing but one token: shards 1 and 2 empty out (their tasks idle).
        let shrunk = replan(&grown.shards, &[tok(0)], 2);
        assert_eq!(shrunk.shards[0], vec![tok(0)]);
        assert!(shrunk.shards[1].is_empty() && shrunk.shards[2].is_empty());
        assert_eq!(shrunk.changed, vec![0, 1, 2]);
    }

    // -- debounce ---------------------------------------------------------------------

    #[tokio::test]
    async fn three_rapid_deltas_produce_exactly_one_detection_pass() {
        let (touch_tx, touch_rx) = mpsc::channel(16);
        let (batch_tx, mut batches) = mpsc::channel(16);
        tokio::spawn(debounce_worker(
            touch_rx,
            batch_tx,
            Duration::from_millis(50),
        ));

        for i in 0..3 {
            touch_tx
                .send(TokenTouch {
                    token: tok(i),
                    server_ts: None,
                    received: Instant::now(),
                })
                .await
                .expect("send");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let batch = tokio::time::timeout(Duration::from_secs(2), batches.recv())
            .await
            .expect("a batch must be emitted")
            .expect("channel open");
        assert_eq!(batch.len(), 3, "one pass covering all three tokens");

        // And nothing else follows from the same burst.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), batches.recv())
                .await
                .is_err(),
            "the burst must produce exactly one pass"
        );

        // A later touch is its own pass.
        touch_tx
            .send(TokenTouch {
                token: tok(9),
                server_ts: None,
                received: Instant::now(),
            })
            .await
            .expect("send");
        let batch = tokio::time::timeout(Duration::from_secs(2), batches.recv())
            .await
            .expect("second batch")
            .expect("channel open");
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn latency_prefers_the_server_clock_and_falls_back_when_it_is_implausible() {
        let now = Instant::now();
        let wall = Utc::now();
        let mut batch = DirtyBatch::default();
        batch.insert(TokenTouch {
            token: tok(1),
            server_ts: Some(wall - chrono::Duration::milliseconds(120)),
            received: now,
        });
        batch.insert(TokenTouch {
            token: tok(2),
            // Nonsense (a clock 10 minutes in the past): fall back to receipt.
            server_ts: Some(wall - chrono::Duration::minutes(10)),
            received: now,
        });
        batch.insert(TokenTouch {
            token: tok(3),
            server_ts: None,
            received: now,
        });

        assert_eq!(batch.latency_ms([&tok(1)], now, wall), Some(120));
        assert_eq!(batch.latency_ms([&tok(2)], now, wall), Some(0));
        assert_eq!(batch.latency_ms([&tok(3)], now, wall), Some(0));
        // The worst (oldest) leg is the one reported.
        assert_eq!(batch.latency_ms([&tok(1), &tok(3)], now, wall), Some(120));
        // A token nobody touched is not this batch's business.
        assert_eq!(batch.latency_ms([&tok(42)], now, wall), None);
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        assert!(backoff(1) >= Duration::from_millis(500));
        assert!(backoff(9) <= Duration::from_millis(MAX_BACKOFF_MS + MAX_BACKOFF_MS / 2));
        assert!(backoff(5) > backoff(0));
    }

    #[test]
    fn health_falls_back_once_and_only_once() {
        let health = StreamHealth::new(3);
        assert!(!health.record_connect_failure());
        assert!(!health.record_connect_failure());
        assert!(!health.fallen_back());
        assert!(
            health.record_connect_failure(),
            "the third failure trips it"
        );
        assert!(health.fallen_back());
        assert!(
            !health.record_connect_failure(),
            "the switch only reports tripping once"
        );

        // A success resets the streak, and the fallback is permanent.
        let health = StreamHealth::new(2);
        assert!(!health.record_connect_failure());
        health.record_connected();
        assert!(!health.record_connect_failure());
        assert!(!health.fallen_back(), "the streak was broken by a success");
    }

    // -- the pool, over real sockets --------------------------------------------------

    /// A stub market channel that reports every subscribe frame it receives and then holds
    /// the connection open. Returns `(url, subscriptions)`.
    async fn mock_channel() -> (String, mpsc::Receiver<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    let Some(Ok(message)) = ws.next().await else {
                        return;
                    };
                    let ids: Vec<String> =
                        serde_json::from_str::<Value>(message.to_text().unwrap_or_default())
                            .ok()
                            .and_then(|v| v["assets_ids"].as_array().cloned())
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|i| i.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                    if tx.send(ids).await.is_err() {
                        return;
                    }
                    while let Some(Ok(_)) = ws.next().await {}
                });
            }
        });
        (format!("ws://{addr}/ws/market"), rx)
    }

    async fn next_subscription(rx: &mut mpsc::Receiver<Vec<String>>) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a subscribe frame must arrive")
            .expect("channel open")
    }

    #[tokio::test]
    async fn the_pool_subscribes_every_shard_and_a_diff_only_resubscribes_what_moved() {
        let (url, mut subscriptions) = mock_channel().await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        // One token per connection, so shard boundaries are unambiguous.
        let mut cfg = Config::default();
        cfg.stream.url = url;
        cfg.stream.max_subs_per_connection = 1;
        cfg.stream.debounce_ms = 10;

        let mut manager = StreamManager::start(&cfg, &[tok(1), tok(2)], shutdown_rx);
        let mut seen = vec![
            next_subscription(&mut subscriptions).await,
            next_subscription(&mut subscriptions).await,
        ];
        seen.sort();
        assert_eq!(
            seen,
            vec![vec!["t1".to_string()], vec!["t2".to_string()]],
            "every shard subscribes exactly its own tokens"
        );

        // Swap one token out for a new one: shard 0 keeps its socket, shard 1 resubscribes.
        manager.update_universe(&[tok(1), tok(3)]);
        assert_eq!(
            manager.shard_tokens(),
            vec![vec![tok(1)], vec![tok(3)]],
            "the surviving token must not be moved between shards"
        );
        assert_eq!(
            next_subscription(&mut subscriptions).await,
            vec!["t3".to_string()],
            "only the changed shard reconnects"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), subscriptions.recv())
                .await
                .is_err(),
            "the untouched shard must not be resubscribed"
        );

        manager.stop().await;
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_trips_the_fallback_and_stops_the_pool() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut cfg = Config::default();
        // Port 1 refuses connections immediately.
        cfg.stream.url = "ws://127.0.0.1:1/ws/market".into();
        cfg.stream.fallback_after_failures = 2;

        let manager = StreamManager::start(&cfg, &[tok(1)], shutdown_rx);
        let health = manager.health().clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !health.fallen_back() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            health.fallen_back(),
            "two refused connections must trip the permanent fallback"
        );
        assert!(manager.books().stats.snapshot().connect_failures >= 2);
        assert_eq!(health.live_connections(), 0);
        manager.stop().await;
    }

    #[test]
    fn the_subscribe_frame_names_every_token() {
        let message = subscribe_message(&tokens(3));
        let parsed: Value = serde_json::from_str(&message).expect("valid JSON");
        assert_eq!(parsed["type"], "market");
        assert_eq!(parsed["assets_ids"], Value::from(vec!["t0", "t1", "t2"]));
    }
}
