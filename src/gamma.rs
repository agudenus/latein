//! Gamma API client — market discovery.
//!
//! `GET https://gamma-api.polymarket.com/events?active=true&closed=false` returns events,
//! each with a `negRisk` flag and a nested `markets[]` array. We keep NegRisk events (the
//! primary strategy) and the binary markets inside every event.
//!
//! Parsing is deliberately permissive: unknown fields are ignored, every field we do not
//! strictly need is optional, and prices/ids arrive as strings. The wire shapes marked
//! `TODO(verify-live)` are inferred from the public docs and must be confirmed against a
//! real response (this container cannot reach the API).

use serde::Deserialize;

use crate::config::Config;
use crate::http::{ApiError, HttpClient};
use crate::types::{Category, TokenId, TrackedEvent, TrackedMarket, Universe};

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
    /// True when pagination stopped at `scan.max_events` rather than at the end of the
    /// event list — discovery is incomplete and the numbers above are a lower bound.
    pub truncated: bool,
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
    /// TODO(verify-live): confirm the exact casing (`negRisk`) on `/events`.
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
    /// TODO(verify-live): confirm the field name (`endDate`) and that it is RFC 3339.
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
    /// TODO(verify-live): confirm Gamma still double-encodes this (it may send a real
    /// array); `de_opt_string_array` accepts both shapes.
    #[serde(
        default,
        rename = "clobTokenIds",
        deserialize_with = "de_opt_string_array"
    )]
    pub clob_token_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "de_opt_string_array")]
    pub outcomes: Option<Vec<String>>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    /// TODO(verify-live): markets without a CLOB book cannot be traded; confirm the field
    /// name (`enableOrderBook`) before relying on it as a filter.
    #[serde(default, rename = "enableOrderBook")]
    pub enable_order_book: Option<bool>,
}

// ---------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------

/// Parse one `/events` page. Accepts either a bare array or `{"data": [...]}`.
pub fn parse_events_page(url: &str, body: &str) -> Result<Vec<RawEvent>, ApiError> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Page {
        Bare(Vec<RawEvent>),
        // TODO(verify-live): some Gamma endpoints wrap results in `{data, pagination}`.
        Wrapped { data: Vec<RawEvent> },
    }

    match serde_json::from_str::<Page>(body) {
        Ok(Page::Bare(v)) | Ok(Page::Wrapped { data: v }) => Ok(v),
        Err(source) => Err(ApiError::Decode {
            url: url.to_string(),
            source,
        }),
    }
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
    })
}

// ---------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------

pub struct GammaClient<'a> {
    http: &'a HttpClient,
    base_url: String,
    page_size: usize,
    max_events: usize,
}

impl<'a> GammaClient<'a> {
    pub fn new(http: &'a HttpClient, cfg: &Config) -> Self {
        Self {
            http,
            base_url: cfg.api.gamma_base_url.trim_end_matches('/').to_string(),
            page_size: cfg.scan.page_size,
            max_events: cfg.scan.max_events,
        }
    }

    /// Paginate `/events?active=true&closed=false` until the API returns a short or empty
    /// page — i.e. until the event list is genuinely exhausted.
    ///
    /// `scan.max_events` is a rate-limit backstop, not the intended stopping point. When
    /// it is what stops us, discovery is incomplete: the universe is missing events, and
    /// (worse) a NegRisk event can be split across the boundary. That is loud, not silent.
    pub async fn fetch_universe(&self) -> Result<(Universe, DiscoveryStats), ApiError> {
        let url = format!("{}/events", self.base_url);
        let mut raw: Vec<RawEvent> = Vec::new();
        let mut offset = 0usize;
        let mut pages = 0usize;
        let mut truncated = false;

        loop {
            let remaining = self.max_events.saturating_sub(raw.len());
            if remaining == 0 {
                truncated = true;
                break;
            }
            let limit = self.page_size.min(remaining);
            let query = [
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ];
            let body = self.http.get_json(&url, &query).await?;
            let page = parse_events_page(&url, &body)?;
            let got = page.len();
            pages += 1;
            tracing::debug!(offset, got, limit, "fetched gamma events page");
            raw.extend(page);
            // A short (or empty) page is the end of the list — the only clean stop.
            if got < limit {
                break;
            }
            offset += got;
        }

        if truncated {
            tracing::warn!(
                max_events = self.max_events,
                page_size = self.page_size,
                pages,
                events = raw.len(),
                "universe truncated at max_events — discovery is incomplete; \
                 raise scan.max_events (or pass --limit) to cover the whole event list"
            );
        }

        let (universe, mut stats) = build_universe(&raw);
        stats.truncated = truncated;
        tracing::info!(
            events_seen = stats.events_seen,
            events_kept = stats.events_kept,
            markets_seen = stats.markets_seen,
            markets_kept = stats.markets_kept,
            markets_dropped = stats.markets_dropped(),
            drop_reasons = %stats.drops.summary(),
            "market discovery drop breakdown"
        );
        Ok((universe, stats))
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

    /// Serve `total` synthetic events, honouring `limit`/`offset`, and return the base URL
    /// plus a counter of how many pages were requested.
    async fn paging_gamma(
        total: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let param = |name: &str| -> usize {
                        head.split(&format!("{name}="))
                            .nth(1)
                            .and_then(|rest| {
                                rest.split(['&', ' ']).next().and_then(|v| v.parse().ok())
                            })
                            .unwrap_or(0)
                    };
                    let (limit, offset) = (param("limit"), param("offset"));
                    counter.fetch_add(1, Ordering::SeqCst);
                    let events: Vec<String> = (offset..(offset + limit).min(total))
                        .map(|i| {
                            format!(
                                r#"{{"id":"{i}","slug":"e{i}","active":true,"closed":false,
                                   "markets":[{{"conditionId":"0x{i}",
                                   "clobTokenIds":"[\"{i}a\",\"{i}b\"]"}}]}}"#
                            )
                        })
                        .collect();
                    let body = format!("[{}]", events.join(","));
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), requests)
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

    /// Two full pages then a short one: pagination must continue past the full pages and
    /// stop only on the short page, collecting every event.
    #[tokio::test]
    async fn pagination_runs_until_a_short_page() {
        let (base, requests) = paging_gamma(25).await;
        let cfg = paging_config(&base, 10, 2_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (universe, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(stats.events_seen, 25, "every event must be collected");
        assert_eq!(universe.events.len(), 25);
        assert_eq!(universe.market_count(), 25);
        // 10 + 10 + 5: the third page is short, so it is also the last.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(!stats.truncated, "the cap must not have been the stopper");
    }

    /// An exactly-full last page is not the end of the list: the next request must still
    /// be made, and it returns empty.
    #[tokio::test]
    async fn pagination_probes_past_an_exactly_full_last_page() {
        let (base, requests) = paging_gamma(20).await;
        let cfg = paging_config(&base, 10, 2_000);
        let http = HttpClient::new(&cfg.api).expect("client");
        let (_, stats) = GammaClient::new(&http, &cfg)
            .fetch_universe()
            .await
            .expect("fetch");

        assert_eq!(stats.events_seen, 20);
        // 10 + 10 + 0: the empty third page is what proves the list is exhausted.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(!stats.truncated);
    }

    /// The `events=500` symptom: `max_events` — not the API — ends discovery. That is the
    /// bug that hid whole NegRisk outcome sets, so it must be reported, not silent.
    #[tokio::test]
    async fn hitting_max_events_is_reported_as_truncation() {
        let (base, _requests) = paging_gamma(60).await;
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

    /// The shipped default must be high enough that the live universe (~500+ events and
    /// growing) is not silently clipped.
    #[test]
    fn shipped_max_events_default_is_not_the_old_500_cap() {
        assert!(
            Config::default().scan.max_events >= 2_000,
            "scan.max_events must be a rate-limit backstop, not the usual stopping point"
        );
    }
}
