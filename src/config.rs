//! Configuration: `config/default.toml` + selected environment overrides.
//!
//! No secrets live here. Per CLAUDE.md, credentials only ever come from the environment,
//! and Phase A needs none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::Category;

pub const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("environment variable {var} is not a valid {expected}: {value:?}")]
    Env {
        var: &'static str,
        expected: &'static str,
        value: String,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Only `dry-run` is implemented in Phase A. Live trading is a later, explicitly
    /// gated milestone (CLAUDE.md).
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub risk: RiskConfig,
    /// M3 dry-run daemon: loop cadences and the daily-summary clock.
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// M3 opportunity lifecycle re-polling.
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    /// M6 WebSocket market-data stream. Off → the daemon polls REST as before.
    #[serde(default)]
    pub stream: StreamConfig,
    /// M3 alerting. Credentials are *never* here — only in the environment.
    #[serde(default)]
    pub alerts: AlertConfig,
    /// M3 on-disk locations for the database, the JSONL event log and the reports.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Category → taker fee rate. Verified against docs.polymarket.com 2026-07; kept in
    /// config because the protocol can change them.
    #[serde(default = "default_fee_rates")]
    pub fees: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub gamma_base_url: String,
    pub clob_base_url: String,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    /// Client-side throttle between HTTP requests to one host.
    pub min_request_interval_ms: u64,
    /// Token ids per `POST /books` call.
    pub books_batch_size: usize,
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    /// Gamma `/events` page size.
    pub page_size: usize,
    /// Hard stop on Gamma `/events` pagination — a rate-limit backstop, **not** the
    /// intended stopping point. Normal discovery ends when the API returns a short page.
    /// When this cap is what stops us the universe is incomplete (a NegRisk event can
    /// even be split across the boundary), so `fetch_universe` logs
    /// `WARN universe truncated at max_events`. Keep it comfortably above the live event
    /// count (~500 and growing).
    pub max_events: usize,
    /// Skip NegRisk events with more outcomes than this (leg count blows up capital).
    pub max_negrisk_outcomes: usize,
    /// Report NegRisk sweeps that cover only part of an event's outcome set.
    ///
    /// Off by default, and deliberately so: if discovery dropped an outcome, buying the
    /// tracked subset is not a lock — the dropped outcome can win and pay us nothing.
    /// When enabled, such a sweep is surfaced as `relative-value` with a
    /// `partial_coverage` flag and is never labelled `true-arb`. The NO-side construction
    /// is suppressed regardless, because its `$(N−1)` payout is only valid for a complete
    /// sweep.
    pub report_partial_negrisk: bool,
    /// Smallest share count worth reporting.
    pub min_size_shares: Decimal,
    /// Binary-search resolution for the depth walker, in shares.
    pub size_search_tolerance: Decimal,
    /// Minimum net profit per share, by category, with a required `default` key.
    ///
    /// Each entry carries a taker floor and an optional maker floor, because the two sides
    /// have different economics: a taker pays `rate · p · (1−p)` per leg, a maker pays
    /// nothing (and is rebate-subsidised). In the 0.07 crypto tier the pair fee at
    /// mid prices is ~0.035/share, so a thin taker gap there is noise while the same gap
    /// captured as a maker is real — hence `crypto = { taker = 0.008, maker = 0.005 }`.
    pub floors: BTreeMap<String, CategoryFloor>,
    /// Report gaps whose taker net dies to fees/slippage but whose maker net clears the
    /// floor. Maker capture is fee-free and rebate-subsidised, but carries legging risk.
    pub report_maker_only: bool,
    /// If non-empty, only scan these categories.
    pub include_categories: Vec<String>,
}

/// Net-per-share floors for one category.
///
/// `maker` is optional and defaults to `taker`, so a category that needs no side-specific
/// treatment stays a one-liner: `geopolitics = { taker = 0.003 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryFloor {
    /// Minimum net-per-share a depth-walked *taker* construction must clear.
    pub taker: Decimal,
    /// Minimum net-per-share a *maker-only* construction must clear. Absent = same as
    /// `taker`. Lowering it below `taker` is the deliberate shape for high-fee categories:
    /// makers pay no fee, so a gap that is noise as a taker can still be worth resting for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<Decimal>,
}

impl CategoryFloor {
    /// One floor for both sides.
    pub fn flat(floor: Decimal) -> Self {
        Self {
            taker: floor,
            maker: None,
        }
    }

    pub fn sided(taker: Decimal, maker: Decimal) -> Self {
        Self {
            taker,
            maker: Some(maker),
        }
    }

    /// The maker floor, resolving the "absent = same as taker" default.
    pub fn maker_floor(&self) -> Decimal {
        self.maker.unwrap_or(self.taker)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    /// Hard per-opportunity capital cap in USDC. Applied inside the depth walker.
    pub per_trade_cap_usd: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Seconds between book scans.
    pub scan_interval_secs: u64,
    /// Seconds between market-universe (Gamma) refreshes. Much slower than the book scan:
    /// events appear and close on a human timescale, books move continuously.
    pub universe_refresh_secs: u64,
    /// UTC wall-clock time (`HH:MM`) at which the daily summary is generated and alerted.
    pub daily_summary_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleConfig {
    /// Seconds between re-polls of a detected opportunity's legs.
    pub repoll_interval_secs: u64,
    /// How long to keep re-polling before closing the opportunity out.
    pub repoll_window_secs: u64,
    /// Cap on concurrently tracked opportunities; the excess is recorded `untracked`
    /// rather than silently distorting the persistence statistics.
    pub max_concurrent: usize,
}

/// M6 streaming market data.
///
/// With the stream on, detection is event-driven: books are maintained from pushed frames
/// and only the events whose books moved are re-evaluated. REST does not go away — it
/// seeds the books at startup, re-fetches anything stale, sweeps the whole universe on a
/// slow cadence as an integrity net, and takes over completely if the socket cannot be
/// reached (see `fallback_after_failures`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    /// Master switch. `false` = the pure-REST daemon, unchanged.
    pub enabled: bool,
    /// CLOB market-channel endpoint.
    ///
    /// TODO(verify-live): unverified from this container — confirm the host, path and
    /// casing against docs.polymarket.com before trusting a live run.
    pub url: String,
    /// Token subscriptions per connection; the universe is sharded across as many
    /// connections as this implies. TODO(verify-live): the real per-connection limit (if
    /// any) is unknown; 500 is a deliberately conservative guess.
    pub max_subs_per_connection: usize,
    /// Quiet period after the last book update before the dirty events are re-evaluated.
    /// Small enough to stay in the tens of milliseconds, large enough that a burst across
    /// an event's legs is one detection pass rather than one per leg.
    pub debounce_ms: u64,
    /// A book not updated for this long is re-fetched over REST. This is the bound on how
    /// wrong a silently-dead subscription can make us.
    pub stale_after_secs: u64,
    /// Cadence of the full REST sweep: re-fetch every book, cross-check a random sample
    /// against the local state, and run detection over the whole universe.
    pub resync_interval_secs: u64,
    /// Consecutive failed connection attempts before streaming is abandoned for the life
    /// of the process (logged at ERROR) and the daemon reverts to REST polling.
    pub fallback_after_failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertConfig {
    /// Master switch for outbound alerts. The JSONL event log is written either way.
    pub enabled: bool,
    /// Minimum net profit per share for an opportunity to be alerted (maker net is used
    /// for maker-only constructions). The fallback for any category not listed in
    /// `alert_min_net_by_category`.
    pub alert_min_net_per_share: Decimal,
    /// Per-category alert thresholds, overriding `alert_min_net_per_share`.
    ///
    /// Crypto ships at 0.008 — the crypto taker floor — so a crypto gap that survived the
    /// harshest fee tier is never silently swallowed by a threshold tuned for the far
    /// busier politics flow. The volume stays bounded because that same higher floor is
    /// what the detector already had to clear.
    #[serde(default = "default_alert_min_net_by_category")]
    pub alert_min_net_by_category: BTreeMap<String, Decimal>,
    /// Anti-spam spacing between routine (opportunity) alerts. Daily summaries ignore it.
    pub min_seconds_between_alerts: u64,
    /// Consecutive delivery failures before the circuit breaker opens.
    pub failure_circuit_break: u32,
    /// How long the breaker stays open before the next attempt is allowed through.
    pub circuit_reprobe_secs: u64,
    /// Bot API base. Overridable so tests never touch the real host.
    pub telegram_api_base: String,
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// SQLite file. Parent directories are created on demand.
    pub database_path: String,
    /// Directory for `events.jsonl`.
    pub log_dir: String,
    /// Directory for the generated daily-summary markdown.
    pub report_dir: String,
}

fn default_mode() -> String {
    "dry-run".to_string()
}

/// Categories that always get a line in the daily summary, even at zero, and the order
/// they appear in. The MVP focus set (CLAUDE.md): politics/NegRisk is primary, sports
/// secondary, geopolitics the fee-free opportunistic tier, crypto the staged second focus.
/// "crypto: 0 opportunities" is information — it is the soak's evidence that the current
/// refresh cadence is not finding any.
pub const REPORTED_CATEGORIES: [&str; 4] = ["politics", "sports", "geopolitics", "crypto"];

fn default_alert_min_net_by_category() -> BTreeMap<String, Decimal> {
    // Matches the crypto taker floor (0.008): anything the detector let through in the
    // 0.07 fee tier is rare enough to be worth seeing.
    [("crypto".to_string(), Decimal::new(8, 3))]
        .into_iter()
        .collect()
}

fn default_fee_rates() -> BTreeMap<String, Decimal> {
    // Verified against docs.polymarket.com/trading/fees, 2026-07.
    [
        ("geopolitics", Decimal::new(0, 2)),
        ("politics", Decimal::new(4, 2)),
        ("finance", Decimal::new(4, 2)),
        ("tech", Decimal::new(4, 2)),
        ("mentions", Decimal::new(4, 2)),
        ("sports", Decimal::new(5, 2)),
        ("economics", Decimal::new(5, 2)),
        ("culture", Decimal::new(5, 2)),
        ("weather", Decimal::new(5, 2)),
        ("other", Decimal::new(5, 2)),
        ("crypto", Decimal::new(7, 2)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            gamma_base_url: "https://gamma-api.polymarket.com".to_string(),
            clob_base_url: "https://clob.polymarket.com".to_string(),
            request_timeout_secs: 20,
            max_retries: 3,
            min_request_interval_ms: 120,
            books_batch_size: 100,
            user_agent: concat!("polyarb/", env!("CARGO_PKG_VERSION")).to_string(),
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            max_events: 2_000,
            max_negrisk_outcomes: 30,
            report_partial_negrisk: false,
            min_size_shares: Decimal::new(5, 0),
            size_search_tolerance: Decimal::new(1, 2),
            floors: [
                (
                    "default".to_string(),
                    CategoryFloor::flat(Decimal::new(5, 3)),
                ),
                (
                    "geopolitics".to_string(),
                    // Fee-free, so a thinner gap still survives.
                    CategoryFloor::flat(Decimal::new(3, 3)),
                ),
                (
                    "crypto".to_string(),
                    // 0.07 tier: the pair fee at mid prices is ~0.035/share, so a taker
                    // gap under ~0.008 is noise. Maker capture is the realistic path
                    // there, and it pays no fee — so its floor stays at the default.
                    CategoryFloor::sided(Decimal::new(8, 3), Decimal::new(5, 3)),
                ),
            ]
            .into_iter()
            .collect(),
            report_maker_only: true,
            include_categories: Vec::new(),
        }
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            per_trade_cap_usd: Decimal::new(50, 0),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            scan_interval_secs: 5,
            universe_refresh_secs: 600,
            daily_summary_utc: "23:55".to_string(),
        }
    }
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            repoll_interval_secs: 2,
            repoll_window_secs: 30,
            max_concurrent: 32,
        }
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            max_subs_per_connection: 500,
            debounce_ms: 50,
            stale_after_secs: 60,
            resync_interval_secs: 300,
            fallback_after_failures: 5,
        }
    }
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alert_min_net_per_share: Decimal::new(1, 2),
            alert_min_net_by_category: default_alert_min_net_by_category(),
            min_seconds_between_alerts: 30,
            failure_circuit_break: 3,
            circuit_reprobe_secs: 900,
            telegram_api_base: "https://api.telegram.org".to_string(),
            request_timeout_secs: 10,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_path: "data/polyarb.sqlite".to_string(),
            log_dir: "logs".to_string(),
            report_dir: "reports".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            api: ApiConfig::default(),
            scan: ScanConfig::default(),
            risk: RiskConfig::default(),
            daemon: DaemonConfig::default(),
            lifecycle: LifecycleConfig::default(),
            stream: StreamConfig::default(),
            alerts: AlertConfig::default(),
            storage: StorageConfig::default(),
            fees: default_fee_rates(),
        }
    }
}

impl Config {
    /// Load from `path` (or `$POLYARB_CONFIG`, or the default path if it exists), then
    /// apply environment overrides and validate.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let explicit = path.map(PathBuf::from).or_else(|| {
            std::env::var("POLYARB_CONFIG")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        });

        let mut cfg = match explicit {
            Some(p) => Self::from_file(&p)?,
            None => {
                let default_path = PathBuf::from(DEFAULT_CONFIG_PATH);
                if default_path.exists() {
                    Self::from_file(&default_path)?
                } else {
                    Self::default()
                }
            }
        };

        cfg.apply_env_overrides()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    fn apply_env_overrides(&mut self) -> Result<(), ConfigError> {
        if let Some(v) = env_str("POLYARB_GAMMA_BASE_URL") {
            self.api.gamma_base_url = v;
        }
        if let Some(v) = env_str("POLYARB_CLOB_BASE_URL") {
            self.api.clob_base_url = v;
        }
        if let Some(v) = env_decimal("POLYARB_PER_TRADE_CAP_USD")? {
            self.risk.per_trade_cap_usd = v;
        }
        if let Some(v) = env_decimal("POLYARB_NET_FLOOR_DEFAULT")? {
            // Only the taker floor moves; a configured maker floor is left alone (and an
            // absent one keeps following the taker floor, as before).
            self.scan
                .floors
                .entry("default".to_string())
                .and_modify(|f| f.taker = v)
                .or_insert_with(|| CategoryFloor::flat(v));
        }
        if let Some(v) = env_str("POLYARB_MODE") {
            self.mode = v;
        }
        if let Some(v) = env_str("POLYARB_DB_PATH") {
            self.storage.database_path = v;
        }
        if let Some(v) = env_str("POLYARB_LOG_DIR") {
            self.storage.log_dir = v;
        }
        if let Some(v) = env_str("POLYARB_REPORT_DIR") {
            self.storage.report_dir = v;
        }
        if let Some(v) = env_u64("POLYARB_SCAN_INTERVAL_SECS")? {
            self.daemon.scan_interval_secs = v;
        }
        if let Some(v) = env_decimal("POLYARB_ALERT_MIN_NET")? {
            self.alerts.alert_min_net_per_share = v;
        }
        if let Some(v) = env_str("POLYARB_STREAM_URL") {
            self.stream.url = v;
        }
        if let Some(v) = env_bool("POLYARB_STREAM_ENABLED")? {
            self.stream.enabled = v;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.mode != "dry-run" {
            return Err(ConfigError::Invalid(format!(
                "mode must be \"dry-run\" in Phase A (got {:?}); live trading is not implemented",
                self.mode
            )));
        }
        if self.risk.per_trade_cap_usd <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "risk.per_trade_cap_usd must be > 0".into(),
            ));
        }
        if self.scan.min_size_shares <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "scan.min_size_shares must be > 0".into(),
            ));
        }
        if self.scan.size_search_tolerance <= Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "scan.size_search_tolerance must be > 0".into(),
            ));
        }
        if self.scan.page_size == 0 || self.api.books_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "scan.page_size and api.books_batch_size must be > 0".into(),
            ));
        }
        if !self.scan.floors.contains_key("default") {
            return Err(ConfigError::Invalid(
                "scan.floors must contain a \"default\" entry".into(),
            ));
        }
        for (name, floor) in &self.scan.floors {
            // A negative floor would accept a construction that loses money at every size.
            if floor.taker < Decimal::ZERO || floor.maker_floor() < Decimal::ZERO {
                return Err(ConfigError::Invalid(format!(
                    "scan.floors.{name}: taker and maker floors must be >= 0"
                )));
            }
        }
        if !self.fees.contains_key(Category::OTHER) {
            return Err(ConfigError::Invalid(
                "fees must contain an \"other\" entry (used as the fallback rate)".into(),
            ));
        }

        // ---- M3 daemon ---------------------------------------------------------------
        if self.daemon.scan_interval_secs == 0 || self.daemon.universe_refresh_secs == 0 {
            return Err(ConfigError::Invalid(
                "daemon.scan_interval_secs and daemon.universe_refresh_secs must be > 0".into(),
            ));
        }
        self.daily_summary_time()?;
        if self.lifecycle.repoll_interval_secs == 0 || self.lifecycle.repoll_window_secs == 0 {
            return Err(ConfigError::Invalid(
                "lifecycle.repoll_interval_secs and lifecycle.repoll_window_secs must be > 0"
                    .into(),
            ));
        }
        if self.lifecycle.repoll_interval_secs > self.lifecycle.repoll_window_secs {
            return Err(ConfigError::Invalid(
                "lifecycle.repoll_interval_secs must not exceed lifecycle.repoll_window_secs \
                 (otherwise no re-poll ever lands and nothing is ever confirmed)"
                    .into(),
            ));
        }
        if self.lifecycle.max_concurrent == 0 {
            return Err(ConfigError::Invalid(
                "lifecycle.max_concurrent must be > 0".into(),
            ));
        }
        // ---- M6 stream ---------------------------------------------------------------
        if self.stream.enabled {
            let url = self.stream.url.trim();
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                return Err(ConfigError::Invalid(format!(
                    "stream.url must be a ws:// or wss:// URL (got {:?})",
                    self.stream.url
                )));
            }
            if self.stream.max_subs_per_connection == 0 {
                return Err(ConfigError::Invalid(
                    "stream.max_subs_per_connection must be > 0".into(),
                ));
            }
            if self.stream.stale_after_secs == 0 || self.stream.resync_interval_secs == 0 {
                return Err(ConfigError::Invalid(
                    "stream.stale_after_secs and stream.resync_interval_secs must be > 0".into(),
                ));
            }
            if self.stream.fallback_after_failures == 0 {
                return Err(ConfigError::Invalid(
                    "stream.fallback_after_failures must be > 0 (0 would fall back before the \
                     first connection attempt)"
                        .into(),
                ));
            }
            // A debounce longer than the scan interval would make the "event-driven" path
            // slower than the polling it replaces.
            if self.stream.debounce_ms >= self.daemon.scan_interval_secs.saturating_mul(1_000) {
                return Err(ConfigError::Invalid(format!(
                    "stream.debounce_ms ({}) must be well under daemon.scan_interval_secs ({} s) \
                     — otherwise streaming detects no faster than polling",
                    self.stream.debounce_ms, self.daemon.scan_interval_secs
                )));
            }
        }

        if self.alerts.alert_min_net_per_share < Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "alerts.alert_min_net_per_share must be >= 0".into(),
            ));
        }
        for (name, threshold) in &self.alerts.alert_min_net_by_category {
            if *threshold < Decimal::ZERO {
                return Err(ConfigError::Invalid(format!(
                    "alerts.alert_min_net_by_category.{name} must be >= 0"
                )));
            }
        }
        if self.alerts.failure_circuit_break == 0 {
            return Err(ConfigError::Invalid(
                "alerts.failure_circuit_break must be > 0".into(),
            ));
        }
        if self.alerts.request_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "alerts.request_timeout_secs must be > 0".into(),
            ));
        }
        if self.alerts.telegram_api_base.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "alerts.telegram_api_base must not be empty".into(),
            ));
        }
        if self.storage.database_path.trim().is_empty()
            || self.storage.log_dir.trim().is_empty()
            || self.storage.report_dir.trim().is_empty()
        {
            return Err(ConfigError::Invalid(
                "storage.database_path, storage.log_dir and storage.report_dir must be set".into(),
            ));
        }
        Ok(())
    }

    /// Parsed `daemon.daily_summary_utc`.
    pub fn daily_summary_time(&self) -> Result<chrono::NaiveTime, ConfigError> {
        chrono::NaiveTime::parse_from_str(self.daemon.daily_summary_utc.trim(), "%H:%M").map_err(
            |_| {
                ConfigError::Invalid(format!(
                    "daemon.daily_summary_utc must be UTC \"HH:MM\" (got {:?})",
                    self.daemon.daily_summary_utc
                ))
            },
        )
    }

    /// Both net-per-share floors for a category, falling back to the `default` entry.
    pub fn floor_for(&self, category: &Category) -> CategoryFloor {
        self.scan
            .floors
            .get(category.as_str())
            .or_else(|| self.scan.floors.get("default"))
            .copied()
            .unwrap_or_else(|| CategoryFloor::flat(Decimal::ZERO))
    }

    /// Minimum acceptable net profit per share for a depth-walked taker construction.
    pub fn net_floor_taker(&self, category: &Category) -> Decimal {
        self.floor_for(category).taker
    }

    /// Minimum acceptable net profit per share for a maker-only construction.
    pub fn net_floor_maker(&self, category: &Category) -> Decimal {
        self.floor_for(category).maker_floor()
    }

    pub fn category_included(&self, category: &Category) -> bool {
        self.scan.include_categories.is_empty()
            || self
                .scan
                .include_categories
                .iter()
                .any(|c| c.eq_ignore_ascii_case(category.as_str()))
    }
}

impl AlertConfig {
    /// Alert threshold for a category: its own entry, else `alert_min_net_per_share`.
    pub fn min_net_for(&self, category: &Category) -> Decimal {
        self.alert_min_net_by_category
            .get(category.as_str())
            .copied()
            .unwrap_or(self.alert_min_net_per_share)
    }
}

fn env_str(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_u64(var: &'static str) -> Result<Option<u64>, ConfigError> {
    match env_str(var) {
        None => Ok(None),
        Some(raw) => raw.parse::<u64>().map(Some).map_err(|_| ConfigError::Env {
            var,
            expected: "positive integer",
            value: raw,
        }),
    }
}

fn env_bool(var: &'static str) -> Result<Option<bool>, ConfigError> {
    match env_str(var) {
        None => Ok(None),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(ConfigError::Env {
                var,
                expected: "boolean",
                value: raw,
            }),
        },
    }
}

fn env_decimal(var: &'static str) -> Result<Option<Decimal>, ConfigError> {
    match env_str(var) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<Decimal>()
            .map(Some)
            .map_err(|_| ConfigError::Env {
                var,
                expected: "decimal",
                value: raw,
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const SHIPPED: &str = include_str!("../config/default.toml");

    #[test]
    fn shipped_default_config_parses_and_validates() {
        let cfg = Config::from_toml_str(SHIPPED).expect("config/default.toml must parse");
        cfg.validate().expect("config/default.toml must validate");
        assert_eq!(cfg.mode, "dry-run");
        assert_eq!(cfg.risk.per_trade_cap_usd, dec!(50));
        assert_eq!(cfg.fees["geopolitics"], dec!(0));
        assert_eq!(cfg.fees["politics"], dec!(0.04));
        assert_eq!(cfg.fees["sports"], dec!(0.05));
        assert_eq!(cfg.fees["crypto"], dec!(0.07));
        // The shipped floor table must agree with the compiled defaults.
        assert_eq!(cfg.scan.floors, Config::default().scan.floors);
        assert_eq!(cfg.net_floor_taker(&Category::new("crypto")), dec!(0.008));
        assert_eq!(cfg.net_floor_maker(&Category::new("crypto")), dec!(0.005));
        assert_eq!(
            cfg.alerts.min_net_for(&Category::new("crypto")),
            dec!(0.008)
        );
    }

    #[test]
    fn net_floor_falls_back_to_default() {
        let cfg = Config::default();
        assert_eq!(
            cfg.net_floor_taker(&Category::new("geopolitics")),
            dec!(0.003)
        );
        assert_eq!(cfg.net_floor_taker(&Category::new("politics")), dec!(0.005));
        assert_eq!(cfg.net_floor_taker(&Category::new("nonsense")), dec!(0.005));
    }

    /// The crypto override is the point of the per-side floor table: a higher taker floor
    /// (the 0.07 tier eats ~0.035/share at mid prices) with the maker floor left at the
    /// default, because a maker pays no fee at all.
    #[test]
    fn crypto_floor_is_sided_and_other_categories_follow_the_default() {
        let cfg = Config::default();
        let crypto = Category::new("crypto");
        assert_eq!(cfg.net_floor_taker(&crypto), dec!(0.008));
        assert_eq!(cfg.net_floor_maker(&crypto), dec!(0.005));

        // A category with no maker override uses its own taker floor on both sides.
        for name in ["politics", "geopolitics", "sports", "not-a-category"] {
            let c = Category::new(name);
            assert_eq!(
                cfg.net_floor_maker(&c),
                cfg.net_floor_taker(&c),
                "{name} must have a single effective floor"
            );
        }
        assert_eq!(
            cfg.net_floor_maker(&Category::new("geopolitics")),
            dec!(0.003)
        );
        // Unknown categories fall through to `default`, on both sides.
        assert_eq!(cfg.net_floor_maker(&Category::new("nonsense")), dec!(0.005));
    }

    #[test]
    fn per_category_floors_parse_from_toml_and_reject_typos_and_negatives() {
        const SHIPPED_CRYPTO: &str = "crypto = { taker = 0.008, maker = 0.005 }";
        assert!(
            SHIPPED.contains(SHIPPED_CRYPTO),
            "shipped floor entry moved"
        );

        let cfg = Config::from_toml_str(
            &SHIPPED.replace(SHIPPED_CRYPTO, "crypto = { taker = 0.01, maker = 0.002 }"),
        )
        .expect("sided floors must parse");
        assert_eq!(cfg.net_floor_taker(&Category::new("crypto")), dec!(0.01));
        assert_eq!(cfg.net_floor_maker(&Category::new("crypto")), dec!(0.002));
        assert_eq!(cfg.net_floor_taker(&Category::new("politics")), dec!(0.005));

        assert!(
            Config::from_toml_str(&SHIPPED.replace(SHIPPED_CRYPTO, "crypto = { takr = 0.01 }"))
                .is_err(),
            "a typo inside a floor entry must not be silently ignored"
        );

        let mut negative = Config::default();
        negative.scan.floors.insert(
            "crypto".into(),
            CategoryFloor::sided(dec!(0.008), dec!(-0.001)),
        );
        assert!(
            negative.validate().is_err(),
            "a negative floor accepts losses"
        );

        let mut no_default = Config::default();
        no_default.scan.floors.remove("default");
        assert!(no_default.validate().is_err());
    }

    #[test]
    fn alert_threshold_is_per_category_with_a_crypto_default() {
        let cfg = Config::default();
        assert_eq!(
            cfg.alerts.min_net_for(&Category::new("crypto")),
            dec!(0.008),
            "crypto alerts at its own taker floor, not the general 0.01"
        );
        assert_eq!(
            cfg.alerts.min_net_for(&Category::new("politics")),
            dec!(0.01)
        );
        assert_eq!(
            cfg.alerts.min_net_for(&Category::new("nonsense")),
            dec!(0.01)
        );

        let mut bad = Config::default();
        bad.alerts
            .alert_min_net_by_category
            .insert("crypto".into(), dec!(-0.01));
        assert!(bad.validate().is_err());
    }

    #[test]
    fn live_mode_is_rejected_in_phase_a() {
        let cfg = Config {
            mode: "live".into(),
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unknown_config_keys_are_rejected_loudly() {
        let err = Config::from_toml_str("mode = \"dry-run\"\nnot_a_key = 1\n");
        assert!(err.is_err(), "typos in config must not be silently ignored");
    }

    #[test]
    fn shipped_config_carries_the_m3_sections() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.daemon.scan_interval_secs, 5);
        assert_eq!(cfg.daemon.universe_refresh_secs, 600);
        assert_eq!(cfg.daemon.daily_summary_utc, "23:55");
        assert_eq!(cfg.lifecycle.repoll_interval_secs, 2);
        assert_eq!(cfg.lifecycle.repoll_window_secs, 30);
        assert_eq!(cfg.lifecycle.max_concurrent, 32);
        assert!(cfg.alerts.enabled);
        assert_eq!(cfg.alerts.alert_min_net_per_share, dec!(0.01));
        assert_eq!(cfg.alerts.telegram_api_base, "https://api.telegram.org");
        assert_eq!(cfg.storage.database_path, "data/polyarb.sqlite");
        assert_eq!(cfg.storage.log_dir, "logs");
        assert_eq!(cfg.storage.report_dir, "reports");
        assert_eq!(
            cfg.daily_summary_time().expect("parses"),
            chrono::NaiveTime::from_hms_opt(23, 55, 0).unwrap()
        );
    }

    #[test]
    fn shipped_config_carries_the_m6_stream_section() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");
        assert!(cfg.stream.enabled, "streaming ships on by default");
        assert_eq!(
            cfg.stream.url,
            "wss://ws-subscriptions-clob.polymarket.com/ws/market"
        );
        assert_eq!(cfg.stream.max_subs_per_connection, 500);
        assert_eq!(cfg.stream.debounce_ms, 50);
        assert_eq!(cfg.stream.stale_after_secs, 60);
        assert_eq!(cfg.stream.resync_interval_secs, 300);
        assert_eq!(cfg.stream.fallback_after_failures, 5);
        // The shipped section must agree with the compiled defaults.
        assert_eq!(cfg.stream, StreamConfig::default());
        // And the file must say out loud that the wire shapes are unverified.
        assert!(
            SHIPPED.contains("TODO(verify-live)"),
            "the stream section must keep its unverified-API warning"
        );
    }

    #[test]
    fn stream_settings_are_validated_only_when_streaming_is_on() {
        let with = |stream: StreamConfig| Config {
            stream,
            ..Config::default()
        };

        for bad in [
            StreamConfig {
                url: "https://clob.polymarket.com".into(),
                ..StreamConfig::default()
            },
            StreamConfig {
                max_subs_per_connection: 0,
                ..StreamConfig::default()
            },
            StreamConfig {
                stale_after_secs: 0,
                ..StreamConfig::default()
            },
            StreamConfig {
                resync_interval_secs: 0,
                ..StreamConfig::default()
            },
            StreamConfig {
                // 0 would fall back before the first attempt.
                fallback_after_failures: 0,
                ..StreamConfig::default()
            },
            StreamConfig {
                // A debounce at/above the scan interval is slower than the polling it
                // replaces (the default scan interval is 5 s).
                debounce_ms: 5_000,
                ..StreamConfig::default()
            },
        ] {
            assert!(
                with(bad.clone()).validate().is_err(),
                "must be rejected: {bad:?}"
            );
            // …but a disabled stream is never in the way of starting up.
            assert!(with(StreamConfig {
                enabled: false,
                ..bad
            })
            .validate()
            .is_ok());
        }

        // A plain ws:// URL (what the tests use) is fine.
        assert!(with(StreamConfig {
            url: "ws://127.0.0.1:9001/ws/market".into(),
            ..StreamConfig::default()
        })
        .validate()
        .is_ok());

        assert!(
            Config::from_toml_str("[stream]\nenabledd = true\n").is_err(),
            "a typo in the stream section must not be silently ignored"
        );
    }

    #[test]
    fn shipped_config_contains_no_credentials() {
        // Belt and braces for the CLAUDE.md rule: secrets live in the environment only.
        // Comments may *name* the environment variables; no key may ever carry a value.
        let settings: String = SHIPPED
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        for forbidden in ["token", "chat_id", "private_key", "secret", "api_key"] {
            assert!(
                !settings.contains(forbidden),
                "config/default.toml must not define a {forbidden:?} setting"
            );
        }
    }

    #[test]
    fn m3_sections_are_validated() {
        let bad_interval = Config {
            daemon: DaemonConfig {
                scan_interval_secs: 0,
                ..DaemonConfig::default()
            },
            ..Config::default()
        };
        assert!(bad_interval.validate().is_err());

        let bad_clock = Config {
            daemon: DaemonConfig {
                daily_summary_utc: "midnight".into(),
                ..DaemonConfig::default()
            },
            ..Config::default()
        };
        assert!(bad_clock.validate().is_err());

        // A re-poll interval longer than the window would never confirm anything.
        let bad_lifecycle = Config {
            lifecycle: LifecycleConfig {
                repoll_interval_secs: 60,
                repoll_window_secs: 30,
                max_concurrent: 4,
            },
            ..Config::default()
        };
        assert!(bad_lifecycle.validate().is_err());

        let bad_threshold = Config {
            alerts: AlertConfig {
                alert_min_net_per_share: dec!(-0.01),
                ..AlertConfig::default()
            },
            ..Config::default()
        };
        assert!(bad_threshold.validate().is_err());

        let bad_storage = Config {
            storage: StorageConfig {
                database_path: "  ".into(),
                ..StorageConfig::default()
            },
            ..Config::default()
        };
        assert!(bad_storage.validate().is_err());
    }

    #[test]
    fn m3_sections_reject_unknown_keys() {
        let err = Config::from_toml_str("[alerts]\nalert_min_net_per_shore = 0.01\n");
        assert!(err.is_err(), "a typo in an M3 section must not be ignored");
    }

    #[test]
    fn category_filter_defaults_to_everything() {
        let mut cfg = Config::default();
        assert!(cfg.category_included(&Category::new("weather")));
        cfg.scan.include_categories = vec!["politics".into()];
        assert!(cfg.category_included(&Category::new("Politics")));
        assert!(!cfg.category_included(&Category::new("weather")));
    }
}
