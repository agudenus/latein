//! Gamma API client — market discovery.
//!
//! `GET https://gamma-api.polymarket.com/events/keyset?active=true&closed=false` returns
//! events, each with a `negRisk` flag and a nested `markets[]` array. We keep NegRisk events
//! (the primary strategy) and the binary markets inside every event.
//!
//! ## Why keyset, not `offset`
//!
//! The legacy `/events` list endpoint caps how deep `offset` may go. Past roughly 2 000
//! events it answers
//! `HTTP 422 {"error":"offset too large, use /events/keyset for deeper pagination"}` —
//! which is exactly what killed the owner's live daemon the night `scan.max_events` was
//! raised from 2 000 to 6 000. Polymarket also deprecates the legacy `/events` and
//! `/markets` list endpoints (2026-05-01) in favour of the cursor-based keyset ones.
//!
//! So: keyset is the primary path, offset pagination survives only as a fallback for a
//! deployment where `/events/keyset` is not there yet (404/405), and in *neither* path is a
//! pagination cap treated as an error — it truncates the universe loudly and returns what
//! it has. Market data is not allowed to kill the process.
//!
//! Parsing is deliberately permissive: unknown fields are ignored, every field we do not
//! strictly need is optional, and prices/ids arrive as strings. The few wire details still
//! marked `TODO(verify-live)` are inferred from the public docs and must be confirmed
//! against a real *request* (this container cannot reach the API); the response shape is no
//! longer among them — see below.
//!
//! ## Verified against the live response, 2026-07-30
//!
//! The owner fetched `GET /events/keyset?active=true&closed=false&limit=2` in a browser and
//! supplied the body. It settles the envelope, the cursor and the event shape:
//!
//! ```text
//! {"$schema": "…/EventsKeysetListResponse.json",
//!  "events": [ …event objects, same shape as the legacy /events… ],
//!  "next_cursor": "<opaque>"}
//! ```
//!
//! The array key is **`events`**, not `data` — and because this parser accepted only a bare
//! array or `{"data": …}`, every keyset page failed to decode, discovery produced nothing,
//! and the daemon retried forever. That is the bug this module's `events` arm fixes.
//!
//! The same response also carries per-market fee data (`feesEnabled`, `feeType`,
//! `feeSchedule`), which is now preferred over our category fee table; see
//! [`crate::types::MarketFees`].
//!
//! What that evidence does **not** settle, and so is still `TODO(verify-live)`: the name of
//! the query parameter that carries the cursor *back* (the capture was a first page with no
//! cursor — see [`DEFAULT_KEYSET_CURSOR_PARAM`]), and the full tag vocabulary behind
//! [`Category::from_text`] (two events is not a taxonomy).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;

use crate::config::Config;
use crate::http::{ApiError, HttpClient};
use crate::types::{
    de_opt_decimal, Category, MarketFees, MarketTrading, TokenId, TrackedEvent, TrackedMarket,
    Universe,
};

/// What the discovery pass kept and dropped — printed by `polyarb markets` and logged on
/// every universe refresh so the tracked universe is auditable rather than a black box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryStats {
    pub events_seen: usize,
    pub events_kept: usize,
    pub events_inactive: usize,
    /// Events that survived the active/closed filter but had no priceable market left.
    pub events_no_usable_market: usize,
    /// Every market Gamma listed, across every event, before any filtering.
    pub markets_seen: usize,
    pub markets_kept: usize,
    /// Why the rest were dropped. `markets_seen == markets_kept + drops.total()`.
    pub drops: DropCounts,
    /// True when pagination stopped before the end of the event list — `scan.max_events`,
    /// the API's own offset-depth cap (HTTP 422), or a keyset cursor that stopped
    /// advancing. Discovery is incomplete and the numbers above are a lower bound; the log
    /// line at the point of truncation says which of the three it was.
    pub truncated: bool,
    /// Kept markets whose taker fee rate came from Gamma's own `feeSchedule` /
    /// `feesEnabled` rather than from our category table.
    pub markets_with_api_fee: usize,
    /// Kept markets whose `feeSchedule.exponent` is not the documented `1`. Their rate is
    /// **not** used: the formula is unknown to this build, so the category table decides
    /// and this count is warned about once per refresh.
    pub markets_unsupported_fee_formula: usize,
}

impl DiscoveryStats {
    pub fn markets_dropped(&self) -> usize {
        self.drops.total()
    }
}

/// Drop reasons, aggregated. Deliberately few and fixed: this is logged once per universe
/// refresh, so the cardinality has to stay bounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DropCounts {
    /// `closed = true` or `active = false` on the market itself.
    pub closed_or_inactive: usize,
    /// `enableOrderBook = false` — there is no CLOB book, so it can never be a leg.
    pub no_order_book: usize,
    /// `clobTokenIds` absent, or present but not decodable as an array (parse failure).
    pub no_token_ids: usize,
    /// Token array present but not exactly two non-empty ids.
    pub not_binary: usize,
    /// No `conditionId` — nothing to key the market on.
    pub no_condition_id: usize,
    /// Belonged to an event that was dropped whole (closed/inactive event, or an event
    /// left with no priceable market). Counted here so `markets_seen` balances.
    pub event_dropped: usize,
    /// Any drop path that does not fit the buckets above.
    pub other: usize,
}

impl DropCounts {
    pub fn total(&self) -> usize {
        self.closed_or_inactive
            + self.no_order_book
            + self.no_token_ids
            + self.not_binary
            + self.no_condition_id
            + self.event_dropped
            + self.other
    }

    fn record(&mut self, reason: DropReason) {
        match reason {
            DropReason::ClosedOrInactive => self.closed_or_inactive += 1,
            DropReason::NoOrderBook => self.no_order_book += 1,
            DropReason::NoTokenIds => self.no_token_ids += 1,
            DropReason::NotBinary => self.not_binary += 1,
            DropReason::NoConditionId => self.no_condition_id += 1,
        }
    }

    /// One low-cardinality line for the INFO log / CLI: only non-zero buckets.
    pub fn summary(&self) -> String {
        let buckets = [
            ("closed_or_inactive", self.closed_or_inactive),
            ("no_order_book", self.no_order_book),
            ("no_token_ids", self.no_token_ids),
            ("not_binary", self.not_binary),
            ("no_condition_id", self.no_condition_id),
            ("event_dropped", self.event_dropped),
            ("other", self.other),
        ];
        let parts: Vec<String> = buckets
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(name, n)| format!("{name}={n}"))
            .collect();
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// Why one market was not tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropReason {
    ClosedOrInactive,
    NoOrderBook,
    NoTokenIds,
    NotBinary,
    NoConditionId,
}

// ---------------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RawEvent {
    #[serde(default, deserialize_with = "de_opt_flex_string")]
    pub id: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Verified live 2026-07-30: the key is `negRisk` on `/events/keyset` event objects.
    #[serde(default, rename = "negRisk")]
    pub neg_risk: Option<bool>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<RawTag>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    /// Event close time. Only used to spot markets that live and die between universe
    /// refreshes (see `short_lived_crypto_events`).
    /// Verified live 2026-07-30: the key is `endDate`. The *format* is still parsed
    /// defensively (see `parse_end_date`) — RFC 3339 with and without a zone marker.
    #[serde(default, rename = "endDate")]
    pub end_date: Option<String>,
    #[serde(default)]
    pub markets: Vec<RawMarket>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTag {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawMarket {
    #[serde(default, rename = "conditionId")]
    pub condition_id: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
    /// JSON-encoded array of the two CLOB token ids, e.g. `"[\"123\",\"456\"]"`.
    /// Verified live 2026-07-30: Gamma still double-encodes this. `de_opt_string_array`
    /// keeps accepting a real array too.
    #[serde(
        default,
        rename = "clobTokenIds",
        deserialize_with = "de_opt_string_array"
    )]
    pub clob_token_ids: Option<Vec<String>>,
    /// Also double-encoded on the live response, e.g. `"[\"Yes\",\"No\"]"`.
    #[serde(default, deserialize_with = "de_opt_string_array")]
    pub outcomes: Option<Vec<String>>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    /// Verified live 2026-07-30: `enableOrderBook` is present on every market object.
    /// Markets without a CLOB book cannot be a leg.
    #[serde(default, rename = "enableOrderBook")]
    pub enable_order_book: Option<bool>,

    // ---- fee data (verified live 2026-07-30) -------------------------------------
    /// `feesEnabled`. `false` (with `feeType: null`) is what a fee-free market looks like.
    #[serde(default, rename = "feesEnabled")]
    pub fees_enabled: Option<bool>,
    /// `feeType`, e.g. `"politics_fees"` / `"finance_prices_fees"`, `null` when off.
    #[serde(default, rename = "feeType")]
    pub fee_type: Option<String>,
    /// `feeSchedule`, absent (or null) when `feesEnabled` is false.
    #[serde(default, rename = "feeSchedule")]
    pub fee_schedule: Option<RawFeeSchedule>,

    // ---- order/reward parameters (verified live 2026-07-30; captured, not yet used) ---
    #[serde(
        default,
        rename = "orderPriceMinTickSize",
        deserialize_with = "de_opt_decimal"
    )]
    pub order_price_min_tick_size: Option<rust_decimal::Decimal>,
    #[serde(default, rename = "orderMinSize", deserialize_with = "de_opt_decimal")]
    pub order_min_size: Option<rust_decimal::Decimal>,
    #[serde(
        default,
        rename = "rewardsMinSize",
        deserialize_with = "de_opt_decimal"
    )]
    pub rewards_min_size: Option<rust_decimal::Decimal>,
    #[serde(
        default,
        rename = "rewardsMaxSpread",
        deserialize_with = "de_opt_decimal"
    )]
    pub rewards_max_spread: Option<rust_decimal::Decimal>,
}

/// `{"exponent": 1, "rate": 0.04, "takerOnly": true, "rebateRate": 0.25}`.
///
/// Unknown fields are ignored and every known one is optional, so a schedule that grows a
/// field cannot break discovery.
#[derive(Debug, Clone, Deserialize)]
pub struct RawFeeSchedule {
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub rate: Option<rust_decimal::Decimal>,
    /// The `n` of `rate · pⁿ · (1 − p)ⁿ`. Only `1` is implemented.
    #[serde(default, deserialize_with = "de_opt_decimal")]
    pub exponent: Option<rust_decimal::Decimal>,
    #[serde(default, rename = "takerOnly")]
    pub taker_only: Option<bool>,
    #[serde(default, rename = "rebateRate", deserialize_with = "de_opt_decimal")]
    pub rebate_rate: Option<rust_decimal::Decimal>,
}

// ---------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------

/// Parse one events page.
///
/// Three shapes are accepted:
///
/// * `{"$schema": …, "events": [...], "next_cursor": "…"}` — **the live `/events/keyset`
///   envelope**, confirmed against a real response on 2026-07-30. This arm is the fix for
///   the discovery outage: without it every keyset page failed to decode, the daemon saw a
///   hard error on every attempt, and it retried forever.
/// * `{"data": [...]}` — the wrapper other Gamma endpoints use; kept, it costs nothing.
/// * a bare array — the legacy `/events` list shape, still served by the fallback path.
pub fn parse_events_page(url: &str, body: &str) -> Result<Vec<RawEvent>, ApiError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Page {
        Bare(Vec<RawEvent>),
        // The live keyset envelope. First, so it is preferred over a `data` wrapper if a
        // response ever carried both.
        Keyset { events: Vec<RawEvent> },
        // Some Gamma endpoints wrap results in `{data, pagination}`.
        Wrapped { data: Vec<RawEvent> },
    }

    match serde_json::from_str::<Page>(body) {
        Ok(Page::Bare(v)) | Ok(Page::Keyset { events: v }) | Ok(Page::Wrapped { data: v }) => Ok(v),
        Err(source) => Err(ApiError::Decode {
            url: url.to_string(),
            source,
        }),
    }
}

/// The cursor that asks for the page *after* this one, or `None` when there is no next page.
///
/// Verified live 2026-07-30: `/events/keyset` returns a **top-level `next_cursor`** holding
/// an opaque base64-ish string — the first shape this function checks. The other spellings
/// (`nextCursor`, `cursor`, and the same keys nested under `pagination`) are kept as cheap
/// tolerance. A bare array (the legacy shape) carries no cursor and yields `None`, which
/// ends pagination — the same as the legacy short-page rule.
///
/// Still unverified: how an *exhausted* list signals the end — by omitting `next_cursor` or
/// by returning an empty string. Both are handled here, so it does not matter.
pub fn parse_next_cursor(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // Cursors are opaque; a numeric one is still a cursor, so accept numbers as strings.
    let at = |v: &serde_json::Value, key: &str| -> Option<String> {
        match v.get(key) {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    };
    let one_of = |v: &serde_json::Value| -> Option<String> {
        at(v, "next_cursor")
            .or_else(|| at(v, "nextCursor"))
            .or_else(|| at(v, "cursor"))
    };
    one_of(&value)
        .or_else(|| value.get("pagination").and_then(one_of))
        .map(|c| c.trim().to_string())
        // An empty cursor is "no more pages", never a request for `after_cursor=`.
        .filter(|c| !c.is_empty())
}

/// Turn raw events into the tracked universe, dropping anything we cannot price.
pub fn build_universe(raw: &[RawEvent]) -> (Universe, DiscoveryStats) {
    let mut stats = DiscoveryStats::default();
    let mut events = Vec::new();

    for ev in raw {
        stats.events_seen += 1;
        // Every market Gamma lists is "seen", including the ones we are about to drop —
        // otherwise `markets_seen` silently excludes whole events and the drop breakdown
        // does not balance.
        stats.markets_seen += ev.markets.len();

        // `active=true&closed=false` is requested server-side; re-check because query
        // filters are not a contract.
        if ev.closed == Some(true) || ev.active == Some(false) {
            stats.events_inactive += 1;
            stats.drops.event_dropped += ev.markets.len();
            continue;
        }

        let tags: Vec<String> = ev
            .tags
            .iter()
            .flat_map(|t| [t.slug.clone(), t.label.clone()])
            .flatten()
            .collect();
        let category = Category::resolve(ev.category.as_deref(), &tags);

        let mut markets = Vec::new();
        for m in &ev.markets {
            match classify_market(m) {
                Ok(tm) => {
                    stats.markets_kept += 1;
                    if tm.fees.api_rate().is_some() {
                        stats.markets_with_api_fee += 1;
                    }
                    if tm.fees.exponent_unsupported() {
                        stats.markets_unsupported_fee_formula += 1;
                    }
                    markets.push(tm);
                }
                Err(reason) => stats.drops.record(reason),
            }
        }

        if markets.is_empty() {
            // The individual markets already carry their own drop reason; do not
            // double-count them as `event_dropped`.
            stats.events_no_usable_market += 1;
            continue;
        }

        let slug = ev
            .slug
            .clone()
            .or_else(|| ev.id.clone())
            .unwrap_or_else(|| "unknown-event".to_string());
        stats.events_kept += 1;
        events.push(TrackedEvent {
            id: ev.id.clone().unwrap_or_else(|| slug.clone()),
            title: ev.title.clone().unwrap_or_else(|| slug.clone()),
            slug,
            neg_risk: ev.neg_risk.unwrap_or(false),
            category,
            // The *pre-filter* count: this is what the full-coverage guard compares
            // `markets.len()` against, so it must include everything we just dropped.
            total_outcomes: ev.markets.len(),
            end_date: ev.end_date.as_deref().and_then(parse_end_date),
            markets,
        });
    }

    (Universe { events }, stats)
}

/// Parse Gamma's event close time. Unparseable input yields `None` (treated as "unknown"),
/// never a guessed timestamp.
fn parse_end_date(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // TODO(verify-live): some Gamma fields drop the zone marker; those are UTC.
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Turn one raw market into a tracked one, or say precisely why it cannot be tracked.
fn classify_market(m: &RawMarket) -> Result<TrackedMarket, DropReason> {
    if m.closed == Some(true) || m.active == Some(false) {
        return Err(DropReason::ClosedOrInactive);
    }
    if m.enable_order_book == Some(false) {
        return Err(DropReason::NoOrderBook);
    }
    let condition_id = m
        .condition_id
        .clone()
        .filter(|c| !c.trim().is_empty())
        .ok_or(DropReason::NoConditionId)?;
    // `None` covers both "field absent" and "present but not decodable" — the permissive
    // deserialiser turns a malformed encoded array into `None` rather than guessing.
    let tokens = m.clob_token_ids.as_ref().ok_or(DropReason::NoTokenIds)?;
    if tokens.iter().any(|t| t.trim().is_empty()) {
        return Err(DropReason::NoTokenIds);
    }
    // Exactly two outcome tokens, or it is not a binary condition we can price.
    if tokens.len() != 2 {
        return Err(DropReason::NotBinary);
    }
    let outcomes = match m.outcomes.as_ref() {
        Some(o) if o.len() == 2 => [o[0].clone(), o[1].clone()],
        // Polymarket's binary convention when the field is absent.
        _ => ["Yes".to_string(), "No".to_string()],
    };
    Ok(TrackedMarket {
        question: m.question.clone().unwrap_or_else(|| condition_id.clone()),
        condition_id,
        outcomes,
        token_ids: [TokenId::new(&tokens[0]), TokenId::new(&tokens[1])],
        fees: market_fees(m),
        trading: MarketTrading {
            min_tick_size: m.order_price_min_tick_size,
            min_order_size: m.order_min_size,
            rewards_min_size: m.rewards_min_size,
            rewards_max_spread: m.rewards_max_spread,
        },
    })
}

/// Fold the three fee fields into the domain type. A missing `feeSchedule` leaves the rate
/// unknown; it is [`MarketFees::api_rate`] that decides what "unknown" costs.
fn market_fees(m: &RawMarket) -> MarketFees {
    let schedule = m.fee_schedule.as_ref();
    MarketFees {
        enabled: m.fees_enabled,
        fee_type: m
            .fee_type
            .clone()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        rate: schedule.and_then(|s| s.rate),
        exponent: schedule.and_then(|s| s.exponent),
        taker_only: schedule.and_then(|s| s.taker_only),
        rebate_rate: schedule.and_then(|s| s.rebate_rate),
    }
}

// ---------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------

/// The query parameter that carries a keyset cursor back to the API.
///
/// TODO(verify-live): unverifiable from this container, and the ecosystem disagrees. The
/// keyset endpoints return a `next_cursor`, but a community bug report
/// (Polymarket/agents#227) says feeding it back as `cursor=` re-serves the *first* page for
/// some clients, and that `after_cursor=` is the parameter that actually advances — so that
/// is the default. `scan.keyset_cursor_param` overrides it without a rebuild if a live run
/// shows otherwise, and the same-page guard in [`GammaClient::fetch_keyset`] means a wrong
/// name truncates the universe with a WARN instead of looping forever.
pub const DEFAULT_KEYSET_CURSOR_PARAM: &str = "after_cursor";

/// Which listing endpoint this process found working, remembered across universe refreshes.
///
/// The daemon builds a fresh [`GammaClient`] on every refresh, so without shared state it
/// would re-probe a missing `/events/keyset` — and re-log the fallback — every ten minutes.
#[derive(Debug, Default)]
pub struct PaginationState {
    /// Set the first time `/events/keyset` answers 404/405.
    keyset_unavailable: AtomicBool,
}

/// One pagination pass: the events collected, whether we stopped before the end of the
/// list (cap reached, API refused to go deeper, or the cursor stopped advancing), and the
/// endpoint that served them — an empty universe is meaningless without knowing which URL
/// produced it.
struct Fetched {
    events: Vec<RawEvent>,
    truncated: bool,
    endpoint: String,
}

pub struct GammaClient<'a> {
    http: &'a HttpClient,
    base_url: String,
    page_size: usize,
    max_events: usize,
    cursor_param: String,
    state: Arc<PaginationState>,
}

impl<'a> GammaClient<'a> {
    /// A client with its own pagination state — right for one-shot commands (`markets`,
    /// `scan`) and tests. Long-running callers should share one state; see
    /// [`GammaClient::with_state`].
    pub fn new(http: &'a HttpClient, cfg: &Config) -> Self {
        Self::with_state(http, cfg, Arc::new(PaginationState::default()))
    }

    pub fn with_state(http: &'a HttpClient, cfg: &Config, state: Arc<PaginationState>) -> Self {
        Self {
            http,
            base_url: cfg.api.gamma_base_url.trim_end_matches('/').to_string(),
            page_size: cfg.scan.page_size,
            max_events: cfg.scan.max_events,
            cursor_param: cfg.scan.keyset_cursor_param.trim().to_string(),
            state,
        }
    }

    /// Discover the whole active event list, keyset-first.
    ///
    /// `scan.max_events` is a rate-limit backstop, not the intended stopping point. When it
    /// is what stops us, discovery is incomplete: the universe is missing events, and
    /// (worse) a NegRisk event can be split across the boundary. That is loud, not silent —
    /// as is every other reason we stop early.
    pub async fn fetch_universe(&self) -> Result<(Universe, DiscoveryStats), ApiError> {
        let fetched = if self.state.keyset_unavailable.load(Ordering::Relaxed) {
            self.fetch_legacy().await?
        } else {
            match self.fetch_keyset().await? {
                Some(fetched) => fetched,
                None => {
                    // Log the hand-over once per process, not once per refresh.
                    if !self.state.keyset_unavailable.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            url = %format!("{}/events/keyset", self.base_url),
                            "the keyset events endpoint is missing (404/405) — falling back to \
                             legacy offset pagination for the life of this process; the API caps \
                             offset depth, so the universe may be truncated"
                        );
                    }
                    self.fetch_legacy().await?
                }
            }
        };

        let (universe, mut stats) = build_universe(&fetched.events);
        stats.truncated = fetched.truncated;
        tracing::info!(
            events_seen = stats.events_seen,
            events_kept = stats.events_kept,
            markets_seen = stats.markets_seen,
            markets_kept = stats.markets_kept,
            markets_dropped = stats.markets_dropped(),
            drop_reasons = %stats.drops.summary(),
            markets_with_api_fee = stats.markets_with_api_fee,
            "market discovery drop breakdown"
        );
        // Once per refresh, with the count — an unknown fee curve is priced from the
        // category table, and that substitution must never be silent.
        if stats.markets_unsupported_fee_formula > 0 {
            tracing::warn!(
                markets = stats.markets_unsupported_fee_formula,
                "Gamma reported a feeSchedule exponent other than 1 on some markets; this \
                 build only implements rate·p·(1−p), so those markets are costed from the \
                 category fee table instead of their stated rate"
            );
        }

        // Nothing came back. Polymarket always has active events, so the overwhelmingly
        // likely cause is that the response shape moved under us again — say so here, in
        // the one line the daemon prints on every retry, instead of leaving an operator to
        // infer it from an endlessly repeating stack of context.
        if universe.events.is_empty() {
            return Err(ApiError::EmptyDiscovery {
                url: fetched.endpoint,
                events: 0,
                detail: if stats.events_seen == 0 {
                    "no event object was found in the response body; the live keyset \
                     envelope is {\"events\": [...], \"next_cursor\": \"…\"} — check \
                     parse_events_page against a fresh response"
                        .to_string()
                } else {
                    format!(
                        "{} event(s) parsed but every one was filtered out ({} inactive or \
                         closed, {} with no priceable market; market drops: {})",
                        stats.events_seen,
                        stats.events_inactive,
                        stats.events_no_usable_market,
                        stats.drops.summary()
                    )
                },
            });
        }
        Ok((universe, stats))
    }

    /// Cursor pagination over `/events/keyset` — the primary path.
    ///
    /// `Ok(None)` means "this endpoint does not exist here" (404/405) and asks the caller to
    /// fall back to offset pagination. Every other stop is a normal termination:
    ///
    /// * an empty page, or a page with no next cursor — the list is exhausted;
    /// * the cursor did not change, or the page repeated its first event id — the API is not
    ///   advancing (a wrong cursor parameter name looks exactly like this), so stop with a
    ///   WARN rather than fetching page one forever;
    /// * `scan.max_events` — the backstop, with the long-standing truncation WARN.
    async fn fetch_keyset(&self) -> Result<Option<Fetched>, ApiError> {
        const NOT_ADVANCING: &str = "keyset pagination did not advance — universe truncated";

        let url = format!("{}/events/keyset", self.base_url);
        let mut events: Vec<RawEvent> = Vec::new();
        let mut pages = 0usize;
        let mut truncated = false;
        let mut cursor: Option<String> = None;
        // The previous page's first event id — the cheap "did we just get page one again?"
        // check, for an API that hands back a *fresh* cursor while ignoring it.
        let mut previous_first: Option<String> = None;

        loop {
            let remaining = self.max_events.saturating_sub(events.len());
            if remaining == 0 {
                truncated = true;
                self.warn_max_events(pages, events.len());
                break;
            }
            let limit = self.page_size.min(remaining);
            let mut query = vec![
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
                ("limit", limit.to_string()),
            ];
            if let Some(cursor) = cursor.as_ref() {
                query.push((self.cursor_param.as_str(), cursor.clone()));
            }

            let body = match self.http.get_json(&url, &query).await {
                Ok(body) => body,
                // The endpoint is not deployed here. Nothing collected so far is lost: the
                // caller restarts from the top over the legacy endpoint.
                Err(ApiError::Status {
                    status: 404 | 405, ..
                }) => {
                    tracing::debug!(%url, pages, "keyset endpoint not found");
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };
            let page = parse_events_page(&url, &body)?;
            let next = parse_next_cursor(&body);
            pages += 1;
            tracing::debug!(
                page = pages,
                got = page.len(),
                limit,
                has_next_cursor = next.is_some(),
                "fetched gamma keyset events page"
            );
            if page.is_empty() {
                break;
            }

            let first = page.first().and_then(|e| e.id.clone());
            // Only a *known* id can prove a repeat; an id-less page falls through to the
            // cursor check below.
            if first.is_some() && first == previous_first {
                tracing::warn!(
                    pages,
                    events = events.len(),
                    cursor_param = %self.cursor_param,
                    first_event_id = ?first,
                    "{NOT_ADVANCING} (the API re-served the same page — check \
                     scan.keyset_cursor_param)"
                );
                truncated = true;
                break;
            }
            previous_first = first;
            events.extend(page);

            match next {
                // The normal case: a new cursor, so there is another page to ask for.
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                Some(_) => {
                    tracing::warn!(
                        pages,
                        events = events.len(),
                        cursor_param = %self.cursor_param,
                        "{NOT_ADVANCING} (the API returned the same cursor twice)"
                    );
                    truncated = true;
                    break;
                }
                // No cursor = no further pages. This is also what a bare-array response
                // (the legacy shape served from the keyset path) does, deliberately.
                None => break,
            }
        }

        Ok(Some(Fetched {
            events,
            truncated,
            endpoint: url,
        }))
    }

    /// Offset pagination over the legacy `/events` list endpoint — the fallback.
    ///
    /// The API caps how deep `offset` may go and answers HTTP 422 beyond it. That is a
    /// *limit*, not a failure: keep everything fetched so far, mark the universe truncated,
    /// and let the daemon scan what it has. A 422 must never reach the caller as an error —
    /// propagating it is what put the live daemon in a Docker restart loop.
    async fn fetch_legacy(&self) -> Result<Fetched, ApiError> {
        let url = format!("{}/events", self.base_url);
        let mut events: Vec<RawEvent> = Vec::new();
        let mut offset = 0usize;
        let mut pages = 0usize;
        let mut truncated = false;

        loop {
            let remaining = self.max_events.saturating_sub(events.len());
            if remaining == 0 {
                truncated = true;
                self.warn_max_events(pages, events.len());
                break;
            }
            let limit = self.page_size.min(remaining);
            let query = [
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ];
            let body = match self.http.get_json(&url, &query).await {
                Ok(body) => body,
                Err(ApiError::Status { status: 422, .. }) => {
                    // Deeper pagination needs the keyset endpoint, which is not available
                    // (that is the only way we get here).
                    let message = "offset pagination capped by the API — universe truncated; \
                                   keyset endpoint unavailable";
                    if events.is_empty() {
                        // Rejected at offset 0: this is not a depth cap, and the universe is
                        // now *empty*. Still not fatal, but it must not read as routine.
                        tracing::error!(offset, %url, "{message} (and nothing was fetched at all)");
                    } else {
                        tracing::warn!(offset, events = events.len(), pages, "{message}");
                    }
                    truncated = true;
                    break;
                }
                Err(err) => return Err(err),
            };
            let page = parse_events_page(&url, &body)?;
            let got = page.len();
            pages += 1;
            tracing::debug!(offset, got, limit, "fetched gamma events page");
            events.extend(page);
            // A short (or empty) page is the end of the list — the only clean stop.
            if got < limit {
                break;
            }
            offset += got;
        }

        Ok(Fetched {
            events,
            truncated,
            endpoint: url,
        })
    }

    fn warn_max_events(&self, pages: usize, events: usize) {
        tracing::warn!(
            max_events = self.max_events,
            page_size = self.page_size,
            pages,
            events,
            "universe truncated at max_events — discovery is incomplete; \
             raise scan.max_events (or pass --limit) to cover the whole event list"
        );
    }
}

// ---------------------------------------------------------------------------------
// Deserialisers
// ---------------------------------------------------------------------------------

/// Gamma ids are sometimes strings, sometimes numbers.
fn de_opt_flex_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        Num(serde_json::Number),
    }
    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => None,
        Some(Raw::Str(s)) => Some(s),
        Some(Raw::Num(n)) => Some(n.to_string()),
    })
}

/// Accepts a real JSON array *or* a JSON-encoded array in a string (Gamma's habit).
fn de_opt_string_array<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Arr(Vec<String>),
        Str(String),
    }
    Ok(match Option::<Raw>::deserialize(deserializer)? {
        None => None,
        Some(Raw::Arr(v)) => Some(v),
        // A malformed encoded array is treated as "no token ids", which drops the market
        // rather than trading on a guess.
        Some(Raw::Str(s)) => serde_json::from_str::<Vec<String>>(&s).ok(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::*;

    const EVENTS: &str = include_str!("../tests/fixtures/gamma_events.json");

    fn parsed() -> Vec<RawEvent> {
        parse_events_page("test", EVENTS).expect("fixture must parse")
    }

    #[test]
    fn parses_a_realistic_events_page() {
        let raw = parsed();
        assert_eq!(raw.len(), 4);
        assert_eq!(raw[0].slug.as_deref(), Some("presidential-election-2028"));
        assert_eq!(raw[0].neg_risk, Some(true));
        // Numeric ids survive as strings.
        assert_eq!(raw[1].id.as_deref(), Some("21055"));
        // Double-encoded arrays are decoded.
        assert_eq!(
            raw[0].markets[0].clob_token_ids.as_deref(),
            Some(["1001".to_string(), "1002".to_string()].as_slice())
        );
        // A real array works too (event index 1 uses the un-encoded shape).
        assert_eq!(
            raw[1].markets[0].outcomes.as_deref(),
            Some(["Yes".to_string(), "No".to_string()].as_slice())
        );
    }

    #[test]
    fn universe_keeps_negrisk_and_binary_and_drops_the_rest() {
        let (universe, stats) = build_universe(&parsed());

        assert_eq!(stats.events_seen, 4);
        // event 0: negrisk politics (3 markets, one closed) ; event 1: binary sports ;
        // event 2: closed → dropped ; event 3: markets with unusable token ids → dropped.
        assert_eq!(stats.events_kept, 2);
        assert_eq!(stats.events_inactive, 1);
        assert_eq!(stats.events_no_usable_market, 1);
        // 3 + 1 + 1 + 2 markets listed across the four events.
        assert_eq!(stats.markets_seen, 7);
        assert_eq!(stats.markets_kept, 3);
        assert_eq!(stats.drops.closed_or_inactive, 1); // event 0's withdrawn candidate
        assert_eq!(stats.drops.event_dropped, 1); // the resolved event's only market
        assert_eq!(stats.drops.not_binary, 1); // ["4001"] — one token id
        assert_eq!(stats.drops.no_condition_id, 1);
        assert_eq!(stats.drops.no_token_ids, 0);
        assert_eq!(stats.drops.no_order_book, 0);
        assert_eq!(stats.drops.other, 0);
        // The books must balance: nothing may vanish unexplained.
        assert_eq!(
            stats.markets_kept + stats.markets_dropped(),
            stats.markets_seen
        );

        assert_eq!(universe.events.len(), 2);
        assert_eq!(universe.neg_risk_event_count(), 1);
        assert_eq!(universe.market_count(), 3);
        assert_eq!(universe.token_ids().len(), 6);

        let ev = &universe.events[0];
        assert!(ev.neg_risk);
        assert_eq!(ev.category.as_str(), "politics");
        assert_eq!(ev.markets.len(), 2);
        // Gamma listed three outcomes; one was dropped, so coverage is NOT complete.
        assert_eq!(ev.total_outcomes, 3);
        assert!(!ev.coverage_complete());
        assert_eq!(ev.missing_outcomes(), 1);
        // The sports event is fully covered (1 of 1).
        assert_eq!(universe.events[1].total_outcomes, 1);
        assert!(universe.events[1].coverage_complete());
        assert_eq!(ev.markets[0].yes_token().as_str(), "1001");
        assert_eq!(ev.markets[0].no_token().as_str(), "1002");

        // Category comes from the tag list when the `category` field is absent.
        assert_eq!(universe.events[1].category.as_str(), "sports");
        assert!(!universe.events[1].neg_risk);
    }

    #[test]
    fn unknown_fields_and_missing_optionals_do_not_break_parsing() {
        let body = r#"[{"id":7,"slug":"s","brandNewField":{"nested":true},"markets":[
            {"conditionId":"0xabc","clobTokenIds":"[\"1\",\"2\"]","somethingElse":42}
        ]}]"#;
        let raw = parse_events_page("test", body).expect("must tolerate unknown fields");
        let (u, _) = build_universe(&raw);
        assert_eq!(u.market_count(), 1);
        // Missing `outcomes` falls back to the Yes/No convention.
        assert_eq!(u.events[0].markets[0].outcomes, ["Yes", "No"]);
        // Missing `negRisk` is treated as not-NegRisk (never assume mutual exclusivity).
        assert!(!u.events[0].neg_risk);
        assert_eq!(u.events[0].category.as_str(), "other");
    }

    /// `endDate` exists only so the daemon can say "these markets die before I next look".
    /// Anything it cannot parse must stay `None` — a guessed close time would be worse
    /// than no close time.
    #[test]
    fn event_end_dates_are_parsed_and_bad_ones_become_unknown() {
        let body = r#"[
          {"id":"1","slug":"z","endDate":"2026-07-29T12:05:00Z","markets":[
            {"conditionId":"0x1","clobTokenIds":"[\"1\",\"2\"]"}]},
          {"id":"2","slug":"naive","endDate":"2026-07-29T12:05:00","markets":[
            {"conditionId":"0x2","clobTokenIds":"[\"3\",\"4\"]"}]},
          {"id":"3","slug":"offset","endDate":"2026-07-29T14:05:00+02:00","markets":[
            {"conditionId":"0x3","clobTokenIds":"[\"5\",\"6\"]"}]},
          {"id":"4","slug":"junk","endDate":"soon","markets":[
            {"conditionId":"0x4","clobTokenIds":"[\"7\",\"8\"]"}]},
          {"id":"5","slug":"absent","markets":[
            {"conditionId":"0x5","clobTokenIds":"[\"9\",\"10\"]"}]}
        ]"#;
        let (universe, _) = build_universe(&parse_events_page("test", body).expect("parses"));
        let expected = chrono::DateTime::parse_from_rfc3339("2026-07-29T12:05:00Z")
            .expect("literal")
            .with_timezone(&chrono::Utc);

        assert_eq!(universe.events[0].end_date, Some(expected));
        assert_eq!(universe.events[1].end_date, Some(expected), "no zone = UTC");
        assert_eq!(
            universe.events[2].end_date,
            Some(expected),
            "offset applied"
        );
        assert_eq!(universe.events[3].end_date, None, "junk is unknown");
        assert_eq!(universe.events[4].end_date, None, "absent is unknown");

        assert!(
            universe.events[0].ends_by(expected),
            "the boundary is inclusive"
        );
        assert!(!universe.events[0].ends_by(expected - chrono::Duration::seconds(1)));
        assert!(
            !universe.events[4].ends_by(expected + chrono::Duration::days(3_650)),
            "an unknown end time is never treated as short-lived"
        );
    }

    // --- the live keyset envelope ------------------------------------------------------

    /// Trimmed copy of the real `GET /events/keyset?active=true&closed=false&limit=2`
    /// response the owner captured on 2026-07-30 (two events; the envelope keys are
    /// verbatim).
    const KEYSET: &str = include_str!("../tests/fixtures/gamma_events_keyset.json");

    /// **The bug.** The live envelope nests the array under `events`, and this parser
    /// accepted only a bare array or `{"data": …}` — so every keyset page failed to decode,
    /// discovery never produced an event, and the daemon retried discovery forever.
    #[test]
    fn the_live_keyset_envelope_parses_its_events_array() {
        let page = parse_events_page("test", KEYSET).expect("the live envelope must parse");
        assert_eq!(page.len(), 2, "both events must come out of the envelope");
        assert_eq!(
            page[0].slug.as_deref(),
            Some("which-party-wins-the-2028-presidential-election")
        );
        assert_eq!(page[0].neg_risk, Some(true));
        assert_eq!(page[0].markets.len(), 2);
        assert_eq!(page[1].slug.as_deref(), Some("kraken-june-listing"));

        // The cursor is top-level and opaque; it must survive verbatim.
        assert_eq!(
            parse_next_cursor(KEYSET).as_deref(),
            Some("eyJpZCI6NDI5ODQsInMiOiJrcmFrZW4tanVuZS1saXN0aW5nIn0=")
        );

        // And the whole page must survive the trip into the tracked universe.
        let (universe, stats) = build_universe(&page);
        assert_eq!(stats.events_seen, 2);
        assert_eq!(stats.events_kept, 2);
        assert_eq!(stats.markets_kept, 3);
        assert_eq!(stats.drops.total(), 0);
        assert_eq!(universe.events[0].category.as_str(), "politics");
        assert!(universe.events[0].coverage_complete());
        assert_eq!(universe.events[0].markets[0].yes_token().as_str(), "7001");
    }

    /// The per-market fee data on the live response, and every case the resolver has to
    /// tell apart.
    #[test]
    fn per_market_fee_data_is_parsed_including_the_disabled_and_absent_cases() {
        let (universe, stats) =
            build_universe(&parse_events_page("test", KEYSET).expect("fixture"));

        // Fees on, standard curve, rate stated.
        let politics = &universe.events[0].markets[0].fees;
        assert_eq!(politics.enabled, Some(true));
        assert_eq!(politics.fee_type.as_deref(), Some("politics_fees"));
        assert_eq!(politics.rate, Some(dec!(0.04)));
        assert_eq!(politics.exponent, Some(dec!(1)));
        assert_eq!(politics.taker_only, Some(true));
        assert_eq!(politics.rebate_rate, Some(dec!(0.25)));
        assert_eq!(politics.api_rate(), Some(dec!(0.04)));

        // `feesEnabled: false` with `feeType: null` and no schedule at all → a stated zero.
        let disabled = &universe.events[1].markets[0].fees;
        assert_eq!(disabled.enabled, Some(false));
        assert_eq!(disabled.fee_type, None);
        assert_eq!(disabled.rate, None);
        assert_eq!(disabled.api_rate(), Some(Decimal::ZERO));
        assert!(!disabled.exponent_unsupported());

        // The order/reward parameters ride along for Phase B.
        let trading = &universe.events[0].markets[0].trading;
        assert_eq!(trading.min_tick_size, Some(dec!(0.01)));
        assert_eq!(trading.min_order_size, Some(dec!(5)));
        assert_eq!(trading.rewards_min_size, Some(dec!(50)));
        assert_eq!(trading.rewards_max_spread, Some(dec!(3.5)));

        // All three kept markets state their own rate, and none needs a fallback.
        assert_eq!(stats.markets_with_api_fee, 3);
        assert_eq!(stats.markets_unsupported_fee_formula, 0);

        // The legacy fixture carries none of these fields: everything stays unknown, and
        // the category table keeps deciding (that path must not regress).
        let (legacy, legacy_stats) = build_universe(&parsed());
        assert_eq!(legacy.events[0].markets[0].fees, MarketFees::default());
        assert_eq!(legacy.events[0].markets[0].fees.api_rate(), None);
        assert_eq!(legacy_stats.markets_with_api_fee, 0);

        // An exponent we do not implement is counted, and its rate is not offered.
        let exotic = r#"{"events":[{"id":"1","slug":"x","active":true,"closed":false,"markets":[
            {"conditionId":"0x1","clobTokenIds":"[\"1\",\"2\"]","feesEnabled":true,
             "feeType":"crypto_fees","feeSchedule":{"exponent":2,"rate":"0.07"}}]}]}"#;
        let (u, s) = build_universe(&parse_events_page("test", exotic).expect("parses"));
        assert_eq!(u.events[0].markets[0].fees.rate, Some(dec!(0.07)));
        assert_eq!(u.events[0].markets[0].fees.api_rate(), None);
        assert!(u.events[0].markets[0].fees.exponent_unsupported());
        assert_eq!(s.markets_unsupported_fee_formula, 1);
        assert_eq!(s.markets_with_api_fee, 0);
    }

    #[test]
    fn wrapped_page_shape_is_accepted() {
        let body =
            r#"{"data":[{"id":"1","slug":"x","markets":[]}],"pagination":{"hasMore":false}}"#;
        assert_eq!(parse_events_page("test", body).expect("wrapped").len(), 1);
    }

    #[test]
    fn malformed_body_produces_a_useful_error() {
        let err = parse_events_page("https://gamma/events", "not json").expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("https://gamma/events"), "got: {msg}");
    }

    // --- drop-reason accounting --------------------------------------------------------

    /// Every drop bucket, hand-counted, over one synthetic page. The soak review needs to
    /// know *why* markets disappear, so each reason must land in its own bucket and the
    /// totals must balance.
    #[test]
    fn drops_are_counted_by_reason_and_balance() {
        let body = r#"[
          {"id":"1","slug":"live","active":true,"closed":false,"negRisk":true,"markets":[
            {"conditionId":"0x1","clobTokenIds":"[\"1\",\"2\"]","active":true,"closed":false},
            {"conditionId":"0x2","clobTokenIds":"[\"3\",\"4\"]","active":true,"closed":false},
            {"conditionId":"0x3","clobTokenIds":"[\"5\",\"6\"]","closed":true},
            {"conditionId":"0x4","clobTokenIds":"[\"7\",\"8\"]","active":false},
            {"conditionId":"0x5","clobTokenIds":"[\"9\",\"10\"]","enableOrderBook":false},
            {"conditionId":"0x6"},
            {"conditionId":"0x7","clobTokenIds":"not-a-json-array"},
            {"conditionId":"0x8","clobTokenIds":"[\"11\",\"12\",\"13\"]"},
            {"conditionId":"0x9","clobTokenIds":"[\"14\",\"\"]"},
            {"clobTokenIds":"[\"15\",\"16\"]"}
          ]},
          {"id":"2","slug":"dead","active":false,"closed":true,"markets":[
            {"conditionId":"0xa","clobTokenIds":"[\"20\",\"21\"]"},
            {"conditionId":"0xb","clobTokenIds":"[\"22\",\"23\"]"}
          ]}
        ]"#;
        let raw = parse_events_page("test", body).expect("parses");
        let (universe, stats) = build_universe(&raw);

        assert_eq!(stats.events_seen, 2);
        assert_eq!(stats.events_kept, 1);
        assert_eq!(stats.events_inactive, 1);
        assert_eq!(stats.markets_seen, 12); // 10 in the live event + 2 in the dead one
        assert_eq!(stats.markets_kept, 2);
        assert_eq!(stats.drops.closed_or_inactive, 2); // closed:true and active:false
        assert_eq!(stats.drops.no_order_book, 1);
        assert_eq!(stats.drops.no_token_ids, 3); // absent, unparseable, empty id
        assert_eq!(stats.drops.not_binary, 1); // three token ids
        assert_eq!(stats.drops.no_condition_id, 1);
        assert_eq!(stats.drops.event_dropped, 2); // both markets of the closed event
        assert_eq!(stats.drops.other, 0);
        assert_eq!(stats.drops.total(), 10);
        assert_eq!(stats.markets_kept + stats.drops.total(), stats.markets_seen);

        // The kept event knows it is only covering 2 of the 10 outcomes Gamma listed.
        let ev = &universe.events[0];
        assert_eq!(ev.total_outcomes, 10);
        assert_eq!(ev.markets.len(), 2);
        assert!(!ev.coverage_complete());

        let line = stats.drops.summary();
        for expected in [
            "closed_or_inactive=2",
            "no_order_book=1",
            "no_token_ids=3",
            "not_binary=1",
            "no_condition_id=1",
            "event_dropped=2",
        ] {
            assert!(
                line.contains(expected),
                "{expected:?} missing from {line:?}"
            );
        }
        // Zero buckets are omitted so the log line stays short.
        assert!(!line.contains("other="), "got: {line}");
    }

    // --- pagination --------------------------------------------------------------------

    /// Minimal HTTP/1.1 stub. Every request line is handed to `respond`, which answers
    /// `(status, body)`. Returns the base URL and the log of request lines, so a test can
    /// assert *which* endpoint was called and *which* cursor parameter it carried.
    async fn gamma_stub<F>(respond: F) -> (String, Arc<std::sync::Mutex<Vec<String>>>)
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = log.clone();
        let respond = Arc::new(respond);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (respond, sink) = (respond.clone(), sink.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let (status, body) = respond(&line);
                    sink.lock().expect("request log").push(line);
                    let phrase = match status {
                        200 => "OK",
                        404 => "Not Found",
                        405 => "Method Not Allowed",
                        422 => "Unprocessable Entity",
                        _ => "Error",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {phrase}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), log)
    }

    /// `GET /events/keyset?limit=10 HTTP/1.1` → `/events/keyset`.
    fn path_of(request_line: &str) -> String {
        request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    fn query_param(request_line: &str, name: &str) -> Option<String> {
        let target = request_line.split_whitespace().nth(1)?;
        let query = target.split('?').nth(1)?;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
            .map(str::to_string)
    }

    fn number_param(request_line: &str, name: &str) -> usize {
        query_param(request_line, name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// A bare JSON array of synthetic single-market events with ids `range`.
    fn events_json(range: std::ops::Range<usize>) -> String {
        let events: Vec<String> = range
            .map(|i| {
                format!(
                    r#"{{"id":"{i}","slug":"e{i}","active":true,"closed":false,
                       "markets":[{{"conditionId":"0x{i}",
                       "clobTokenIds":"[\"{i}a\",\"{i}b\"]"}}]}}"#
                )
            })
            .collect();
        format!("[{}]", events.join(","))
    }

    /// A Gamma that only speaks the legacy list endpoint: `/events/keyset` answers 404 (an
    /// older deployment), and `/events` honours `limit`/`offset` over `total` synthetic
    /// events but refuses any offset at or beyond `offset_cap` with the live 422 body.
    async fn legacy_gamma(
        total: usize,
        offset_cap: usize,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        gamma_stub(move |line| {
            if path_of(line) == "/events/keyset" {
                return (404, r#"{"error":"not found"}"#.to_string());
            }
            let (limit, offset) = (number_param(line, "limit"), number_param(line, "offset"));
            if offset >= offset_cap {
                return (
                    422,
                    r#"{"type":"validation error",
                        "error":"offset too large, use /events/keyset for deeper pagination"}"#
                        .to_string(),
                );
            }
            (200, events_json(offset..(offset + limit).min(total)))
        })
        .await
    }

    /// A Gamma that speaks keyset properly: `{data, next_cursor}` pages, the cursor being
    /// the index to resume from, and no cursor at all on the last page. The legacy endpoint
    /// answers 422 at every offset, so a fallback that should not happen fails loudly.
    async fn keyset_gamma(total: usize) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        gamma_stub(move |line| {
            if path_of(line) != "/events/keyset" {
                return (
                    422,
                    r#"{"error":"offset too large, use /events/keyset for deeper pagination"}"#
                        .to_string(),
                );
            }
            let limit = number_param(line, "limit");
            let start = number_param(line, DEFAULT_KEYSET_CURSOR_PARAM);
            let end = (start + limit).min(total);
            let data = events_json(start..end);
            let body = if end < total {
                format!(r#"{{"data":{data},"next_cursor":"{end}"}}"#)
            } else {
                // The terminator: a page with no next cursor is the end of the list.
                format!(r#"{{"data":{data}}}"#)
            };
            (200, body)
        })
        .await
    }

    fn paging_config(base: &str, page_size: usize, max_events: usize) -> Config {
        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.to_string();
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.scan.page_size = page_size;
        cfg.scan.max_events = max_events;
        cfg
    }

    fn requests(log: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
        log.lock().expect("request log").clone()
    }

    /// The primary path: two full keyset pages plus a terminator page, every event kept, and
    /// the cursor fed back under the parameter name the client claims to use. If the cursor
    /// were dropped the stub would serve page one forever and the same-page guard would trip
    /// — so a clean run here is also proof the cursor round-trips.
    #[tokio::test]
    async fn keyset_pagination_follows_the_cursor_to_the_end_of_the_list() {
        let (base, log) = keyset_gamma(25).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(stats.events_seen, 25, "every event must be collected");
        assert_eq!(universe.events.len(), 25);
        assert_eq!(universe.market_count(), 25);
        assert!(!stats.truncated, "the list ended on its own terms");

        let seen = requests(&log);
        assert_eq!(
            seen.len(),
            3,
            "10 + 10 + 5, then the missing cursor stops us"
        );
        assert!(
            seen.iter().all(|l| path_of(l) == "/events/keyset"),
            "the legacy endpoint must not be touched: {seen:?}"
        );
        // The cursor parameter name is the one thing we cannot verify from here, so assert
        // exactly what went out on the wire.
        assert_eq!(query_param(&seen[0], "after_cursor"), None, "{seen:?}");
        assert_eq!(query_param(&seen[1], "after_cursor").as_deref(), Some("10"));
        assert_eq!(query_param(&seen[2], "after_cursor").as_deref(), Some("20"));
        // The filters ride along on every page.
        for line in &seen {
            assert_eq!(query_param(line, "active").as_deref(), Some("true"));
            assert_eq!(query_param(line, "closed").as_deref(), Some("false"));
            assert_eq!(query_param(line, "limit").as_deref(), Some("10"));
        }
    }

    /// The infinite-loop guard. A cursor the API ignores (the reported `cursor=` vs
    /// `after_cursor=` confusion) looks exactly like this: a fresh cursor every time, and the
    /// same first page forever. Stop, say so, and keep what we have.
    #[tokio::test]
    async fn keyset_pagination_stops_when_the_api_re_serves_the_same_page() {
        let (base, log) = gamma_stub(|line| {
            assert_eq!(path_of(line), "/events/keyset");
            // Always page one — but always with a cursor, so only the same-page guard can
            // end this.
            (
                200,
                format!(
                    r#"{{"data":{},"pagination":{{"next_cursor":"page-{}"}}}}"#,
                    events_json(0..10),
                    query_param(line, DEFAULT_KEYSET_CURSOR_PARAM).unwrap_or_default()
                ),
            )
        })
        .await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");

        // A guard that did not work would hang until max_events (600 pages) or forever, so
        // the timeout is part of the assertion.
        let (universe, stats) = tokio::time::timeout(
            Duration::from_secs(10),
            GammaClient::new(&http, &cfg).fetch_universe(),
        )
        .await
        .expect("pagination must terminate, not loop")
        .expect("a stuck cursor is a truncation, not an error");

        assert!(
            stats.truncated,
            "a universe cut short by a stuck cursor must be reported as truncated"
        );
        assert_eq!(
            universe.events.len(),
            10,
            "the first page is kept; the repeat is not duplicated into the universe"
        );
        assert_eq!(
            requests(&log).len(),
            2,
            "one page, one repeat, then stop — bounded"
        );
    }

    /// `max_events` still backstops the keyset path.
    #[tokio::test]
    async fn keyset_pagination_reports_hitting_max_events() {
        let (base, _log) = keyset_gamma(60).await;
        let cfg = paging_config(&base, 10, 25);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(universe.events.len(), 25, "stopped exactly at the cap");
        assert!(
            stats.truncated,
            "discovery stopped at max_events and must say so"
        );
    }

    /// A deployment without the keyset endpoint: 404 hands over to offset pagination, which
    /// must still work end to end.
    #[tokio::test]
    async fn a_missing_keyset_endpoint_falls_back_to_legacy_offset_pagination() {
        let (base, log) = legacy_gamma(25, 2_000).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("a missing keyset endpoint is not an error");

        assert_eq!(
            universe.events.len(),
            25,
            "the fallback collected everything"
        );
        assert!(!stats.truncated);

        let seen = requests(&log);
        assert_eq!(path_of(&seen[0]), "/events/keyset", "keyset is tried first");
        let legacy: Vec<&String> = seen.iter().filter(|l| path_of(l) == "/events").collect();
        // 10 + 10 + 5: a short page is the end of the legacy list.
        assert_eq!(legacy.len(), 3, "got {seen:?}");
        assert_eq!(query_param(legacy[1], "offset").as_deref(), Some("10"));
    }

    /// A client that shares one [`PaginationState`] (the daemon) probes the missing keyset
    /// endpoint once, not once per universe refresh.
    #[tokio::test]
    async fn the_legacy_fallback_is_remembered_across_refreshes() {
        let (base, log) = legacy_gamma(5, 2_000).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let state = Arc::new(PaginationState::default());

        for _ in 0..3 {
            let (universe, _) = GammaClient::with_state(&http, &cfg, state.clone())
                .fetch_universe()
                .await
                .expect("fetch");
            assert_eq!(universe.events.len(), 5);
        }

        let probes = requests(&log)
            .iter()
            .filter(|l| path_of(l) == "/events/keyset")
            .count();
        assert_eq!(probes, 1, "the 404 must be remembered for the process");
    }

    /// The crash: `offset` past the API's cap answers HTTP 422, and propagating it killed the
    /// daemon at startup. It is a depth limit, not a failure — keep the events already
    /// fetched, mark the universe truncated, return no error.
    #[tokio::test]
    async fn a_legacy_offset_cap_truncates_instead_of_failing() {
        // The live shape: no keyset endpoint, 422 from offset 2 000 on.
        let (base, log) = legacy_gamma(6_000, 2_000).await;
        let cfg = paging_config(&base, 500, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("HTTP 422 mid-pagination must never be fatal");

        assert_eq!(
            universe.events.len(),
            2_000,
            "everything fetched before the cap is kept"
        );
        assert!(
            stats.truncated,
            "a universe cut short by the API's offset cap must be reported as truncated"
        );
        let legacy = requests(&log)
            .iter()
            .filter(|l| path_of(l) == "/events")
            .count();
        assert_eq!(legacy, 5, "4 pages of 500, then the 422 at offset 2 000");
    }

    /// Legacy pagination probes past an exactly-full last page: an empty page is what proves
    /// the list is exhausted.
    #[tokio::test]
    async fn legacy_pagination_probes_past_an_exactly_full_last_page() {
        let (base, log) = legacy_gamma(20, 2_000).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (_, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(stats.events_seen, 20);
        assert!(!stats.truncated);
        let legacy = requests(&log)
            .iter()
            .filter(|l| path_of(l) == "/events")
            .count();
        assert_eq!(legacy, 3, "10 + 10 + 0");
    }

    /// A bare array from the keyset path (no envelope, no cursor) is a single page and a
    /// clean stop — the shape our own test mocks and any transitional deployment serve.
    #[tokio::test]
    async fn a_keyset_page_without_a_cursor_ends_pagination() {
        let (base, log) = gamma_stub(|_| (200, events_json(0..3))).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(universe.events.len(), 3);
        assert!(!stats.truncated);
        assert_eq!(requests(&log).len(), 1);
    }

    /// An empty universe is never a normal outcome, and the daemon's only symptom of it is
    /// an endlessly repeating discovery retry. The one ERROR line it prints therefore has
    /// to name the count *and* the endpoint, so this class of bug (a moved response shape)
    /// is identifiable without a debugger.
    #[tokio::test]
    async fn a_discovery_that_finds_no_events_says_so_with_the_count_and_the_endpoint() {
        // A well-formed but empty keyset page: parses fine, yields nothing.
        let (base, _log) = gamma_stub(|_| {
            (
                200,
                r#"{"$schema":"https://gamma-api.polymarket.com/schemas/EventsKeysetListResponse.json","events":[],"next_cursor":""}"#.to_string(),
            )
        })
        .await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let err = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect_err("an empty universe must not be reported as a successful discovery");
        let msg = err.to_string();
        assert!(msg.contains("discovery parsed 0 events"), "got: {msg}");
        assert!(msg.contains("/events/keyset"), "got: {msg}");
        assert!(msg.contains("shape mismatch"), "got: {msg}");
        assert!(msg.contains("no event object was found"), "got: {msg}");

        // A page full of events that are *all* filtered out is a different diagnosis, and
        // must not be blamed on the response shape.
        let (base, _log) = gamma_stub(|_| {
            (
                200,
                r#"{"events":[{"id":"1","slug":"dead","active":false,"closed":true,
                    "markets":[{"conditionId":"0x1","clobTokenIds":"[\"1\",\"2\"]"}]}]}"#
                    .to_string(),
            )
        })
        .await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let msg = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect_err("nothing to scan is still an error")
            .to_string();
        assert!(msg.contains("discovery parsed 0 events"), "got: {msg}");
        assert!(msg.contains("every one was filtered out"), "got: {msg}");
        assert!(msg.contains("1 inactive or closed"), "got: {msg}");
    }

    /// The same diagnostic over the legacy path names the legacy endpoint, so the log line
    /// says which URL actually served the nothing.
    #[tokio::test]
    async fn the_empty_discovery_error_names_the_legacy_endpoint_when_that_is_what_was_used() {
        let (base, _log) = legacy_gamma(0, 2_000).await;
        let cfg = paging_config(&base, 10, 6_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let msg = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect_err("empty is an error on the fallback path too")
            .to_string();
        assert!(msg.contains("discovery parsed 0 events"), "got: {msg}");
        assert!(msg.ends_with(')'), "got: {msg}");
        assert!(
            msg.contains("/events —") || msg.contains("/events "),
            "the legacy endpoint must be named, got: {msg}"
        );
        assert!(!msg.contains("/events/keyset"), "got: {msg}");
    }

    /// The cursor may arrive under any of the reported names, at the top level or nested,
    /// and an empty one means "no more pages".
    #[test]
    fn next_cursor_is_read_from_every_reported_shape() {
        assert_eq!(
            parse_next_cursor(r#"{"data":[],"next_cursor":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_next_cursor(r#"{"data":[],"nextCursor":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_next_cursor(r#"{"data":[],"cursor":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_next_cursor(r#"{"data":[],"pagination":{"next_cursor":"abc"}}"#).as_deref(),
            Some("abc")
        );
        // Numeric cursors are cursors too.
        assert_eq!(
            parse_next_cursor(r#"{"next_cursor":1234}"#).as_deref(),
            Some("1234")
        );
        // Absent, empty, blank, null or not-an-object all mean "stop".
        assert_eq!(parse_next_cursor(r#"{"data":[]}"#), None);
        assert_eq!(parse_next_cursor(r#"{"next_cursor":""}"#), None);
        assert_eq!(parse_next_cursor(r#"{"next_cursor":"  "}"#), None);
        assert_eq!(parse_next_cursor(r#"{"next_cursor":null}"#), None);
        assert_eq!(parse_next_cursor("[]"), None);
        assert_eq!(parse_next_cursor("not json"), None);
    }

    /// The cursor parameter name is the one unverified piece of the request, so it is
    /// configurable — and the shipped default is the one the ecosystem reports working.
    #[tokio::test]
    async fn the_cursor_parameter_name_is_configurable() {
        assert_eq!(
            Config::default().scan.keyset_cursor_param,
            DEFAULT_KEYSET_CURSOR_PARAM
        );
        assert_eq!(DEFAULT_KEYSET_CURSOR_PARAM, "after_cursor");

        // A deployment where only `cursor=` advances: two pages, driven by the override.
        let (base, log) = gamma_stub(|line| {
            let start: usize = query_param(line, "cursor")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let body = if start == 0 {
                format!(r#"{{"data":{},"next_cursor":"5"}}"#, events_json(0..5))
            } else {
                format!(r#"{{"data":{}}}"#, events_json(5..8))
            };
            (200, body)
        })
        .await;
        let mut cfg = paging_config(&base, 5, 6_000);
        cfg.scan.keyset_cursor_param = "cursor".into();
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(universe.events.len(), 8);
        assert!(!stats.truncated);
        assert_eq!(
            query_param(&requests(&log)[1], "cursor").as_deref(),
            Some("5")
        );
    }

    /// The shipped default must be high enough that the live universe is not silently
    /// clipped. 500 was the original guess, 2 000 was raised to after that clipped, and a
    /// live overnight run then filled all 20 pages of *that* — 8.3k markets / 16.6k tokens —
    /// so the real event count is above 2 000 too.
    #[test]
    fn shipped_max_events_default_covers_the_observed_live_universe() {
        assert_eq!(
            Config::default().scan.max_events,
            6_000,
            "scan.max_events must be a rate-limit backstop, not the usual stopping point"
        );
    }
}
