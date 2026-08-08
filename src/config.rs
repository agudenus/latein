//! Configuration: `config/default.toml` + selected environment overrides.
//!
//! No secrets live here. Per CLAUDE.md, credentials only ever come from the environment,
//! and Phase A needs none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{Category, MarketActivity};

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
    /// M7 read-only web dashboard (`polyarb dashboard`). A separate process from the
    /// daemon, and one that opens the same SQLite read-only.
    #[serde(default)]
    pub dashboard: DashboardConfig,
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
    /// Cap on the connect phase alone — DNS resolution plus TCP plus TLS (M7.1).
    ///
    /// `request_timeout_secs` bounds a request that is *making progress badly*; this bounds
    /// one that has not started at all. A live DNS outage stretched the daemon's loop tick
    /// to many minutes because every one of ~1 500 book batches sat in resolution before
    /// failing, and a request that will never connect should give up in seconds, not in
    /// however long the resolver takes to admit defeat.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
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
    /// Gamma events page size.
    pub page_size: usize,
    /// Query parameter that carries the keyset cursor back to `/events/keyset`.
    ///
    /// Optional, and only here because the ecosystem disagrees about the name (see
    /// [`crate::gamma::DEFAULT_KEYSET_CURSOR_PARAM`]): if a live run shows the universe
    /// truncating with `keyset pagination did not advance`, the wrong name is the first
    /// suspect and this changes it without a rebuild.
    #[serde(default = "default_keyset_cursor_param")]
    pub keyset_cursor_param: String,
    /// Hard stop on Gamma `/events` pagination — a rate-limit backstop, **not** the
    /// intended stopping point. Normal discovery ends when the API returns a short page.
    /// When this cap is what stops us the universe is incomplete (a NegRisk event can
    /// even be split across the boundary), so `fetch_universe` logs
    /// `WARN universe truncated at max_events`. Keep it comfortably above the live event
    /// count (~500 and growing).
    pub max_events: usize,
    /// M6.4 — prune untradeable dust from the tracked universe. See [`ActivityFloor`].
    ///
    /// `#[serde(default)]` so an operator's older `config/default.toml` keeps loading; it
    /// simply gets the shipped floor.
    #[serde(default)]
    pub activity_floor: ActivityFloor,
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

/// The activity floor: which markets are worth tracking at all (M6.4).
///
/// A live discovery pass keeps ~44 000 markets across ~5 900 events, which is 88 000 token
/// subscriptions, ~180 WebSocket connections and several minutes per REST sweep. Most of
/// those markets have near-zero liquidity and could not fill a $50 order — they are pure
/// overhead, and the overhead is what delays detection on the markets that matter.
///
/// This filter reads Gamma's own reported figures (`liquidityClob`, `volume24hr`). Those
/// numbers are **only ever** used to prune: nothing downstream sizes, costs or profits from
/// them. CLAUDE.md forbids it — reported volume is double-counted and heavily wash-inflated,
/// and executable liquidity comes from order book depth. A reported figure is trustworthy
/// enough for "is this market alive?" and for nothing else.
///
/// Two rules keep the filter from doing damage:
///
/// * **Absence of data is never a drop.** A market carrying none of the fields an enabled
///   criterion reads is kept. The legacy `/events` payload has no activity fields at all,
///   and a missing field must not look like a zero.
/// * **NegRisk events are judged whole.** See [`crate::gamma::build_universe`]:
///   pruning individual outcomes of a mutually-exclusive event would manufacture exactly
///   the partial coverage the full-coverage guard exists to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityFloor {
    /// Master switch. `false` tracks every market Gamma lists, as before M6.4.
    pub enabled: bool,
    /// Keep a market whose reported CLOB liquidity is at least this many USD. `0` disables
    /// the liquidity criterion (it never judges, and so never drops).
    pub min_liquidity_usd: Decimal,
    /// Keep a market whose reported 24 h volume is at least this many USD. `0` disables the
    /// volume criterion. A market passing *either* enabled criterion is kept.
    pub min_volume24h_usd: Decimal,
}

impl ActivityFloor {
    /// Verdict for one market: `Some(true)` passes, `Some(false)` fails, `None` means no
    /// *enabled* criterion had data to judge it by.
    ///
    /// `None` is always a keep (see [`keeps`](Self::keeps)); it is a distinct answer only so
    /// the reason stays visible at the call site.
    pub fn judge(&self, activity: &MarketActivity) -> Option<bool> {
        if !self.enabled {
            return None;
        }
        let mut judged = false;
        let mut passed = false;
        // Either criterion passing is enough: a market can be thin on resting depth and
        // still trade, or quiet today and still be deep.
        for (floor, reported) in [
            (self.min_liquidity_usd, activity.liquidity),
            (self.min_volume24h_usd, activity.volume_24h),
        ] {
            if floor <= Decimal::ZERO {
                continue; // criterion switched off — it neither passes nor judges
            }
            if let Some(value) = reported {
                judged = true;
                passed |= value >= floor;
            }
        }
        judged.then_some(passed)
    }

    /// Whether this market survives the floor. Unjudgeable markets survive.
    pub fn keeps(&self, activity: &MarketActivity) -> bool {
        self.judge(activity).unwrap_or(true)
    }
}

impl Default for ActivityFloor {
    fn default() -> Self {
        Self {
            enabled: true,
            // $500 of reported CLOB liquidity (raised from $100, 2026-08).
            //
            // It is still a dust cutoff, not a tradability test — that is the depth
            // walker's job, on the real book. The raise is a scale decision, from live
            // evidence: at $100 discovery kept 73 450 markets / 146 900 tokens / 294 WS
            // shards, a full REST sweep of those books took 195 s, and startup opened 294
            // sockets at once. That is past what a home PC on a domestic connection can
            // sweep comfortably, and the overhead lands precisely on the markets that can
            // be traded. The per-trade cap is $50, so nothing under $500 reported liquidity
            // was going to fill both legs of a reportable construction anyway.
            min_liquidity_usd: Decimal::new(500, 0),
            // Off by default: liquidity alone is the cleaner signal, and volume is the
            // figure the research says is inflated. Raise it to rescue markets that trade
            // in bursts without resting depth.
            min_volume24h_usd: Decimal::ZERO,
        }
    }
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
    /// Progress watchdog (M7.1): seconds of **no loop progress at all** before the daemon
    /// logs its last known position at ERROR and exits with
    /// [`crate::dryrun::WATCHDOG_EXIT_CODE`], so a restart policy can revive it clean. `0`
    /// disables the watchdog.
    ///
    /// "Progress" is any finished unit of work — a discovery pass, a book batch, a
    /// detection pass, a loop iteration — not a completed tick, so a slow-but-working loop
    /// (a full REST sweep of 147 000 books takes ~195 s) never trips it. The watchdog is
    /// also **unarmed until the first unit of work completes**, so the long first
    /// discovery-and-seed at startup cannot be mistaken for a wedge.
    #[serde(default = "default_watchdog_stall_secs")]
    pub watchdog_stall_secs: u64,
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
    /// Rate limit / batching cap on the targeted stale-book resync: one token is
    /// re-requested over REST at most once per this window.
    ///
    /// It is **not** an idle timeout (M6.1). Silence on a healthy shard means the market
    /// is quiet, which is the normal state of most books; treating it as staleness made
    /// this path re-fetch ~16 500 books every 90 s, heavier than the polling loop it
    /// replaced. Only an explicitly invalidated book, or one whose shard dropped after its
    /// last update, is resynced — and this window caps how often.
    ///
    /// It governs the two cases that need rate-limiting (a token the API has no book for,
    /// and a book suspect only because of a shard gap). A book we hold and have explicitly
    /// invalidated is repaired on a much shorter fixed floor — see
    /// [`crate::ws::BookStore::missing_or_stale`].
    pub stale_after_secs: u64,
    /// Cadence of the full REST sweep: re-fetch every book, cross-check a random sample
    /// against the local state, and run detection over the whole universe. This, not
    /// `stale_after_secs`, is the global integrity net.
    pub resync_interval_secs: u64,
    /// Consecutive failed connection attempts before streaming hands over to REST polling
    /// (logged at ERROR).
    ///
    /// The streak is necessary but not sufficient: the daemon only trips the switch when
    /// REST demonstrably worked *during* the same streak. Both transports failing together
    /// is a network outage, not a broken socket, and is retried indefinitely.
    pub fallback_after_failures: u32,
    /// After a fallback, how often to open one throwaway connection to see whether the
    /// endpoint answers again. On success the pool is restarted and streaming resumes
    /// (logged loudly), so "permanent" really means "until proven working again".
    #[serde(default = "default_reprobe_interval_secs")]
    pub reprobe_interval_secs: u64,
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
    /// Re-alert cooldown per `(event, kind, side)` (M6.1).
    ///
    /// A slow-moving opportunity is re-detected on every sweep, and a one-tick ask move
    /// (6.32 → 6.33) is different economics, so it is correctly a *new* row — but it is not
    /// news. Inside this window the same construction is not alerted again unless it got
    /// materially better (`realert_improvement`). Suppressed alerts are still written to
    /// the JSONL log with `delivery = "cooldown"`, so nothing is lost from the record.
    /// `0` disables the cooldown.
    #[serde(default = "default_per_event_cooldown_secs")]
    pub per_event_cooldown_secs: u64,
    /// How much better, in net per share, a re-detection must be to break the cooldown.
    /// Compared against the last *alerted* figure on both the taker and the maker side, so
    /// a slow cumulative drift eventually re-alerts while tick noise never does.
    #[serde(default = "default_realert_improvement")]
    pub realert_improvement: Decimal,
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

/// M7 — the read-only soak dashboard.
///
/// There is nothing to configure about *what* it may do: the dashboard opens the database
/// with `SQLITE_OPEN_READ_ONLY`, serves `GET` routes only, and shares no state with the
/// daemon. These knobs are address, cadence and window — nothing here can widen it into a
/// control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// Listen address. Loopback by default, deliberately: this page shows a soak's whole
    /// evidence trail and has no authentication. Put it behind an SSH tunnel or a reverse
    /// proxy before binding anything routable.
    pub bind: String,
    /// How often the page re-fetches `/api/state`. The design asks for 2–5 s.
    pub poll_interval_ms: u64,
    /// Width of the evidence window the funnel, the KPIs and the category table cover.
    pub window_hours: u64,
    /// Rows in the opportunity table. Not a page size — there is no paging; it is the cap
    /// on how much of the newest-first feed is rendered.
    pub max_rows: usize,
    /// Planned soak length, for the "day n of m" eyebrow. Evidence-gathering runs for a
    /// fixed window and the header says how far through it is.
    pub soak_days: u32,
    /// Seconds without loop progress before the page reports the daemon as *alive but
    /// stalled* (M7.1). `0` switches that break state off.
    ///
    /// It is a different question from `runtime_status` staleness: the heartbeat publishes
    /// from its own task, so a fresh status row now proves only that the **process** is
    /// alive. This threshold is what turns "the loop has not finished anything since T"
    /// into a takeover.
    ///
    /// Shipped at half of `daemon.watchdog_stall_secs`, deliberately: a stall is visible on
    /// the page for ~5 minutes before the watchdog restarts the process, and it sits well
    /// above the ~195 s a full REST sweep takes at live scale, so a slow-but-progressing
    /// loop never reaches it.
    #[serde(default = "default_loop_stall_secs")]
    pub loop_stall_secs: u64,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            poll_interval_ms: 3_000,
            window_hours: 24,
            max_rows: 40,
            soak_days: 7,
            loop_stall_secs: default_loop_stall_secs(),
        }
    }
}

impl DashboardConfig {
    /// The parsed listen address.
    pub fn socket_addr(&self) -> Result<std::net::SocketAddr, ConfigError> {
        self.bind.trim().parse().map_err(|_| {
            ConfigError::Invalid(format!(
                "dashboard.bind must be an address:port (got {:?}); e.g. \"127.0.0.1:8080\"",
                self.bind
            ))
        })
    }
}

fn default_mode() -> String {
    "dry-run".to_string()
}

fn default_keyset_cursor_param() -> String {
    crate::gamma::DEFAULT_KEYSET_CURSOR_PARAM.to_string()
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

/// One hour. Long enough that a genuinely dead endpoint is not hammered, short enough that
/// a night-long outage does not cost a night of streaming.
fn default_reprobe_interval_secs() -> u64 {
    3_600
}

/// Ten seconds for DNS + TCP + TLS. Well above a healthy handshake, far below the
/// request timeout, so a resolver that has stopped answering fails fast instead of
/// stretching one book batch — and with it the whole loop tick — into minutes.
fn default_connect_timeout_secs() -> u64 {
    10
}

/// Ten minutes of *no progress whatsoever*. A full REST sweep at the live universe scale
/// (147 000 books) takes ~195 s and reports progress on every batch, so this is roughly
/// three sweeps' worth of silence: long past "slow", squarely "wedged".
fn default_watchdog_stall_secs() -> u64 {
    600
}

/// Five minutes: half the watchdog's threshold, so the dashboard shows the stall before the
/// watchdog restarts the process, and still far above a healthy sweep.
fn default_loop_stall_secs() -> u64 {
    300
}

/// 30 minutes: the same slow-moving opportunity is worth one message per half hour.
fn default_per_event_cooldown_secs() -> u64 {
    1_800
}

/// A cent per share. Below that a re-alert is a rounding artefact of the book ticking.
fn default_realert_improvement() -> Decimal {
    Decimal::new(1, 2)
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
            connect_timeout_secs: default_connect_timeout_secs(),
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
            keyset_cursor_param: default_keyset_cursor_param(),
            // Raised 2 000 → 6 000 → 20 000: a live run filled all 60 pages of the 6 000
            // cap too, so the active-event count is past that as well. It is a rate-limit
            // backstop, and a truncated universe can split a NegRisk event; with the
            // activity floor doing the actual pruning, this only has to be out of the way.
            max_events: 20_000,
            activity_floor: ActivityFloor::default(),
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
            watchdog_stall_secs: default_watchdog_stall_secs(),
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
            stale_after_secs: 900,
            resync_interval_secs: 300,
            fallback_after_failures: 5,
            reprobe_interval_secs: default_reprobe_interval_secs(),
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
            per_event_cooldown_secs: default_per_event_cooldown_secs(),
            realert_improvement: default_realert_improvement(),
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
            dashboard: DashboardConfig::default(),
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
        // Overridable because the address depends on where the process runs, not on what
        // the operator wants it to do: inside a container it must bind 0.0.0.0 for the
        // port mapping to reach it, while the mapping itself keeps it on the host's
        // loopback. See docker-compose.yml.
        if let Some(v) = env_str("POLYARB_DASHBOARD_BIND") {
            self.dashboard.bind = v;
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
        if self.scan.keyset_cursor_param.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "scan.keyset_cursor_param must not be empty (it is the query parameter that \
                 carries the keyset cursor; the default is \"after_cursor\")"
                    .into(),
            ));
        }
        if self.scan.activity_floor.min_liquidity_usd < Decimal::ZERO
            || self.scan.activity_floor.min_volume24h_usd < Decimal::ZERO
        {
            return Err(ConfigError::Invalid(
                "scan.activity_floor.min_liquidity_usd and min_volume24h_usd must be >= 0 \
                 (0 switches that criterion off)"
                    .into(),
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

        if self.api.request_timeout_secs == 0 || self.api.connect_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "api.request_timeout_secs and api.connect_timeout_secs must be > 0 (a request \
                 with no ceiling can hang the scan loop indefinitely)"
                    .into(),
            ));
        }

        // ---- M3 daemon ---------------------------------------------------------------
        if self.daemon.scan_interval_secs == 0 || self.daemon.universe_refresh_secs == 0 {
            return Err(ConfigError::Invalid(
                "daemon.scan_interval_secs and daemon.universe_refresh_secs must be > 0".into(),
            ));
        }
        // 0 disables the watchdog; anything positive has to leave room for one full REST
        // sweep, or the watchdog restarts a daemon that was merely busy.
        if self.daemon.watchdog_stall_secs > 0 && self.daemon.watchdog_stall_secs < 60 {
            return Err(ConfigError::Invalid(format!(
                "daemon.watchdog_stall_secs must be 0 (disabled) or >= 60 (got {}); a shorter \
                 threshold restarts a daemon that is merely slow",
                self.daemon.watchdog_stall_secs
            )));
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
            if self.stream.reprobe_interval_secs == 0 {
                return Err(ConfigError::Invalid(
                    "stream.reprobe_interval_secs must be > 0 (0 would re-probe the endpoint \
                     on every tick after a fallback)"
                        .into(),
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
        if self.alerts.realert_improvement < Decimal::ZERO {
            return Err(ConfigError::Invalid(
                "alerts.realert_improvement must be >= 0 (a negative bar would re-alert on \
                 every worsening re-detection)"
                    .into(),
            ));
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

        // ---- M7 dashboard ------------------------------------------------------------
        self.dashboard.socket_addr()?;
        // A poll faster than this is a busy loop against SQLite for a page whose slowest
        // input (the universe refresh) moves every ten minutes.
        if self.dashboard.poll_interval_ms < 250 {
            return Err(ConfigError::Invalid(format!(
                "dashboard.poll_interval_ms must be >= 250 (got {}); the design asks for \
                 2000–5000",
                self.dashboard.poll_interval_ms
            )));
        }
        if self.dashboard.window_hours == 0 {
            return Err(ConfigError::Invalid(
                "dashboard.window_hours must be > 0 (it is the evidence window the funnel \
                 and the KPIs cover)"
                    .into(),
            ));
        }
        if self.dashboard.max_rows == 0 {
            return Err(ConfigError::Invalid(
                "dashboard.max_rows must be > 0".into(),
            ));
        }
        if self.dashboard.soak_days == 0 {
            return Err(ConfigError::Invalid(
                "dashboard.soak_days must be > 0 (it is the denominator of \"soak day n of \
                 m\")"
                    .into(),
            ));
        }
        // 0 switches the stall break state off. A positive value below the scan interval
        // would report every ordinary tick as a stall.
        if self.dashboard.loop_stall_secs > 0
            && self.dashboard.loop_stall_secs <= self.daemon.scan_interval_secs
        {
            return Err(ConfigError::Invalid(format!(
                "dashboard.loop_stall_secs ({}) must be 0 (off) or well above \
                 daemon.scan_interval_secs ({}); otherwise a healthy loop reads as stalled",
                self.dashboard.loop_stall_secs, self.daemon.scan_interval_secs
            )));
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
    fn the_dashboard_section_parses_validates_and_stays_read_only_shaped() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.dashboard.bind, "127.0.0.1:8080");
        assert_eq!(cfg.dashboard.poll_interval_ms, 3_000);
        assert_eq!(cfg.dashboard.window_hours, 24);
        assert_eq!(cfg.dashboard.soak_days, 7);
        assert_eq!(
            cfg.dashboard.socket_addr().expect("addr").to_string(),
            "127.0.0.1:8080"
        );
        assert!(
            cfg.dashboard.bind.starts_with("127.0.0.1"),
            "the shipped bind must stay on loopback: the page has no authentication and \
             shows a whole soak's evidence"
        );

        // A config written before M7 keeps loading and gets the shipped defaults.
        const HEADER: &str = "\n[dashboard]\n";
        let older: String = {
            let start = SHIPPED.find(HEADER).expect("section present") + 1;
            let end = SHIPPED[start..]
                .find("\n[")
                .map(|i| start + i + 1)
                .expect("a section follows");
            format!("{}{}", &SHIPPED[..start], &SHIPPED[end..])
        };
        assert!(!older.contains(HEADER), "the section must really be gone");
        let cfg = Config::from_toml_str(&older).expect("a config without the section must load");
        cfg.validate().expect("validate");
        assert_eq!(cfg.dashboard, DashboardConfig::default());

        // A typo is refused rather than silently ignored.
        assert!(
            Config::from_toml_str("[dashboard]\nbindd = \"0.0.0.0:80\"\n").is_err(),
            "deny_unknown_fields must cover the new section too"
        );

        // Every knob is validated, and each rejection says what the knob is for.
        let mut bad = Config::default();
        bad.dashboard.bind = "not-an-address".into();
        assert!(bad.validate().is_err());

        let mut fast = Config::default();
        fast.dashboard.poll_interval_ms = 10;
        assert!(fast.validate().is_err(), "a 10ms poll is a busy loop");

        for mutate in [
            |c: &mut Config| c.dashboard.window_hours = 0,
            |c: &mut Config| c.dashboard.max_rows = 0,
            |c: &mut Config| c.dashboard.soak_days = 0,
        ] {
            let mut cfg = Config::default();
            mutate(&mut cfg);
            assert!(cfg.validate().is_err());
        }
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

    /// The keyset cursor parameter is the one request detail that could not be verified
    /// against the live API, so it ships as a knob: named in the file, defaulted when an
    /// older copy of the file lacks it, and never allowed to be blank.
    #[test]
    fn the_keyset_cursor_param_is_shipped_defaulted_and_validated() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(
            cfg.scan.keyset_cursor_param,
            crate::gamma::DEFAULT_KEYSET_CURSOR_PARAM
        );
        assert_eq!(
            cfg.scan.keyset_cursor_param,
            ScanConfig::default().keyset_cursor_param
        );

        let older = SHIPPED
            .lines()
            .filter(|l| !l.starts_with("keyset_cursor_param"))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = Config::from_toml_str(&older).expect("a config without the key must load");
        cfg.validate().expect("validate");
        assert_eq!(
            cfg.scan.keyset_cursor_param,
            crate::gamma::DEFAULT_KEYSET_CURSOR_PARAM
        );

        let mut blank = Config::default();
        blank.scan.keyset_cursor_param = "  ".into();
        assert!(
            blank.validate().is_err(),
            "an empty cursor parameter would silently re-request page one"
        );
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
        assert_eq!(cfg.stream.stale_after_secs, 900);
        assert_eq!(cfg.stream.resync_interval_secs, 300);
        assert_eq!(cfg.stream.fallback_after_failures, 5);
        assert_eq!(cfg.stream.reprobe_interval_secs, 3_600);
        // The shipped section must agree with the compiled defaults.
        assert_eq!(cfg.stream, StreamConfig::default());
        // And the file must say out loud that the wire shapes are unverified.
        assert!(
            SHIPPED.contains("TODO(verify-live)"),
            "the stream section must keep its unverified-API warning"
        );
    }

    /// The four M6.1 knobs, each one the fix for something a live overnight run did wrong.
    #[test]
    fn shipped_config_carries_the_m6_1_soak_fixes() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");

        // Issue 3: 2 000 was not enough — all 20 pages came back full. Nor was the 6 000
        // that replaced it (all 60 pages full); see the M6.4 test below.
        assert_eq!(cfg.scan.max_events, 20_000);
        // Issue 2: silence is not staleness; this is now only the re-request rate limit.
        assert_eq!(cfg.stream.stale_after_secs, 900);
        // Issue 1: a fallback lasts until the endpoint proves itself again.
        assert_eq!(cfg.stream.reprobe_interval_secs, 3_600);
        // Issue 4: the same slow-moving construction is one message per half hour.
        assert_eq!(cfg.alerts.per_event_cooldown_secs, 1_800);
        assert_eq!(cfg.alerts.realert_improvement, dec!(0.01));

        // The shipped file and the compiled defaults must not drift apart.
        assert_eq!(cfg.stream, StreamConfig::default());
        assert_eq!(cfg.alerts, AlertConfig::default());
        assert_eq!(cfg.scan.max_events, ScanConfig::default().max_events);
    }

    /// (g) M6.4: the shipped file carries the activity floor and the raised backstop, and
    /// both agree with the compiled defaults.
    #[test]
    fn shipped_config_carries_the_m6_4_scale_settings() {
        let cfg = Config::from_toml_str(SHIPPED).expect("parse");
        cfg.validate().expect("validate");

        assert_eq!(
            cfg.scan.max_events, 20_000,
            "6 000 filled all 60 of its pages on a live run — the backstop had to move again"
        );
        assert_eq!(cfg.scan.max_events, ScanConfig::default().max_events);

        assert_eq!(cfg.scan.activity_floor, ActivityFloor::default());
        assert!(cfg.scan.activity_floor.enabled, "the floor ships on");
        assert_eq!(cfg.scan.activity_floor.min_liquidity_usd, dec!(100));
        assert_eq!(
            cfg.scan.activity_floor.min_volume24h_usd,
            dec!(0),
            "the volume criterion ships switched off"
        );

        // The file must say out loud that these figures never reach the money path.
        assert!(
            SHIPPED.contains("book-depth-based"),
            "the activity floor section must keep its 'pruning only' warning"
        );

        // An older copy of the file, with no [scan.activity_floor] section at all, must
        // still start up — and get the shipped floor.
        const HEADER: &str = "\n[scan.activity_floor]\n";
        let older: String = {
            let start = SHIPPED.find(HEADER).expect("section present") + 1;
            let end = SHIPPED[start..]
                .find("\n[")
                .map(|i| start + i + 1)
                .expect("a section follows");
            format!("{}{}", &SHIPPED[..start], &SHIPPED[end..])
        };
        assert!(!older.contains(HEADER), "the section must really be gone");
        let cfg = Config::from_toml_str(&older).expect("a config without the section must load");
        cfg.validate().expect("validate");
        assert_eq!(cfg.scan.activity_floor, ActivityFloor::default());

        // A typo inside the section is not silently ignored, and a negative floor (which
        // would judge nothing while pretending to) is rejected.
        assert!(
            Config::from_toml_str("[scan.activity_floor]\nenabledd = true\n").is_err(),
            "a typo in the activity floor must not be swallowed"
        );
        let mut negative = Config::default();
        negative.scan.activity_floor.min_liquidity_usd = dec!(-1);
        assert!(negative.validate().is_err());
        negative.scan.activity_floor.min_liquidity_usd = dec!(100);
        negative.scan.activity_floor.min_volume24h_usd = dec!(-1);
        assert!(negative.validate().is_err());
    }

    /// The new keys are optional so an operator's older `config/default.toml` copy keeps
    /// starting up — it simply gets the defaults rather than a parse error.
    #[test]
    fn the_m6_1_keys_are_optional_and_validated() {
        let older = SHIPPED
            .lines()
            .filter(|l| {
                !l.starts_with("reprobe_interval_secs")
                    && !l.starts_with("per_event_cooldown_secs")
                    && !l.starts_with("realert_improvement")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = Config::from_toml_str(&older).expect("a config without the new keys must load");
        cfg.validate().expect("validate");
        assert_eq!(cfg.stream.reprobe_interval_secs, 3_600);
        assert_eq!(cfg.alerts.per_event_cooldown_secs, 1_800);
        assert_eq!(cfg.alerts.realert_improvement, dec!(0.01));

        // A zero re-probe interval would probe on every tick.
        let mut bad = Config::default();
        bad.stream.reprobe_interval_secs = 0;
        assert!(bad.validate().is_err());
        bad.stream.enabled = false;
        assert!(bad.validate().is_ok(), "not checked when streaming is off");

        // A negative bar would re-alert on every *worse* re-detection.
        let mut negative = Config::default();
        negative.alerts.realert_improvement = dec!(-0.01);
        assert!(negative.validate().is_err());

        // Zero is legal on both: no cooldown, and re-alert on any improvement at all.
        let mut eager = Config::default();
        eager.alerts.per_event_cooldown_secs = 0;
        eager.alerts.realert_improvement = Decimal::ZERO;
        assert!(eager.validate().is_ok());
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
                reprobe_interval_secs: 0,
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
