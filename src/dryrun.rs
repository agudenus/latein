//! M3 — the `polyarb run` dry-run daemon, opportunity lifecycle simulation, and the
//! daily summary.
//!
//! The daemon is deliberately boring: refresh the universe slowly, re-fetch books on the
//! scan interval, run the M2 detectors, persist what they find, and measure what happened
//! next. Nothing here signs, quotes, or sends an order — [`ensure_dry_run`] refuses to
//! start in any other mode, and there is no code path that could place one.
//!
//! ## Lifecycle simulation (how `filled_simulated` is decided)
//!
//! On first detection an opportunity is persisted with status `open` and a tracker task
//! re-polls *only its legs* every `lifecycle.repoll_interval_secs` for up to
//! `lifecycle.repoll_window_secs`.
//!
//! * The **first** re-poll decides the fill, and that verdict is never revised. If every
//!   leg can still be filled at the size we walked, for a VWAP no worse than the one we
//!   detected, the opportunity becomes `filled_simulated` and its P&L is credited. If any
//!   leg cannot, it becomes `vanished` and nothing is credited.
//! * Later re-polls only extend `persistence_ms` — the time from detection to the last
//!   re-poll at which the fills were still there.
//! * Only taker-sized opportunities credit `simulated_pnl_taker`. A maker-only row's
//!   taker net is below the floor by construction, so crediting it would be fiction; its
//!   maker number is reported separately and always labelled hypothetical, because we
//!   never rest an order and therefore never learn whether it would have been crossed.
//!
//! ## Two ways in, one way through (M6)
//!
//! Detection is triggered either by the REST scan (a timer) or by the WebSocket stream (a
//! book actually moved). Both hand their opportunities to the *same*
//! [`Daemon::process_opportunities`], so persistence, alerting, lifecycle tracking and the
//! daily summary cannot drift apart between the two paths. The only difference is what
//! each can honestly report: a stream-triggered row carries `detection_latency_ms`
//! measured from the triggering frame, a REST-triggered row carries none.
//!
//! With `stream.enabled = false` — or after the stream has been abandoned as unreachable —
//! the daemon is exactly the M3 polling loop.
//!
//! ## Who decides the stream is dead (M6.1)
//!
//! The socket tasks retry forever; only this loop may hand over to REST-only polling, and
//! only when it has seen REST work while the socket kept failing (see
//! [`crate::ws::StreamHealth::evaluate_fallback`]). That is the difference between "the
//! endpoint is wrong" and "the machine was offline for a minute", and a live run showed the
//! second one costing a whole night of streaming. After a hand-over the loop re-probes the
//! endpoint every `stream.reprobe_interval_secs` and restores streaming if it answers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::alert::{format_opportunity, AlertStats, Alerter, Delivery, EventLog, Priority};
use crate::clob::ClobClient;
use crate::config::{Config, REPORTED_CATEGORIES};
use crate::detect;
use crate::gamma::{GammaClient, PaginationState};
use crate::http::HttpClient;
use crate::store::{
    CycleStats, LifecycleOutcome, LifecycleStatus, OpportunityRow, RuntimeStatus, ScanTotals, Store,
};
use crate::types::{BookMap, Opportunity, Side, TokenId, Universe};
use crate::ws::{self, DirtyBatch, StreamManager, StreamStatsSnapshot, DIVERGENCE_SAMPLE};

/// Phase A is dry-run only. Live execution does not exist yet — not behind a flag, not
/// behind a feature: there is no order-placing code in this binary.
pub fn ensure_dry_run(cfg: &Config) -> Result<()> {
    anyhow::ensure!(
        cfg.mode == "dry-run",
        "refusing to start: mode is {:?} but only \"dry-run\" is implemented (Phase A). \
         No execution path exists in this binary.",
        cfg.mode
    );
    Ok(())
}

// ---------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------

/// Run the daemon until SIGINT/SIGTERM (or `max_cycles` scan cycles, for tests).
pub async fn run(cfg: Config, max_cycles: Option<u64>) -> Result<()> {
    ensure_dry_run(&cfg)?;

    let cfg = Arc::new(cfg);
    let store = Arc::new(
        Store::open(Path::new(&cfg.storage.database_path))
            .with_context(|| format!("could not open {}", cfg.storage.database_path))?,
    );
    let events = Arc::new(
        EventLog::open(Path::new(&cfg.storage.log_dir))
            .with_context(|| format!("could not open the event log in {}", cfg.storage.log_dir))?,
    );
    let alerter = Arc::new(Alerter::new(&cfg.alerts, events.clone()));
    let http = Arc::new(HttpClient::new(&cfg.api).context("failed to build the HTTP client")?);

    // A previous process may have been killed mid-window; those rows can never be
    // resolved honestly, so close them out rather than leaving them `open` forever.
    match store.close_stale_open_rows(Utc::now()) {
        Ok(n) if n > 0 => tracing::warn!(
            rows = n,
            "closed opportunity rows left open by a previous run (marked unresolved)"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(%err, "could not close stale opportunity rows"),
    }

    tracing::info!(
        mode = %cfg.mode,
        db = store.path(),
        log = %events.path().display(),
        scan_interval_secs = cfg.daemon.scan_interval_secs,
        universe_refresh_secs = cfg.daemon.universe_refresh_secs,
        market_data = if cfg.stream.enabled { "stream+rest" } else { "rest" },
        repoll = format!(
            "{}s/{}s",
            cfg.lifecycle.repoll_interval_secs, cfg.lifecycle.repoll_window_secs
        ),
        "starting polyarb dry-run daemon (no orders are ever placed)"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_signal_handler(shutdown_tx);

    let mut daemon = Daemon {
        cfg: cfg.clone(),
        http,
        store,
        alerter,
        slots: Arc::new(Semaphore::new(cfg.lifecycle.max_concurrent)),
        shutdown: shutdown_rx,
        trackers: JoinSet::new(),
        last_summary_day: None,
        status: DaemonStatus {
            started_at: Utc::now(),
            // Until the first REST call proves otherwise, assume nothing: `rest_ok` starts
            // true so a daemon that has not yet made a request is not reported as blind.
            rest_ok: true,
            ..DaemonStatus::default()
        },
        last_status_publish: None,
        token_events: HashMap::new(),
        gamma: Arc::new(PaginationState::default()),
    };
    daemon.main_loop(max_cycles).await
}

fn spawn_signal_handler(tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::info!("shutdown signal received — finishing in-flight work");
        let _ = tx.send(true);
    });
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "could not install the SIGTERM handler");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---------------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------------

/// How often the daemon republishes [`RuntimeStatus`] to SQLite (M7).
///
/// One tiny upsert; at 5 s it is cheaper than a single scan cycle's own telemetry row and
/// keeps the dashboard's "is it alive" question answerable without the dashboard ever
/// touching the daemon. The dashboard treats a status older than
/// [`RUNTIME_STATUS_STALE_SECS`] as "the daemon is not reporting".
const STATUS_PUBLISH_SECS: u64 = 5;

/// The daemon-memory state the dashboard cannot see any other way.
///
/// It is *state*, not telemetry: every field answers a question whose wrong answer would
/// make the dashboard lie — whether discovery is returning an empty universe, whether REST
/// is working while the socket is not, how much of the fee model came from a guess.
#[derive(Debug, Clone, Default)]
struct DaemonStatus {
    started_at: chrono::DateTime<Utc>,
    universe_events: i64,
    universe_markets: i64,
    universe_tokens: i64,
    universe_refreshed_at: Option<chrono::DateTime<Utc>>,
    last_good_discovery_at: Option<chrono::DateTime<Utc>>,
    discovery_empty_streak: i64,
    discovery_error: Option<String>,
    rest_ok: bool,
    rest_failure_streak: i64,
    stream_rest_only: bool,
    fee_fallback_markets: i64,
    fee_unsupported_formula: i64,
    markets_with_api_fee: i64,
}

struct Daemon {
    cfg: Arc<Config>,
    http: Arc<HttpClient>,
    store: Arc<Store>,
    alerter: Arc<Alerter>,
    slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    trackers: JoinSet<()>,
    last_summary_day: Option<NaiveDate>,
    /// M7: republished to `runtime_status` every [`STATUS_PUBLISH_SECS`].
    status: DaemonStatus,
    last_status_publish: Option<Instant>,
    /// token id → index into `universe.events`. Rebuilt on every universe refresh; this is
    /// what turns "this book moved" into "re-evaluate exactly this event".
    token_events: HashMap<TokenId, usize>,
    /// Which Gamma listing endpoint works, remembered across refreshes so a missing keyset
    /// endpoint is probed (and logged) once per process rather than once per refresh.
    gamma: Arc<PaginationState>,
}

/// An error rendered with its whole cause chain.
///
/// `anyhow`'s plain `Display` prints only the outermost context, so the discovery retry
/// line read `market discovery via the Gamma API at … failed` on every attempt and never
/// said *why*. The alternate form (`{:#}`) appends each `Caused by` link inline, which is
/// what turns "discovery is broken" into "discovery parsed 0 events from /events/keyset".
fn error_chain(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

/// Is this discovery failure *the* one — a pass that parsed fine and produced no events?
///
/// The distinction matters because only this failure means the scanner is blind rather
/// than merely unlucky: an HTTP 500 or a timeout leaves the previous universe standing and
/// is retried, while an empty universe that parses cleanly looks exactly like a quiet
/// market and would otherwise be reported as one.
fn is_empty_discovery(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<crate::http::ApiError>(),
            Some(crate::http::ApiError::EmptyDiscovery { .. })
        )
    })
}

/// Backoff between failed startup discovery attempts: start at the daemon's own scan
/// cadence, double, cap at a minute. With the shipped 5 s interval that is 5→10→20→40→60 s.
fn discovery_backoff(scan_interval_secs: u64, attempt: u32) -> Duration {
    const CAP_SECS: u64 = 60;
    let base = scan_interval_secs.clamp(1, CAP_SECS);
    let secs = base
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(6))
        .min(CAP_SECS);
    Duration::from_secs(secs)
}

impl Daemon {
    async fn main_loop(&mut self, max_cycles: Option<u64>) -> Result<()> {
        // Discovery is market data, and market data is never allowed to kill the process:
        // this retries until it works (or we are told to shut down). Exiting instead put the
        // owner's live daemon in a Docker restart loop the night Gamma started rejecting
        // deep `offset` pagination with an HTTP 422.
        let Some(mut universe) = self.initial_universe().await else {
            tracing::info!("shutdown requested before the first universe was discovered");
            return Ok(());
        };
        let mut universe_fetched = Instant::now();
        self.index_universe(&universe);

        // If we start after today's summary time, do not immediately fire a partial-day
        // summary; wait for tomorrow's.
        let summary_time = self.cfg.daily_summary_time()?;
        if Utc::now().time() >= summary_time {
            self.last_summary_day = Some(Utc::now().date_naive());
        }

        // The stream, when enabled. Books are seeded over REST first so the daemon is
        // never blind while the first snapshots are in flight.
        let mut stream = if self.cfg.stream.enabled {
            let manager =
                StreamManager::start(&self.cfg, &universe.token_ids(), self.shutdown.clone());
            self.seed_stream_books(&manager, &universe).await;
            Some(manager)
        } else {
            None
        };
        // First publication: from here the dashboard can tell a running daemon from a
        // stopped one, and does so by this row's age.
        self.publish_status(stream.as_ref(), true);

        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.cfg.daemon.scan_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let refresh_after = Duration::from_secs(self.cfg.daemon.universe_refresh_secs);
        let resync_after = Duration::from_secs(self.cfg.stream.resync_interval_secs);
        let stale_after = Duration::from_secs(self.cfg.stream.stale_after_secs);
        let reprobe_after = Duration::from_secs(self.cfg.stream.reprobe_interval_secs.max(1));
        let probe_timeout = Duration::from_secs(self.cfg.api.request_timeout_secs.max(1));
        let mut last_sweep = Instant::now();
        // `Some(when)` = streaming has handed over to REST and is due for a re-probe
        // `reprobe_after` later. `None` = nothing to restore.
        let mut fallback_since: Option<Instant> = None;
        let mut last_pulse: Option<StreamPulse> = None;
        let mut cycles = 0u64;
        let mut stream_passes = 0u64;

        loop {
            // A tick and a dirty batch are the two ways detection starts; everything after
            // the trigger is shared.
            let batch = tokio::select! {
                _ = self.shutdown.changed() => break,
                _ = ticker.tick() => None,
                batch = recv_batch(stream.as_mut()) => match batch {
                    Some(batch) => Some(batch),
                    // The pool is gone: stop selecting on it, and treat it as a fallback
                    // so the slow re-probe can bring streaming back.
                    None => {
                        tracing::warn!("the market stream pool stopped — polling REST until a re-probe succeeds");
                        stream = None;
                        fallback_since = Some(Instant::now());
                        self.status.stream_rest_only = true;
                        self.publish_status(None, true);
                        continue;
                    }
                },
            };
            if *self.shutdown.borrow() {
                break;
            }
            self.publish_status(stream.as_ref(), false);

            if let Some(batch) = batch {
                stream_passes += 1;
                if let Some(manager) = stream.as_ref() {
                    self.stream_cycle(&universe, manager, &batch).await;
                }
                continue;
            }

            // --- timer tick -----------------------------------------------------------
            if universe_fetched.elapsed() >= refresh_after {
                match self.fetch_universe().await {
                    Ok(fresh) => {
                        universe = fresh;
                        self.index_universe(&universe);
                        if let Some(manager) = stream.as_mut() {
                            manager.update_universe(&universe.token_ids());
                        }
                    }
                    Err(err) => {
                        // Keep scanning the last known universe rather than going blind — and
                        // never propagate: a discovery hiccup mid-run must not end the run.
                        tracing::error!(
                            err = %error_chain(&err),
                            events = universe.events.len(),
                            markets = universe.market_count(),
                            universe_refresh_secs = self.cfg.daemon.universe_refresh_secs,
                            "universe refresh failed — keeping the previous universe and \
                             carrying on; it will be retried on the next refresh interval"
                        );
                    }
                }
                universe_fetched = Instant::now();
            }

            let mut hand_over = false;
            match stream.as_ref() {
                Some(manager) => {
                    // Cheap every tick: anything explicitly stale or behind a shard gap.
                    self.resync_stale(manager, &universe, stale_after).await;
                    // Slow and thorough: the whole universe over REST, a divergence
                    // cross-check, and a full detection pass as the integrity net.
                    if last_sweep.elapsed() >= resync_after {
                        last_sweep = Instant::now();
                        self.full_sweep(manager, &universe, &mut last_pulse).await;
                    }
                    // Now — and only now, with this tick's REST evidence in hand — decide
                    // whether the socket is specifically broken.
                    if manager.health().evaluate_fallback() {
                        let health = manager.health();
                        tracing::error!(
                            consecutive_ws_failures = health.consecutive_failures(),
                            rest_failures_seen = health.rest_failures(),
                            scan_interval_secs = self.cfg.daemon.scan_interval_secs,
                            reprobe_interval_secs = self.cfg.stream.reprobe_interval_secs,
                            "the market stream keeps failing while REST works — handing \
                             detection back to REST polling; latency returns to the scan \
                             interval. Check stream.url. Streaming will be re-probed on \
                             the interval above and restored if it answers."
                        );
                        hand_over = true;
                    }
                }
                None => self.scan_cycle(&universe).await,
            }
            if hand_over {
                if let Some(manager) = stream.take() {
                    manager.stop().await;
                }
                last_pulse = None;
                fallback_since = Some(Instant::now());
                self.status.stream_rest_only = true;
                self.publish_status(None, true);
                // Take over immediately rather than leaving one scan interval unscanned:
                // from here on REST *is* the detector.
                self.scan_cycle(&universe).await;
            }

            // "Permanent" fallback means "until the endpoint proves itself again".
            if stream.is_none() && self.cfg.stream.enabled {
                if let Some(since) = fallback_since {
                    if since.elapsed() >= reprobe_after {
                        fallback_since = Some(Instant::now());
                        match ws::probe(&self.cfg.stream.url, probe_timeout).await {
                            Ok(()) => {
                                tracing::warn!(
                                    url = %self.cfg.stream.url,
                                    "the market stream answered a re-probe — restoring \
                                     streaming after the REST-only fallback"
                                );
                                let manager = StreamManager::start(
                                    &self.cfg,
                                    &universe.token_ids(),
                                    self.shutdown.clone(),
                                );
                                self.seed_stream_books(&manager, &universe).await;
                                stream = Some(manager);
                                fallback_since = None;
                                last_sweep = Instant::now();
                                self.status.stream_rest_only = false;
                                self.publish_status(stream.as_ref(), true);
                            }
                            Err(err) => tracing::info!(
                                %err,
                                url = %self.cfg.stream.url,
                                reprobe_interval_secs = self.cfg.stream.reprobe_interval_secs,
                                "the market stream is still unreachable — staying on REST polling"
                            ),
                        }
                    }
                }
            }
            self.maybe_daily_summary(summary_time).await;

            cycles += 1;
            if max_cycles.is_some_and(|max| cycles >= max) {
                tracing::info!(cycles, "reached the configured cycle limit — stopping");
                break;
            }
        }

        if let Some(manager) = stream.take() {
            let stats = manager.books().stats.snapshot();
            tracing::info!(
                messages = stats.messages,
                snapshots = stats.snapshots,
                deltas = stats.deltas,
                unknown_frames = stats.unknown_frames,
                malformed_frames = stats.malformed_frames,
                orphan_deltas = stats.orphan_deltas,
                out_of_order = stats.out_of_order,
                hash_contradictions = stats.hash_contradictions,
                reconnects = stats.reconnects,
                connect_failures = stats.connect_failures,
                resynced = stats.resynced,
                divergences = stats.divergences,
                "market stream stopping"
            );
            manager.stop().await;
        }

        self.drain_trackers().await;
        let stats = self.alerter.stats();
        tracing::info!(
            cycles,
            stream_passes,
            alerts_sent = stats.sent,
            alerts_failed = stats.failed,
            alerts_suppressed = stats.suppressed,
            alerts_cooldown_suppressed = stats.cooldown_suppressed,
            "polyarb daemon stopped"
        );
        Ok(())
    }

    /// Rebuild the token → event index used by incremental detection.
    fn index_universe(&mut self, universe: &Universe) {
        self.token_events.clear();
        for (index, event) in universe.events.iter().enumerate() {
            for market in &event.markets {
                for token in &market.token_ids {
                    self.token_events.insert(token.clone(), index);
                }
            }
        }
    }

    /// The first universe, retried until it arrives. `None` = shut down before it did.
    ///
    /// A market-data failure at startup is a hiccup, not a configuration error: the process
    /// stays up, logs loudly, and keeps asking. Nothing else can start until it succeeds —
    /// there is nothing to scan — so this is the one place in the daemon that loops on
    /// failure rather than carrying on with what it had.
    async fn initial_universe(&mut self) -> Option<Universe> {
        let mut attempt = 0u32;
        loop {
            match self.fetch_universe().await {
                Ok(universe) => {
                    if attempt > 0 {
                        tracing::info!(
                            attempts = attempt + 1,
                            events = universe.events.len(),
                            "market discovery recovered — the daemon never went down"
                        );
                    }
                    return Some(universe);
                }
                Err(err) => {
                    attempt += 1;
                    // Publish before sleeping: a daemon stuck here has no universe at all,
                    // and that is precisely the state the dashboard must take the page over
                    // for rather than render as a quiet market.
                    self.publish_status(None, true);
                    let wait = discovery_backoff(self.cfg.daemon.scan_interval_secs, attempt);
                    tracing::error!(
                        err = %error_chain(&err),
                        attempt,
                        retry_in_secs = wait.as_secs(),
                        "market discovery failed at startup — retrying; the daemon stays up \
                         (a market-data failure must never end the process)"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.shutdown.changed() => {}
                    }
                    if *self.shutdown.borrow() {
                        return None;
                    }
                }
            }
        }
    }

    /// Note the outcome of a REST call, for the "is this a fallback or an outage?" question
    /// the dashboard has to answer (M7). Mirrors what [`crate::ws::StreamHealth`] already
    /// records for the fallback decision; this side is only ever read, never acted on.
    fn note_rest(&mut self, ok: bool) {
        self.status.rest_ok = ok;
        self.status.rest_failure_streak = if ok {
            0
        } else {
            self.status.rest_failure_streak.saturating_add(1)
        };
    }

    /// Republish [`RuntimeStatus`] if it is due (or `force`d by a state change).
    ///
    /// Best-effort by design: a failed status write is logged at debug and nothing else —
    /// losing a dashboard refresh must never disturb the scan loop.
    fn publish_status(&mut self, stream: Option<&StreamManager>, force: bool) {
        let now = Instant::now();
        let due = self.last_status_publish.is_none_or(|last| {
            now.duration_since(last) >= Duration::from_secs(STATUS_PUBLISH_SECS)
        });
        if !force && !due {
            return;
        }
        self.last_status_publish = Some(now);

        let stats = stream
            .map(|m| m.books().stats.snapshot())
            .unwrap_or_default();
        let alerts = self.alerter.stats();
        let status = RuntimeStatus {
            mode: self.cfg.mode.clone(),
            started_at: crate::store::now_str(self.status.started_at),
            universe_events: self.status.universe_events,
            universe_markets: self.status.universe_markets,
            universe_tokens: self.status.universe_tokens,
            universe_refreshed_at: self.status.universe_refreshed_at.map(crate::store::now_str),
            universe_refresh_secs: self.cfg.daemon.universe_refresh_secs,
            scan_interval_secs: self.cfg.daemon.scan_interval_secs,
            last_good_discovery_at: self
                .status
                .last_good_discovery_at
                .map(crate::store::now_str),
            discovery_empty_streak: self.status.discovery_empty_streak,
            discovery_error: self.status.discovery_error.clone(),
            stream_enabled: self.cfg.stream.enabled,
            stream_shards: stream.map(|m| m.shard_count() as i64).unwrap_or(0),
            stream_shards_connected: stream
                .map(|m| i64::from(m.health().live_connections()))
                .unwrap_or(0),
            stream_rest_only: self.status.stream_rest_only,
            books_total: stream.map(|m| m.books().len() as i64).unwrap_or(0),
            books_stale: stream.map(|m| m.books().stale_count() as i64).unwrap_or(0),
            frames: stats.messages,
            delta_entries_applied: stats.delta_entries_applied,
            frames_unrecognized: stats.frames_unrecognized,
            rest_ok: self.status.rest_ok,
            rest_failure_streak: self.status.rest_failure_streak,
            fee_fallback_markets: self.status.fee_fallback_markets,
            fee_unsupported_formula: self.status.fee_unsupported_formula,
            markets_with_api_fee: self.status.markets_with_api_fee,
            telegram_circuit_open: alerts.circuit_open,
            alerts_sent: alerts.sent,
            alerts_failed: alerts.failed,
            alerts_cooldown_held: alerts.cooldown_suppressed,
            net_floor_default_taker: self
                .cfg
                .net_floor_taker(&crate::types::Category::new("default"))
                .to_string(),
        };
        if let Err(err) = self.store.write_runtime_status(&status, Utc::now()) {
            tracing::debug!(%err, "could not publish the runtime status row");
        }
    }

    async fn fetch_universe(&mut self) -> Result<Universe> {
        let result = GammaClient::with_state(&self.http, &self.cfg, self.gamma.clone())
            .fetch_universe()
            .await
            .with_context(|| {
                format!(
                    "market discovery via the Gamma API at {} failed",
                    self.cfg.api.gamma_base_url
                )
            });
        let (universe, stats) = match result {
            Ok(pair) => pair,
            Err(err) => {
                // An empty-but-well-formed discovery pass is the failure the break state
                // exists for, and it is the only one that gets a streak: everything else is
                // an ordinary error the daemon retries with its previous universe intact.
                if is_empty_discovery(&err) {
                    self.status.discovery_empty_streak =
                        self.status.discovery_empty_streak.saturating_add(1);
                }
                self.status.discovery_error = Some(error_chain(&err));
                return Err(err);
            }
        };
        self.status.discovery_empty_streak = 0;
        self.status.discovery_error = None;
        self.status.last_good_discovery_at = Some(Utc::now());
        self.status.universe_refreshed_at = self.status.last_good_discovery_at;
        self.status.universe_events = universe.events.len() as i64;
        self.status.universe_markets = universe.market_count() as i64;
        self.status.universe_tokens = universe.token_count() as i64;
        self.status.markets_with_api_fee = stats.markets_with_api_fee as i64;
        // "Fell back to the category table" is everything the API did not state a usable
        // rate for — including the markets whose stated formula this build cannot price.
        self.status.fee_fallback_markets = (stats
            .markets_kept
            .saturating_sub(stats.markets_with_api_fee))
            as i64;
        self.status.fee_unsupported_formula = stats.markets_unsupported_fee_formula as i64;
        let partial_negrisk = universe
            .events
            .iter()
            .filter(|e| e.neg_risk && !e.coverage_complete())
            .count();
        tracing::info!(
            events = universe.events.len(),
            markets = universe.market_count(),
            // The number that actually drives cost: one subscription and one `/books` slot
            // each, so this is what the activity floor is there to hold down.
            tokens = universe.token_count(),
            negrisk_events = universe.neg_risk_event_count(),
            partial_negrisk_events = partial_negrisk,
            markets_seen = stats.markets_seen,
            dropped_markets = stats.markets_dropped(),
            markets_below_activity_floor = stats.drops.below_activity_floor,
            events_below_activity_floor = stats.events_below_activity_floor,
            drop_reasons = %stats.drops.summary(),
            truncated = stats.truncated,
            "market universe refreshed"
        );
        // Once per refresh, and only when there is something to say.
        let short_lived =
            short_lived_crypto_events(&universe, self.cfg.daemon.universe_refresh_secs, Utc::now());
        if short_lived > 0 {
            tracing::warn!(
                events = short_lived,
                universe_refresh_secs = self.cfg.daemon.universe_refresh_secs,
                "short-lived crypto markets present; refresh cadence misses most 5m/15m \
                 series — crypto engine milestone"
            );
        }
        Ok(universe)
    }

    /// One REST scan tick: fetch every tracked book, detect, persist, alert, track.
    async fn scan_cycle(&mut self, universe: &Universe) {
        let started = Instant::now();
        let tokens = universe.token_ids();
        let books = match ClobClient::new(&self.http, &self.cfg)
            .fetch_books(&tokens)
            .await
        {
            Ok(books) => books,
            Err(err) => {
                tracing::warn!(%err, "book fetch failed — skipping this cycle");
                self.note_rest(false);
                self.record_cycle(CycleStats {
                    events: universe.events.len() as i64,
                    markets: universe.market_count() as i64,
                    duration_ms: elapsed_ms(started),
                    failed: true,
                    ..CycleStats::default()
                });
                return;
            }
        };

        self.note_rest(true);
        let (opportunities, counters) = detect::scan_counted(&self.cfg, universe, &books);
        // REST detection has no triggering frame, so it has no latency to report.
        let new_opportunities = self
            .process_opportunities(&opportunities, &books, None)
            .await;

        let duration_ms = elapsed_ms(started);
        tracing::info!(
            books = books.len(),
            markets = universe.market_count(),
            opportunities = opportunities.len(),
            new = new_opportunities,
            duration_ms,
            "scan cycle complete"
        );
        self.record_cycle(CycleStats {
            events: universe.events.len() as i64,
            markets: universe.market_count() as i64,
            books: books.len() as i64,
            opportunities: opportunities.len() as i64,
            new_opportunities,
            duration_ms,
            failed: false,
            gaps_detected: counters.gaps_detected as i64,
            fee_survivors: counters.fee_survivors as i64,
        });
    }

    /// Persist / alert / track a detection pass's opportunities. The single point where
    /// both the REST and the stream path meet, so neither can drift.
    ///
    /// Returns how many were new.
    async fn process_opportunities(
        &mut self,
        opportunities: &[Opportunity],
        books: &BookMap,
        batch: Option<&DirtyBatch>,
    ) -> i64 {
        let now = Utc::now();
        let instant = Instant::now();
        let mut new_opportunities = 0i64;
        for op in opportunities {
            // The latency of the *slowest* leg we were told about: the age of the market
            // data this construction actually rests on.
            let latency =
                batch.and_then(|b| b.latency_ms(op.legs.iter().map(|l| &l.token_id), instant, now));
            match self.store.record_opportunity(op, books, now, latency) {
                Ok(recorded) if recorded.is_new() => {
                    new_opportunities += 1;
                    self.on_new_opportunity(recorded.id(), op, latency).await;
                }
                Ok(recorded) => {
                    // Same construction at the same quotes: update `last_seen_at`, never
                    // duplicate the row.
                    tracing::debug!(id = recorded.id(), "opportunity still on the book");
                }
                Err(err) => tracing::warn!(%err, "could not persist an opportunity"),
            }
        }
        new_opportunities
    }

    /// Seed the stream's books from REST so detection works from the first tick, before
    /// any snapshot frame has arrived.
    async fn seed_stream_books(&mut self, manager: &StreamManager, universe: &Universe) {
        let tokens = universe.token_ids();
        match ClobClient::new(&self.http, &self.cfg)
            .fetch_books(&tokens)
            .await
        {
            Ok(books) => {
                tracing::info!(
                    books = books.len(),
                    "seeded the stream book state over REST"
                );
                // Also the first evidence that this machine has a network: a socket that
                // never connects while *this* worked is a socket problem.
                manager.health().record_rest_success();
                self.note_rest(true);
                manager.books().apply_rest(&tokens, &books, Instant::now());
            }
            Err(err) => {
                manager.health().record_rest_failure();
                self.note_rest(false);
                tracing::warn!(
                    %err,
                    "could not seed book state over REST — detection waits for stream snapshots"
                );
            }
        }
    }

    /// Incremental detection: re-evaluate **only** the events whose books just moved.
    async fn stream_cycle(
        &mut self,
        universe: &Universe,
        manager: &StreamManager,
        batch: &DirtyBatch,
    ) {
        let started = Instant::now();
        let mut dirty: Vec<usize> = batch
            .tokens()
            .filter_map(|token| self.token_events.get(token).copied())
            .collect();
        dirty.sort_unstable();
        dirty.dedup();
        if dirty.is_empty() {
            // Books we track but no event claims (a universe refresh mid-flight).
            return;
        }

        let subset = Universe {
            events: dirty
                .iter()
                .filter_map(|i| universe.events.get(*i).cloned())
                .collect(),
        };
        // Only the affected events' books are copied out of the shared state — the whole
        // point of the incremental path is not to touch the other ~2 000 books.
        let books = manager.books().snapshot_of(&subset.token_ids());
        let (opportunities, counters) = detect::scan_counted(&self.cfg, &subset, &books);
        let new_opportunities = self
            .process_opportunities(&opportunities, &books, Some(batch))
            .await;

        let duration_ms = elapsed_ms(started);
        tracing::debug!(
            touched_tokens = batch.len(),
            events = subset.events.len(),
            books = books.len(),
            opportunities = opportunities.len(),
            new = new_opportunities,
            duration_ms,
            "stream detection pass complete"
        );
        self.record_cycle(CycleStats {
            events: subset.events.len() as i64,
            markets: subset.market_count() as i64,
            books: books.len() as i64,
            opportunities: opportunities.len() as i64,
            new_opportunities,
            duration_ms,
            failed: false,
            gaps_detected: counters.gaps_detected as i64,
            fee_survivors: counters.fee_survivors as i64,
        });
    }

    /// Targeted REST re-fetch of the books the stream cannot vouch for: never seen,
    /// explicitly invalidated, or sitting behind a shard disconnect. A *quiet* book is not
    /// in that set (M6.1), which is what makes this path cheap again — usually it is a
    /// no-op and costs nothing at all.
    async fn resync_stale(
        &mut self,
        manager: &StreamManager,
        universe: &Universe,
        stale_after: Duration,
    ) {
        let stale =
            manager
                .books()
                .missing_or_stale(&universe.token_ids(), Instant::now(), stale_after);
        if stale.is_empty() {
            return;
        }
        match ClobClient::new(&self.http, &self.cfg)
            .fetch_books(&stale)
            .await
        {
            Ok(books) => {
                tracing::info!(
                    requested = stale.len(),
                    returned = books.len(),
                    stale_after_secs = stale_after.as_secs(),
                    "resynced stale books over REST"
                );
                manager.health().record_rest_success();
                self.note_rest(true);
                manager
                    .books()
                    .stats
                    .resynced
                    .fetch_add(books.len() as u64, std::sync::atomic::Ordering::Relaxed);
                manager.books().apply_rest(&stale, &books, Instant::now());
            }
            Err(err) => {
                manager.health().record_rest_failure();
                self.note_rest(false);
                tracing::warn!(%err, stale = stale.len(), "stale-book resync failed");
            }
        }
    }

    /// The integrity net: re-fetch the whole universe over REST, count how far a sample of
    /// our locally maintained books had drifted, adopt the REST view, and run a full
    /// detection pass over it.
    ///
    /// This is what makes a silently-wrong stream survivable. If the frames carry no
    /// verifiable checksum (they may not — see `ws.rs`), this sweep is the *only* thing
    /// that can tell us our books are wrong, so its divergence count is the number to
    /// watch during the soak.
    async fn full_sweep(
        &mut self,
        manager: &StreamManager,
        universe: &Universe,
        last_pulse: &mut Option<StreamPulse>,
    ) {
        let started = Instant::now();
        let tokens = universe.token_ids();

        // The divergence budget is spread over the sweep's batches rather than taken at the
        // end: the live universe needs 30–70 s to fetch, and a comparison made a minute
        // after the answer arrived measures nothing but elapsed time.
        let batches = tokens
            .len()
            .div_ceil(self.cfg.api.books_batch_size.max(1))
            .max(1);
        let per_batch = DIVERGENCE_SAMPLE.div_ceil(batches).max(1);
        let mut diverged = 0usize;
        let mut sampled = 0usize;

        let fetched = ClobClient::new(&self.http, &self.cfg)
            .fetch_books_batched(&tokens, |batch| {
                let (d, s) =
                    manager
                        .books()
                        .count_divergence(batch.books, per_batch, batch.requested_at);
                diverged += d;
                sampled += s;
            })
            .await;
        let books = match fetched {
            Ok(books) => books,
            Err(err) => {
                manager.health().record_rest_failure();
                self.note_rest(false);
                tracing::warn!(%err, "full REST resync sweep failed");
                self.record_cycle(CycleStats {
                    events: universe.events.len() as i64,
                    markets: universe.market_count() as i64,
                    duration_ms: elapsed_ms(started),
                    failed: true,
                    ..CycleStats::default()
                });
                return;
            }
        };
        manager.health().record_rest_success();
        self.note_rest(true);

        if diverged > 0 {
            tracing::warn!(
                diverged,
                sampled,
                "locally streamed books disagreed with REST at the top of book — the REST \
                 view wins; investigate before trusting stream-only detection"
            );
        }
        self.log_stream_pulse(manager, last_pulse);
        manager.books().apply_rest(&tokens, &books, Instant::now());

        let (opportunities, counters) = detect::scan_counted(&self.cfg, universe, &books);
        let new_opportunities = self
            .process_opportunities(&opportunities, &books, None)
            .await;
        let duration_ms = elapsed_ms(started);
        tracing::info!(
            books = books.len(),
            live_books = manager.books().len(),
            connections = manager.health().live_connections(),
            markets = universe.market_count(),
            diverged,
            opportunities = opportunities.len(),
            new = new_opportunities,
            duration_ms,
            "full REST resync sweep complete"
        );
        self.record_cycle(CycleStats {
            events: universe.events.len() as i64,
            markets: universe.market_count() as i64,
            books: books.len() as i64,
            opportunities: opportunities.len() as i64,
            new_opportunities,
            duration_ms,
            failed: false,
            gaps_detected: counters.gaps_detected as i64,
            fee_survivors: counters.fee_survivors as i64,
        });
    }

    /// Per-sweep data-plane health: is traffic actually arriving?
    ///
    /// A subscribed-but-silent shard and a genuinely quiet market look identical from the
    /// book state alone, which is exactly the ambiguity the soak needs resolved. Counting
    /// payloads, applied deltas and the rate between sweeps separates them.
    fn log_stream_pulse(&self, manager: &StreamManager, last: &mut Option<StreamPulse>) {
        let stats = manager.books().stats.snapshot();
        let now = Instant::now();
        emit_stream_pulse(
            &stats,
            last.as_ref(),
            now,
            manager.health().live_connections(),
        );
        *last = Some(StreamPulse { at: now, stats });
    }

    fn record_cycle(&self, stats: CycleStats) {
        if let Err(err) = self.store.record_cycle(&stats, Utc::now()) {
            tracing::warn!(%err, "could not record scan-cycle stats");
        }
    }

    /// Log, alert (if it clears the threshold), and start lifecycle tracking.
    async fn on_new_opportunity(&mut self, id: i64, op: &Opportunity, latency_ms: Option<i64>) {
        let mut payload = json!({
            "id": id,
            "dedupe_key": crate::store::dedupe_key(op),
            "detection_latency_ms": latency_ms,
            "opportunity": op,
        });

        // Two gates, in order: is it big enough to be worth a message at all, and is it
        // *news* — or the same slow-moving construction we already reported? A suppressed
        // alert is still a full JSONL record, so the soak loses nothing either way.
        let held_back = if !self.alerter.passes_threshold(op) {
            Some(Delivery::BelowThreshold)
        } else if !self.alerter.allow_alert_at(op, Instant::now()) {
            Some(Delivery::Cooldown)
        } else {
            None
        };
        match held_back {
            Some(reason) => {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("delivery".into(), json!(reason.as_str()));
                }
                self.alerter.events().append("opportunity", payload);
            }
            None => {
                self.alerter
                    .notify(
                        "opportunity",
                        &format_opportunity(op),
                        Priority::Routine,
                        payload,
                    )
                    .await;
            }
        }

        match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => {
                let ctx = TrackerCtx {
                    cfg: self.cfg.clone(),
                    http: self.http.clone(),
                    store: self.store.clone(),
                    alerter: self.alerter.clone(),
                    shutdown: self.shutdown.clone(),
                };
                let op = op.clone();
                self.trackers.spawn(async move {
                    track_opportunity(ctx, id, op, permit).await;
                });
            }
            Err(_) => {
                // Better to say "we did not measure this one" than to pretend we did.
                tracing::warn!(
                    id,
                    limit = self.cfg.lifecycle.max_concurrent,
                    "lifecycle tracker limit reached — recording the opportunity as untracked"
                );
                let outcome = LifecycleOutcome {
                    status: LifecycleStatus::Untracked,
                    repolls: 0,
                    first_repoll_available: None,
                    persistence_ms: None,
                    simulated_pnl_taker: None,
                    simulated_pnl_maker: None,
                    resolved_at: Utc::now(),
                };
                if let Err(err) = self.store.record_lifecycle(id, &outcome) {
                    tracing::warn!(%err, id, "could not record the untracked lifecycle");
                }
            }
        }

        // Reap finished trackers so the set does not grow without bound.
        while self.trackers.try_join_next().is_some() {}
    }

    async fn drain_trackers(&mut self) {
        if self.trackers.is_empty() {
            return;
        }
        let grace = Duration::from_secs(self.cfg.lifecycle.repoll_window_secs + 5);
        tracing::info!(
            trackers = self.trackers.len(),
            grace_secs = grace.as_secs(),
            "waiting for lifecycle trackers"
        );
        let drain = async {
            while let Some(joined) = self.trackers.join_next().await {
                if let Err(err) = joined {
                    tracing::warn!(%err, "a lifecycle tracker panicked");
                }
            }
        };
        if tokio::time::timeout(grace, drain).await.is_err() {
            tracing::warn!("lifecycle trackers did not finish in the grace period — aborting them");
            self.trackers.abort_all();
        }
    }

    async fn maybe_daily_summary(&mut self, summary_time: chrono::NaiveTime) {
        let now = Utc::now();
        let today = now.date_naive();
        if self.last_summary_day == Some(today) || now.time() < summary_time {
            return;
        }
        self.last_summary_day = Some(today);
        if let Err(err) =
            emit_daily_summary(&self.cfg, &self.store, &self.alerter, today, true).await
        {
            tracing::warn!(%err, "could not generate the daily summary");
        }
    }
}

/// Stream counters at one moment, so the next sweep can report a rate rather than a total.
#[derive(Debug, Clone, Copy)]
struct StreamPulse {
    at: Instant,
    stats: StreamStatsSnapshot,
}

/// Render one data-plane health line: everything as a per-sweep delta against `previous`.
///
/// Split out of [`Daemon::log_stream_pulse`] so the *line itself* is testable. M6.5's whole
/// lesson is that this line is the only place a wire-format break becomes visible, so what
/// it prints is behaviour, not decoration:
///
/// * `frames` vs `price_changes_applied` — traffic arriving vs traffic understood. The soak
///   that motivated M6.5 ran at 8 500 frames/s with `price_changes_applied=0`.
/// * `delta_entries_applied` / `delta_entries_skipped` — level updates that reached a book,
///   and those dropped for an unknown asset or an out-of-order frame.
/// * `frames_unrecognized` — parsed, but matched no shape we handle. Non-zero means the
///   channel is saying something we do not understand; the first few such payloads are also
///   in the log verbatim (see [`ws::BookStore::sample_unrecognized`]).
/// * `delta_top_mismatch` — applied deltas whose own `best_bid`/`best_ask` disagreed with
///   the book we built from them. Drift between REST sweeps, counted but never acted on.
fn emit_stream_pulse(
    stats: &StreamStatsSnapshot,
    previous: Option<&StreamPulse>,
    now: Instant,
    connections: u32,
) {
    let Some(previous) = previous else {
        // First sweep: totals only, because there is no interval to divide by yet.
        tracing::info!(
            frames_total = stats.messages,
            price_changes_applied_total = stats.deltas,
            delta_entries_applied_total = stats.delta_entries_applied,
            frames_unrecognized_total = stats.frames_unrecognized,
            connections,
            "stream data-plane health (first sweep — no rate yet)"
        );
        return;
    };
    let elapsed = now.saturating_duration_since(previous.at);
    let since = &previous.stats;
    let messages = stats.messages.saturating_sub(since.messages);
    tracing::info!(
        frames = messages,
        price_changes_applied = stats.deltas.saturating_sub(since.deltas),
        snapshots_applied = stats.snapshots.saturating_sub(since.snapshots),
        delta_entries_applied = stats
            .delta_entries_applied
            .saturating_sub(since.delta_entries_applied),
        delta_entries_skipped = stats
            .delta_entries_skipped
            .saturating_sub(since.delta_entries_skipped),
        frames_unrecognized = stats
            .frames_unrecognized
            .saturating_sub(since.frames_unrecognized),
        delta_top_mismatch = stats
            .delta_top_mismatch
            .saturating_sub(since.delta_top_mismatch),
        events_per_sec = %ws::events_per_sec(messages, elapsed),
        since_secs = elapsed.as_secs(),
        connections,
        "stream data-plane health"
    );
}

fn elapsed_ms(since: Instant) -> i64 {
    i64::try_from(since.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Await the next dirty batch, or never resolve when there is no stream. Written as a
/// helper so the `select!` arm reads the same either way.
async fn recv_batch(stream: Option<&mut StreamManager>) -> Option<DirtyBatch> {
    match stream {
        Some(manager) => manager.recv().await,
        None => std::future::pending().await,
    }
}

/// Crypto events that will already have closed by the time the universe is next refreshed.
///
/// The BTC/ETH Up-or-Down series cycle every 5–15 minutes, far inside the 600 s universe
/// refresh, so most of them are created and resolved without this scanner ever seeing
/// them. Counting the ones we *did* catch on their way out is the cheap evidence that the
/// cadence — not the market — is the limit. Fixing it (a dedicated fast poller for the
/// crypto series) is the crypto-engine milestone, deliberately not this one.
pub fn short_lived_crypto_events(
    universe: &Universe,
    universe_refresh_secs: u64,
    now: chrono::DateTime<Utc>,
) -> usize {
    let horizon = i64::try_from(universe_refresh_secs).unwrap_or(i64::MAX);
    let cutoff = now + chrono::Duration::seconds(horizon);
    universe
        .events
        .iter()
        .filter(|e| e.category.as_str() == "crypto" && e.ends_by(cutoff))
        .count()
}

// ---------------------------------------------------------------------------------
// Lifecycle tracking
// ---------------------------------------------------------------------------------

struct TrackerCtx {
    cfg: Arc<Config>,
    http: Arc<HttpClient>,
    store: Arc<Store>,
    alerter: Arc<Alerter>,
    shutdown: watch::Receiver<bool>,
}

/// Would the fills we walked at detection still be there?
///
/// * Taker-sized opportunity: every leg must still fill `leg.size` from the ask book at a
///   VWAP no worse than the one we detected. A thinner book, a wider ask, or a missing
///   book all mean "gone".
/// * Maker-only opportunity: we would have rested at the bid, so the question is whether
///   that quote is still the one to join — the best bid must not have moved above the
///   price we planned to post at (a lower bid is fine: it is cheaper).
pub fn fills_still_available(op: &Opportunity, books: &BookMap) -> bool {
    op.legs.iter().all(|leg| {
        let Some(book) = books.get(&leg.token_id) else {
            return false;
        };
        if op.maker_only {
            match (book.best_bid(), leg.best_bid) {
                (Some(now), Some(then)) => now <= then,
                _ => false,
            }
        } else {
            match book.vwap_for_size(Side::Ask, leg.size) {
                Some(vwap) => vwap <= leg.vwap,
                None => false,
            }
        }
    })
}

/// Simulated P&L for a filled opportunity: `(taker, maker)`.
///
/// The taker number is only credited for a construction we actually sized as a taker.
/// The maker number is always hypothetical — we never rest an order, so we never learn
/// whether it would have been crossed — and the summary labels it as such.
pub fn simulated_pnl(op: &Opportunity) -> (Option<Decimal>, Option<Decimal>) {
    let taker = (!op.maker_only).then_some(op.net_taker_total);
    (taker, op.net_maker_total)
}

async fn track_opportunity(
    ctx: TrackerCtx,
    id: i64,
    op: Opportunity,
    _permit: OwnedSemaphorePermit,
) {
    let detected = Instant::now();
    let interval = Duration::from_secs(ctx.cfg.lifecycle.repoll_interval_secs);
    let window = Duration::from_secs(ctx.cfg.lifecycle.repoll_window_secs);
    let tokens: Vec<TokenId> = op.legs.iter().map(|l| l.token_id.clone()).collect();
    let clob = ClobClient::new(&ctx.http, &ctx.cfg);

    let mut shutdown = ctx.shutdown.clone();
    let mut repolls = 0i64;
    let mut first_available: Option<bool> = None;
    let mut persistence_ms: Option<i64> = None;
    // Until a re-poll actually lands, the honest status is "we do not know".
    let mut status = LifecycleStatus::Unresolved;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }

        let books = match clob.fetch_books(&tokens).await {
            Ok(books) => books,
            Err(err) => {
                tracing::warn!(%err, id, "lifecycle re-poll failed — closing the window");
                break;
            }
        };
        repolls += 1;
        let available = fills_still_available(&op, &books);
        let elapsed = elapsed_ms(detected);

        if first_available.is_none() {
            // The first re-poll is the fill decision, and it is never revised.
            first_available = Some(available);
            status = if available {
                LifecycleStatus::FilledSimulated
            } else {
                LifecycleStatus::Vanished
            };
            persistence_ms = Some(if available { elapsed } else { 0 });
        } else if available {
            persistence_ms = Some(elapsed);
        }

        if !available || detected.elapsed() >= window {
            break;
        }
    }

    let (pnl_taker, pnl_maker) = if status == LifecycleStatus::FilledSimulated {
        simulated_pnl(&op)
    } else {
        (None, None)
    };
    let outcome = LifecycleOutcome {
        status,
        repolls,
        first_repoll_available: first_available,
        persistence_ms,
        simulated_pnl_taker: pnl_taker,
        simulated_pnl_maker: pnl_maker,
        resolved_at: Utc::now(),
    };
    if let Err(err) = ctx.store.record_lifecycle(id, &outcome) {
        tracing::warn!(%err, id, "could not record the lifecycle outcome");
    }
    ctx.alerter.events().append(
        "lifecycle",
        json!({
            "id": id,
            "status": status.as_str(),
            "repolls": repolls,
            "first_repoll_available": first_available,
            "persistence_ms": persistence_ms,
            "simulated_pnl_taker": pnl_taker,
            "simulated_pnl_maker": pnl_maker,
            "event_slug": op.event_slug,
            "kind": op.kind.as_str(),
            "category": op.category.as_str(),
        }),
    );
    tracing::info!(
        id,
        status = status.as_str(),
        repolls,
        persistence_ms,
        "lifecycle window closed"
    );
}

// ---------------------------------------------------------------------------------
// Daily summary
// ---------------------------------------------------------------------------------

/// Aggregated view of one UTC day, derived entirely from the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailySummary {
    pub day: NaiveDate,
    pub total: usize,
    pub maker_only: usize,
    /// `(strategy, category) → count`.
    pub by_strategy_category: BTreeMap<(String, String), usize>,
    /// `category → count`, over every category that appeared that day.
    pub by_category: BTreeMap<String, usize>,
    /// Net per share, as constructed (maker net for maker-only rows).
    pub net_p50: Option<Decimal>,
    pub net_p90: Option<Decimal>,
    pub net_max: Option<Decimal>,
    pub persistence_p50_ms: Option<i64>,
    pub persistence_p90_ms: Option<i64>,
    pub persistence_max_ms: Option<i64>,
    /// Rows detected from the stream (i.e. carrying a latency measurement), and the
    /// distribution of that latency. REST-detected rows are excluded rather than counted
    /// as some invented number.
    pub stream_detected: usize,
    pub latency_p50_ms: Option<i64>,
    pub latency_p95_ms: Option<i64>,
    pub latency_max_ms: Option<i64>,
    pub filled: usize,
    pub vanished: usize,
    pub unresolved: usize,
    pub untracked: usize,
    pub open: usize,
    /// Filled / (filled + vanished), as a percentage. `None` when nothing resolved.
    pub fill_rate_pct: Option<Decimal>,
    pub pnl_taker: Decimal,
    pub pnl_maker_hypothetical: Decimal,
    pub capital_max: Option<Decimal>,
    pub capital_mean: Option<Decimal>,
    pub capital_filled_total: Decimal,
    pub per_trade_cap: Decimal,
    /// Rows sized at 99%+ of the per-trade cap: the cap, not the book, was the binder.
    pub capped_rows: usize,
    pub scan: ScanTotals,
    pub alerts: Option<AlertStats>,
}

/// Build the summary. Pure over its inputs, so the arithmetic is unit-testable.
pub fn summarize(
    day: NaiveDate,
    rows: &[OpportunityRow],
    scan: ScanTotals,
    per_trade_cap: Decimal,
) -> DailySummary {
    let mut by_strategy_category: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut nets: Vec<Decimal> = Vec::with_capacity(rows.len());
    let mut persistences: Vec<i64> = Vec::new();
    let mut latencies: Vec<i64> = Vec::new();
    let mut capitals: Vec<Decimal> = Vec::with_capacity(rows.len());
    let (mut filled, mut vanished, mut unresolved, mut untracked, mut open) = (0, 0, 0, 0, 0);
    let mut maker_only = 0usize;
    let mut pnl_taker = Decimal::ZERO;
    let mut pnl_maker = Decimal::ZERO;
    let mut capital_filled_total = Decimal::ZERO;
    let mut capped_rows = 0usize;

    for row in rows {
        *by_strategy_category
            .entry((row.kind.clone(), row.category.clone()))
            .or_default() += 1;
        *by_category.entry(row.category.clone()).or_default() += 1;

        let net = if row.maker_only {
            row.net_maker.unwrap_or(Decimal::ZERO)
        } else {
            row.net_taker
        };
        nets.push(net);
        capitals.push(row.capital_required);
        if row.maker_only {
            maker_only += 1;
        }
        if per_trade_cap > Decimal::ZERO
            && row.capital_required * Decimal::ONE_HUNDRED >= per_trade_cap * Decimal::from(99)
        {
            capped_rows += 1;
        }

        match row.status.as_str() {
            "filled_simulated" => {
                filled += 1;
                pnl_taker += row.simulated_pnl_taker.unwrap_or(Decimal::ZERO);
                pnl_maker += row.simulated_pnl_maker.unwrap_or(Decimal::ZERO);
                capital_filled_total += row.capital_required;
            }
            "vanished" => vanished += 1,
            "unresolved" => unresolved += 1,
            "untracked" => untracked += 1,
            _ => open += 1,
        }
        if let Some(ms) = row.persistence_ms {
            persistences.push(ms);
        }
        if let Some(ms) = row.detection_latency_ms {
            latencies.push(ms);
        }
    }

    nets.sort();
    persistences.sort_unstable();
    latencies.sort_unstable();
    let resolved = filled + vanished;
    let fill_rate_pct = (resolved > 0).then(|| {
        (Decimal::from(filled) * Decimal::ONE_HUNDRED / Decimal::from(resolved)).round_dp(1)
    });
    let capital_mean = (!capitals.is_empty()).then(|| {
        (capitals.iter().copied().sum::<Decimal>() / Decimal::from(capitals.len())).round_dp(2)
    });

    DailySummary {
        day,
        total: rows.len(),
        maker_only,
        by_strategy_category,
        by_category,
        net_p50: quantile(&nets, 50),
        net_p90: quantile(&nets, 90),
        net_max: nets.last().copied(),
        persistence_p50_ms: quantile(&persistences, 50),
        persistence_p90_ms: quantile(&persistences, 90),
        persistence_max_ms: persistences.last().copied(),
        stream_detected: latencies.len(),
        latency_p50_ms: quantile(&latencies, 50),
        latency_p95_ms: quantile(&latencies, 95),
        latency_max_ms: latencies.last().copied(),
        filled,
        vanished,
        unresolved,
        untracked,
        open,
        fill_rate_pct,
        pnl_taker,
        pnl_maker_hypothetical: pnl_maker,
        capital_max: capitals.iter().copied().max(),
        capital_mean,
        capital_filled_total,
        per_trade_cap,
        capped_rows,
        scan,
        alerts: None,
    }
}

/// Category counts in report order: the focus categories first, always, in
/// [`REPORTED_CATEGORIES`] order and including the ones that saw nothing, then anything
/// else that did appear, alphabetically.
///
/// A zero is a result: "crypto: 0 opportunities" is the daily evidence that the staged
/// crypto focus is not yet finding anything, and it cannot be read off a table that only
/// lists what fired.
pub fn category_counts_in_report_order(
    by_category: &BTreeMap<String, usize>,
) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = REPORTED_CATEGORIES
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                by_category.get(*name).copied().unwrap_or(0),
            )
        })
        .collect();
    // BTreeMap iteration is already alphabetical, so the tail is sorted.
    out.extend(
        by_category
            .iter()
            .filter(|(name, _)| !REPORTED_CATEGORIES.contains(&name.as_str()))
            .map(|(name, count)| (name.clone(), *count)),
    );
    out
}

/// Nearest-rank percentile over an already-sorted slice.
fn quantile<T: Copy>(sorted: &[T], pct: usize) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    // rank = ceil(n * pct / 100), clamped to [1, n]
    let rank = (sorted.len() * pct).div_ceil(100).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

impl DailySummary {
    /// One-line version, used as the alert headline.
    pub fn headline(&self) -> String {
        format!(
            "[DRY-RUN] {} — {} opportunities, {} filled / {} vanished, simulated taker P&L ${:.2}",
            self.day,
            self.total,
            self.filled,
            self.vanished,
            usd(self.pnl_taker)
        )
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# polyarb daily summary — {} (UTC)\n\n", self.day));
        out.push_str(
            "Mode: **dry-run**. No orders were placed; every number below is simulated.\n\n",
        );

        out.push_str("## Scan loop\n\n");
        let mean_cycle = if self.scan.cycles > 0 {
            self.scan.duration_ms_total / self.scan.cycles
        } else {
            0
        };
        out.push_str(&format!(
            "- cycles: {} ({} failed)\n- cycle duration: mean {} ms, max {} ms\n\
             - markets scanned (peak): {}\n- books fetched (peak): {}\n\n",
            self.scan.cycles,
            self.scan.errors,
            mean_cycle,
            self.scan.duration_ms_max,
            self.scan.markets_scanned_max,
            self.scan.books_fetched_max,
        ));

        out.push_str("## Opportunities\n\n");
        out.push_str(&format!(
            "- distinct opportunities: **{}** ({} maker-only)\n",
            self.total, self.maker_only
        ));
        if self.by_strategy_category.is_empty() {
            out.push_str("- none detected\n");
        } else {
            out.push_str("\n| strategy | category | count |\n|---|---|---:|\n");
            for ((kind, category), count) in &self.by_strategy_category {
                out.push_str(&format!("| {kind} | {category} | {count} |\n"));
            }
        }

        out.push_str("\n## By category\n\n");
        for (category, count) in category_counts_in_report_order(&self.by_category) {
            out.push_str(&format!("- {category}: {count} opportunities\n"));
        }
        out.push_str(
            "\nThe focus categories are always listed, including at zero. Crypto is a \
             staged second focus and is expected to read zero for now: the BTC/ETH \
             Up-or-Down series cycle every 5–15 minutes, well inside the universe refresh \
             interval, so most of those markets open and resolve without ever entering the \
             scanned universe. Closing that gap is the crypto-engine milestone.\n",
        );

        out.push_str("\n## Net edge per share (as constructed)\n\n");
        out.push_str(&format!(
            "- p50: {}\n- p90: {}\n- max: {}\n",
            fmt_opt_dec(self.net_p50),
            fmt_opt_dec(self.net_p90),
            fmt_opt_dec(self.net_max),
        ));
        out.push_str(
            "\nMaker-only rows contribute their maker net (their taker net is below the \
             floor by construction).\n",
        );

        out.push_str("\n## Detection latency (stream)\n\n");
        if self.stream_detected == 0 {
            out.push_str(
                "- no opportunity was detected from a streamed book update today \
                 (REST-detected rows carry no latency measurement)\n",
            );
        } else {
            out.push_str(&format!(
                "- detected from streamed book updates: {} of {}\n\
                 - latency from the triggering market data: p50 {} ms, p95 {} ms, max {} ms\n",
                self.stream_detected,
                self.total,
                fmt_opt_i64(self.latency_p50_ms),
                fmt_opt_i64(self.latency_p95_ms),
                fmt_opt_i64(self.latency_max_ms),
            ));
            out.push_str(
                "\nMeasured from the triggering frame's server timestamp when it carries a \
                 plausible one, otherwise from the moment we read the frame off the socket \
                 (so it is a lower bound on true end-to-end latency, never an overstatement).\n",
            );
        }

        out.push_str("\n## Persistence and fills\n\n");
        out.push_str(&format!(
            "- persistence: p50 {}, p90 {}, max {}\n",
            fmt_opt_ms(self.persistence_p50_ms),
            fmt_opt_ms(self.persistence_p90_ms),
            fmt_opt_ms(self.persistence_max_ms),
        ));
        out.push_str(&format!(
            "- filled (simulated): {} · vanished: {} · unresolved: {} · untracked: {} · still open: {}\n",
            self.filled, self.vanished, self.unresolved, self.untracked, self.open
        ));
        out.push_str(&format!(
            "- fill rate: {}\n",
            self.fill_rate_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "n/a".into())
        ));

        out.push_str("\n## Simulated P&L\n\n");
        out.push_str(&format!(
            "- taker (credited only for taker-sized fills): **${:.2}**\n",
            usd(self.pnl_taker)
        ));
        out.push_str(&format!(
            "- maker view (hypothetical — assumes every resting leg is crossed): ${:.2}\n",
            usd(self.pnl_maker_hypothetical)
        ));

        out.push_str("\n## Capital\n\n");
        out.push_str(&format!(
            "- per-trade cap: ${:.2}\n- capital per opportunity: mean {}, max {}\n\
             - rows sized at the cap (>=99%): {}\n- capital committed by simulated fills: ${:.2}\n",
            usd(self.per_trade_cap),
            self.capital_mean
                .map(|c| format!("${:.2}", usd(c)))
                .unwrap_or_else(|| "n/a".into()),
            self.capital_max
                .map(|c| format!("${:.2}", usd(c)))
                .unwrap_or_else(|| "n/a".into()),
            self.capped_rows,
            usd(self.capital_filled_total),
        ));

        if let Some(alerts) = self.alerts {
            out.push_str(&format!(
                "\n## Alerts\n\n- sent: {} · failed: {} · suppressed: {} · \
                 held back by the per-event cooldown: {} · circuit open: {}\n",
                alerts.sent,
                alerts.failed,
                alerts.suppressed,
                alerts.cooldown_suppressed,
                alerts.circuit_open
            ));
            out.push_str(
                "\nA cooldown-suppressed alert is a re-detection of a construction already \
                 reported, no better than last time; its measurement row and JSONL record \
                 exist either way.\n",
            );
        }
        out
    }
}

/// Dollars are *rounded* to cents for display; `Decimal`'s `{:.2}` truncates, which would
/// print $49.998 as $49.99 next to a mean of $50.00 and look like an arithmetic bug.
fn usd(v: Decimal) -> Decimal {
    v.round_dp(2)
}

fn fmt_opt_dec(v: Option<Decimal>) -> String {
    v.map(|d| format!("{d:+.6}"))
        .unwrap_or_else(|| "n/a".into())
}

fn fmt_opt_i64(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "n/a".into())
}

fn fmt_opt_ms(v: Option<i64>) -> String {
    v.map(|ms| format!("{:.1}s", Decimal::from(ms) / Decimal::ONE_THOUSAND))
        .unwrap_or_else(|| "n/a".into())
}

/// Build, store, write and (optionally) alert the summary for `day`.
pub async fn emit_daily_summary(
    cfg: &Config,
    store: &Store,
    alerter: &Alerter,
    day: NaiveDate,
    send: bool,
) -> Result<DailySummary> {
    let rows = store
        .opportunities_for_day(day)
        .context("could not read the day's opportunities")?;
    let scan = store
        .scan_totals_for_day(day)
        .context("could not read the day's scan stats")?;
    let mut summary = summarize(day, &rows, scan, cfg.risk.per_trade_cap_usd);
    summary.alerts = Some(alerter.stats());
    let markdown = summary.to_markdown();

    if let Err(err) = store.save_daily_summary(day, &markdown) {
        tracing::warn!(%err, "could not store the daily summary");
    }
    let path = write_report(&cfg.storage.report_dir, day, &markdown)?;
    tracing::info!(path = %path.display(), "daily summary written");

    let payload = json!({
        "day": day.to_string(),
        "total": summary.total,
        "filled": summary.filled,
        "vanished": summary.vanished,
        "pnl_taker": summary.pnl_taker,
        "pnl_maker_hypothetical": summary.pnl_maker_hypothetical,
        "report_path": path.display().to_string(),
    });
    if send {
        alerter
            .notify(
                "daily_summary",
                &format!("{}\n\n{}", summary.headline(), markdown),
                Priority::Important,
                payload,
            )
            .await;
    } else {
        alerter.events().append("daily_summary", payload);
    }
    Ok(summary)
}

fn write_report(dir: &str, day: NaiveDate, markdown: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create the report directory {}", dir.display()))?;
    let path = dir.join(format!("summary-{day}.md"));
    std::fs::write(&path, markdown)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// `polyarb report [--date YYYY-MM-DD]`.
pub async fn report(cfg: &Config, day: Option<NaiveDate>, send: bool) -> Result<()> {
    let day = day.unwrap_or_else(|| Utc::now().date_naive());
    let store = Store::open(Path::new(&cfg.storage.database_path))
        .with_context(|| format!("could not open {}", cfg.storage.database_path))?;
    let events = Arc::new(
        EventLog::open(Path::new(&cfg.storage.log_dir))
            .with_context(|| format!("could not open the event log in {}", cfg.storage.log_dir))?,
    );
    let alerter = Alerter::new(&cfg.alerts, events);
    let summary = emit_daily_summary(cfg, &store, &alerter, day, send).await?;
    println!("{}", summary.to_markdown());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::sample_opportunity;
    use crate::types::{OrderBook, PriceLevel};
    use rust_decimal_macros::dec;

    fn books_at(ask_a: Decimal, size_a: Decimal, ask_b: Decimal, size_b: Decimal) -> BookMap {
        let mut books = BookMap::new();
        for (id, ask, size, bid) in [
            ("1001", ask_a, size_a, dec!(0.39)),
            ("1002", ask_b, size_b, dec!(0.54)),
        ] {
            books.insert(
                TokenId::new(id),
                OrderBook::new(
                    TokenId::new(id),
                    vec![PriceLevel::new(bid, dec!(500))],
                    vec![PriceLevel::new(ask, size)],
                )
                .normalized(),
            );
        }
        books
    }

    #[test]
    fn availability_requires_every_leg_to_still_fill_at_size() {
        let op = sample_opportunity(); // legs: 100 shares each, vwap 0.40 / 0.55

        // Unchanged book → the walked fills are still there.
        assert!(fills_still_available(
            &op,
            &books_at(dec!(0.40), dec!(500), dec!(0.55), dec!(500))
        ));

        // Better prices are still "available" — never worse than detection.
        assert!(fills_still_available(
            &op,
            &books_at(dec!(0.39), dec!(500), dec!(0.54), dec!(500))
        ));

        // One leg ticked up: the walked VWAP is no longer achievable.
        assert!(!fills_still_available(
            &op,
            &books_at(dec!(0.41), dec!(500), dec!(0.55), dec!(500))
        ));

        // Same price, but the size we walked is gone.
        assert!(!fills_still_available(
            &op,
            &books_at(dec!(0.40), dec!(500), dec!(0.55), dec!(20))
        ));

        // A leg's book missing entirely counts as gone, never as available.
        let mut partial = books_at(dec!(0.40), dec!(500), dec!(0.55), dec!(500));
        partial.remove(&TokenId::new("1002"));
        assert!(!fills_still_available(&op, &partial));
    }

    #[test]
    fn maker_only_availability_tracks_the_bid_we_would_join() {
        let mut op = sample_opportunity();
        op.maker_only = true;

        // Bids unchanged → the same quote is still there to join.
        assert!(fills_still_available(
            &op,
            &books_at(dec!(0.40), dec!(500), dec!(0.55), dec!(500))
        ));

        // Someone outbid us on a leg: joining now costs more than we modelled.
        let mut outbid = books_at(dec!(0.40), dec!(500), dec!(0.55), dec!(500));
        outbid.insert(
            TokenId::new("1001"),
            OrderBook::new(
                TokenId::new("1001"),
                vec![PriceLevel::new(dec!(0.395), dec!(500))],
                vec![PriceLevel::new(dec!(0.40), dec!(500))],
            ),
        );
        assert!(!fills_still_available(&op, &outbid));
    }

    #[test]
    fn simulated_pnl_is_credited_only_where_it_is_honest() {
        let op = sample_opportunity();
        assert_eq!(simulated_pnl(&op), (Some(dec!(3.05)), Some(dec!(7))));

        let mut maker = op;
        maker.maker_only = true;
        let (taker, maker_view) = simulated_pnl(&maker);
        assert_eq!(taker, None, "a maker-only row never credits taker P&L");
        assert_eq!(maker_view, Some(dec!(7)));
    }

    // -- summary arithmetic ---------------------------------------------------------

    fn row(
        id: i64,
        kind: &str,
        category: &str,
        net_taker: Decimal,
        status: &str,
        persistence_ms: Option<i64>,
        capital: Decimal,
    ) -> OpportunityRow {
        let filled = status == "filled_simulated";
        OpportunityRow {
            id,
            dedupe_key: format!("k{id}"),
            kind: kind.into(),
            category: category.into(),
            event_slug: format!("e{id}"),
            net_taker,
            net_maker: Some(net_taker + dec!(0.01)),
            net_taker_total: net_taker * dec!(100),
            net_maker_total: Some((net_taker + dec!(0.01)) * dec!(100)),
            capital_required: capital,
            executable_size: dec!(100),
            maker_only: false,
            status: status.into(),
            persistence_ms,
            seen_count: 1,
            simulated_pnl_taker: filled.then(|| net_taker * dec!(100)),
            simulated_pnl_maker: filled.then(|| (net_taker + dec!(0.01)) * dec!(100)),
            detected_at: "2026-07-26T00:00:00.000Z".into(),
            detection_latency_ms: None,
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 26).unwrap()
    }

    #[test]
    fn summary_aggregates_counts_percentiles_pnl_and_capital() {
        let rows = vec![
            row(
                1,
                "binary_yes_no",
                "politics",
                dec!(0.01),
                "filled_simulated",
                Some(4_000),
                dec!(50),
            ),
            row(
                2,
                "binary_yes_no",
                "politics",
                dec!(0.02),
                "vanished",
                Some(0),
                dec!(20),
            ),
            row(
                3,
                "neg_risk_yes_side",
                "sports",
                dec!(0.03),
                "filled_simulated",
                Some(30_000),
                dec!(49.6),
            ),
            row(
                4,
                "neg_risk_yes_side",
                "sports",
                dec!(0.04),
                "unresolved",
                None,
                dec!(10),
            ),
            row(
                5,
                "binary_yes_no",
                "geopolitics",
                dec!(0.05),
                "untracked",
                None,
                dec!(5),
            ),
        ];
        let scan = ScanTotals {
            cycles: 10,
            errors: 1,
            duration_ms_total: 5_000,
            duration_ms_max: 900,
            markets_scanned_max: 300,
            books_fetched_max: 600,
        };
        let s = summarize(day(), &rows, scan, dec!(50));

        assert_eq!(s.total, 5);
        assert_eq!(
            s.by_strategy_category[&("binary_yes_no".into(), "politics".into())],
            2
        );
        assert_eq!(
            s.by_strategy_category[&("neg_risk_yes_side".into(), "sports".into())],
            2
        );
        assert_eq!(s.by_strategy_category.len(), 3);

        // nets sorted: 0.01 0.02 0.03 0.04 0.05 — nearest rank of 5 values.
        assert_eq!(s.net_p50, Some(dec!(0.03)));
        assert_eq!(s.net_p90, Some(dec!(0.05)));
        assert_eq!(s.net_max, Some(dec!(0.05)));

        // persistence: 0, 4000, 30000 (rows without a measurement are excluded).
        assert_eq!(s.persistence_p50_ms, Some(4_000));
        assert_eq!(s.persistence_p90_ms, Some(30_000));
        assert_eq!(s.persistence_max_ms, Some(30_000));

        assert_eq!(
            (s.filled, s.vanished, s.unresolved, s.untracked, s.open),
            (2, 1, 1, 1, 0)
        );
        // 2 filled of 3 resolved (unresolved/untracked are excluded from the rate).
        assert_eq!(s.fill_rate_pct, Some(dec!(66.7)));

        // Only the two filled rows contribute: 0.01*100 + 0.03*100.
        assert_eq!(s.pnl_taker, dec!(4));
        assert_eq!(s.pnl_maker_hypothetical, dec!(6));
        assert_eq!(s.capital_filled_total, dec!(99.6));

        assert_eq!(s.capital_max, Some(dec!(50)));
        assert_eq!(s.capital_mean, Some(dec!(26.92)));
        assert_eq!(s.capped_rows, 2, "rows at >=99% of the $50 cap");
        assert_eq!(s.per_trade_cap, dec!(50));
        assert_eq!(s.scan.cycles, 10);
    }

    #[test]
    fn maker_only_rows_use_their_maker_net_and_credit_no_taker_pnl() {
        let mut maker = row(
            1,
            "binary_yes_no",
            "sports",
            dec!(-0.002),
            "filled_simulated",
            Some(2_000),
            dec!(30),
        );
        maker.maker_only = true;
        maker.net_maker = Some(dec!(0.02));
        maker.simulated_pnl_taker = None;
        maker.simulated_pnl_maker = Some(dec!(2));
        let s = summarize(day(), &[maker], ScanTotals::default(), dec!(50));

        assert_eq!(s.maker_only, 1);
        assert_eq!(
            s.net_p50,
            Some(dec!(0.02)),
            "maker net is the honest figure"
        );
        assert_eq!(s.pnl_taker, Decimal::ZERO);
        assert_eq!(s.pnl_maker_hypothetical, dec!(2));
    }

    #[test]
    fn empty_day_summarizes_without_panicking() {
        let s = summarize(day(), &[], ScanTotals::default(), dec!(50));
        assert_eq!(s.total, 0);
        assert_eq!(s.net_p50, None);
        assert_eq!(s.fill_rate_pct, None);
        assert_eq!(s.pnl_taker, Decimal::ZERO);
        let md = s.to_markdown();
        assert!(md.contains("none detected"));
        assert!(md.contains("dry-run"));
    }

    #[test]
    fn markdown_states_the_dry_run_and_the_maker_caveat() {
        let rows = vec![row(
            1,
            "binary_yes_no",
            "politics",
            dec!(0.01),
            "filled_simulated",
            Some(1_000),
            dec!(50),
        )];
        let md = summarize(day(), &rows, ScanTotals::default(), dec!(50)).to_markdown();
        assert!(md.contains("# polyarb daily summary — 2026-07-26 (UTC)"));
        assert!(md.contains("Mode: **dry-run**"));
        assert!(md.contains("hypothetical"));
        assert!(md.contains("| binary_yes_no | politics | 1 |"));
        assert!(md.contains("taker (credited only for taker-sized fills): **$1.00**"));
    }

    /// A category that saw nothing is still a finding. Crypto in particular must appear at
    /// zero, because "we found no crypto edge" and "we never looked" read identically on a
    /// table that only lists what fired.
    #[test]
    fn the_summary_names_every_focus_category_including_an_empty_crypto() {
        let empty = summarize(day(), &[], ScanTotals::default(), dec!(50)).to_markdown();
        for line in [
            "- politics: 0 opportunities",
            "- sports: 0 opportunities",
            "- geopolitics: 0 opportunities",
            "- crypto: 0 opportunities",
        ] {
            assert!(empty.contains(line), "{line:?} missing from:\n{empty}");
        }
        // And the reason a zero is expected there is stated, not left to be inferred.
        assert!(empty.contains("crypto-engine milestone"));

        // Focus categories keep their fixed order; anything else follows, alphabetically.
        let rows = vec![
            row(
                1,
                "binary_yes_no",
                "weather",
                dec!(0.01),
                "open",
                None,
                dec!(5),
            ),
            row(
                2,
                "binary_yes_no",
                "crypto",
                dec!(0.02),
                "open",
                None,
                dec!(5),
            ),
            row(
                3,
                "binary_yes_no",
                "crypto",
                dec!(0.02),
                "open",
                None,
                dec!(5),
            ),
            row(
                4,
                "binary_yes_no",
                "culture",
                dec!(0.03),
                "open",
                None,
                dec!(5),
            ),
        ];
        let s = summarize(day(), &rows, ScanTotals::default(), dec!(50));
        assert_eq!(s.by_category["crypto"], 2);
        assert_eq!(
            category_counts_in_report_order(&s.by_category),
            vec![
                ("politics".to_string(), 0),
                ("sports".to_string(), 0),
                ("geopolitics".to_string(), 0),
                ("crypto".to_string(), 2),
                ("culture".to_string(), 1),
                ("weather".to_string(), 1),
            ]
        );
        let md = s.to_markdown();
        assert!(md.contains("- crypto: 2 opportunities"), "got:\n{md}");
        assert!(md.contains("- weather: 1 opportunities"), "got:\n{md}");
    }

    /// The refresh-cadence gap this milestone deliberately does *not* fix: count what we
    /// can see closing before the next refresh, so the WARN is evidence rather than a
    /// guess. Anything without a known end time is never counted.
    #[test]
    fn short_lived_crypto_events_are_counted_only_when_they_really_are_short_lived() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-29T12:00:00Z")
            .expect("fixed clock")
            .with_timezone(&Utc);
        let at = |mins: i64| Some(now + chrono::Duration::minutes(mins));

        let ev = |category: &str, end: Option<chrono::DateTime<Utc>>| crate::types::TrackedEvent {
            id: "1".into(),
            slug: format!("{category}-event"),
            title: "t".into(),
            neg_risk: false,
            category: crate::types::Category::new(category),
            markets: Vec::new(),
            total_outcomes: 0,
            end_date: end,
        };

        let universe = Universe {
            events: vec![
                ev("crypto", at(5)),   // a 5m series closing inside the window
                ev("crypto", at(9)),   // ditto, still inside 600 s
                ev("crypto", at(11)),  // 11 min out — survives the next refresh
                ev("crypto", None),    // unknown end time is never assumed short-lived
                ev("politics", at(5)), // not crypto: this is normal event churn
            ],
        };
        // 600 s = 10 minutes, so only the 5- and 9-minute events count.
        assert_eq!(short_lived_crypto_events(&universe, 600, now), 2);
        // A 30-minute cadence would sweep the 11-minute one in too.
        assert_eq!(short_lived_crypto_events(&universe, 1_800, now), 3);
        // A cadence faster than every event catches none of them.
        assert_eq!(short_lived_crypto_events(&universe, 60, now), 0);
        assert_eq!(
            short_lived_crypto_events(&Universe::default(), 600, now),
            0,
            "an empty universe must not produce a warning"
        );
    }

    #[test]
    fn quantile_uses_nearest_rank() {
        let v = vec![1, 2, 3, 4];
        assert_eq!(quantile(&v, 50), Some(2));
        assert_eq!(quantile(&v, 90), Some(4));
        assert_eq!(quantile(&v, 100), Some(4));
        assert_eq!(quantile::<i32>(&[], 50), None);
        assert_eq!(quantile(&[7], 50), Some(7));
    }

    #[test]
    fn the_daemon_refuses_to_run_outside_dry_run() {
        let cfg = Config {
            mode: "live".into(),
            ..Config::default()
        };
        let err = ensure_dry_run(&cfg).expect_err("live mode must be refused");
        assert!(err.to_string().contains("dry-run"));
        assert!(ensure_dry_run(&Config::default()).is_ok());
    }

    // -- end-to-end smoke test against a local mock of both APIs ---------------------

    const MOCK_EVENTS: &str = r#"[{
        "id": "1", "slug": "mock-event", "title": "Mock binary market",
        "negRisk": false, "category": "Politics", "active": true, "closed": false,
        "markets": [{
            "conditionId": "0xmock", "question": "Will it?",
            "clobTokenIds": "[\"1001\",\"1002\"]", "outcomes": "[\"Yes\",\"No\"]",
            "active": true, "closed": false, "enableOrderBook": true
        }]
    }]"#;

    // ask 0.40 + 0.55 = 0.95 → 5c gross, ~2c fee at the 0.04 politics rate → clears the floor.
    const MOCK_BOOKS: &str = r#"[
        {"asset_id":"1001","bids":[{"price":"0.39","size":"500"}],"asks":[{"price":"0.40","size":"500"}]},
        {"asset_id":"1002","bids":[{"price":"0.54","size":"500"}],"asks":[{"price":"0.55","size":"500"}]}
    ]"#;

    async fn mock_api() -> String {
        mock_api_with(MOCK_EVENTS, MOCK_BOOKS).await
    }

    /// Minimal HTTP/1.1 stub for the two Polymarket endpoints. Returns its base URL.
    async fn mock_api_with(events: &'static str, books: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body = if head.starts_with("GET /events") {
                        events
                    } else {
                        books
                    };
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
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn daemon_smoke_test_persists_alerts_and_resolves_a_lifecycle() {
        let base = mock_api().await;
        let tmp = std::env::temp_dir().join(format!("polyarb-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.clone();
        cfg.api.clob_base_url = base;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.daemon.scan_interval_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        // This is the REST-only daemon — the mode the owner runs today, and the one the
        // stream must never be allowed to regress.
        cfg.stream.enabled = false;
        cfg.validate().expect("smoke config must validate");

        run(cfg.clone(), Some(2)).await.expect("daemon run");

        // One distinct opportunity, seen on both cycles, resolved as filled.
        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "two cycles over one unchanged book = one row"
        );
        assert_eq!(rows[0].kind, "binary_yes_no");
        assert_eq!(rows[0].category, "politics");
        assert_eq!(rows[0].seen_count, 2, "the repeat sighting updated the row");
        assert_eq!(rows[0].status, "filled_simulated");
        assert!(rows[0].simulated_pnl_taker.unwrap_or_default() > Decimal::ZERO);
        assert!(rows[0].capital_required <= cfg.risk.per_trade_cap_usd);

        let totals = store
            .scan_totals_for_day(Utc::now().date_naive())
            .expect("scan totals");
        assert_eq!(totals.cycles, 2);
        assert_eq!(totals.errors, 0);

        // The JSONL log has the opportunity and its lifecycle outcome, and no secrets.
        let log = std::fs::read_to_string(tmp.join("logs").join("events.jsonl")).expect("log");
        let kinds: Vec<String> = log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .map(|v| v["event"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(kinds.contains(&"opportunity".to_string()));
        assert!(kinds.contains(&"lifecycle".to_string()));
        assert!(!log.to_ascii_lowercase().contains("bot_token"));

        // And the on-demand report writes markdown without needing the network.
        report(&cfg, Some(Utc::now().date_naive()), false)
            .await
            .expect("report");
        let path = tmp
            .join("reports")
            .join(format!("summary-{}.md", Utc::now().date_naive()));
        let md = std::fs::read_to_string(&path).expect("report file");
        assert!(md.contains("polyarb daily summary"));
        assert!(md.contains("| binary_yes_no | politics | 1 |"));
        assert!(
            md.contains("no opportunity was detected from a streamed book update today"),
            "a REST-only day must say so rather than print an invented latency:\n{md}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- market discovery must never kill the daemon ---------------------------------

    /// Which discovery requests the stub should fail. Everything else (the books) keeps
    /// working, so a test failure means the *daemon* stopped, not the mock.
    #[derive(Debug, Clone, Copy)]
    enum EventsFailure {
        /// Only the first attempt — the one that used to be fatal at startup.
        FirstAttempt,
        /// The first attempt succeeds; every refresh after it fails.
        EveryAttemptAfterTheFirst,
    }

    /// [`mock_api_with`] with a programmable failure on the events endpoint. Returns the base
    /// URL and the number of discovery requests served (keyset or legacy — both begin
    /// `GET /events`).
    async fn mock_api_flaky_events(
        events: &'static str,
        books: &'static str,
        failure: EventsFailure,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
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
                    let (status, body) = if head.starts_with("GET /events") {
                        let attempt = counter.fetch_add(1, Ordering::SeqCst);
                        let fail = match failure {
                            EventsFailure::FirstAttempt => attempt == 0,
                            EventsFailure::EveryAttemptAfterTheFirst => attempt > 0,
                        };
                        if fail {
                            (500, r#"{"error":"upstream is having a moment"}"#)
                        } else {
                            (200, events)
                        }
                    } else {
                        (200, books)
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), attempts)
    }

    /// The crash loop, from the daemon's side: discovery fails at startup. It used to
    /// propagate out of `run`, exit the process, and let Docker restart it forever. Now it
    /// logs, waits, and tries again — and the daemon that comes out of it scans normally.
    #[tokio::test]
    async fn a_failed_first_discovery_is_retried_instead_of_ending_the_process() {
        let (base, attempts) =
            mock_api_flaky_events(MOCK_EVENTS, MOCK_BOOKS, EventsFailure::FirstAttempt).await;
        let tmp = std::env::temp_dir().join(format!("polyarb-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.clone();
        cfg.api.clob_base_url = base;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        // Also the first retry delay (see `discovery_backoff`), so the test waits 1 s.
        cfg.daemon.scan_interval_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        cfg.stream.enabled = false;
        cfg.validate().expect("config must validate");

        run(cfg.clone(), Some(1))
            .await
            .expect("a discovery failure at startup must not end the daemon");

        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "discovery must have been retried, not given up on"
        );
        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "the universe must have been built on the retry and scanned"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The retry line is the *only* thing an operator sees while discovery is failing, so
    /// it has to carry the whole cause chain. It used to log `anyhow`'s plain `Display`,
    /// which prints the outermost context and nothing else — every attempt read "market
    /// discovery … failed" and the actual reason (the HTTP status, the decode error, the
    /// empty-page diagnostic) was invisible.
    #[tokio::test]
    async fn the_discovery_retry_line_carries_the_whole_cause_chain() {
        let (base, _attempts) =
            mock_api_flaky_events(MOCK_EVENTS, MOCK_BOOKS, EventsFailure::FirstAttempt).await;
        let tmp = std::env::temp_dir().join(format!("polyarb-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.clone();
        cfg.api.clob_base_url = base;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.daemon.scan_interval_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        cfg.stream.enabled = false;
        cfg.validate().expect("config must validate");

        let capture = crate::testlog::LogCapture::new();
        {
            let _guard = capture.install(tracing::Level::ERROR);
            run(cfg, Some(1)).await.expect("daemon run");
        }

        let log = capture.text();
        assert!(
            log.contains("market discovery failed at startup"),
            "the retry line is missing entirely:\n{log}"
        );
        // The outer context…
        assert!(
            log.contains("market discovery via the Gamma API"),
            "got:\n{log}"
        );
        // …and the nested cause, which plain `Display` would have dropped on the floor.
        assert!(
            log.contains("HTTP 500"),
            "the cause chain is missing from the retry line:\n{log}"
        );
        assert!(log.contains("upstream is having a moment"), "got:\n{log}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// M6.5. The health line is the only place a wire-format break is visible, and the
    /// first soak proved a line can be *correct* and still hide the failure: it reported
    /// `frames=4807541 price_changes_applied=0` for ten hours, which is indistinguishable
    /// from a quiet market unless you know deltas exist. Every counter that would have
    /// named the fault is now on the line, so this asserts they are all actually printed —
    /// and as per-sweep deltas, not totals.
    #[test]
    fn the_data_plane_health_line_names_every_counter_that_can_hide_a_break() {
        let previous = StreamPulse {
            at: Instant::now(),
            stats: StreamStatsSnapshot {
                messages: 100,
                deltas: 10,
                snapshots: 5,
                delta_entries_applied: 40,
                delta_entries_skipped: 1,
                frames_unrecognized: 2,
                delta_top_mismatch: 3,
                ..StreamStatsSnapshot::default()
            },
        };
        let current = StreamStatsSnapshot {
            messages: 350,
            deltas: 60,
            snapshots: 9,
            delta_entries_applied: 240,
            delta_entries_skipped: 8,
            frames_unrecognized: 11,
            delta_top_mismatch: 4,
            ..StreamStatsSnapshot::default()
        };

        let capture = crate::testlog::LogCapture::new();
        {
            let _guard = capture.install(tracing::Level::INFO);
            emit_stream_pulse(&current, Some(&previous), Instant::now(), 177);
            emit_stream_pulse(&current, None, Instant::now(), 177);
        }
        let log = capture.text();

        for field in [
            "frames=250",
            "price_changes_applied=50",
            "snapshots_applied=4",
            "delta_entries_applied=200",
            "delta_entries_skipped=7",
            "frames_unrecognized=9",
            "delta_top_mismatch=1",
            "connections=177",
        ] {
            assert!(
                log.contains(field),
                "the health line is missing {field}:\n{log}"
            );
        }
        // The first sweep has no interval to divide by, but must still show whether deltas
        // are landing at all — that is the whole point of it.
        assert!(log.contains("delta_entries_applied_total=240"), "{log}");
        assert!(log.contains("frames_unrecognized_total=11"), "{log}");
    }

    /// The renderer itself, over a hand-built chain.
    #[test]
    fn error_chain_renders_every_link() {
        let err = anyhow::anyhow!("the root cause")
            .context("the middle")
            .context("the outermost thing that failed");
        let rendered = error_chain(&err);
        assert!(rendered.contains("the outermost thing that failed"));
        assert!(rendered.contains("the middle"));
        assert!(rendered.contains("the root cause"));
        // Plain Display is exactly what the bug was: only the outermost link.
        assert!(!format!("{err}").contains("the root cause"));
    }

    /// A refresh that fails mid-run is a hiccup, not the end: the previous universe stays,
    /// and every scan cycle keeps happening.
    #[tokio::test]
    async fn a_failed_universe_refresh_keeps_the_previous_universe_and_keeps_scanning() {
        let (base, attempts) = mock_api_flaky_events(
            MOCK_EVENTS,
            MOCK_BOOKS,
            EventsFailure::EveryAttemptAfterTheFirst,
        )
        .await;
        let tmp = std::env::temp_dir().join(format!("polyarb-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.clone();
        cfg.api.clob_base_url = base;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.daemon.scan_interval_secs = 1;
        // Refresh on every cycle after the first, so the failure is exercised twice.
        cfg.daemon.universe_refresh_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        cfg.stream.enabled = false;
        cfg.validate().expect("config must validate");

        run(cfg.clone(), Some(3))
            .await
            .expect("a refresh failure must not end the daemon");

        assert!(
            attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "at least one refresh must have been attempted (and failed)"
        );
        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].seen_count, 3,
            "every cycle must have scanned the kept universe"
        );
        let totals = store
            .scan_totals_for_day(Utc::now().date_naive())
            .expect("totals");
        assert_eq!(totals.cycles, 3);
        assert_eq!(totals.errors, 0, "the books never failed");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discovery_backoff_grows_from_the_scan_cadence_and_is_capped() {
        // The shipped 5 s scan interval gives the intended 5 → 10 → 20 → 40 → 60 s ramp.
        let secs = |attempt| discovery_backoff(5, attempt).as_secs();
        assert_eq!(
            [secs(1), secs(2), secs(3), secs(4), secs(5), secs(9)],
            [5, 10, 20, 40, 60, 60]
        );
        // Never zero (a hot loop) and never longer than a minute, whatever the config says.
        assert_eq!(discovery_backoff(0, 1).as_secs(), 1);
        assert_eq!(discovery_backoff(86_400, 1).as_secs(), 60);
    }

    // -- M6: end-to-end over a mock market channel -----------------------------------

    /// Two events. Only the first one is ever touched by the stream; the second carries a
    /// standing arb in its REST books that **only a full scan could find**, which is how
    /// this test proves detection really is incremental.
    const STREAM_EVENTS: &str = r#"[
        {"id": "1", "slug": "mock-event-a", "title": "Touched by the stream",
         "negRisk": false, "category": "Politics", "active": true, "closed": false,
         "markets": [{"conditionId": "0xaaa", "question": "A?",
                      "clobTokenIds": "[\"1001\",\"1002\"]", "outcomes": "[\"Yes\",\"No\"]",
                      "active": true, "closed": false, "enableOrderBook": true}]},
        {"id": "2", "slug": "mock-event-b", "title": "Never touched",
         "negRisk": false, "category": "Politics", "active": true, "closed": false,
         "markets": [{"conditionId": "0xbbb", "question": "B?",
                      "clobTokenIds": "[\"2001\",\"2002\"]", "outcomes": "[\"Yes\",\"No\"]",
                      "active": true, "closed": false, "enableOrderBook": true}]}
    ]"#;

    /// A: 0.50 + 0.55 = 1.05 → no gap at all until the stream moves it.
    /// B: 0.40 + 0.55 = 0.95 → a 5¢ gap sitting there the whole time.
    const STREAM_BOOKS: &str = r#"[
        {"asset_id":"1001","bids":[{"price":"0.49","size":"500"}],"asks":[{"price":"0.50","size":"500"}]},
        {"asset_id":"1002","bids":[{"price":"0.54","size":"500"}],"asks":[{"price":"0.55","size":"500"}]},
        {"asset_id":"2001","bids":[{"price":"0.39","size":"500"}],"asks":[{"price":"0.40","size":"500"}]},
        {"asset_id":"2002","bids":[{"price":"0.54","size":"500"}],"asks":[{"price":"0.55","size":"500"}]}
    ]"#;

    /// A stub of the CLOB market channel.
    ///
    /// It only pushes frames for tokens the client actually asked for, so a wrong
    /// subscribe frame shows up as "nothing was ever detected" rather than passing
    /// silently. Returns its `ws://` URL.
    async fn mock_market_channel(gap: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(serve_market_channel(listener, gap));
        format!("ws://{addr}/ws/market")
    }

    /// The accept loop of [`mock_market_channel`], separated so a test can start serving on
    /// an address that was deliberately left dead for a while.
    async fn serve_market_channel(listener: tokio::net::TcpListener, gap: Duration) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    // The subscribe frame comes first, and decides what we push.
                    let Some(Ok(message)) = ws.next().await else {
                        return;
                    };
                    let subscribed: Vec<String> = serde_json::from_str::<serde_json::Value>(
                        message.to_text().unwrap_or_default(),
                    )
                    .ok()
                    .and_then(|v| {
                        v.get("assets_ids").and_then(|a| {
                            a.as_array().map(|ids| {
                                ids.iter()
                                    .filter_map(|i| i.as_str().map(str::to_string))
                                    .collect()
                            })
                        })
                    })
                    .unwrap_or_default();
                    if !subscribed.iter().any(|id| id == "1001") {
                        return;
                    }

                    let ts = || Utc::now().timestamp_millis().to_string();
                    // 1. A snapshot that agrees with REST: no gap, one detection pass.
                    let snapshot = json!({
                        "event_type": "book", "asset_id": "1001", "timestamp": ts(),
                        "hash": "h1",
                        "bids": [{"price": "0.49", "size": "500"}],
                        "asks": [{"price": "0.50", "size": "500"}]
                    });
                    if ws.send(Message::Text(snapshot.to_string())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(gap).await;
                    // 2. A delta that puts a 0.40 ask on top: 0.40 + 0.55 = 0.95.
                    let delta = json!({
                        "event_type": "price_change", "asset_id": "1001", "timestamp": ts(),
                        "changes": [{"price": "0.40", "size": "500", "side": "SELL"}]
                    });
                    if ws.send(Message::Text(delta.to_string())).await.is_err() {
                        return;
                    }
                    // Stay open; the daemon closes us on shutdown.
                    while let Some(Ok(_)) = ws.next().await {}
                });
            }
        }
    }

    fn stream_test_config(rest: String, ws_url: String, tmp: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.api.gamma_base_url = rest.clone();
        cfg.api.clob_base_url = rest;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.daemon.scan_interval_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        cfg.stream.enabled = true;
        cfg.stream.url = ws_url;
        cfg.stream.debounce_ms = 50;
        cfg.stream.stale_after_secs = 900;
        // Long enough that no full REST sweep runs inside the test window: what is
        // detected here was detected from the stream, and nothing else.
        cfg.stream.resync_interval_secs = 3_600;
        cfg.stream.fallback_after_failures = 5;
        // Likewise: no re-probe unless a test asks for one.
        cfg.stream.reprobe_interval_secs = 3_600;
        cfg
    }

    /// A pushed delta creates a gap; only the event that moved is re-evaluated, and the
    /// opportunity flows through the ordinary persist/alert/track pipeline with a
    /// measured detection latency.
    #[tokio::test]
    async fn a_streamed_delta_detects_only_the_affected_event_and_records_its_latency() {
        let rest = mock_api_with(STREAM_EVENTS, STREAM_BOOKS).await;
        let ws_url = mock_market_channel(Duration::from_millis(150)).await;
        let tmp = std::env::temp_dir().join(format!("polyarb-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let cfg = stream_test_config(rest, ws_url, &tmp);
        cfg.validate().expect("stream config must validate");
        run(cfg.clone(), Some(3)).await.expect("daemon run");

        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "only the streamed event may be re-evaluated; got {:?}",
            rows.iter()
                .map(|r| (r.event_slug.clone(), r.net_taker))
                .collect::<Vec<_>>()
        );
        let row = &rows[0];
        assert_eq!(
            row.event_slug, "mock-event-a",
            "event B's standing 5¢ gap must stay unseen — nothing touched it"
        );
        assert_eq!(row.kind, "binary_yes_no");
        assert_eq!(row.category, "politics");
        // asks 0.40 + 0.55 = 0.95 → gross 0.05
        // fee (politics 0.04): 0.04*0.40*0.60 + 0.04*0.55*0.45 = 0.0096 + 0.0099 = 0.0195
        // net_taker = 0.0305 ; size capped at 50 / 0.95 = 52.6315... shares
        assert_eq!(row.net_taker, dec!(0.0305));
        assert!(row.executable_size > dec!(52.6) && row.executable_size <= dec!(52.64));
        assert!(row.capital_required <= dec!(50));

        let latency = row
            .detection_latency_ms
            .expect("a stream-detected row must carry its latency");
        assert!(
            (0..60_000).contains(&latency),
            "implausible latency {latency} ms"
        );
        assert!(
            latency < 5_000,
            "streamed detection must beat the 5 s polling floor, got {latency} ms"
        );

        // The same number reaches the JSONL log and the daily summary.
        let log = std::fs::read_to_string(tmp.join("logs").join("events.jsonl")).expect("log");
        let logged: Vec<serde_json::Value> = log
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| v["event"] == "opportunity")
            .collect();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0]["detection_latency_ms"].as_i64(), Some(latency));

        let summary = emit_daily_summary(
            &cfg,
            &store,
            &Alerter::new(
                &cfg.alerts,
                Arc::new(EventLog::open(Path::new(&cfg.storage.log_dir)).expect("log")),
            ),
            Utc::now().date_naive(),
            false,
        )
        .await
        .expect("summary");
        assert_eq!(summary.stream_detected, 1);
        assert_eq!(summary.latency_p50_ms, Some(latency));
        assert_eq!(summary.latency_p95_ms, Some(latency));
        assert!(summary
            .to_markdown()
            .contains("detected from streamed book updates: 1 of 1"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An unreachable market channel must degrade to the M3 polling daemon, not stop it.
    #[tokio::test]
    async fn an_unreachable_stream_falls_back_to_rest_polling() {
        let rest = mock_api().await; // MOCK_BOOKS: 0.40 + 0.55 = 0.95, a detectable gap
        let tmp = std::env::temp_dir().join(format!("polyarb-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        // Port 1 refuses connections immediately, so the failure streak is fast.
        let mut cfg = stream_test_config(rest, "ws://127.0.0.1:1/ws/market".into(), &tmp);
        cfg.stream.fallback_after_failures = 2;
        cfg.validate().expect("config must validate");
        run(cfg.clone(), Some(4)).await.expect("daemon run");

        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "REST polling must have carried on and found the gap"
        );
        assert_eq!(rows[0].kind, "binary_yes_no");
        assert_eq!(
            rows[0].detection_latency_ms, None,
            "a REST-detected row has no triggering frame to measure against"
        );
        let totals = store
            .scan_totals_for_day(Utc::now().date_naive())
            .expect("totals");
        assert!(
            totals.cycles >= 2,
            "the daemon must keep scanning after giving up on the stream (cycles={})",
            totals.cycles
        );
        assert_eq!(totals.errors, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// M6.1 issue 1, end to end: a fallback is not a life sentence. The endpoint is dead
    /// when the daemon starts (so REST carries detection), then comes back — and the slow
    /// re-probe has to notice and restore streaming. The proof is a row carrying a detection
    /// latency, which only the stream path can produce.
    #[tokio::test]
    async fn a_fallen_back_stream_is_restored_when_the_endpoint_answers_a_reprobe() {
        let rest = mock_api_with(STREAM_EVENTS, STREAM_BOOKS).await;
        // Reserve an address and let it go: connections are refused until we bind again.
        let addr = {
            let reserved = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            reserved.local_addr().expect("addr")
        };
        let tmp = std::env::temp_dir().join(format!("polyarb-reprobe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = stream_test_config(rest, format!("ws://{addr}/ws/market"), &tmp);
        cfg.stream.fallback_after_failures = 2;
        cfg.stream.reprobe_interval_secs = 1;
        cfg.validate().expect("config must validate");

        // The channel appears well after the hand-over to REST.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            let listener = tokio::net::TcpListener::bind(addr).await.expect("re-bind");
            serve_market_channel(listener, Duration::from_millis(100)).await;
        });

        run(cfg.clone(), Some(8)).await.expect("daemon run");

        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        // REST polling found event B's standing gap while the socket was dead…
        assert!(
            rows.iter().any(|r| r.event_slug == "mock-event-b"),
            "REST polling must have carried detection during the fallback; got {:?}",
            rows.iter()
                .map(|r| r.event_slug.clone())
                .collect::<Vec<_>>()
        );
        // …and the restored stream then found event A's pushed one.
        let streamed: Vec<&OpportunityRow> = rows
            .iter()
            .filter(|r| r.detection_latency_ms.is_some())
            .collect();
        assert_eq!(
            streamed.len(),
            1,
            "the re-probe must restore streaming and detect event A from a pushed delta; \
             rows were {:?}",
            rows.iter()
                .map(|r| (r.event_slug.clone(), r.detection_latency_ms))
                .collect::<Vec<_>>()
        );
        assert_eq!(streamed[0].event_slug, "mock-event-a");
        assert!(
            streamed[0].detection_latency_ms.unwrap_or(i64::MAX) < 5_000,
            "a restored stream must still beat the polling floor"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -- M6.1: the alert cooldown, through the real daemon ----------------------------

    /// Books whose YES ask ticks one notch better on the second call: different economics,
    /// so correctly a second measurement row — but only a 0.001/share improvement, which is
    /// under the 0.01 re-alert bar.
    const DRIFTED_BOOKS: &str = r#"[
        {"asset_id":"1001","bids":[{"price":"0.389","size":"500"}],"asks":[{"price":"0.399","size":"500"}]},
        {"asset_id":"1002","bids":[{"price":"0.54","size":"500"}],"asks":[{"price":"0.55","size":"500"}]}
    ]"#;

    /// Like [`mock_api_with`], but the first `/books` call gets `first` and every later one
    /// gets `rest` — enough to make the daemon see the same event at two different prices.
    async fn mock_api_drifting(
        events: &'static str,
        first: &'static str,
        rest: &'static str,
    ) -> String {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let served_first = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let served_first = served_first.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let body = if head.starts_with("GET /events") {
                        events
                    } else if served_first.swap(true, Ordering::SeqCst) {
                        rest
                    } else {
                        first
                    };
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
        format!("http://{addr}")
    }

    /// M6.1 issue 4, end to end. The row-level dedupe is unchanged — a one-tick ask move is
    /// different economics and gets its own row — but the *message* is held back, and the
    /// JSONL record says so rather than going missing.
    #[tokio::test]
    async fn a_re_detection_that_is_no_better_is_logged_as_cooldown_not_alerted_again() {
        let base = mock_api_drifting(MOCK_EVENTS, MOCK_BOOKS, DRIFTED_BOOKS).await;
        let tmp = std::env::temp_dir().join(format!("polyarb-cooldown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let mut cfg = Config::default();
        cfg.api.gamma_base_url = base.clone();
        cfg.api.clob_base_url = base;
        cfg.api.min_request_interval_ms = 0;
        cfg.api.max_retries = 0;
        cfg.daemon.scan_interval_secs = 1;
        cfg.lifecycle.repoll_interval_secs = 1;
        cfg.lifecycle.repoll_window_secs = 1;
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();
        cfg.storage.database_path = tmp.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = tmp.join("logs").display().to_string();
        cfg.storage.report_dir = tmp.join("reports").display().to_string();
        cfg.stream.enabled = false;
        cfg.validate().expect("config must validate");

        run(cfg.clone(), Some(2)).await.expect("daemon run");

        // Two distinct rows: the economics really did change.
        let store = Store::open(Path::new(&cfg.storage.database_path)).expect("reopen db");
        let rows = store
            .opportunities_for_day(Utc::now().date_naive())
            .expect("rows");
        assert_eq!(
            rows.len(),
            2,
            "different quotes are different economics and must still each get a row"
        );
        // asks 0.40 + 0.55 → net 0.0305 ; asks 0.399 + 0.55 → net 0.03150804
        // improvement 0.00100804 per share, far under the 0.01 re-alert bar
        let mut nets: Vec<Decimal> = rows.iter().map(|r| r.net_taker).collect();
        nets.sort();
        assert_eq!(nets, vec![dec!(0.0305), dec!(0.03150804)]);

        // But only the first one was a notification; the second is on record as suppressed.
        let log = std::fs::read_to_string(tmp.join("logs").join("events.jsonl")).expect("log");
        let deliveries: Vec<String> = log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] == "opportunity")
            .map(|v| v["delivery"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            deliveries,
            vec!["no_credentials".to_string(), "cooldown".to_string()],
            "the second sighting must be logged, and logged as held back"
        );

        let summary = emit_daily_summary(
            &cfg,
            &store,
            &Alerter::new(
                &cfg.alerts,
                Arc::new(EventLog::open(Path::new(&cfg.storage.log_dir)).expect("log")),
            ),
            Utc::now().date_naive(),
            false,
        )
        .await
        .expect("summary");
        assert!(summary
            .to_markdown()
            .contains("held back by the per-event cooldown"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
