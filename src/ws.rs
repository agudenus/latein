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
//! The `price_change` delta shape is **verified** (official docs + community clients,
//! 2026-07-30) and is documented at [`RawFrame::price_changes`]. Everything else is still
//! an assumption marked `TODO(verify-live)` at its definition:
//!
//! * the endpoint (`stream.url`), the subscribe frame shape, and whether one connection
//!   has a subscription limit at all;
//! * the `book` snapshot field names (`asset_id`, `bids`/`asks`, `hash`);
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
//! ## M6.5 — the delta shape, and why "defensive" was not enough
//!
//! The first live soak read 4 807 541 frames and applied **zero** deltas: every book moved
//! only when a periodic `book` snapshot arrived, and the 5-minute integrity sweep found
//! ~29 % of sampled books diverged from REST. The cause was pure wire format — the channel
//! sends `price_changes[]` with a *per-entry* `asset_id`, and this parser only understood
//! `changes[]` or a single top-level change. Every one of those frames was classified
//! `Malformed`, counted, and dropped: correct behaviour, invisible outcome, because the
//! data-plane health line reported only `frames`, `price_changes_applied` and
//! `snapshots_applied` — a *zero* that looked exactly like a quiet market.
//!
//! Two things changed, and the second matters more than the first:
//!
//! 1. `price_changes[]` is now the primary shape (the older ones stay as fallbacks — they
//!    cost nothing), and one frame may touch several assets of one market, so it fans out
//!    to one [`MarketFrame::PriceChange`] per asset carrying the parent's timestamp.
//! 2. Silence in the log is no longer possible for this class of bug: `frames_unrecognized`
//!    and `delta_entries_applied`/`delta_entries_skipped` are counted and printed, and the
//!    first [`UNRECOGNIZED_SAMPLE_LIMIT`] unrecognized payloads are logged **raw** at WARN
//!    (truncated; this is public market data). The next shape drift announces itself with
//!    the bytes needed to fix it instead of requiring a debugging session.
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
//! * **Quiet is not stale** (M6.1). Silence on a healthy, connected shard is the *normal*
//!   state of a 16 000-book universe: nothing traded, so nothing was pushed. Treating
//!   silence as staleness turned the cheap targeted resync into a heavier poller than the
//!   loop it replaced (a live run re-requested 16 500 books every ~90 s). A book is
//!   therefore only stale when it is explicitly marked, or when **its shard disconnected
//!   after the book's last update** — i.e. when there really is a window we could have
//!   missed. `stream.stale_after_secs` survives as the cap on how often one token may be
//!   re-requested by that path, and the slow full REST sweep remains the global net.
//!
//! ## Fallback: "the socket is broken" vs "the machine is offline" (M6.1)
//!
//! A live overnight run lost its network entirely for ~60 s: every shard took a TLS EOF at
//! once, reconnects failed with DNS errors, and the REST `/books` call failed in the same
//! moment. The old one-way switch read that as "streaming is broken" and disabled it for
//! the life of the process, even though everything recovered a minute later.
//!
//! So the switch now needs *two* facts, and [`StreamHealth::evaluate_fallback`] is only
//! allowed to trip when both hold: a streak of `fallback_after_failures` WS connect
//! failures with no live connection left, **and** at least one REST request that succeeded
//! during that same streak (proof the box has a network and it is the socket that is at
//! fault). When both transports are failing, both keep retrying on capped backoff forever
//! — a total outage is survivable and must not degrade the process permanently.
//!
//! Even a tripped fallback is no longer forever: the daemon re-probes the endpoint every
//! `stream.reprobe_interval_secs` with [`probe`] and restores streaming if it answers.

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
/// Cooldown on re-requesting a book we *hold* but have explicitly invalidated (reconnect,
/// out-of-order delta, contradictory hash). Deliberately short and not configurable: this
/// is the repair path, it fires rarely, and `stream.stale_after_secs` — which is minutes —
/// exists to rate-limit the *other* two cases (see [`BookStore::missing_or_stale`]).
const TARGETED_RESYNC_COOLDOWN_SECS: u64 = 15;
const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;
/// How many unrecognized payloads get their raw bytes logged before the sampler goes quiet.
/// Budgeted per [`BookStore`] — i.e. per stream pool, which the daemon rebuilds only when a
/// fallen-back stream is restored, so in practice this is per process.
pub const UNRECOGNIZED_SAMPLE_LIMIT: u64 = 3;
/// How much of such a payload is logged. Market data is public, so the only reason to trim
/// is log volume.
const UNRECOGNIZED_SAMPLE_CHARS: usize = 500;
/// Keepalive cadence. TODO(verify-live): the public docs mention a client keepalive on the
/// market channel; we send a WebSocket ping, which any compliant server answers.
const PING_INTERVAL_SECS: u64 = 10;
/// Ceiling on one shard's connect attempt — DNS, TCP and the WebSocket handshake together
/// (M7.1).
///
/// `connect_async` has no timeout of its own, so a resolver that never answers parks the
/// shard task forever: it neither connects nor records a failure, so the health tracker sees
/// no streak, the daemon never considers a fallback, and the shard is simply gone. A live
/// DNS outage is exactly that shape. Generous enough for a cold TLS handshake on a slow
/// link with 294 shards dialling at once, short enough that a dead endpoint returns to the
/// retry loop — where the backoff and the failure streak can do their job.
const CONNECT_TIMEOUT_SECS: u64 = 15;

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
    /// The venue's own claim about the top of book at this entry (verified: `best_bid` /
    /// `best_ask` on each `price_changes[]` entry). Informational **only** — it is a free
    /// cross-check on our locally applied book, never a source of levels. Overwriting book
    /// levels from it would invent liquidity at a price with no size attached to it.
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

impl LevelChange {
    /// Test/constructor shorthand for the common case with no top-of-book claim.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(side: Side, price: Decimal, size: Decimal) -> Self {
        Self {
            side,
            price,
            size,
            best_bid: None,
            best_ask: None,
        }
    }
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
    /// Level-wise delta for **one** asset — applied on top of the current copy. A wire
    /// frame that touches several assets of the same market fans out to one of these per
    /// asset, all carrying the parent frame's `meta`.
    PriceChange {
        asset_id: TokenId,
        changes: Vec<LevelChange>,
        meta: FrameMeta,
    },
    /// A trade print (`last_trade_price`) — **verified live, 2026-08**: a legitimate frame
    /// carrying `market`, `asset_id`, `price`, `size`, `fee_rate_bps`, `side`, `timestamp`
    /// and `transaction_hash`.
    ///
    /// Recognized and deliberately ignored. It reports what *did* trade, not what is
    /// resting, so applying it to a book would invent liquidity at a price nobody is
    /// quoting. Its own counter exists because it used to land in `frames_unrecognized`
    /// (~4 000 per 5.1 M messages in the first soak) — a permanent non-zero reading on the
    /// one counter whose job is to say "the wire format has drifted".
    TradePrint { asset_id: Option<TokenId> },
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
    /// **VERIFIED 2026-07-30** (official docs + community clients) — the real delta array:
    ///
    /// ```json
    /// {"market":"0x5f65…","event_type":"price_change","timestamp":"1757908892351",
    ///  "price_changes":[{"asset_id":"7132…","price":"0.5","size":"200","side":"BUY",
    ///                    "hash":"56621a…","best_bid":"0.5","best_ask":"1"}]}
    /// ```
    ///
    /// Note the two facts that broke the previous parser: the key is `price_changes`, not
    /// `changes`, and the `asset_id` lives on the **entry**, not the frame, so one frame
    /// can touch several assets of the same market.
    #[serde(default)]
    price_changes: Option<Vec<RawChange>>,
    /// Legacy/fallback delta array. Kept because it costs nothing and a parser that only
    /// understands one spelling is exactly how M6.5 happened.
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
    /// VERIFIED: present on every `price_changes[]` entry. Absent on the legacy `changes[]`
    /// shape, where the frame-level `asset_id` applies to all entries.
    #[serde(default, alias = "assetId")]
    asset_id: Option<String>,
    #[serde(deserialize_with = "crate::types::de_decimal")]
    price: Decimal,
    #[serde(deserialize_with = "crate::types::de_decimal")]
    size: Decimal,
    #[serde(default)]
    side: Option<String>,
    /// Informational top-of-book claims (see [`LevelChange::best_bid`]). Read through
    /// [`as_decimal`] so a number or a string both work, and a junk value is simply absent
    /// rather than fatal.
    #[serde(default)]
    best_bid: Option<Value>,
    #[serde(default)]
    best_ask: Option<Value>,
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
    let mut frames = Vec::with_capacity(items.len());
    for item in items {
        classify(item, &mut frames);
    }
    Ok(frames)
}

/// Classify one JSON element, pushing the frame(s) it describes onto `out`.
///
/// One element usually yields one frame; a `price_change` touching several assets yields
/// one per asset (see [`MarketFrame::PriceChange`]).
fn classify(item: Value, out: &mut Vec<MarketFrame>) {
    let raw: RawFrame = match serde_json::from_value(item) {
        Ok(raw) => raw,
        // The element is JSON, but not a shape we can read at all.
        Err(_) => {
            out.push(MarketFrame::Malformed {
                event_type: "?".to_string(),
                reason: "frame fields have unexpected types",
            });
            return;
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
                out.push(MarketFrame::Malformed {
                    event_type,
                    reason: "book snapshot without an asset id",
                });
                return;
            };
            out.push(MarketFrame::Book {
                asset_id,
                bids: raw.bids.or(raw.buys).unwrap_or_default(),
                asks: raw.asks.or(raw.sells).unwrap_or_default(),
                meta,
            });
        }
        "price_change" => classify_price_change(&raw, event_type, asset_id, meta, out),
        // Known and ignored on purpose — see [`MarketFrame::TradePrint`].
        "last_trade_price" => out.push(MarketFrame::TradePrint { asset_id }),
        _ => out.push(MarketFrame::Unknown {
            event_type: if event_type.is_empty() {
                "(none)".to_string()
            } else {
                event_type
            },
        }),
    }
}

/// The delta path, in shape precedence order:
///
/// 1. `price_changes[]` — the verified live shape, `asset_id` per entry;
/// 2. `changes[]` — the legacy array, `asset_id` on the frame;
/// 3. one level at the top level of the frame (`price`/`size`/`side`).
///
/// Entries are grouped by asset in first-seen order (not a `HashMap`: the order entries
/// arrive in is the order they must be applied in, and a stable order also keeps the frames
/// this produces reproducible for tests).
///
/// An entry we cannot place — unreadable side, no resolvable asset — fails the **whole**
/// frame as `Malformed` rather than applying its siblings. Guessing a side would corrupt a
/// book, and a partially applied frame is a book that is wrong in a way nothing downstream
/// can detect; a frame counted as malformed (and now raw-sampled) is one we can fix.
fn classify_price_change(
    raw: &RawFrame,
    event_type: String,
    frame_asset: Option<TokenId>,
    meta: FrameMeta,
    out: &mut Vec<MarketFrame>,
) {
    let entries = raw.price_changes.as_ref().or(raw.changes.as_ref());
    let mut grouped: Vec<(TokenId, Vec<LevelChange>)> = Vec::new();

    for entry in entries.into_iter().flatten() {
        let Some(side) = entry.side.as_deref().and_then(parse_side) else {
            out.push(MarketFrame::Malformed {
                event_type,
                reason: "price change level with an unreadable side",
            });
            return;
        };
        let asset = entry
            .asset_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(TokenId::new)
            .or_else(|| frame_asset.clone());
        let Some(asset) = asset else {
            out.push(MarketFrame::Malformed {
                event_type,
                reason: "price change level without an asset id",
            });
            return;
        };
        let change = LevelChange {
            side,
            price: entry.price,
            size: entry.size,
            best_bid: entry.best_bid.as_ref().and_then(as_decimal),
            best_ask: entry.best_ask.as_ref().and_then(as_decimal),
        };
        match grouped.iter_mut().find(|(token, _)| *token == asset) {
            Some((_, changes)) => changes.push(change),
            None => grouped.push((asset, vec![change])),
        }
    }

    if grouped.is_empty() {
        // Single-change form: the level is spread across the frame's own fields.
        let Some(asset_id) = frame_asset else {
            out.push(MarketFrame::Malformed {
                event_type,
                reason: "price change without an asset id",
            });
            return;
        };
        match (
            raw.price.as_ref().and_then(as_decimal),
            raw.size.as_ref().and_then(as_decimal),
            raw.side.as_deref().and_then(parse_side),
        ) {
            (Some(price), Some(size), Some(side)) => {
                grouped.push((asset_id, vec![LevelChange::new(side, price, size)]));
            }
            _ => {
                out.push(MarketFrame::Malformed {
                    event_type,
                    reason: "price change carried no readable level",
                });
                return;
            }
        }
    }

    // The parent timestamp dates every entry in the frame, whichever asset it belongs to.
    for (asset_id, changes) in grouped {
        out.push(MarketFrame::PriceChange {
            asset_id,
            changes,
            meta: meta.clone(),
        });
    }
}

/// Whether our top of book contradicts a `best_bid`/`best_ask` claim carried by a delta
/// entry. Only a claim we can actually check counts: an absent claim, or a side of our book
/// that is empty, disagrees for reasons that say nothing about drift.
fn disagrees(ours: Option<Decimal>, theirs: Option<Decimal>) -> bool {
    matches!((ours, theirs), (Some(o), Some(t)) if o != t)
}

/// A raw payload, trimmed for the log. Cuts on a character boundary and says how much was
/// dropped, so a truncated sample is never mistaken for a complete one.
fn truncate_payload(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}… (+{} more bytes)", &text[..cut], text.len() - cut),
        None => text.to_string(),
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
    /// Which connection is (or was last) responsible for this token. Silence only makes a
    /// book suspect if *this* shard has had a gap since the book's last update.
    shard: Option<usize>,
    /// Whether we have ever actually held a book for this token, from REST or from a
    /// snapshot frame. `false` means "asked about, never answered" — the tokens with no
    /// order book at all, which must be re-requested slowly or not at all.
    ever_had_a_book: bool,
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
            shard: None,
            ever_had_a_book: false,
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
    /// Payloads read off a socket, whatever they turned out to contain. This is the one
    /// counter that separates "the market is quiet" from "the subscription is dead": a
    /// silent shard and a quiet book are otherwise indistinguishable.
    pub messages: AtomicU64,
    pub snapshots: AtomicU64,
    pub deltas: AtomicU64,
    pub unknown_frames: AtomicU64,
    pub malformed_frames: AtomicU64,
    /// `last_trade_price` frames: recognized, never applied to a book, and explicitly *not*
    /// unrecognized. Counted so the health line can show that the channel's trade traffic is
    /// accounted for rather than silently swallowed.
    pub trade_prints: AtomicU64,
    /// Every frame that parsed as JSON but matched no shape we can use — the sum of
    /// `unknown_frames` and `malformed_frames`, surfaced as one number because *this* is
    /// the number that was silently 4.8 M during the first soak (see the module docs). It
    /// belongs on the health line next to `frames`; the split stays for detail.
    pub frames_unrecognized: AtomicU64,
    /// Individual `price_changes[]` entries that reached a book …
    pub delta_entries_applied: AtomicU64,
    /// … and those that did not (unknown asset, out-of-order frame). A healthy stream has
    /// `delta_entries_applied` climbing and this flat.
    pub delta_entries_skipped: AtomicU64,
    /// Applied deltas whose entry claimed a `best_bid`/`best_ask` our book disagreed with.
    /// A cross-check only: it never triggers a resync (the periodic REST sweep is the
    /// authority), it just says whether the local book is drifting between sweeps.
    pub delta_top_mismatch: AtomicU64,
    /// How much of the raw-sample budget ([`UNRECOGNIZED_SAMPLE_LIMIT`]) has been spent.
    unrecognized_samples: AtomicU64,
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
            messages: self.messages.load(Ordering::Relaxed),
            snapshots: self.snapshots.load(Ordering::Relaxed),
            deltas: self.deltas.load(Ordering::Relaxed),
            unknown_frames: self.unknown_frames.load(Ordering::Relaxed),
            malformed_frames: self.malformed_frames.load(Ordering::Relaxed),
            frames_unrecognized: self.frames_unrecognized.load(Ordering::Relaxed),
            delta_entries_applied: self.delta_entries_applied.load(Ordering::Relaxed),
            delta_entries_skipped: self.delta_entries_skipped.load(Ordering::Relaxed),
            delta_top_mismatch: self.delta_top_mismatch.load(Ordering::Relaxed),
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
    pub messages: u64,
    pub snapshots: u64,
    pub deltas: u64,
    pub unknown_frames: u64,
    pub malformed_frames: u64,
    pub frames_unrecognized: u64,
    pub delta_entries_applied: u64,
    pub delta_entries_skipped: u64,
    pub delta_top_mismatch: u64,
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
    /// shard index → when that connection last dropped. The only thing that can turn a
    /// *quiet* book into a suspect one.
    shard_disconnects: Mutex<HashMap<usize, Instant>>,
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

    /// Copy of the per-shard disconnect times. Always taken *before* the book lock, so the
    /// two mutexes have one global order and cannot deadlock.
    fn disconnects(&self) -> HashMap<usize, Instant> {
        self.shard_disconnects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Note which connection owns these tokens. Called on every (re)connect, because a
    /// universe re-plan can move a token to a different shard.
    pub fn assign_shard(&self, shard: usize, tokens: &[TokenId], now: Instant) {
        let mut live = self.lock();
        for token in tokens {
            live.entry(token.clone())
                .or_insert_with(|| LiveBook::placeholder(token.clone(), now))
                .shard = Some(shard);
        }
    }

    /// Record that a connection dropped. Every book on that shard whose last update
    /// predates this moment now has an unexplained gap and is due for a REST resync.
    pub fn record_shard_disconnect(&self, shard: usize, at: Instant) {
        self.shard_disconnects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(shard, at);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn shard_disconnected_at(&self, shard: usize) -> Option<Instant> {
        self.disconnects().get(&shard).copied()
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
                entry.ever_had_a_book = true;
                entry.bump(received, meta.server_ts);
                StreamStats::bump(&self.stats.snapshots);
                Some(asset_id.clone())
            }
            MarketFrame::PriceChange {
                asset_id,
                changes,
                meta,
            } => {
                let skipped = changes.len() as u64;
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
                    self.stats
                        .delta_entries_skipped
                        .fetch_add(skipped, Ordering::Relaxed);
                    return None;
                };
                // Out of order: applying a level from before the last frame would
                // overwrite newer state with older state.
                if let (Some(frame_ts), Some(last)) = (meta.server_ts, entry.last_server_ts) {
                    if frame_ts < last {
                        entry.stale = true;
                        StreamStats::bump(&self.stats.out_of_order);
                        self.stats
                            .delta_entries_skipped
                            .fetch_add(skipped, Ordering::Relaxed);
                        return None;
                    }
                }
                for change in changes {
                    entry
                        .book
                        .apply_level(change.side, change.price, change.size);
                }
                self.stats
                    .delta_entries_applied
                    .fetch_add(changes.len() as u64, Ordering::Relaxed);
                // Free integrity signal: the last entry's own claim about the top of book,
                // against what we just built. Only compared where both sides exist and the
                // book is trusted — an empty side or a stale book disagrees for reasons
                // that say nothing about drift. Counted, never acted on.
                if !entry.stale {
                    if let Some(last) = changes.last() {
                        if disagrees(entry.book.best_bid(), last.best_bid)
                            || disagrees(entry.book.best_ask(), last.best_ask)
                        {
                            StreamStats::bump(&self.stats.delta_top_mismatch);
                        }
                    }
                }
                entry.last_hash = meta.hash.clone();
                entry.bump(received, meta.server_ts);
                StreamStats::bump(&self.stats.deltas);
                Some(asset_id.clone())
            }
            MarketFrame::Unknown { .. } => {
                StreamStats::bump(&self.stats.unknown_frames);
                StreamStats::bump(&self.stats.frames_unrecognized);
                None
            }
            MarketFrame::Malformed { .. } => {
                StreamStats::bump(&self.stats.malformed_frames);
                StreamStats::bump(&self.stats.frames_unrecognized);
                None
            }
        }
    }

    /// Log the raw bytes of a payload we could not use, up to
    /// [`UNRECOGNIZED_SAMPLE_LIMIT`] times.
    ///
    /// M6.5 exists because a counter can only tell you *that* something is wrong. This
    /// tells you *what*: the next time the channel changes shape, the first few offending
    /// payloads are in the log at WARN, truncated, and the fix is a diff rather than an
    /// investigation. Market data is public — there is nothing here to redact.
    ///
    /// The budget is claimed atomically, so a burst across every shard at once still logs
    /// exactly the limit.
    pub fn sample_unrecognized(&self, shard: usize, payload: &str) {
        let claimed = self.stats.unrecognized_samples.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |spent| (spent < UNRECOGNIZED_SAMPLE_LIMIT).then_some(spent + 1),
        );
        let Ok(spent) = claimed else { return };
        tracing::warn!(
            shard,
            sample = spent + 1,
            of = UNRECOGNIZED_SAMPLE_LIMIT,
            payload = %truncate_payload(payload, UNRECOGNIZED_SAMPLE_CHARS),
            "unrecognized market-data payload — raw sample; if frames_unrecognized keeps \
             climbing, the wire format has drifted and books are not being updated"
        );
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
            entry.ever_had_a_book = true;
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

    /// How many held books are explicitly invalidated (M7 dashboard).
    ///
    /// Explicitly is the operative word: this counts books a reconnect, an out-of-order
    /// delta or a contradictory hash marked unverified. A book nobody has pushed an update
    /// for is *quiet*, not stale, and is deliberately not counted here — surfacing silence
    /// as a fault is the exact mistake M6.1 removed.
    pub fn stale_count(&self) -> usize {
        self.lock().values().filter(|b| b.stale).count()
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

    /// Tokens that need a targeted REST re-fetch.
    ///
    /// A token qualifies when it is
    ///
    /// * **never seen** — we have no book at all; or
    /// * **explicitly stale** — invalidated by a reconnect, an out-of-order delta or a
    ///   self-contradicting hash; or
    /// * **behind a shard gap** — its connection dropped after the book's last update, so
    ///   there is a window of pushes we may have missed.
    ///
    /// Silence is deliberately *not* on that list (M6.1). Most of a 16 000-book universe
    /// never trades in a given minute; re-fetching every quiet book made the "cheap"
    /// targeted path heavier than the polling loop it was supposed to replace.
    ///
    /// How often one token may be re-requested depends on *why* it is due, because the
    /// three reasons want very different rates:
    ///
    /// * **explicitly invalidated, and we do hold a book** — [`TARGETED_RESYNC_COOLDOWN_SECS`].
    ///   Rare, and repairing it promptly is the entire point: a reconnect must not have to
    ///   wait out a 15-minute window, not least because that REST call is also the evidence
    ///   the fallback decision needs.
    /// * **asked about but never answered** — `stale_after`. Some tokens have no order book
    ///   at all and never will; this is pure rate limit, and without it they would be
    ///   requested on every tick for as long as the daemon runs.
    /// * **only its shard had a gap** — `stale_after`, the batching cap, with the full REST
    ///   sweep as the real integrity net.
    pub fn missing_or_stale(
        &self,
        tokens: &[TokenId],
        now: Instant,
        stale_after: Duration,
    ) -> Vec<TokenId> {
        let prompt = Duration::from_secs(TARGETED_RESYNC_COOLDOWN_SECS).min(stale_after);
        let disconnects = self.disconnects();
        let live = self.lock();
        tokens
            .iter()
            .filter(|t| match live.get(*t) {
                None => true,
                Some(entry) => {
                    let shard_gap = entry
                        .shard
                        .and_then(|shard| disconnects.get(&shard).copied())
                        .is_some_and(|down| down >= entry.last_update);
                    if !(entry.stale || shard_gap) {
                        return false;
                    }
                    let cooldown = if entry.stale && entry.ever_had_a_book {
                        prompt
                    } else {
                        stale_after
                    };
                    entry.last_rest_attempt.is_none_or(|attempted| {
                        now.saturating_duration_since(attempted) >= cooldown
                    })
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

    /// How many of a random sample of `rest` disagree with our local copy at the top of
    /// book — the number that matters, since that is what the detectors price.
    ///
    /// Returns `(diverged, compared)`.
    ///
    /// Call this per REST *batch*, as the batch lands, and pass that batch's request start
    /// as `fetch_started`. A full sweep of the live universe takes 30–70 s, and both parts
    /// of that matter (M6.1):
    ///
    /// * comparing at the end of the sweep would judge our local book against a REST body
    ///   that is up to a minute old, and
    /// * a book we updated *while the request was in flight* legitimately disagrees with
    ///   the answer — the market moved, our copy is the newer one. Such a book is skipped
    ///   rather than counted, so the WARN means "our state has rotted", not "the market is
    ///   active".
    ///
    /// Call this *before* [`apply_rest`], which overwrites the evidence.
    pub fn count_divergence(
        &self,
        rest: &[OrderBook],
        sample: usize,
        fetch_started: Instant,
    ) -> (usize, usize) {
        let mut picked: Vec<&OrderBook> = rest.iter().collect();
        if picked.len() > sample {
            picked.shuffle(&mut rand::thread_rng());
            picked.truncate(sample);
        }
        let live = self.lock();
        let mut diverged = 0usize;
        let mut compared = 0usize;
        for theirs in picked {
            let Some(entry) = live.get(&theirs.asset_id) else {
                continue;
            };
            // A book we already know is stale is not evidence of drift.
            if entry.stale {
                continue;
            }
            // Our copy changed after the request went out: any disagreement is in-flight
            // market movement, which says nothing about whether our state is correct.
            if entry.last_update >= fetch_started {
                continue;
            }
            compared += 1;
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
        (diverged, compared)
    }
}

/// Message rate since the previous sample, as a `Decimal` (no `f64` anywhere, per the
/// project convention). Zero elapsed time yields zero rather than a division by zero.
pub fn events_per_sec(count: u64, elapsed: Duration) -> Decimal {
    let millis = Decimal::from(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
    if millis.is_zero() {
        return Decimal::ZERO;
    }
    (Decimal::from(count) * Decimal::ONE_THOUSAND / millis).round_dp(2)
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

/// Connection health, and the switch to REST-only polling.
///
/// The switch is deliberately *not* driven by WebSocket failures alone. See the module
/// docs: a total network outage fails the socket and REST at the same moment, and reading
/// that as "streaming is broken" cost a live run its stream for the rest of the night.
#[derive(Debug)]
pub struct StreamHealth {
    consecutive_failures: AtomicU32,
    fallback_after: u32,
    fallen_back: AtomicBool,
    connected: AtomicU32,
    /// At least one REST request has succeeded since this failure streak began. This is
    /// the evidence that the machine has a working network and the socket does not.
    rest_ok_during_streak: AtomicBool,
    /// Telemetry only: REST failures observed by the daemon while streaming was up.
    rest_failures: AtomicU64,
}

impl StreamHealth {
    fn new(fallback_after: u32) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            fallback_after: fallback_after.max(1),
            fallen_back: AtomicBool::new(false),
            connected: AtomicU32::new(0),
            rest_ok_during_streak: AtomicBool::new(false),
            rest_failures: AtomicU64::new(0),
        }
    }

    fn record_connected(&self) {
        self.begin_streak();
        self.connected.fetch_add(1, Ordering::Relaxed);
    }

    fn record_disconnected(&self) {
        // saturating: a task that exits after fallback must not wrap this to u32::MAX.
        let _ = self
            .connected
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
        // A drop starts a fresh streak, with fresh REST evidence: whatever REST managed
        // *before* the socket died says nothing about the network right now.
        self.begin_streak();
    }

    /// Reset the streak and the REST evidence that goes with it.
    fn begin_streak(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.rest_ok_during_streak.store(false, Ordering::SeqCst);
    }

    /// Returns the length of the current failure streak.
    fn record_connect_failure(&self) -> u32 {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The daemon reporting that a REST call went through. Only meaningful *during* a WS
    /// failure streak, which is exactly when it is consulted.
    pub fn record_rest_success(&self) {
        self.rest_ok_during_streak.store(true, Ordering::SeqCst);
    }

    /// The daemon reporting that a REST call failed. Counted for the log line; it never
    /// clears the flag, because one failed request does not unprove a working network.
    pub fn record_rest_failure(&self) {
        self.rest_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Decide whether streaming should hand over to REST polling, and trip the switch if
    /// so. Returns true only on the transition.
    ///
    /// Called from the daemon loop rather than from a socket task on purpose: the decision
    /// needs the REST evidence, which only the daemon has, and it needs it *after* this
    /// tick's REST work. All three conditions must hold:
    ///
    /// 1. the WS failure streak has reached `fallback_after_failures`;
    /// 2. no connection is currently live (one bad shard must not sink 33 good ones);
    /// 3. REST succeeded at least once during that same streak.
    ///
    /// Fail (3) — the both-down case — and we keep retrying both, indefinitely.
    pub fn evaluate_fallback(&self) -> bool {
        if self.fallen_back() {
            return false;
        }
        if self.consecutive_failures.load(Ordering::Relaxed) < self.fallback_after {
            return false;
        }
        if self.live_connections() > 0 {
            return false;
        }
        if !self.rest_ok_during_streak.load(Ordering::SeqCst) {
            return false;
        }
        !self.fallen_back.swap(true, Ordering::SeqCst)
    }

    /// True once the stream has been given up on — until a re-probe proves it works again,
    /// at which point the daemon starts a fresh pool with a fresh [`StreamHealth`].
    pub fn fallen_back(&self) -> bool {
        self.fallen_back.load(Ordering::SeqCst)
    }

    pub fn live_connections(&self) -> u32 {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rest_ok_during_streak(&self) -> bool {
        self.rest_ok_during_streak.load(Ordering::SeqCst)
    }

    pub fn rest_failures(&self) -> u64 {
        self.rest_failures.load(Ordering::Relaxed)
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
    /// never blocks the daemon. An endpoint that is down is retried forever on capped
    /// backoff; whether that ever becomes a REST-only fallback is the daemon's call, via
    /// [`StreamHealth::evaluate_fallback`], because only the daemon knows whether REST is
    /// working at the same time.
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

    /// How many shards the universe is spread across — the denominator of the dashboard's
    /// `n/m connected`. Cheap, unlike [`StreamManager::shard_tokens`], which clones every
    /// token list.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
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

        // A connect that never resolves would strand this shard silently; time it out and
        // let the ordinary failure path (streak, backoff, the daemon's fallback decision)
        // handle it like any other refusal.
        let connect = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        .unwrap_or_else(|_| {
            Err(tokio_tungstenite::tungstenite::Error::Io(
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no connection within {CONNECT_TIMEOUT_SECS}s (DNS, TCP or handshake)"),
                ),
            ))
        });
        match connect {
            Ok((socket, _response)) => {
                health.record_connected();
                attempt = 0;
                books.assign_shard(id, &tokens, Instant::now());
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
                // book on this shard is unverified until a fresh snapshot lands, and any
                // book that stays quiet from here on is suspect too.
                books.record_shard_disconnect(id, Instant::now());
                books.mark_stale(&tokens);
                StreamStats::bump(&books.stats.reconnects);
                if *shutdown.borrow() {
                    return;
                }
            }
            Err(err) => {
                StreamStats::bump(&books.stats.connect_failures);
                let streak = health.record_connect_failure();
                // Never a permanent decision from here: this task cannot tell a broken
                // socket from a broken network. The daemon owns that call (it has the REST
                // evidence) — see `StreamHealth::evaluate_fallback`.
                tracing::warn!(
                    shard = id,
                    %url,
                    %err,
                    attempt,
                    streak,
                    "market stream connect failed — retrying with backoff"
                );
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

/// One-off reachability probe, used to decide whether a fallen-back stream can be restored.
///
/// Deliberately *not* part of the pool: it opens a connection, subscribes to nothing, and
/// closes again, so a probe against a live endpoint costs one handshake and cannot disturb
/// the REST path that is currently carrying detection.
pub async fn probe(url: &str, timeout: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url)).await {
        Ok(Ok((mut socket, _response))) => {
            let _ = socket.close(None).await;
            Ok(())
        }
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err(format!("no answer within {} s", timeout.as_secs())),
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
    StreamStats::bump(&books.stats.messages);
    let frames = match parse_frames(text) {
        Ok(frames) => frames,
        Err(err) => {
            StreamStats::bump(&books.stats.malformed_frames);
            StreamStats::bump(&books.stats.frames_unrecognized);
            tracing::debug!(shard = id, %err, "unparseable market frame ignored");
            books.sample_unrecognized(id, text);
            return true;
        }
    };
    // One sample per *payload*, not per frame: the payload is the thing whose shape we got
    // wrong, and it is what a reader needs to see.
    if frames.iter().any(|f| {
        matches!(
            f,
            MarketFrame::Unknown { .. } | MarketFrame::Malformed { .. }
        )
    }) {
        books.sample_unrecognized(id, text);
    }
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
                assert_eq!(changes[0], LevelChange::new(Side::Ask, dec!(0.41), dec!(0)));
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

    /// The verified live delta frame, **verbatim** from the official docs (2026-07-30) —
    /// the shape whose `price_changes` key this parser used to miss entirely. Kept byte-for
    /// byte so a future edit that "tidies" it has to notice it is copying a wire capture.
    const LIVE_DELTA: &str = r#"{
  "market": "0x5f65...f8f1",
  "price_changes": [
    {
      "asset_id": "71321045679252212594626385532706912750332728571942532289631379312455583992563",
      "price": "0.5",
      "size": "200",
      "side": "BUY",
      "hash": "56621a121a47ed9333273e21c83b660cff37ae50",
      "best_bid": "0.5",
      "best_ask": "1"
    }
  ],
  "timestamp": "1757908892351",
  "event_type": "price_change"
}"#;

    const LIVE_ASSET: &str =
        "71321045679252212594626385532706912750332728571942532289631379312455583992563";

    /// A hand-built snapshot for `asset`, so a delta test has a book to change.
    fn snapshot_for(asset: &str, bids: &str, asks: &str, ts: i64) -> String {
        format!(
            r#"{{"event_type":"book","asset_id":"{asset}","timestamp":"{ts}",
                "bids":[{bids}],"asks":[{asks}]}}"#
        )
    }

    /// (a) The frame the venue actually sends parses, routes to the asset named *inside*
    /// the entry, and moves the book. This is the M6.5 regression: 4.8 M of these were
    /// received and none applied, because the parser wanted `changes[]` and a frame-level
    /// `asset_id`.
    #[test]
    fn the_verbatim_live_delta_frame_parses_and_applies() {
        let asset = tok_id(LIVE_ASSET);
        let frames = parse_frames(LIVE_DELTA).expect("the live delta must parse");
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            MarketFrame::PriceChange {
                asset_id,
                changes,
                meta,
            } => {
                assert_eq!(asset_id, &asset, "routed by the entry's own asset_id");
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].side, Side::Bid, "BUY is the bid side");
                assert_eq!(changes[0].price, dec!(0.5));
                assert_eq!(changes[0].size, dec!(200));
                assert_eq!(changes[0].best_bid, Some(dec!(0.5)));
                assert_eq!(changes[0].best_ask, Some(dec!(1)));
                assert_eq!(
                    meta.server_ts,
                    DateTime::from_timestamp_millis(1_757_908_892_351),
                    "the parent timestamp dates the entry"
                );
            }
            other => panic!("expected a price change, got {other:?}"),
        }

        // Before: bids 0.49/100 ; asks 1/10.
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(&snapshot_for(
            LIVE_ASSET,
            r#"{"price":"0.49","size":"100"}"#,
            r#"{"price":"1","size":"10"}"#,
            1_757_908_892_000i64,
        )) {
            store.apply_frame(&frame, now);
        }
        let before = &store.snapshot_of(std::slice::from_ref(&asset))[&asset];
        assert_eq!(before.best_bid(), Some(dec!(0.49)));
        assert_eq!(before.bids.len(), 1);

        for frame in &frames {
            assert_eq!(store.apply_frame(frame, now).as_ref(), Some(&asset));
        }

        // After: 0.5/200 is set on the bid side and becomes the top; nothing else moved.
        let after = &store.snapshot_of(std::slice::from_ref(&asset))[&asset];
        assert_eq!(
            after.bids,
            vec![
                PriceLevel::new(dec!(0.5), dec!(200)),
                PriceLevel::new(dec!(0.49), dec!(100)),
            ]
        );
        assert_eq!(after.asks, vec![PriceLevel::new(dec!(1), dec!(10))]);

        let stats = store.stats.snapshot();
        assert_eq!(stats.deltas, 1);
        assert_eq!(stats.delta_entries_applied, 1);
        assert_eq!(stats.delta_entries_skipped, 0);
        assert_eq!(stats.frames_unrecognized, 0);
        assert_eq!(
            stats.delta_top_mismatch, 0,
            "the entry's best_bid/best_ask agree with the book we built"
        );
    }

    /// The cross-check earns its keep only if it can fire. It counts and stops there — the
    /// periodic REST sweep stays the authority on what a book really is.
    #[test]
    fn a_top_of_book_claim_we_contradict_is_counted_but_not_acted_on() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(&snapshot_for(
            "1001",
            r#"{"price":"0.49","size":"100"}"#,
            r#"{"price":"0.52","size":"50"}"#,
            1_757_908_892_000i64,
        )) {
            store.apply_frame(&frame, now);
        }
        // We apply 0.50/200 to the bid side; the venue claims the best ask is 0.90, which
        // our copy (0.52) disagrees with.
        let delta = r#"{"event_type":"price_change","timestamp":"1757908892351","price_changes":[
            {"asset_id":"1001","price":"0.50","size":"200","side":"BUY",
             "best_bid":"0.50","best_ask":"0.90"}]}"#;
        for frame in frames_of(delta) {
            store.apply_frame(&frame, now);
        }
        let stats = store.stats.snapshot();
        assert_eq!(stats.delta_top_mismatch, 1);
        assert_eq!(stats.delta_entries_applied, 1, "the level still landed");
        assert_eq!(
            store.is_stale(&tok_id("1001")),
            Some(false),
            "a mismatch never invalidates the book by itself"
        );
    }

    /// (b) One market's frame may carry entries for several of its assets — that is the
    /// whole reason `asset_id` moved onto the entry. Each must reach its own book, and each
    /// must produce its own touch so the detector wakes for both events.
    #[test]
    fn one_frame_updates_every_asset_it_touches() {
        let store = BookStore::new();
        let now = Instant::now();
        for asset in ["1001", "2002"] {
            for frame in frames_of(&snapshot_for(
                asset,
                r#"{"price":"0.30","size":"10"}"#,
                r#"{"price":"0.70","size":"10"}"#,
                1_757_908_892_000i64,
            )) {
                store.apply_frame(&frame, now);
            }
        }

        let delta = r#"{"market":"0xabc","event_type":"price_change","timestamp":"1757908892351",
            "price_changes":[
                {"asset_id":"1001","price":"0.31","size":"55","side":"BUY"},
                {"asset_id":"2002","price":"0.69","size":"77","side":"SELL"},
                {"asset_id":"1001","price":"0.72","size":"12","side":"SELL"}
            ]}"#;
        let frames = frames_of(delta);
        assert_eq!(frames.len(), 2, "one frame per asset, in first-seen order");

        let touched: Vec<TokenId> = frames
            .iter()
            .filter_map(|f| store.apply_frame(f, now))
            .collect();
        assert_eq!(touched, vec![tok_id("1001"), tok_id("2002")]);

        let books = store.snapshot_of(&[tok_id("1001"), tok_id("2002")]);
        let one = &books[&tok_id("1001")];
        assert_eq!(one.best_bid(), Some(dec!(0.31)), "new best bid");
        assert_eq!(
            one.asks,
            vec![
                PriceLevel::new(dec!(0.70), dec!(10)),
                PriceLevel::new(dec!(0.72), dec!(12)),
            ],
            "both of this asset's entries applied, in order"
        );
        let two = &books[&tok_id("2002")];
        assert_eq!(
            two.asks,
            vec![
                PriceLevel::new(dec!(0.69), dec!(77)),
                PriceLevel::new(dec!(0.70), dec!(10)),
            ],
            "SELL is the ask side, and 0.69 sorts in front of the untouched 0.70"
        );
        assert_eq!(two.bids, vec![PriceLevel::new(dec!(0.30), dec!(10))]);

        let stats = store.stats.snapshot();
        assert_eq!(stats.deltas, 2);
        assert_eq!(stats.delta_entries_applied, 3);
        assert_eq!(stats.delta_entries_skipped, 0);
    }

    /// (c) Size "0" is how the channel says "this price is gone".
    #[test]
    fn a_zero_size_entry_removes_the_level() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(&snapshot_for(
            "1001",
            r#"{"price":"0.49","size":"100"},{"price":"0.48","size":"20"}"#,
            r#"{"price":"0.52","size":"50"}"#,
            1_757_908_892_000i64,
        )) {
            store.apply_frame(&frame, now);
        }
        let delta = r#"{"event_type":"price_change","timestamp":"1757908892351","price_changes":[
            {"asset_id":"1001","price":"0.49","size":"0","side":"BUY"}]}"#;
        for frame in frames_of(delta) {
            store.apply_frame(&frame, now);
        }
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.bids, vec![PriceLevel::new(dec!(0.48), dec!(20))]);
        assert_eq!(book.best_bid(), Some(dec!(0.48)));
        assert_eq!(store.stats.snapshot().delta_entries_applied, 1);
    }

    /// (d) An entry for an asset we hold no book for cannot be applied — but it must not
    /// take its siblings down with it, and the fact that it was dropped has to be visible.
    #[test]
    fn an_entry_for_an_unknown_asset_is_skipped_without_dropping_the_frame() {
        let store = BookStore::new();
        let now = Instant::now();
        for frame in frames_of(&snapshot_for(
            "1001",
            r#"{"price":"0.49","size":"100"}"#,
            r#"{"price":"0.52","size":"50"}"#,
            1_757_908_892_000i64,
        )) {
            store.apply_frame(&frame, now);
        }
        let delta = r#"{"event_type":"price_change","timestamp":"1757908892351","price_changes":[
            {"asset_id":"9999","price":"0.10","size":"5","side":"BUY"},
            {"asset_id":"9999","price":"0.90","size":"5","side":"SELL"},
            {"asset_id":"1001","price":"0.50","size":"200","side":"BUY"}
        ]}"#;
        let frames = frames_of(delta);
        assert_eq!(frames.len(), 2);
        let touched: Vec<TokenId> = frames
            .iter()
            .filter_map(|f| store.apply_frame(f, now))
            .collect();
        assert_eq!(touched, vec![tok_id("1001")], "only the known asset moved");

        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.best_bid(), Some(dec!(0.50)), "the good entry applied");

        let stats = store.stats.snapshot();
        assert_eq!(stats.delta_entries_applied, 1);
        assert_eq!(stats.delta_entries_skipped, 2, "both unknown-asset entries");
        assert_eq!(stats.orphan_deltas, 1);
        assert_eq!(
            stats.frames_unrecognized, 0,
            "an unroutable entry is not a shape we failed to understand"
        );
        // …and the unknown asset is remembered so the resync sweep fetches it over REST.
        assert_eq!(store.is_stale(&tok_id("9999")), Some(true));
    }

    /// (e) The shapes we accepted before M6.5 still work. They cost nothing to keep, and a
    /// parser that understands exactly one spelling is how this bug happened.
    #[test]
    fn the_legacy_delta_shapes_remain_accepted() {
        // `changes[]` with the asset on the frame.
        let legacy = r#"{"event_type":"price_change","asset_id":"1001","timestamp":"1757908892351",
            "changes":[{"price":"0.41","size":"7","side":"SELL"}]}"#;
        match &frames_of(legacy)[0] {
            MarketFrame::PriceChange {
                asset_id, changes, ..
            } => {
                assert_eq!(asset_id, &tok_id("1001"));
                assert_eq!(changes[0], LevelChange::new(Side::Ask, dec!(0.41), dec!(7)));
            }
            other => panic!("expected a price change, got {other:?}"),
        }

        // The single-level form, fields at the top of the frame.
        let single =
            r#"{"event_type":"price_change","asset_id":"7","price":0.5,"size":"12","side":"buy"}"#;
        match &frames_of(single)[0] {
            MarketFrame::PriceChange { changes, .. } => {
                assert_eq!(changes[0], LevelChange::new(Side::Bid, dec!(0.5), dec!(12)));
            }
            other => panic!("expected a price change, got {other:?}"),
        }

        // Both arrays present: the verified one wins.
        let both = r#"{"event_type":"price_change","asset_id":"1001",
            "price_changes":[{"asset_id":"2002","price":"0.6","size":"1","side":"BUY"}],
            "changes":[{"price":"0.4","size":"9","side":"BUY"}]}"#;
        match &frames_of(both)[0] {
            MarketFrame::PriceChange {
                asset_id, changes, ..
            } => {
                assert_eq!(asset_id, &tok_id("2002"));
                assert_eq!(changes[0].size, dec!(1));
            }
            other => panic!("expected a price change, got {other:?}"),
        }
    }

    /// (f) The diagnosability half of M6.5. A frame we cannot use is counted *and* the
    /// first few are dumped raw, so the next shape drift arrives in the log with the bytes
    /// needed to fix it. The dump is budgeted: an 8 500 frames/s stream must not write the
    /// whole market to disk.
    #[tokio::test]
    async fn unrecognized_payloads_are_counted_and_the_first_few_are_logged_raw() {
        let books = Arc::new(BookStore::new());
        let (touches, _rx) = mpsc::channel(64);
        // Exactly the class of payload that broke us: a `price_change` whose level array is
        // under a key this build has never heard of.
        let payload = r#"{"event_type":"price_change","market":"0xabc",
            "some_future_key":[{"asset_id":"1001","price":"0.5","size":"200","side":"BUY"}]}"#;
        let bursts = UNRECOGNIZED_SAMPLE_LIMIT + 5;

        let capture = crate::testlog::LogCapture::new();
        {
            let _guard = capture.install(tracing::Level::WARN);
            for _ in 0..bursts {
                assert!(
                    handle_payload(3, payload, &books, &touches, Instant::now()).await,
                    "an unusable payload must never kill the reader"
                );
            }
        }
        let log = capture.text();

        assert_eq!(
            log.matches("unrecognized market-data payload").count(),
            UNRECOGNIZED_SAMPLE_LIMIT as usize,
            "the raw sample is rate limited to the budget:\n{log}"
        );
        assert!(
            log.contains("some_future_key"),
            "the sample must carry the raw payload:\n{log}"
        );
        assert!(
            log.contains("shard=3"),
            "…and say where it came from:\n{log}"
        );

        let stats = books.stats.snapshot();
        assert_eq!(stats.frames_unrecognized, bursts, "every one is counted");
        assert_eq!(stats.malformed_frames, bursts);
        assert_eq!(stats.messages, bursts);
        assert_eq!(stats.delta_entries_applied, 0);
    }

    #[test]
    fn a_sampled_payload_is_truncated_on_a_character_boundary() {
        let long = format!("{}€tail", "x".repeat(600));
        let cut = truncate_payload(&long, UNRECOGNIZED_SAMPLE_CHARS);
        assert!(cut.starts_with(&"x".repeat(500)));
        assert!(cut.contains("more bytes"), "{cut}");
        // A payload that fits is passed through untouched.
        assert_eq!(
            truncate_payload("short", UNRECOGNIZED_SAMPLE_CHARS),
            "short"
        );
        // Multi-byte characters must not be sliced in half.
        assert_eq!(truncate_payload("€€€", 2), "€€… (+3 more bytes)");
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
    fn staleness_covers_never_seen_books() {
        let store = BookStore::new();
        let t0 = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, t0);
        }
        let watched = vec![tok_id("1001"), tok_id("2002")];
        // 2002 was never seen at all; 1001 was just snapshotted.
        assert_eq!(
            store.missing_or_stale(&watched, t0, Duration::from_secs(900)),
            vec![tok_id("2002")]
        );
    }

    /// The M6.1 regression this milestone exists for. With staleness driven by silence, a
    /// live run re-requested 16 500 books over REST every ~90 s — every quiet book in the
    /// universe, on a loop, because nothing had traded in them. Silence on a healthy shard
    /// must cost nothing; only a shard that actually dropped makes its quiet books suspect.
    #[test]
    fn a_quiet_book_is_only_resynced_when_its_shard_had_a_gap() {
        let store = BookStore::new();
        let t0 = Instant::now();
        let quiet = tok_id("1001");
        let watched = vec![quiet.clone()];
        let window = Duration::from_secs(900);

        store.assign_shard(0, &watched, t0);
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, t0);
        }
        assert_eq!(store.is_stale(&quiet), Some(false));

        // An hour of total silence on a connected shard: nothing traded, nothing to do.
        for age_secs in [61, 901, 3_600] {
            assert!(
                store
                    .missing_or_stale(&watched, t0 + Duration::from_secs(age_secs), window)
                    .is_empty(),
                "a quiet book on a healthy shard must not be resynced after {age_secs} s"
            );
        }

        // Its shard drops: now there is a window of pushes we cannot account for.
        let down = t0 + Duration::from_secs(1_000);
        store.record_shard_disconnect(0, down);
        assert_eq!(store.shard_disconnected_at(0), Some(down));
        assert_eq!(
            store.missing_or_stale(&watched, down + Duration::from_secs(1), window),
            watched,
            "a book whose shard dropped after its last update must be resynced"
        );

        // A fresh update after the drop closes the gap again.
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, down + Duration::from_secs(2));
        }
        assert!(
            store
                .missing_or_stale(&watched, down + Duration::from_secs(3), window)
                .is_empty(),
            "an update newer than the disconnect is evidence the shard recovered"
        );

        // A book on a *different*, healthy shard is untouched by shard 0's trouble.
        let other = tok_id("7777");
        store.assign_shard(1, std::slice::from_ref(&other), t0);
        store.apply_rest(
            std::slice::from_ref(&other),
            &[(other.clone(), OrderBook::new(other.clone(), vec![], vec![]))]
                .into_iter()
                .collect(),
            t0,
        );
        assert!(store
            .missing_or_stale(&[other], down + Duration::from_secs(3), window)
            .is_empty());
    }

    /// The three reasons a book can be due are rate-limited differently on purpose: a
    /// reconnect must be repaired in seconds (that REST call is also the evidence the
    /// fallback decision needs), while a token the API has no book for must be left alone.
    #[test]
    fn an_invalidated_book_is_repaired_promptly_and_a_bookless_token_is_not() {
        let store = BookStore::new();
        let t0 = Instant::now();
        let real = tok_id("1001");
        let ghost = tok_id("ghost");
        let watched = vec![real.clone(), ghost.clone()];
        let window = Duration::from_secs(900);

        // We asked about both; only `real` came back.
        store.apply_rest(
            &watched,
            &[(
                real.clone(),
                OrderBook::new(
                    real.clone(),
                    vec![],
                    vec![PriceLevel::new(dec!(0.4), dec!(1))],
                ),
            )]
            .into_iter()
            .collect(),
            t0,
        );

        // A reconnect invalidates the book we hold. It is re-fetched after the short
        // repair cooldown, not after the 15-minute batching window.
        store.mark_stale(std::slice::from_ref(&real));
        assert!(
            store
                .missing_or_stale(&watched, t0 + Duration::from_secs(5), window)
                .is_empty(),
            "not instantly — a burst of frames must not turn into a burst of requests"
        );
        assert_eq!(
            store.missing_or_stale(
                &watched,
                t0 + Duration::from_secs(TARGETED_RESYNC_COOLDOWN_SECS + 1),
                window
            ),
            vec![real.clone()],
            "an invalidated book is repaired promptly; the bookless token waits"
        );

        // The ghost only comes back round on the long window.
        assert_eq!(
            store.missing_or_stale(&watched, t0 + Duration::from_secs(901), window),
            watched
        );

        // And a `stale_after` shorter than the repair cooldown wins — the configured value
        // is never exceeded.
        assert!(store
            .missing_or_stale(
                &watched,
                t0 + Duration::from_secs(5),
                Duration::from_secs(2)
            )
            .contains(&real));
    }

    #[test]
    fn rest_is_authoritative_and_divergence_is_counted_before_it_lands() {
        let store = BookStore::new();
        let applied = Instant::now();
        for frame in frames_of(SNAPSHOT) {
            store.apply_frame(&frame, applied);
        }
        // The request went out after our copy was last touched, so a disagreement is ours.
        let requested_at = applied + Duration::from_millis(1);
        let rest = vec![OrderBook::new(
            tok_id("1001"),
            vec![PriceLevel::new(dec!(0.39), dec!(500))],
            vec![PriceLevel::new(dec!(0.42), dec!(300))], // we think 0.41
        )
        .normalized()];

        assert_eq!(
            store.count_divergence(&rest, DIVERGENCE_SAMPLE, requested_at),
            (1, 1)
        );
        let returned_at = requested_at + Duration::from_millis(1);
        store.apply_rest(
            &[tok_id("1001")],
            &rest
                .iter()
                .map(|b| (b.asset_id.clone(), b.clone()))
                .collect(),
            returned_at,
        );
        let book = &store.snapshot_of(&[tok_id("1001")])[&tok_id("1001")];
        assert_eq!(book.best_ask(), Some(dec!(0.42)));
        // Adopting the REST view also means our copy is now newer than that request, so the
        // same comparison is no longer even attempted.
        assert_eq!(
            store.count_divergence(&rest, DIVERGENCE_SAMPLE, requested_at),
            (0, 0)
        );
        assert_eq!(store.stats.snapshot().divergences, 1);
    }

    /// The live WARN fired every sweep with 5–15 of 64 sampled "diverged", and part of that
    /// was simply the market moving during a 30–70 s sweep. A book we updated *after* the
    /// request went out is newer than the answer: it is not evidence our state has rotted,
    /// so it must not be counted (nor sampled).
    #[test]
    fn divergence_excludes_books_updated_after_the_fetch_started() {
        let store = BookStore::new();
        let t0 = Instant::now();
        let requested_at = t0 + Duration::from_secs(1);

        // `settled` was last touched before the request; `moving` while it was in flight.
        for (id, at) in [
            ("1001", t0),
            ("2002", requested_at + Duration::from_millis(5)),
        ] {
            let snapshot =
                SNAPSHOT.replace("\"asset_id\": \"1001\"", &format!("\"asset_id\": \"{id}\""));
            for frame in frames_of(&snapshot) {
                store.apply_frame(&frame, at);
            }
        }

        // REST disagrees about both (0.42 where we hold 0.41).
        let rest: Vec<OrderBook> = ["1001", "2002"]
            .into_iter()
            .map(|id| {
                OrderBook::new(
                    tok_id(id),
                    vec![PriceLevel::new(dec!(0.39), dec!(500))],
                    vec![PriceLevel::new(dec!(0.42), dec!(300))],
                )
                .normalized()
            })
            .collect();

        assert_eq!(
            store.count_divergence(&rest, DIVERGENCE_SAMPLE, requested_at),
            (1, 1),
            "only the book that was already settled when the request went out counts"
        );
        assert_eq!(store.stats.snapshot().divergences, 1);

        // With the request start moved past both updates, both are fair game again.
        let later = requested_at + Duration::from_secs(1);
        assert_eq!(
            store.count_divergence(&rest, DIVERGENCE_SAMPLE, later),
            (2, 2)
        );
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
        // Once the window has passed the ghost is retried — we do not give up on it — but
        // the book we successfully fetched stays quiet and stays alone.
        assert_eq!(
            store.missing_or_stale(&watched, t0 + Duration::from_secs(61), window),
            vec![tok_id("ghost")]
        );

        // A reconnect still forces a resync of a book we do have, because the last REST
        // attempt for it is outside the window by then.
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
        health.record_rest_success(); // REST works; the socket is the suspect
        assert_eq!(health.record_connect_failure(), 1);
        assert!(!health.evaluate_fallback());
        assert_eq!(health.record_connect_failure(), 2);
        assert!(!health.evaluate_fallback());
        assert!(!health.fallen_back());
        assert_eq!(health.record_connect_failure(), 3);
        assert!(health.evaluate_fallback(), "the third failure trips it");
        assert!(health.fallen_back());
        assert!(
            !health.evaluate_fallback(),
            "the switch only reports tripping once"
        );

        // A success resets the streak.
        let health = StreamHealth::new(2);
        health.record_rest_success();
        health.record_connect_failure();
        health.record_connected();
        health.record_rest_success();
        health.record_connect_failure();
        assert!(
            !health.evaluate_fallback(),
            "the streak was broken by a success"
        );
        assert!(!health.fallen_back());
    }

    /// Issue 1 of M6.1, in isolation. At 21:26 on the live box the network vanished: every
    /// shard took a TLS EOF, reconnects failed with DNS errors, *and* the REST `/books`
    /// call failed in the same moment. The old switch read that as "streaming is broken"
    /// and disabled it for the rest of the process, though the network was back in ~60 s.
    #[test]
    fn a_total_outage_never_trips_the_fallback_but_a_dead_socket_does() {
        let health = StreamHealth::new(3);

        // Normal operation: connected, and REST is fine.
        health.record_connected();
        health.record_rest_success();
        assert_eq!(health.live_connections(), 1);

        // 21:26 — the socket drops and every retry fails, with REST failing too.
        health.record_disconnected();
        assert!(
            !health.rest_ok_during_streak(),
            "REST evidence from before the drop says nothing about the network now"
        );
        for _ in 0..20 {
            health.record_connect_failure();
            health.record_rest_failure();
            assert!(
                !health.evaluate_fallback(),
                "both transports down is an outage: keep retrying, never degrade"
            );
        }
        assert!(!health.fallen_back());
        assert_eq!(health.rest_failures(), 20);

        // ~60 s later the network is back. REST proves it; the socket reconnects too.
        health.record_rest_success();
        health.record_connected();
        assert!(!health.evaluate_fallback());
        assert!(!health.fallen_back(), "the process must still be streaming");

        // Now the *socket specifically* breaks: it drops and will not come back, while
        // REST keeps answering. That is the case the fallback exists for.
        health.record_disconnected();
        for _ in 0..3 {
            health.record_connect_failure();
        }
        health.record_rest_success();
        assert!(health.evaluate_fallback());
        assert!(health.fallen_back());
    }

    /// One broken shard must not take the other 33 down with it.
    #[test]
    fn a_live_connection_anywhere_blocks_the_fallback() {
        let health = StreamHealth::new(2);
        health.record_connected(); // shard 0 is happily connected
        health.record_rest_success();
        for _ in 0..10 {
            health.record_connect_failure();
        }
        assert!(
            !health.evaluate_fallback(),
            "some shard is receiving data — streaming is not what is broken"
        );
        assert_eq!(health.live_connections(), 1);
    }

    #[test]
    fn events_per_sec_is_a_decimal_rate_and_survives_a_zero_interval() {
        assert_eq!(events_per_sec(500, Duration::from_secs(10)), dec!(50));
        assert_eq!(events_per_sec(3, Duration::from_secs(2)), dec!(1.5));
        assert_eq!(events_per_sec(1, Duration::from_millis(300)), dec!(3.33));
        assert_eq!(events_per_sec(0, Duration::from_secs(300)), Decimal::ZERO);
        assert_eq!(events_per_sec(9, Duration::ZERO), Decimal::ZERO);
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
        tokio::spawn(serve_subscriptions(listener, tx));
        (format!("ws://{addr}/ws/market"), rx)
    }

    /// Accept forever, reporting the token list of every subscribe frame received.
    async fn serve_subscriptions(listener: tokio::net::TcpListener, tx: mpsc::Sender<Vec<String>>) {
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

    /// Wait until `health` has seen `want` consecutive connect failures, or give up.
    async fn wait_for_failures(health: &StreamHealth, want: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while health.consecutive_failures() < want && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            health.consecutive_failures() >= want,
            "expected at least {want} connect failures, got {}",
            health.consecutive_failures()
        );
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_falls_back_only_once_rest_is_shown_to_work() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut cfg = Config::default();
        // Port 1 refuses connections immediately.
        cfg.stream.url = "ws://127.0.0.1:1/ws/market".into();
        cfg.stream.fallback_after_failures = 2;

        let manager = StreamManager::start(&cfg, &[tok(1)], shutdown_rx);
        let health = manager.health().clone();
        wait_for_failures(&health, 2).await;

        // No REST evidence yet: the socket may be down because the whole box is.
        assert!(
            !health.evaluate_fallback(),
            "a failing socket alone must not disable streaming"
        );
        assert!(!health.fallen_back());

        // The daemon reports a REST call that went through — the network is fine and the
        // endpoint is not.
        health.record_rest_success();
        assert!(health.evaluate_fallback());
        assert!(health.fallen_back());
        assert!(manager.books().stats.snapshot().connect_failures >= 2);
        assert_eq!(health.live_connections(), 0);
        manager.stop().await;
    }

    /// The other half of issue 1: a socket that comes back must be used again. The pool
    /// keeps retrying through the outage, so no restart (and no re-probe) is needed for the
    /// window where the daemon has not handed over yet.
    #[tokio::test]
    async fn the_pool_reconnects_by_itself_when_the_endpoint_returns() {
        // Reserve a port, then release it: connections are refused until we bind again.
        let addr = {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            probe.local_addr().expect("addr")
        };
        let url = format!("ws://{addr}/ws/market");

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut cfg = Config::default();
        cfg.stream.url = url.clone();
        cfg.stream.max_subs_per_connection = 1;
        cfg.stream.fallback_after_failures = 2;

        let manager = StreamManager::start(&cfg, &[tok(1)], shutdown_rx);
        let health = manager.health().clone();
        wait_for_failures(&health, 2).await;
        assert!(
            !health.fallen_back(),
            "nothing may disable streaming without the daemon's REST evidence"
        );

        // The "network" comes back on the same address.
        let listener = tokio::net::TcpListener::bind(addr).await.expect("re-bind");
        let (tx, mut subscriptions) = mpsc::channel(8);
        tokio::spawn(serve_subscriptions(listener, tx));

        assert_eq!(
            next_subscription(&mut subscriptions).await,
            vec!["t1".to_string()],
            "the pool must resubscribe by itself once the endpoint answers"
        );
        assert!(health.live_connections() >= 1);
        assert!(!health.fallen_back());
        manager.stop().await;
    }

    #[tokio::test]
    async fn the_probe_reports_reachability_both_ways() {
        // Nothing listening: an error, and never a panic or a hang.
        let err = probe("ws://127.0.0.1:1/ws/market", Duration::from_secs(5))
            .await
            .expect_err("a refused port must not look reachable");
        assert!(!err.is_empty());

        // A live channel answers, and the probe leaves no pool behind.
        let (url, _subscriptions) = mock_channel().await;
        probe(&url, Duration::from_secs(5))
            .await
            .expect("a live endpoint must probe clean");
    }

    #[test]
    fn the_subscribe_frame_names_every_token() {
        let message = subscribe_message(&tokens(3));
        let parsed: Value = serde_json::from_str(&message).expect("valid JSON");
        assert_eq!(parsed["type"], "market");
        assert_eq!(parsed["assets_ids"], Value::from(vec!["t0", "t1", "t2"]));
    }
}
