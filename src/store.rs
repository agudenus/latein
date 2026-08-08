//! SQLite persistence for the dry-run daemon (M3).
//!
//! Everything the soak test needs to answer "was this real?" lives here:
//!
//! * `opportunities` — one row per *distinct* opportunity (see [`dedupe_key`]), with the
//!   full cost breakdown, the legs, the triggering book snapshots, and the lifecycle
//!   verdict written back once the re-poll window closes.
//! * `scan_stats` — scan-cycle telemetry aggregated per UTC hour, so a 5-second loop does
//!   not write 17k rows a day.
//! * `daily_summaries` — the generated markdown, so a report can be re-read without
//!   re-deriving it.
//!
//! Money is stored as TEXT (the exact `Decimal` string), never REAL: SQLite's REAL is an
//! f64 and no money value in this codebase is allowed to pass through one. Durations are
//! stored as integer milliseconds for the same reason.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{BookMap, Opportunity};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not create the database directory {path}: {source}")]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error at {path}: {source}")]
    Sqlite {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("corrupt value in column {column}: {value:?}")]
    Value { column: &'static str, value: String },
}

type Result<T> = std::result::Result<T, StoreError>;

// ---------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------

const SCHEMA_V1: &str = r#"
CREATE TABLE opportunities (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    dedupe_key             TEXT    NOT NULL UNIQUE,
    kind                   TEXT    NOT NULL,
    label                  TEXT    NOT NULL,
    category               TEXT    NOT NULL,
    event_slug             TEXT    NOT NULL,
    event_title            TEXT    NOT NULL,
    fee_rate               TEXT    NOT NULL,
    payout                 TEXT    NOT NULL,
    gross_gap              TEXT    NOT NULL,
    slippage_cost          TEXT    NOT NULL,
    spread_cost            TEXT,
    fee_taker              TEXT    NOT NULL,
    net_taker              TEXT    NOT NULL,
    net_maker              TEXT,
    executable_size        TEXT    NOT NULL,
    capital_required       TEXT    NOT NULL,
    net_taker_total        TEXT    NOT NULL,
    net_maker_total        TEXT,
    conversion_required    INTEGER NOT NULL,
    maker_only             INTEGER NOT NULL,
    resolution_flags       TEXT    NOT NULL,
    legs_json              TEXT    NOT NULL,
    books_json             TEXT    NOT NULL,
    detected_at            TEXT    NOT NULL,
    last_seen_at           TEXT    NOT NULL,
    seen_count             INTEGER NOT NULL,
    status                 TEXT    NOT NULL,
    repolls                INTEGER NOT NULL DEFAULT 0,
    first_repoll_available INTEGER,
    persistence_ms         INTEGER,
    simulated_pnl_taker    TEXT,
    simulated_pnl_maker    TEXT,
    resolved_at            TEXT
);
CREATE INDEX idx_opportunities_detected_at ON opportunities(detected_at);
CREATE INDEX idx_opportunities_status      ON opportunities(status);

CREATE TABLE scan_stats (
    hour_utc          TEXT    PRIMARY KEY,
    cycles            INTEGER NOT NULL,
    errors            INTEGER NOT NULL,
    events_scanned    INTEGER NOT NULL,
    markets_scanned   INTEGER NOT NULL,
    books_fetched     INTEGER NOT NULL,
    duration_ms_total INTEGER NOT NULL,
    duration_ms_max   INTEGER NOT NULL,
    opportunities     INTEGER NOT NULL,
    new_opportunities INTEGER NOT NULL,
    updated_at        TEXT    NOT NULL
);

CREATE TABLE daily_summaries (
    day          TEXT PRIMARY KEY,
    generated_at TEXT NOT NULL,
    markdown     TEXT NOT NULL
);
"#;

/// M6: how long after the market moved we detected the opportunity.
///
/// Nullable, and legitimately so: a REST-driven detection has no triggering frame, so it
/// has no latency to report. Writing a 0 there would fabricate a measurement, and writing
/// the scan interval would fabricate a different one.
const SCHEMA_V2: &str = r#"
ALTER TABLE opportunities ADD COLUMN detection_latency_ms INTEGER;
"#;

/// M7 — what the dashboard needs that only the daemon knows.
///
/// Two additions, and both exist because a *reader* cannot see them otherwise:
///
/// * `runtime_status` — a single row of daemon-memory state (stream shards, REST role,
///   discovery health, alerter breaker …) republished every few seconds. It is the only
///   way a separate read-only process can tell "the market is quiet" from "the scanner is
///   blind", which is the whole point of the break-state screen.
/// * `scan_stats.gaps_detected` / `.fee_survivors` — the two funnel stages that happen
///   *before* an opportunity row exists, so nothing downstream can reconstruct them. They
///   default to 0, so hourly buckets written by an older build read as "not measured"
///   rather than as a zero that means something.
const SCHEMA_V3: &str = r#"
CREATE TABLE runtime_status (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    updated_at TEXT    NOT NULL,
    payload    TEXT    NOT NULL
);
ALTER TABLE scan_stats ADD COLUMN gaps_detected INTEGER NOT NULL DEFAULT 0;
ALTER TABLE scan_stats ADD COLUMN fee_survivors INTEGER NOT NULL DEFAULT 0;
"#;

/// M8 — the maker-fill simulator's measurement rows.
///
/// One row per *closed* simulation, and only per closed one: a simulation in flight has no
/// verdict, and a row that could be revised is not the kind of evidence this table exists to
/// hold. (The live count of open simulations travels in `runtime_status`, which is
/// explicitly the daemon's volatile memory.) A process killed mid-window therefore loses its
/// open simulations — the honest outcome, since none of them ever reached a verdict.
///
/// `legs_json` carries the whole placement snapshot per leg — price, the size visible ahead
/// of us at placement, our own size — plus that leg's fill state and the number of prints
/// that touched it. It is what makes the verdict re-derivable, and what would let a future
/// run re-score the same soak under a *different* queue assumption without re-collecting
/// anything.
///
/// Money is TEXT here as everywhere: `pnl_lower_bound` is credited only on `maker_filled`,
/// `legging_exposure` only on `maker_partial`, and both are NULL otherwise rather than 0 —
/// "not applicable" and "zero" are different facts.
const SCHEMA_V4: &str = r#"
CREATE TABLE maker_sims (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    opportunity_id   INTEGER NOT NULL REFERENCES opportunities(id),
    event_slug       TEXT    NOT NULL,
    kind             TEXT    NOT NULL,
    category         TEXT    NOT NULL,
    legs_json        TEXT    NOT NULL,
    legs_total       INTEGER NOT NULL,
    legs_filled      INTEGER NOT NULL,
    net_maker_total  TEXT    NOT NULL,
    pnl_lower_bound  TEXT,
    legging_exposure TEXT,
    prints_observed  INTEGER NOT NULL,
    time_to_fill_ms  INTEGER,
    status           TEXT    NOT NULL,
    no_print_feed    INTEGER NOT NULL,
    opened_at        TEXT    NOT NULL,
    closed_at        TEXT    NOT NULL
);
CREATE INDEX idx_maker_sims_opened_at ON maker_sims(opened_at);
CREATE INDEX idx_maker_sims_status    ON maker_sims(status);
"#;

/// `(version, name, ddl)`. Append-only: never edit a shipped migration.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "initial schema", SCHEMA_V1),
    (2, "detection latency", SCHEMA_V2),
    (3, "runtime status and funnel counters", SCHEMA_V3),
    (4, "maker fill simulations", SCHEMA_V4),
];

/// The schema version the dashboard needs to read (`runtime_status` + funnel counters, and
/// since M8 the `maker_sims` table it reads the maker lower bound from).
pub const DASHBOARD_MIN_SCHEMA: i64 = 4;

// ---------------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------------

/// Dry-run lifecycle state of an opportunity.
///
/// The honest question the soak test has to answer is *"could we have filled it?"* —
/// which is decided on the **first** re-poll and never revised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleStatus {
    /// Detected; the re-poll window is still open.
    Open,
    /// The walked fills were still on the book at the first re-poll → P&L credited.
    FilledSimulated,
    /// The fills were gone at the first re-poll → nothing credited.
    Vanished,
    /// The daemon shut down before the first re-poll landed. Never credited.
    Unresolved,
    /// Too many concurrent trackers; no lifecycle data was collected. Never credited.
    Untracked,
}

impl LifecycleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::FilledSimulated => "filled_simulated",
            Self::Vanished => "vanished",
            Self::Unresolved => "unresolved",
            Self::Untracked => "untracked",
        }
    }
}

impl std::fmt::Display for LifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The verdict written back to an opportunity row once its re-poll window closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleOutcome {
    pub status: LifecycleStatus,
    pub repolls: i64,
    /// `None` when no re-poll ever landed (`Unresolved` / `Untracked`).
    pub first_repoll_available: Option<bool>,
    /// Time from detection to the last re-poll at which the fills were still available.
    pub persistence_ms: Option<i64>,
    /// Credited only for `FilledSimulated` rows that were sized as a taker.
    pub simulated_pnl_taker: Option<Decimal>,
    /// Hypothetical maker-view number; requires fills we never simulate. Reported apart.
    pub simulated_pnl_maker: Option<Decimal>,
    pub resolved_at: DateTime<Utc>,
}

/// Result of persisting a detected opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// First sighting — the caller should alert and start lifecycle tracking.
    New { id: i64 },
    /// Already known: `last_seen_at` / `seen_count` were bumped, nothing else changed.
    Repeat { id: i64, seen_count: i64 },
}

impl Recorded {
    pub fn id(self) -> i64 {
        match self {
            Self::New { id } | Self::Repeat { id, .. } => id,
        }
    }

    pub fn is_new(self) -> bool {
        matches!(self, Self::New { .. })
    }
}

/// Stable identity of an opportunity across scan cycles.
///
/// Structure (kind + the legs' token ids) plus a price signature (each leg's *executable*
/// best ask at 4 dp) plus the construction (taker-sized vs maker-only). Consecutive scans
/// over an unchanged book therefore produce one row; a genuine quote change produces a new
/// row, because the economics — and hence the thing whose persistence we are measuring —
/// are different. Order of legs is irrelevant: the parts are sorted.
pub fn dedupe_key(op: &Opportunity) -> String {
    let mut parts: Vec<String> = op
        .legs
        .iter()
        .map(|l| format!("{}@{:.4}", l.token_id, l.best_ask))
        .collect();
    parts.sort();
    format!(
        "v1|{}|{}|{}",
        op.kind.as_str(),
        if op.maker_only { "maker" } else { "taker" },
        parts.join(",")
    )
}

// ---------------------------------------------------------------------------------
// Rows read back for reporting
// ---------------------------------------------------------------------------------

/// The subset of an opportunity row the daily summary aggregates over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunityRow {
    pub id: i64,
    pub dedupe_key: String,
    pub kind: String,
    pub category: String,
    pub event_slug: String,
    pub net_taker: Decimal,
    pub net_maker: Option<Decimal>,
    pub net_taker_total: Decimal,
    pub net_maker_total: Option<Decimal>,
    pub capital_required: Decimal,
    pub executable_size: Decimal,
    pub maker_only: bool,
    pub status: String,
    pub persistence_ms: Option<i64>,
    pub seen_count: i64,
    pub simulated_pnl_taker: Option<Decimal>,
    pub simulated_pnl_maker: Option<Decimal>,
    pub detected_at: String,
    /// Milliseconds from the triggering market data to detection. `None` for anything the
    /// REST scan found — there is no frame to measure against.
    pub detection_latency_ms: Option<i64>,
}

/// The richer projection the dashboard renders. Separate from [`OpportunityRow`] on
/// purpose: the daily summary aggregates a fixed set of fields and must not grow a
/// dependency on what a screen happens to show this week.
///
/// Every money field stays a `Decimal` here and is serialised as a *string* at the JSON
/// boundary (`src/dashboard`), never as a JSON number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardRow {
    pub id: i64,
    pub kind: String,
    pub label: String,
    pub category: String,
    pub event_slug: String,
    pub event_title: String,
    pub fee_rate: Decimal,
    pub payout: Decimal,
    pub gross_gap: Decimal,
    pub spread_cost: Option<Decimal>,
    pub net_taker: Decimal,
    pub net_maker: Option<Decimal>,
    pub executable_size: Decimal,
    pub capital_required: Decimal,
    pub net_taker_total: Decimal,
    pub net_maker_total: Option<Decimal>,
    pub maker_only: bool,
    pub status: String,
    pub detected_at: String,
    pub resolved_at: Option<String>,
    pub persistence_ms: Option<i64>,
    pub detection_latency_ms: Option<i64>,
    pub simulated_pnl_taker: Option<Decimal>,
    pub simulated_pnl_maker: Option<Decimal>,
}

/// One closed maker-fill simulation, read back for the report and the dashboard (M8).
///
/// The per-leg placement snapshot stays in the database as JSON; nothing that aggregates
/// these rows needs it, and re-parsing it here would put a re-derivable detail in front of
/// every reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakerSimRow {
    pub id: i64,
    pub opportunity_id: i64,
    pub event_slug: String,
    pub kind: String,
    pub category: String,
    pub legs_total: i64,
    pub legs_filled: i64,
    pub net_maker_total: Decimal,
    /// Credited only on `maker_filled`. This — summed — is the maker P&L lower bound.
    pub pnl_lower_bound: Option<Decimal>,
    /// Only on `maker_partial`: what the legs that *did* fill would have cost.
    pub legging_exposure: Option<Decimal>,
    pub prints_observed: i64,
    pub time_to_fill_ms: Option<i64>,
    pub status: String,
    /// The window contained time with no trade-print feed, so an unfilled verdict on this
    /// row is an absence of evidence, not evidence of absence.
    pub no_print_feed: bool,
    pub opened_at: String,
    pub closed_at: String,
}

/// Scan telemetry summed over an arbitrary window of hourly buckets (dashboard funnel).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowScanTotals {
    pub cycles: i64,
    pub errors: i64,
    /// Buckets in the window that predate the M7 counters. While this is non-zero the
    /// funnel's first stages cover only part of the window and must say so.
    pub buckets: i64,
    pub gaps_detected: i64,
    pub fee_survivors: i64,
    /// Opportunities emitted per detection pass, summed — *not* distinct rows.
    pub opportunities: i64,
    pub new_opportunities: i64,
}

/// Daemon state that lives only in the running process's memory, republished to SQLite so
/// a read-only reader can see it. No money values: counts, flags and timestamps only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeStatus {
    /// Always `dry-run` in Phase A; carried so the dashboard states it from the daemon's
    /// own mouth rather than from its own config file.
    pub mode: String,
    pub started_at: String,
    pub universe_events: i64,
    pub universe_markets: i64,
    pub universe_tokens: i64,
    pub universe_refreshed_at: Option<String>,
    pub universe_refresh_secs: u64,
    pub scan_interval_secs: u64,
    /// Last discovery pass that returned a usable universe.
    pub last_good_discovery_at: Option<String>,
    /// Consecutive discovery passes that parsed cleanly and yielded zero events
    /// (`ApiError::EmptyDiscovery`). The break state's primary trigger.
    pub discovery_empty_streak: i64,
    pub discovery_error: Option<String>,
    pub stream_enabled: bool,
    pub stream_shards: i64,
    pub stream_shards_connected: i64,
    /// The stream was handed over to REST-only polling (and is being re-probed).
    pub stream_rest_only: bool,
    pub books_total: i64,
    pub books_stale: i64,
    pub frames: u64,
    pub delta_entries_applied: u64,
    pub frames_unrecognized: u64,
    /// Recognized-and-ignored `last_trade_price` frames. Separate from
    /// `frames_unrecognized` on purpose: only the latter means the wire format drifted.
    pub trade_prints: u64,
    /// M7.1 — loop progress, published by a heartbeat task that does **not** run inside the
    /// scan loop. Together with the row's own `updated_at` these separate the two failures
    /// a single timestamp used to conflate:
    ///
    /// * a stale row = the process is gone;
    /// * a fresh row with an old `last_progress_at` = the process is alive and its loop is
    ///   stuck (or merely very slow, which is why the threshold is minutes).
    ///
    /// `last_progress_at` moves on every finished unit of work — a discovery pass, a book
    /// batch, a detection pass — so a 195 s REST sweep is progress, not silence.
    pub last_progress_at: Option<String>,
    /// What the loop last finished (`rest_sweep`, `discovery`, `stream_detect`, …). The
    /// "last known position" the watchdog names when it gives up.
    pub last_progress_phase: String,
    /// Completed full loop iterations, and when the last one finished.
    pub ticks_completed: u64,
    pub last_tick_completed_at: Option<String>,
    /// `daemon.watchdog_stall_secs`: after this long without progress the daemon exits for
    /// a restart policy to revive. `0` = watchdog disabled. Carried so the dashboard can
    /// say what is about to happen rather than guess.
    pub watchdog_stall_secs: u64,
    /// Did the most recent REST call succeed? With the socket down too, this is what
    /// separates a fallback from a network outage.
    pub rest_ok: bool,
    pub rest_failure_streak: i64,
    /// Markets costed from the category table because the API stated nothing usable.
    pub fee_fallback_markets: i64,
    pub fee_unsupported_formula: i64,
    pub markets_with_api_fee: i64,
    pub telegram_circuit_open: bool,
    pub alerts_sent: u64,
    pub alerts_failed: u64,
    pub alerts_cooldown_held: u64,
    /// M8 — the maker-fill simulator's live state. Simulations in flight exist only in the
    /// daemon's memory (only closed ones reach `maker_sims`), so this is the only way a
    /// reader can see them at all.
    pub maker_sim_enabled: bool,
    pub maker_sim_window_secs: u64,
    pub maker_sims_open: i64,
    pub maker_sims_opened: u64,
    pub maker_sims_filled: u64,
    pub maker_sims_partial: u64,
    pub maker_sims_unfilled: u64,
    pub maker_sims_untracked: u64,
    /// Trade prints that moved a tracked queue. Zero while the simulator is tracking
    /// something is the signal that the prints are not arriving — which is a different
    /// finding from "our quotes do not fill".
    pub maker_sim_prints_matched: u64,
    /// Whether a trade-print feed is running at all. False in REST-only fallback, where no
    /// simulation can fill and every verdict is flagged `no_print_feed`.
    pub maker_sim_print_feed_live: bool,
    /// `scan.floors.default.taker` as an exact decimal string — the "net floor" the
    /// opportunity table's note quotes. A string, because it is money.
    pub net_floor_default_taker: String,
}

/// A [`RuntimeStatus`] plus the moment the daemon wrote it. The age is load-bearing: a
/// status from ten minutes ago describes a daemon that is no longer talking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatusRecord {
    pub updated_at: DateTime<Utc>,
    pub status: RuntimeStatus,
}

/// Scan-loop telemetry for a day, summed over the hourly buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanTotals {
    pub cycles: i64,
    pub errors: i64,
    pub duration_ms_total: i64,
    pub duration_ms_max: i64,
    pub markets_scanned_max: i64,
    pub books_fetched_max: i64,
}

/// One scan cycle's telemetry, folded into the hourly bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleStats {
    pub events: i64,
    pub markets: i64,
    pub books: i64,
    pub opportunities: i64,
    pub new_opportunities: i64,
    pub duration_ms: i64,
    pub failed: bool,
    /// M7 funnel stage 1: constructions whose executable ask sum was below the payout,
    /// counted *before* any cost filtering. See [`crate::detect::ScanCounters`].
    pub gaps_detected: i64,
    /// M7 funnel stage 3: of those, the ones still net-positive as a taker once the fee
    /// curve and slippage were charged — before the configured net floor.
    pub fee_survivors: i64,
}

// ---------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------

pub struct Store {
    /// A std mutex, never held across an `.await`: every statement here is a short local
    /// SQLite call.
    conn: Mutex<Connection>,
    path: String,
}

impl Store {
    /// Open (creating parent directories) and migrate.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Dir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let label = path.display().to_string();
        let conn = Connection::open(path).map_err(|source| StoreError::Sqlite {
            path: label.clone(),
            source,
        })?;
        Self::from_connection(conn, label)
    }

    /// In-memory database — used by the tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|source| StoreError::Sqlite {
            path: ":memory:".to_string(),
            source,
        })?;
        Self::from_connection(conn, ":memory:".to_string())
    }

    /// Open an **existing** database for reading only (M7 dashboard).
    ///
    /// Read-only by construction, not by convention: `SQLITE_OPEN_READ_ONLY` plus
    /// `PRAGMA query_only` means the dashboard process cannot write a byte even if a
    /// future edit asked it to, and no migration runs from here — a reader must never
    /// change a schema the writer owns. WAL lets it read while the daemon writes.
    ///
    /// The file must already exist and already be migrated; both failures are the
    /// operator's answer ("start the daemon first"), not something to paper over by
    /// creating an empty database that would render as a healthy, empty soak.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let label = path.display().to_string();
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| StoreError::Sqlite {
            path: label.clone(),
            source,
        })?;
        let store = Self {
            conn: Mutex::new(conn),
            path: label,
        };
        store.exec_batch(
            "PRAGMA query_only=1;
             PRAGMA busy_timeout=3000;",
        )?;
        Ok(store)
    }

    /// Highest applied migration version, or 0 for a database with no migration table.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.lock();
        let has_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'migrations'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| self.err(e))?;
        if has_table == 0 {
            return Ok(0);
        }
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM migrations",
            [],
            |r| r.get(0),
        )
        .map_err(|e| self.err(e))
    }

    fn from_connection(conn: Connection, path: String) -> Result<Self> {
        let store = Self {
            conn: Mutex::new(conn),
            path,
        };
        store.exec_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )?;
        store.migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned lock means another thread panicked mid-statement; the connection is
        // still usable and losing telemetry is worse than carrying on.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn err(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Sqlite {
            path: self.path.clone(),
            source,
        }
    }

    fn exec_batch(&self, sql: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(sql).map_err(|e| self.err(e))
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS migrations (
                 version    INTEGER PRIMARY KEY,
                 name       TEXT NOT NULL,
                 applied_at TEXT NOT NULL
             );",
        )
        .map_err(|e| self.err(e))?;

        let applied: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM migrations",
                [],
                |r| r.get(0),
            )
            .map_err(|e| self.err(e))?;

        for (version, name, ddl) in MIGRATIONS {
            if *version <= applied {
                continue;
            }
            conn.execute_batch(&format!("BEGIN; {ddl}"))
                .map_err(|e| self.err(e))?;
            conn.execute(
                "INSERT INTO migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![version, name, now_str(Utc::now())],
            )
            .map_err(|e| self.err(e))?;
            conn.execute_batch("COMMIT;").map_err(|e| self.err(e))?;
            tracing::info!(version, name, "applied database migration");
        }
        Ok(())
    }

    /// Insert a newly detected opportunity, or bump `last_seen_at` if it is the same one
    /// we already recorded (same [`dedupe_key`]).
    ///
    /// `detection_latency_ms` is the M6 stream measurement (`None` from the REST path) and
    /// is written **only on first insert**: a repeat sighting is the same opportunity seen
    /// again, and overwriting the first measurement with a later one would turn the honest
    /// "how fast did we see it" into "how recently did we look".
    pub fn record_opportunity(
        &self,
        op: &Opportunity,
        books: &BookMap,
        now: DateTime<Utc>,
        detection_latency_ms: Option<i64>,
    ) -> Result<Recorded> {
        let key = dedupe_key(op);
        let ts = now_str(now);
        let legs_json = serde_json::to_string(&op.legs).unwrap_or_else(|_| "[]".to_string());
        let books_json = snapshot_json(op, books);
        let flags_json =
            serde_json::to_string(&op.resolution_flags).unwrap_or_else(|_| "[]".to_string());

        let conn = self.lock();
        let (id, seen_count): (i64, i64) = conn
            .query_row(
                "INSERT INTO opportunities (
                     dedupe_key, kind, label, category, event_slug, event_title, fee_rate,
                     payout, gross_gap, slippage_cost, spread_cost, fee_taker, net_taker,
                     net_maker, executable_size, capital_required, net_taker_total,
                     net_maker_total, conversion_required, maker_only, resolution_flags,
                     legs_json, books_json, detected_at, last_seen_at, seen_count, status,
                     detection_latency_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?24, 1, ?25, ?26
                 )
                 ON CONFLICT(dedupe_key) DO UPDATE SET
                     last_seen_at = excluded.last_seen_at,
                     seen_count   = opportunities.seen_count + 1
                 RETURNING id, seen_count",
                params![
                    key,
                    op.kind.as_str(),
                    op.label.as_str(),
                    op.category.as_str(),
                    op.event_slug,
                    op.event_title,
                    dec(op.fee_rate),
                    dec(op.payout),
                    dec(op.gross_gap),
                    dec(op.slippage_cost),
                    op.spread_cost.map(dec),
                    dec(op.fee_taker),
                    dec(op.net_taker),
                    op.net_maker.map(dec),
                    dec(op.executable_size),
                    dec(op.capital_required),
                    dec(op.net_taker_total),
                    op.net_maker_total.map(dec),
                    op.conversion_required as i64,
                    op.maker_only as i64,
                    flags_json,
                    legs_json,
                    books_json,
                    ts,
                    LifecycleStatus::Open.as_str(),
                    detection_latency_ms,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| self.err(e))?;

        Ok(if seen_count <= 1 {
            Recorded::New { id }
        } else {
            Recorded::Repeat { id, seen_count }
        })
    }

    /// Write back the lifecycle verdict. Idempotent per opportunity id.
    pub fn record_lifecycle(&self, id: i64, outcome: &LifecycleOutcome) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE opportunities SET
                 status                 = ?2,
                 repolls                = ?3,
                 first_repoll_available = ?4,
                 persistence_ms         = ?5,
                 simulated_pnl_taker    = ?6,
                 simulated_pnl_maker    = ?7,
                 resolved_at            = ?8
             WHERE id = ?1",
            params![
                id,
                outcome.status.as_str(),
                outcome.repolls,
                outcome.first_repoll_available.map(|b| b as i64),
                outcome.persistence_ms,
                outcome.simulated_pnl_taker.map(dec),
                outcome.simulated_pnl_maker.map(dec),
                now_str(outcome.resolved_at),
            ],
        )
        .map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Persist one **closed** maker-fill simulation (M8).
    ///
    /// Insert-only, by design: there is no update path, so a verdict cannot be revised by
    /// any code that exists. The row is written once, when the window closed.
    pub fn record_maker_sim(&self, sim: &crate::makersim::ClosedSim) -> Result<i64> {
        let legs_json = serde_json::to_string(&sim.legs).unwrap_or_else(|_| "[]".to_string());
        let conn = self.lock();
        conn.query_row(
            "INSERT INTO maker_sims (
                 opportunity_id, event_slug, kind, category, legs_json, legs_total,
                 legs_filled, net_maker_total, pnl_lower_bound, legging_exposure,
                 prints_observed, time_to_fill_ms, status, no_print_feed, opened_at, closed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             RETURNING id",
            params![
                sim.opportunity_id,
                sim.event_slug,
                sim.kind,
                sim.category,
                legs_json,
                sim.legs_total as i64,
                sim.legs_filled as i64,
                dec(sim.net_maker_total),
                sim.pnl_lower_bound.map(dec),
                sim.legging_exposure.map(dec),
                sim.prints_observed as i64,
                sim.time_to_fill_ms,
                sim.status.as_str(),
                sim.no_print_feed as i64,
                now_str(sim.opened_at),
                now_str(sim.closed_at),
            ],
            |row| row.get(0),
        )
        .map_err(|e| self.err(e))
    }

    /// Closed simulations opened on `day` (UTC), oldest first — the daily summary's basis.
    ///
    /// Keyed on `opened_at`, not `closed_at`, so a simulation belongs to the day whose
    /// market conditions produced it. A window that straddles midnight is therefore counted
    /// on the day it started, which is also the day its opportunity row is on.
    pub fn maker_sims_for_day(&self, day: NaiveDate) -> Result<Vec<MakerSimRow>> {
        let (from, to) = day_bounds(day);
        self.maker_sims_where(
            "opened_at >= ?1 AND opened_at < ?2 ORDER BY opened_at ASC, id ASC",
            params![from, to],
        )
    }

    /// Closed simulations opened at or after `from`, newest first (the dashboard window).
    pub fn maker_sims_since(&self, from: DateTime<Utc>, limit: usize) -> Result<Vec<MakerSimRow>> {
        self.maker_sims_where(
            "opened_at >= ?1 ORDER BY opened_at DESC, id DESC LIMIT ?2",
            params![now_str(from), limit as i64],
        )
    }

    fn maker_sims_where(
        &self,
        predicate: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<MakerSimRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, opportunity_id, event_slug, kind, category, legs_total,
                        legs_filled, net_maker_total, pnl_lower_bound, legging_exposure,
                        prints_observed, time_to_fill_ms, status, no_print_feed,
                        opened_at, closed_at
                 FROM maker_sims WHERE {predicate}"
            ))
            .map_err(|e| self.err(e))?;
        let raw = stmt
            .query_map(args, |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)? != 0,
                    row.get::<_, String>(14)?,
                    row.get::<_, String>(15)?,
                ))
            })
            .map_err(|e| self.err(e))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))?;

        raw.into_iter()
            .map(|r| {
                Ok(MakerSimRow {
                    id: r.0,
                    opportunity_id: r.1,
                    event_slug: r.2,
                    kind: r.3,
                    category: r.4,
                    legs_total: r.5,
                    legs_filled: r.6,
                    net_maker_total: parse_dec("net_maker_total", &r.7)?,
                    pnl_lower_bound: parse_opt_dec("pnl_lower_bound", r.8.as_deref())?,
                    legging_exposure: parse_opt_dec("legging_exposure", r.9.as_deref())?,
                    prints_observed: r.10,
                    time_to_fill_ms: r.11,
                    status: r.12,
                    no_print_feed: r.13,
                    opened_at: r.14,
                    closed_at: r.15,
                })
            })
            .collect()
    }

    /// Fold one scan cycle into its UTC-hour bucket (low cardinality: 24 rows/day).
    pub fn record_cycle(&self, stats: &CycleStats, now: DateTime<Utc>) -> Result<()> {
        let hour = now.format("%Y-%m-%dT%H").to_string();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO scan_stats (
                 hour_utc, cycles, errors, events_scanned, markets_scanned, books_fetched,
                 duration_ms_total, duration_ms_max, opportunities, new_opportunities,
                 updated_at, gaps_detected, fee_survivors
             ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(hour_utc) DO UPDATE SET
                 cycles            = scan_stats.cycles + 1,
                 errors            = scan_stats.errors + excluded.errors,
                 events_scanned    = MAX(scan_stats.events_scanned, excluded.events_scanned),
                 markets_scanned   = MAX(scan_stats.markets_scanned, excluded.markets_scanned),
                 books_fetched     = MAX(scan_stats.books_fetched, excluded.books_fetched),
                 duration_ms_total = scan_stats.duration_ms_total + excluded.duration_ms_total,
                 duration_ms_max   = MAX(scan_stats.duration_ms_max, excluded.duration_ms_max),
                 opportunities     = scan_stats.opportunities + excluded.opportunities,
                 new_opportunities = scan_stats.new_opportunities + excluded.new_opportunities,
                 gaps_detected     = scan_stats.gaps_detected + excluded.gaps_detected,
                 fee_survivors     = scan_stats.fee_survivors + excluded.fee_survivors,
                 updated_at        = excluded.updated_at",
            params![
                hour,
                stats.failed as i64,
                stats.events,
                stats.markets,
                stats.books,
                stats.duration_ms,
                stats.opportunities,
                stats.new_opportunities,
                now_str(now),
                stats.gaps_detected,
                stats.fee_survivors,
            ],
        )
        .map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Every opportunity detected on `day` (UTC), oldest first.
    pub fn opportunities_for_day(&self, day: NaiveDate) -> Result<Vec<OpportunityRow>> {
        let (from, to) = day_bounds(day);
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, dedupe_key, kind, category, event_slug, net_taker, net_maker,
                        net_taker_total, net_maker_total, capital_required, executable_size,
                        maker_only, status, persistence_ms, seen_count,
                        simulated_pnl_taker, simulated_pnl_maker, detected_at,
                        detection_latency_ms
                 FROM opportunities
                 WHERE detected_at >= ?1 AND detected_at < ?2
                 ORDER BY detected_at ASC, id ASC",
            )
            .map_err(|e| self.err(e))?;

        let rows = stmt
            .query_map(params![from, to], |row| {
                Ok(RawRow {
                    id: row.get(0)?,
                    dedupe_key: row.get(1)?,
                    kind: row.get(2)?,
                    category: row.get(3)?,
                    event_slug: row.get(4)?,
                    net_taker: row.get(5)?,
                    net_maker: row.get(6)?,
                    net_taker_total: row.get(7)?,
                    net_maker_total: row.get(8)?,
                    capital_required: row.get(9)?,
                    executable_size: row.get(10)?,
                    maker_only: row.get::<_, i64>(11)? != 0,
                    status: row.get(12)?,
                    persistence_ms: row.get(13)?,
                    seen_count: row.get(14)?,
                    simulated_pnl_taker: row.get(15)?,
                    simulated_pnl_maker: row.get(16)?,
                    detected_at: row.get(17)?,
                    detection_latency_ms: row.get(18)?,
                })
            })
            .map_err(|e| self.err(e))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))?;

        rows.into_iter().map(RawRow::into_row).collect()
    }

    /// Scan telemetry for `day`, summed across the hourly buckets.
    pub fn scan_totals_for_day(&self, day: NaiveDate) -> Result<ScanTotals> {
        let prefix = format!("{}T%", day.format("%Y-%m-%d"));
        let conn = self.lock();
        conn.query_row(
            "SELECT COALESCE(SUM(cycles), 0), COALESCE(SUM(errors), 0),
                    COALESCE(SUM(duration_ms_total), 0), COALESCE(MAX(duration_ms_max), 0),
                    COALESCE(MAX(markets_scanned), 0), COALESCE(MAX(books_fetched), 0)
             FROM scan_stats WHERE hour_utc LIKE ?1",
            params![prefix],
            |row| {
                Ok(ScanTotals {
                    cycles: row.get(0)?,
                    errors: row.get(1)?,
                    duration_ms_total: row.get(2)?,
                    duration_ms_max: row.get(3)?,
                    markets_scanned_max: row.get(4)?,
                    books_fetched_max: row.get(5)?,
                })
            },
        )
        .map_err(|e| self.err(e))
    }

    /// Republish the daemon's in-memory runtime state (M7). One row, upserted — cheap
    /// enough to call every few seconds, and it never grows.
    pub fn write_runtime_status(&self, status: &RuntimeStatus, now: DateTime<Utc>) -> Result<()> {
        let payload = serde_json::to_string(status).unwrap_or_else(|_| "{}".to_string());
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runtime_status (id, updated_at, payload) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at,
                                           payload    = excluded.payload",
            params![now_str(now), payload],
        )
        .map_err(|e| self.err(e))?;
        Ok(())
    }

    /// The daemon's last published runtime state, with the moment it was written.
    ///
    /// `None` means the row has never been written — a daemon that has not started, or
    /// one older than this build. Never invent a default: "we do not know" is a state the
    /// dashboard has to render as such.
    pub fn read_runtime_status(&self) -> Result<Option<RuntimeStatusRecord>> {
        let conn = self.lock();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT updated_at, payload FROM runtime_status WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(self.err(other)),
            })?;
        let Some((updated_at, payload)) = row else {
            return Ok(None);
        };
        let updated_at = DateTime::parse_from_rfc3339(&updated_at)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|_| StoreError::Value {
                column: "runtime_status.updated_at",
                value: updated_at.clone(),
            })?;
        let status: RuntimeStatus =
            serde_json::from_str(&payload).map_err(|_| StoreError::Value {
                column: "runtime_status.payload",
                value: payload,
            })?;
        Ok(Some(RuntimeStatusRecord { updated_at, status }))
    }

    /// Every opportunity detected at or after `from`, **newest first**.
    ///
    /// Ordering is the server's job (the design forbids parsing Decimal strings into JS
    /// floats to sort them), and `limit` is a memory guard, not a page size: the caller
    /// aggregates over the whole window.
    pub fn opportunities_since(
        &self,
        from: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<DashboardRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, label, category, event_slug, event_title, fee_rate, payout,
                        gross_gap, spread_cost, net_taker, net_maker, executable_size,
                        capital_required, net_taker_total, net_maker_total, maker_only, status,
                        detected_at, resolved_at, persistence_ms, detection_latency_ms,
                        simulated_pnl_taker, simulated_pnl_maker
                 FROM opportunities
                 WHERE detected_at >= ?1
                 ORDER BY detected_at DESC, id DESC
                 LIMIT ?2",
            )
            .map_err(|e| self.err(e))?;
        let raw = stmt
            .query_map(params![now_str(from), limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, i64>(16)? != 0,
                    row.get::<_, String>(17)?,
                    row.get::<_, String>(18)?,
                    row.get::<_, Option<String>>(19)?,
                    row.get::<_, Option<i64>>(20)?,
                    row.get::<_, Option<i64>>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, Option<String>>(23)?,
                ))
            })
            .map_err(|e| self.err(e))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| self.err(e))?;

        raw.into_iter()
            .map(|r| {
                Ok(DashboardRow {
                    id: r.0,
                    kind: r.1,
                    label: r.2,
                    category: r.3,
                    event_slug: r.4,
                    event_title: r.5,
                    fee_rate: parse_dec("fee_rate", &r.6)?,
                    payout: parse_dec("payout", &r.7)?,
                    gross_gap: parse_dec("gross_gap", &r.8)?,
                    spread_cost: parse_opt_dec("spread_cost", r.9.as_deref())?,
                    net_taker: parse_dec("net_taker", &r.10)?,
                    net_maker: parse_opt_dec("net_maker", r.11.as_deref())?,
                    executable_size: parse_dec("executable_size", &r.12)?,
                    capital_required: parse_dec("capital_required", &r.13)?,
                    net_taker_total: parse_dec("net_taker_total", &r.14)?,
                    net_maker_total: parse_opt_dec("net_maker_total", r.15.as_deref())?,
                    maker_only: r.16,
                    status: r.17,
                    detected_at: r.18,
                    resolved_at: r.19,
                    persistence_ms: r.20,
                    detection_latency_ms: r.21,
                    simulated_pnl_taker: parse_opt_dec("simulated_pnl_taker", r.22.as_deref())?,
                    simulated_pnl_maker: parse_opt_dec("simulated_pnl_maker", r.23.as_deref())?,
                })
            })
            .collect()
    }

    /// Scan telemetry summed over the hourly buckets at or after `from`.
    ///
    /// The buckets are hour-granular, so the window is rounded *out* to the containing
    /// hour: the funnel's denominator can only ever be too generous, never too flattering.
    pub fn scan_totals_since(&self, from: DateTime<Utc>) -> Result<WindowScanTotals> {
        let hour = from.format("%Y-%m-%dT%H").to_string();
        let conn = self.lock();
        conn.query_row(
            "SELECT COALESCE(SUM(cycles), 0), COALESCE(SUM(errors), 0), COUNT(*),
                    COALESCE(SUM(gaps_detected), 0), COALESCE(SUM(fee_survivors), 0),
                    COALESCE(SUM(opportunities), 0), COALESCE(SUM(new_opportunities), 0)
             FROM scan_stats WHERE hour_utc >= ?1",
            params![hour],
            |row| {
                Ok(WindowScanTotals {
                    cycles: row.get(0)?,
                    errors: row.get(1)?,
                    buckets: row.get(2)?,
                    gaps_detected: row.get(3)?,
                    fee_survivors: row.get(4)?,
                    opportunities: row.get(5)?,
                    new_opportunities: row.get(6)?,
                })
            },
        )
        .map_err(|e| self.err(e))
    }

    /// The earliest hourly telemetry bucket (`YYYY-MM-DDTHH`), i.e. when this database
    /// first saw a scan cycle.
    ///
    /// This, not the process start time, is the soak's day 1: the evidence window survives
    /// restarts, and a daemon restarted this morning has not reset a five-day soak.
    pub fn first_scan_hour(&self) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row("SELECT MIN(hour_utc) FROM scan_stats", [], |r| r.get(0))
            .map_err(|e| self.err(e))
    }

    /// Total opportunity rows on disk — the rail's "store" line.
    pub fn opportunity_row_count(&self) -> Result<i64> {
        let conn = self.lock();
        conn.query_row("SELECT COUNT(*) FROM opportunities", [], |r| r.get(0))
            .map_err(|e| self.err(e))
    }

    pub fn save_daily_summary(&self, day: NaiveDate, markdown: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO daily_summaries (day, generated_at, markdown) VALUES (?1, ?2, ?3)
             ON CONFLICT(day) DO UPDATE SET generated_at = excluded.generated_at,
                                            markdown     = excluded.markdown",
            params![day.to_string(), now_str(Utc::now()), markdown],
        )
        .map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Any row still `open` — used at startup to close out rows a crash left behind.
    pub fn close_stale_open_rows(&self, now: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock();
        let n = conn
            .execute(
                "UPDATE opportunities SET status = ?1, resolved_at = ?2 WHERE status = ?3",
                params![
                    LifecycleStatus::Unresolved.as_str(),
                    now_str(now),
                    LifecycleStatus::Open.as_str()
                ],
            )
            .map_err(|e| self.err(e))?;
        Ok(n)
    }

    /// Test/debug helper: read one row's status and simulated P&L.
    #[cfg(test)]
    pub fn status_of(&self, id: i64) -> Result<(String, Option<Decimal>)> {
        let conn = self.lock();
        let (status, pnl): (String, Option<String>) = conn
            .query_row(
                "SELECT status, simulated_pnl_taker FROM opportunities WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| self.err(e))?;
        let pnl = match pnl {
            Some(s) => Some(parse_dec("simulated_pnl_taker", &s)?),
            None => None,
        };
        Ok((status, pnl))
    }

    /// Test/debug helper: the stored book snapshot JSON for an opportunity.
    #[cfg(test)]
    pub fn books_json(&self, id: i64) -> Result<String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT books_json FROM opportunities WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(|e| self.err(e))
    }
}

/// Raw text form straight out of SQLite, before decimals are parsed.
struct RawRow {
    id: i64,
    dedupe_key: String,
    kind: String,
    category: String,
    event_slug: String,
    net_taker: String,
    net_maker: Option<String>,
    net_taker_total: String,
    net_maker_total: Option<String>,
    capital_required: String,
    executable_size: String,
    maker_only: bool,
    status: String,
    persistence_ms: Option<i64>,
    seen_count: i64,
    simulated_pnl_taker: Option<String>,
    simulated_pnl_maker: Option<String>,
    detected_at: String,
    detection_latency_ms: Option<i64>,
}

impl RawRow {
    fn into_row(self) -> Result<OpportunityRow> {
        Ok(OpportunityRow {
            id: self.id,
            dedupe_key: self.dedupe_key,
            kind: self.kind,
            category: self.category,
            event_slug: self.event_slug,
            net_taker: parse_dec("net_taker", &self.net_taker)?,
            net_maker: parse_opt_dec("net_maker", self.net_maker.as_deref())?,
            net_taker_total: parse_dec("net_taker_total", &self.net_taker_total)?,
            net_maker_total: parse_opt_dec("net_maker_total", self.net_maker_total.as_deref())?,
            capital_required: parse_dec("capital_required", &self.capital_required)?,
            executable_size: parse_dec("executable_size", &self.executable_size)?,
            maker_only: self.maker_only,
            status: self.status,
            persistence_ms: self.persistence_ms,
            seen_count: self.seen_count,
            simulated_pnl_taker: parse_opt_dec(
                "simulated_pnl_taker",
                self.simulated_pnl_taker.as_deref(),
            )?,
            simulated_pnl_maker: parse_opt_dec(
                "simulated_pnl_maker",
                self.simulated_pnl_maker.as_deref(),
            )?,
            detected_at: self.detected_at,
            detection_latency_ms: self.detection_latency_ms,
        })
    }
}

/// The books that triggered this opportunity, keyed by token id. Only the legs' books are
/// kept: the rest of the universe is noise and would bloat the row.
fn snapshot_json(op: &Opportunity, books: &BookMap) -> String {
    let snapshot: std::collections::BTreeMap<&str, &crate::types::OrderBook> = op
        .legs
        .iter()
        .filter_map(|leg| books.get(&leg.token_id).map(|b| (leg.token_id.as_str(), b)))
        .collect();
    serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_string())
}

fn dec(d: Decimal) -> String {
    d.to_string()
}

fn parse_dec(column: &'static str, raw: &str) -> Result<Decimal> {
    Decimal::from_str(raw).map_err(|_| StoreError::Value {
        column,
        value: raw.to_string(),
    })
}

fn parse_opt_dec(column: &'static str, raw: Option<&str>) -> Result<Option<Decimal>> {
    match raw {
        None => Ok(None),
        Some(s) => parse_dec(column, s).map(Some),
    }
}

/// RFC3339 with milliseconds and a `Z` suffix — fixed width, so lexicographic order is
/// chronological order and the day-range queries can use the index.
pub fn now_str(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn day_bounds(day: NaiveDate) -> (String, String) {
    let next = day.succ_opt().unwrap_or(day);
    (
        format!("{}T00:00:00.000Z", day.format("%Y-%m-%d")),
        format!("{}T00:00:00.000Z", next.format("%Y-%m-%d")),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::{Category, Label, Leg, OpportunityKind, OrderBook, PriceLevel, TokenId};
    use rust_decimal_macros::dec as d;

    pub(crate) fn sample_opportunity() -> Opportunity {
        Opportunity {
            kind: OpportunityKind::BinaryYesNo,
            label: Label::TrueArb,
            event_slug: "an-event".into(),
            event_title: "An event".into(),
            category: Category::new("politics"),
            fee_rate: d!(0.04),
            payout: d!(1),
            legs: vec![
                Leg {
                    token_id: TokenId::new("1001"),
                    condition_id: "0xaaa".into(),
                    question: "Yes?".into(),
                    outcome: "Yes".into(),
                    best_ask: d!(0.40),
                    best_bid: Some(d!(0.39)),
                    vwap: d!(0.40),
                    size: d!(100),
                    ask_depth: d!(500),
                    fee_rate: d!(0.04),
                },
                Leg {
                    token_id: TokenId::new("1002"),
                    condition_id: "0xaaa".into(),
                    question: "Yes?".into(),
                    outcome: "No".into(),
                    best_ask: d!(0.55),
                    best_bid: Some(d!(0.54)),
                    vwap: d!(0.55),
                    size: d!(100),
                    ask_depth: d!(500),
                    fee_rate: d!(0.04),
                },
            ],
            gross_gap: d!(0.05),
            slippage_cost: d!(0),
            spread_cost: Some(d!(0.02)),
            fee_taker: d!(0.0195),
            net_taker: d!(0.0305),
            net_maker: Some(d!(0.07)),
            executable_size: d!(100),
            capital_required: d!(95),
            net_taker_total: d!(3.05),
            net_maker_total: Some(d!(7)),
            partial_coverage: None,
            resolution_flags: vec!["single condition".into()],
            conversion_required: false,
            maker_only: false,
        }
    }

    fn sample_books() -> BookMap {
        let mut books = BookMap::new();
        books.insert(
            TokenId::new("1001"),
            OrderBook::new(
                TokenId::new("1001"),
                vec![PriceLevel::new(d!(0.39), d!(500))],
                vec![PriceLevel::new(d!(0.40), d!(500))],
            ),
        );
        books.insert(
            TokenId::new("1002"),
            OrderBook::new(
                TokenId::new("1002"),
                vec![PriceLevel::new(d!(0.54), d!(500))],
                vec![PriceLevel::new(d!(0.55), d!(500))],
            ),
        );
        // A book that is not a leg: it must not end up in the snapshot.
        books.insert(
            TokenId::new("9999"),
            OrderBook::new(TokenId::new("9999"), vec![], vec![]),
        );
        books
    }

    #[test]
    fn dedupe_key_is_stable_and_leg_order_independent() {
        let op = sample_opportunity();
        let mut reordered = op.clone();
        reordered.legs.reverse();
        assert_eq!(dedupe_key(&op), dedupe_key(&reordered));

        // Untouched economics → same key on the next scan cycle.
        assert_eq!(dedupe_key(&op), dedupe_key(&op.clone()));

        // A quote move is a different opportunity.
        let mut moved = op.clone();
        moved.legs[0].best_ask = d!(0.41);
        assert_ne!(dedupe_key(&op), dedupe_key(&moved));

        // Sub-tick noise below the 4-dp signature does not fork the key.
        let mut noise = op.clone();
        noise.legs[0].best_ask = d!(0.4000001);
        assert_eq!(dedupe_key(&op), dedupe_key(&noise));

        // Same books, different construction → different rows.
        let mut maker = op.clone();
        maker.maker_only = true;
        assert_ne!(dedupe_key(&op), dedupe_key(&maker));

        // Same books, different strategy → different rows.
        let mut kind = op.clone();
        kind.kind = OpportunityKind::NegRiskYesSide;
        assert_ne!(dedupe_key(&op), dedupe_key(&kind));

        // Depth (and therefore size) moving on deeper levels does not fork the key.
        let mut deeper = op;
        deeper.executable_size = d!(250);
        deeper.legs[0].ask_depth = d!(9000);
        assert_eq!(dedupe_key(&deeper), dedupe_key(&sample_opportunity()));
    }

    #[test]
    fn opportunity_round_trips_through_sqlite() {
        let store = Store::in_memory().expect("in-memory db");
        let op = sample_opportunity();
        let now = Utc::now();

        let first = store
            .record_opportunity(&op, &sample_books(), now, Some(42))
            .expect("insert");
        assert!(first.is_new(), "first sighting must be a new row");

        let rows = store
            .opportunities_for_day(now.date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, first.id());
        assert_eq!(row.kind, "binary_yes_no");
        assert_eq!(row.category, "politics");
        assert_eq!(row.net_taker, d!(0.0305));
        assert_eq!(row.net_maker, Some(d!(0.07)));
        assert_eq!(row.net_taker_total, d!(3.05));
        assert_eq!(row.net_maker_total, Some(d!(7)));
        assert_eq!(row.capital_required, d!(95));
        assert_eq!(row.executable_size, d!(100));
        assert!(!row.maker_only);
        assert_eq!(row.status, "open");
        assert_eq!(row.seen_count, 1);
        assert_eq!(row.dedupe_key, dedupe_key(&op));
        assert_eq!(row.detection_latency_ms, Some(42));

        // The triggering snapshot is stored for the legs only.
        let snapshot = store.books_json(first.id()).expect("snapshot");
        assert!(snapshot.contains("\"1001\"") && snapshot.contains("\"1002\""));
        assert!(!snapshot.contains("9999"));
        assert!(snapshot.contains("0.40"), "prices survive as exact strings");
    }

    #[test]
    fn repeated_sightings_update_instead_of_duplicating() {
        let store = Store::in_memory().expect("db");
        let op = sample_opportunity();
        let books = sample_books();
        let t0 = Utc::now();

        let a = store
            .record_opportunity(&op, &books, t0, Some(11))
            .expect("insert");
        let b = store
            .record_opportunity(&op, &books, t0 + chrono::Duration::seconds(5), Some(999))
            .expect("second sighting");
        let c = store
            .record_opportunity(&op, &books, t0 + chrono::Duration::seconds(10), None)
            .expect("third sighting");

        assert!(a.is_new());
        assert!(!b.is_new() && !c.is_new());
        assert_eq!(a.id(), b.id());
        assert_eq!(
            c,
            Recorded::Repeat {
                id: a.id(),
                seen_count: 3
            }
        );

        let rows = store
            .opportunities_for_day(t0.date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 1, "one row per distinct opportunity");
        assert_eq!(rows[0].seen_count, 3);
    }

    #[test]
    fn lifecycle_verdict_is_written_back() {
        let store = Store::in_memory().expect("db");
        let op = sample_opportunity();
        let id = store
            .record_opportunity(&op, &sample_books(), Utc::now(), None)
            .expect("insert")
            .id();

        store
            .record_lifecycle(
                id,
                &LifecycleOutcome {
                    status: LifecycleStatus::FilledSimulated,
                    repolls: 3,
                    first_repoll_available: Some(true),
                    persistence_ms: Some(6_000),
                    simulated_pnl_taker: Some(d!(3.05)),
                    simulated_pnl_maker: Some(d!(7)),
                    resolved_at: Utc::now(),
                },
            )
            .expect("update");

        let (status, pnl) = store.status_of(id).expect("status");
        assert_eq!(status, "filled_simulated");
        assert_eq!(pnl, Some(d!(3.05)));
    }

    #[test]
    fn migrations_are_recorded_and_idempotent() {
        let store = Store::in_memory().expect("db");
        // Re-running the migration pass must be a no-op, not an error. This matters more
        // for v2 than for v1: `ALTER TABLE ... ADD COLUMN` fails outright on a second run,
        // so the version guard is the only thing that makes a restart safe.
        store.migrate().expect("second migrate");
        store.migrate().expect("third migrate");
        let conn = store.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM migrations", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, MIGRATIONS.len() as i64);
        let latest: i64 = conn
            .query_row("SELECT MAX(version) FROM migrations", [], |r| r.get(0))
            .expect("max");
        assert_eq!(latest, DASHBOARD_MIN_SCHEMA);
        // v3's `runtime_status` is a single-row table by constraint, and the two funnel
        // counters default to 0 so an older bucket reads as "not measured", not as a zero
        // that means something.
        let single_row: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'runtime_status'",
                [],
                |r| r.get(0),
            )
            .expect("pragma");
        assert_eq!(single_row, 1);
        for column in ["gaps_detected", "fee_survivors"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('scan_stats')
                     WHERE name = ?1 AND \"notnull\" = 1 AND dflt_value = '0'",
                    params![column],
                    |r| r.get(0),
                )
                .expect("pragma");
            assert_eq!(n, 1, "{column} must be NOT NULL DEFAULT 0");
        }
        // The v2 column exists exactly once and is nullable.
        let columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('opportunities')
                 WHERE name = 'detection_latency_ms' AND \"notnull\" = 0",
                [],
                |r| r.get(0),
            )
            .expect("pragma");
        assert_eq!(columns, 1);
    }

    /// (g) M8 — migration v4 is idempotent, and a closed simulation round-trips with its
    /// money intact as exact decimal strings.
    #[test]
    fn maker_sims_migrate_idempotently_and_round_trip() {
        use crate::makersim::{ClosedSim, PlacedLeg, SimStatus};
        use crate::types::TokenId;
        use chrono::{Duration, TimeZone};

        let store = Store::in_memory().expect("db");
        store.migrate().expect("second migrate");
        store.migrate().expect("third migrate");
        assert_eq!(store.schema_version().expect("version"), 4);

        // The FK is real: a simulation belongs to an opportunity row.
        let opportunity_id = store
            .record_opportunity(&sample_opportunity(), &sample_books(), Utc::now(), None)
            .expect("insert")
            .id();

        let opened = Utc.with_ymd_and_hms(2026, 8, 8, 9, 0, 0).unwrap();
        let leg = |token: &str, price, visible, filled| PlacedLeg {
            token_id: TokenId::new(token),
            price,
            visible_size: visible,
            our_size: d!(100),
            volume_through: if filled { visible + d!(100) } else { d!(3) },
            prints_observed: if filled { 2 } else { 1 },
            filled_at: filled.then_some(opened + Duration::seconds(12)),
            fill_ms: filled.then_some(12_000),
        };
        let sim = ClosedSim {
            opportunity_id,
            event_slug: "an-event".into(),
            kind: "binary_yes_no".into(),
            category: "politics".into(),
            legs: vec![
                leg("1001", d!(0.47), d!(250), true),
                leg("1002", d!(0.46), d!(80), false),
            ],
            legs_total: 2,
            legs_filled: 1,
            net_maker_total: d!(7.00),
            pnl_lower_bound: None,
            legging_exposure: Some(d!(47.00)),
            prints_observed: 3,
            time_to_fill_ms: None,
            status: SimStatus::MakerPartial,
            no_print_feed: false,
            opened_at: opened,
            closed_at: opened + Duration::seconds(3_600),
        };
        let id = store.record_maker_sim(&sim).expect("insert sim");
        assert!(id > 0);

        let rows = store
            .maker_sims_for_day(opened.date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.opportunity_id, opportunity_id);
        assert_eq!(row.status, "maker_partial");
        assert_eq!(row.legs_filled, 1);
        assert_eq!(row.legs_total, 2);
        assert_eq!(row.net_maker_total, d!(7.00));
        assert_eq!(row.pnl_lower_bound, None, "a partial credits nothing");
        assert_eq!(row.legging_exposure, Some(d!(47.00)));
        assert_eq!(row.prints_observed, 3);
        assert!(!row.no_print_feed);

        // Money is TEXT, never REAL: an f64 round trip is exactly how 47.00 becomes
        // 46.999999999999996.
        {
            let conn = store.lock();
            let stored: String = conn
                .query_row(
                    "SELECT legging_exposure FROM maker_sims WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .expect("raw");
            assert_eq!(stored, "47.00");
            let kind: String = conn
                .query_row(
                    "SELECT typeof(net_maker_total) FROM maker_sims WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .expect("typeof");
            assert_eq!(kind, "text");
        }

        // The placement snapshot survives: it is what makes the verdict re-derivable, and
        // what would let a later run re-score the soak under a different queue assumption.
        let legs_json: String = {
            let conn = store.lock();
            conn.query_row(
                "SELECT legs_json FROM maker_sims WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .expect("legs")
        };
        let legs: Vec<crate::makersim::PlacedLeg> =
            serde_json::from_str(&legs_json).expect("legs round-trip");
        assert_eq!(legs, sim.legs);
        assert_eq!(legs[0].visible_size, d!(250));
        assert_eq!(legs[0].threshold(), d!(350));

        // A different day sees none of it.
        assert!(store
            .maker_sims_for_day(opened.date_naive().succ_opt().unwrap())
            .expect("other day")
            .is_empty());
    }

    /// A v1 database (no latency column) must migrate in place, keeping its rows.
    #[test]
    fn a_v1_database_upgrades_to_v2_without_losing_rows() {
        let dir = std::env::temp_dir().join(format!("polyarb-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("polyarb.sqlite");

        // Build a v1-only database by hand.
        {
            std::fs::create_dir_all(&dir).expect("dir");
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
                                          applied_at TEXT NOT NULL);",
            )
            .expect("migrations table");
            conn.execute_batch(SCHEMA_V1).expect("v1 schema");
            conn.execute(
                "INSERT INTO migrations (version, name, applied_at) VALUES (1, 'initial schema', ?1)",
                params![now_str(Utc::now())],
            )
            .expect("record v1");
        }

        let store = Store::open(&path).expect("open and migrate");
        let id = store
            .record_opportunity(&sample_opportunity(), &sample_books(), Utc::now(), Some(7))
            .expect("insert into the upgraded schema")
            .id();
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].detection_latency_ms, Some(7));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detection_latency_is_recorded_once_and_never_revised() {
        let store = Store::in_memory().expect("db");
        let op = sample_opportunity();
        let books = sample_books();
        let t0 = Utc::now();

        let first = store
            .record_opportunity(&op, &books, t0, Some(37))
            .expect("insert");
        // The same opportunity seen again, with a different (later) measurement.
        store
            .record_opportunity(&op, &books, t0 + chrono::Duration::seconds(5), Some(4_000))
            .expect("repeat");

        let rows = store
            .opportunities_for_day(t0.date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, first.id());
        assert_eq!(
            rows[0].detection_latency_ms,
            Some(37),
            "the first detection's latency is the honest one"
        );

        // A REST-detected row has no latency at all rather than a fabricated zero.
        let mut other = op;
        other.legs[0].best_ask = d!(0.41);
        store
            .record_opportunity(&other, &books, t0, None)
            .expect("rest insert");
        let rows = store
            .opportunities_for_day(t0.date_naive())
            .expect("read back");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.detection_latency_ms.is_none()));
    }

    #[test]
    fn scan_cycles_fold_into_hourly_buckets() {
        let store = Store::in_memory().expect("db");
        let t0 = DateTime::parse_from_rfc3339("2026-07-26T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for i in 0..3 {
            store
                .record_cycle(
                    &CycleStats {
                        events: 40,
                        markets: 120,
                        books: 240,
                        opportunities: 2,
                        new_opportunities: 1,
                        duration_ms: 100 + i * 10,
                        failed: i == 2,
                        gaps_detected: 30,
                        fee_survivors: 4,
                    },
                    t0 + chrono::Duration::minutes(i),
                )
                .expect("record cycle");
        }
        // A second hour, to prove the bucketing is per hour.
        store
            .record_cycle(
                &CycleStats {
                    markets: 130,
                    duration_ms: 90,
                    ..CycleStats::default()
                },
                t0 + chrono::Duration::hours(1),
            )
            .expect("record cycle");

        let totals = store.scan_totals_for_day(t0.date_naive()).expect("totals");
        assert_eq!(totals.cycles, 4);
        assert_eq!(totals.errors, 1);
        assert_eq!(totals.duration_ms_total, 100 + 110 + 120 + 90);
        assert_eq!(totals.duration_ms_max, 120);
        assert_eq!(totals.markets_scanned_max, 130);
    }

    #[test]
    fn runtime_status_round_trips_and_stays_a_single_row() {
        let store = Store::in_memory().expect("db");
        assert_eq!(
            store.read_runtime_status().expect("read"),
            None,
            "a daemon that has never published must read as 'we do not know', not as a \
             default-constructed healthy daemon"
        );

        let t0 = DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let status = RuntimeStatus {
            mode: "dry-run".into(),
            started_at: now_str(t0 - chrono::Duration::hours(30)),
            universe_events: 312,
            stream_enabled: true,
            stream_shards: 4,
            stream_shards_connected: 4,
            books_total: 1_240,
            books_stale: 0,
            rest_ok: true,
            fee_fallback_markets: 7,
            alerts_cooldown_held: 2,
            net_floor_default_taker: "0.005".into(),
            ..RuntimeStatus::default()
        };
        store.write_runtime_status(&status, t0).expect("write");

        let read = store.read_runtime_status().expect("read").expect("row");
        assert_eq!(read.updated_at, t0);
        assert_eq!(read.status, status);

        // An upsert, not an append: the dashboard asks for *the* current state.
        let mut later = status.clone();
        later.stream_shards_connected = 3;
        let t1 = t0 + chrono::Duration::seconds(5);
        store
            .write_runtime_status(&later, t1)
            .expect("second write");
        let read = store.read_runtime_status().expect("read").expect("row");
        assert_eq!(read.updated_at, t1);
        assert_eq!(read.status.stream_shards_connected, 3);
        let rows: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM runtime_status", [], |r| r.get(0))
            .expect("count");
        assert_eq!(rows, 1);
    }

    #[test]
    fn the_dashboard_window_reads_rows_newest_first_and_sums_the_funnel_counters() {
        let store = Store::in_memory().expect("db");
        let t0 = DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let books = sample_books();

        let mut older = sample_opportunity();
        older.event_slug = "older".into();
        store
            .record_opportunity(&older, &books, t0 - chrono::Duration::hours(2), None)
            .expect("insert");
        let mut newer = sample_opportunity();
        newer.event_slug = "newer".into();
        newer.legs[0].best_ask = d!(0.41);
        store
            .record_opportunity(&newer, &books, t0 - chrono::Duration::minutes(5), Some(12))
            .expect("insert");
        // Outside the window entirely.
        let mut ancient = sample_opportunity();
        ancient.event_slug = "ancient".into();
        ancient.legs[0].best_ask = d!(0.42);
        store
            .record_opportunity(&ancient, &books, t0 - chrono::Duration::days(3), None)
            .expect("insert");

        let rows = store
            .opportunities_since(t0 - chrono::Duration::hours(24), 100)
            .expect("read");
        let slugs: Vec<&str> = rows.iter().map(|r| r.event_slug.as_str()).collect();
        assert_eq!(slugs, vec!["newer", "older"]);
        // The exact decimal strings survive the round trip; no f64 anywhere in the path.
        assert_eq!(rows[0].net_taker, d!(0.0305));
        assert_eq!(rows[0].net_maker, Some(d!(0.07)));
        assert_eq!(rows[0].gross_gap, d!(0.05));
        assert_eq!(rows[0].spread_cost, Some(d!(0.02)));
        assert_eq!(rows[0].label, "true-arb");

        for (i, gaps) in [(0i64, 40i64), (1, 60)] {
            store
                .record_cycle(
                    &CycleStats {
                        opportunities: 3,
                        gaps_detected: gaps,
                        fee_survivors: 5,
                        ..CycleStats::default()
                    },
                    t0 - chrono::Duration::hours(i),
                )
                .expect("cycle");
        }
        let totals = store
            .scan_totals_since(t0 - chrono::Duration::hours(24))
            .expect("totals");
        assert_eq!(totals.gaps_detected, 100);
        assert_eq!(totals.fee_survivors, 10);
        assert_eq!(totals.opportunities, 6);
        assert_eq!(totals.cycles, 2);
    }

    #[test]
    fn a_read_only_store_cannot_write_and_never_migrates() {
        let dir = std::env::temp_dir().join(format!("polyarb-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("polyarb.sqlite");
        {
            let store = Store::open(&path).expect("create");
            store
                .record_opportunity(&sample_opportunity(), &sample_books(), Utc::now(), None)
                .expect("insert");
        }

        let reader = Store::open_read_only(&path).expect("open read-only");
        assert_eq!(
            reader.schema_version().expect("version"),
            DASHBOARD_MIN_SCHEMA
        );
        assert_eq!(reader.opportunity_row_count().expect("count"), 1);
        // Read-only by construction: even a caller who asked for a write cannot get one.
        let write = reader.write_runtime_status(&RuntimeStatus::default(), Utc::now());
        assert!(
            write.is_err(),
            "a read-only connection must refuse to write"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_filter_excludes_other_days() {
        let store = Store::in_memory().expect("db");
        let day = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let inside = DateTime::parse_from_rfc3339("2026-07-26T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        let outside = DateTime::parse_from_rfc3339("2026-07-27T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut op = sample_opportunity();
        store
            .record_opportunity(&op, &sample_books(), inside, None)
            .expect("insert");
        op.legs[0].best_ask = d!(0.41); // different key → different row
        store
            .record_opportunity(&op, &sample_books(), outside, None)
            .expect("insert");

        assert_eq!(store.opportunities_for_day(day).expect("rows").len(), 1);
    }

    #[test]
    fn stale_open_rows_are_closed_on_restart() {
        let store = Store::in_memory().expect("db");
        let id = store
            .record_opportunity(&sample_opportunity(), &sample_books(), Utc::now(), None)
            .expect("insert")
            .id();
        assert_eq!(store.close_stale_open_rows(Utc::now()).expect("close"), 1);
        let (status, pnl) = store.status_of(id).expect("status");
        assert_eq!(status, "unresolved");
        assert_eq!(pnl, None, "an unresolved row is never credited");
    }
}
