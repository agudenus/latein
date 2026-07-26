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

/// What the discovery pass kept and dropped — printed by `polyarb markets` so the
/// tracked universe is auditable rather than a black box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryStats {
    pub events_seen: usize,
    pub events_kept: usize,
    pub events_inactive: usize,
    pub markets_seen: usize,
    pub markets_kept: usize,
    pub markets_inactive: usize,
    /// Dropped because the token-id / outcome arrays were missing or not a clean pair.
    pub markets_unusable: usize,
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
        // `active=true&closed=false` is requested server-side; re-check because query
        // filters are not a contract.
        if ev.closed == Some(true) || ev.active == Some(false) {
            stats.events_inactive += 1;
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
            stats.markets_seen += 1;
            if m.closed == Some(true)
                || m.active == Some(false)
                || m.enable_order_book == Some(false)
            {
                stats.markets_inactive += 1;
                continue;
            }
            match to_tracked_market(m) {
                Some(tm) => {
                    stats.markets_kept += 1;
                    markets.push(tm);
                }
                None => stats.markets_unusable += 1,
            }
        }

        if markets.is_empty() {
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
            markets,
        });
    }

    (Universe { events }, stats)
}

fn to_tracked_market(m: &RawMarket) -> Option<TrackedMarket> {
    let condition_id = m.condition_id.clone()?;
    let tokens = m.clob_token_ids.as_ref()?;
    // Exactly two outcome tokens, or it is not a binary condition we can price.
    if tokens.len() != 2 || tokens.iter().any(|t| t.trim().is_empty()) {
        return None;
    }
    let outcomes = match m.outcomes.as_ref() {
        Some(o) if o.len() == 2 => [o[0].clone(), o[1].clone()],
        // Polymarket's binary convention when the field is absent.
        _ => ["Yes".to_string(), "No".to_string()],
    };
    Some(TrackedMarket {
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

    /// Paginate `/events?active=true&closed=false` until the API runs out of pages or we
    /// hit `scan.max_events`.
    pub async fn fetch_universe(&self) -> Result<(Universe, DiscoveryStats), ApiError> {
        let url = format!("{}/events", self.base_url);
        let mut raw: Vec<RawEvent> = Vec::new();
        let mut offset = 0usize;

        while raw.len() < self.max_events {
            let limit = self.page_size.min(self.max_events - raw.len());
            let query = [
                ("active", "true".to_string()),
                ("closed", "false".to_string()),
                ("limit", limit.to_string()),
                ("offset", offset.to_string()),
            ];
            let body = self.http.get_json(&url, &query).await?;
            let page = parse_events_page(&url, &body)?;
            let got = page.len();
            tracing::debug!(offset, got, "fetched gamma events page");
            raw.extend(page);
            if got < limit {
                break;
            }
            offset += got;
        }

        Ok(build_universe(&raw))
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
        assert_eq!(stats.markets_inactive, 1);
        assert_eq!(stats.markets_unusable, 2);
        assert_eq!(stats.markets_kept, 3);

        assert_eq!(universe.events.len(), 2);
        assert_eq!(universe.neg_risk_event_count(), 1);
        assert_eq!(universe.market_count(), 3);
        assert_eq!(universe.token_ids().len(), 6);

        let ev = &universe.events[0];
        assert!(ev.neg_risk);
        assert_eq!(ev.category.as_str(), "politics");
        assert_eq!(ev.markets.len(), 2);
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
}
