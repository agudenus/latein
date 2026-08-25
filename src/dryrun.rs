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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
// The watchdog's clock, deliberately tokio's: it follows `tokio::time::pause()`, so the
// stall tests run in microseconds instead of minutes.
use tokio::time::Instant as TokioInstant;

use crate::alert::{format_opportunity, AlertStats, Alerter, Delivery, EventLog, Priority};
use crate::clob::ClobClient;
use crate::config::{Config, REPORTED_CATEGORIES};
use crate::costs::FeeModel;
use crate::detect;
use crate::gamma::{GammaClient, PaginationState};
use crate::http::HttpClient;
use crate::makersim::{MakerSimulator, NotOpened, Opened};
use crate::nearres::{self, NearResObserver, NearResSummary};
use crate::rewardsim::{
    self, CandidateScore, ClosedEpochSummary, RewardCandidate, RewardKillLine, RewardParams,
    RewardSimulator, RewardsSummary,
};
use crate::store::{
    CycleStats, LifecycleOutcome, LifecycleStatus, MakerSimRow, OpportunityRow, RuntimeStatus,
    ScanTotals, Store,
};
use crate::types::{BookMap, Opportunity, Side, TokenId, Universe};
use crate::ws::{
    self, BookStore, DirtyBatch, PrintFanout, PrintObserver, StreamHealth, StreamManager,
    StreamStatsSnapshot, DIVERGENCE_SAMPLE,
};

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

    // M8. Created here, before anything else, so the simulator's `Arc` is the same one the
    // heartbeat reads and the stream pool feeds — there is exactly one of these per process.
    let maker_sim = Arc::new(MakerSimulator::new(&cfg.maker_sim));
    if maker_sim.enabled() {
        tracing::info!(
            window_secs = cfg.maker_sim.window_secs,
            max_concurrent = cfg.maker_sim.max_concurrent,
            "maker-fill simulation on: maker-only opportunities are simulated as resting \
             orders LAST in their queue, from trade prints only. No order is placed."
        );
    }

    // Measurement Phase R. Both instruments are created before anything else so their `Arc`s
    // are the ones the stream pool feeds and the loop samples — one of each per process.
    let rewardsim = Arc::new(RewardSimulator::new(&cfg.rewardsim));
    if rewardsim.enabled() {
        tracing::info!(
            capital_cap_usd = %cfg.rewardsim.capital_cap_usd,
            quote_spread_cents = %cfg.rewardsim.quote_spread_cents,
            quote_size_shares = %cfg.rewardsim.quote_size_shares,
            sample_interval_secs = cfg.rewardsim.sample_interval_secs,
            kill_line_net_usd_per_day = %cfg.rewardsim.kill_line_net_usd_per_day,
            "R1 rewards farming simulation on: a two-sided quote is SCORED and marked out \
             against the tape on each selected market. No quote is posted and no order is \
             ever placed."
        );
    }
    let nearres_observer = Arc::new(NearResObserver::new(&cfg.nearres));
    if nearres_observer.enabled() {
        // A restart must not re-qualify a market that already has an observation row (the
        // insert-only schema would refuse it anyway) nor lose the ones still in flight.
        match (
            store.nearres_resolved_tokens(),
            store.nearres_open_observations(),
        ) {
            (Ok(resolved), Ok(open)) => {
                tracing::info!(
                    resolved = resolved.len(),
                    still_open = open.len(),
                    min_ask = %cfg.nearres.min_ask,
                    max_ask = %cfg.nearres.max_ask,
                    max_hours_to_resolution = cfg.nearres.max_hours_to_resolution,
                    "R2 near-resolution observation on (pure observation — nothing about our \
                     own orders)"
                );
                nearres_observer.reseed(resolved, open);
            }
            (Err(err), _) | (_, Err(err)) => tracing::warn!(
                %err,
                "could not re-seed the near-resolution study from the database — markets \
                 already observed will be skipped by the insert-only schema, not re-measured"
            ),
        }
    }

    let hb = Arc::new(Heartbeat::new(
        cfg.clone(),
        store.clone(),
        alerter.clone(),
        maker_sim.clone(),
        rewardsim.clone(),
        DaemonStatus {
            started_at: Utc::now(),
            // Until the first REST call proves otherwise, assume nothing: `rest_ok` starts
            // true so a daemon that has not yet made a request is not reported as blind.
            rest_ok: true,
            ..DaemonStatus::default()
        },
    ));
    // Publish once before anything slow starts, so a daemon that spends its first minutes
    // in discovery is visibly *starting*, not missing.
    hb.publish(Utc::now());
    let heartbeat = tokio::spawn(heartbeat_task(hb.clone(), shutdown_rx.clone()));

    // The watchdog is the other half of the same fix: the heartbeat keeps the dashboard
    // honest about a slow loop, this ends a stuck one. Docker's restart policy does the
    // reviving — the daemon's job is to stop lying about being alive.
    let watchdog = (cfg.daemon.watchdog_stall_secs > 0).then(|| {
        let hb = hb.clone();
        let stall = Duration::from_secs(cfg.daemon.watchdog_stall_secs);
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Some(report) = watchdog_task(hb, stall, shutdown).await {
                tracing::error!(
                    phase = report.phase,
                    stalled_for_secs = report.stalled_for_secs,
                    ticks_completed = report.ticks_completed,
                    last_progress_at = report
                        .last_progress_at
                        .map(crate::store::now_str)
                        .unwrap_or_else(|| "never".into()),
                    watchdog_stall_secs = stall.as_secs(),
                    exit_code = WATCHDOG_EXIT_CODE,
                    "scan loop made no progress within the watchdog threshold — exiting so a \
                     restart policy can revive a clean process (set daemon.watchdog_stall_secs \
                     = 0 to disable)"
                );
                std::process::exit(WATCHDOG_EXIT_CODE);
            }
        })
    });

    let mut daemon = Daemon {
        cfg: cfg.clone(),
        http,
        store,
        alerter,
        slots: Arc::new(Semaphore::new(cfg.lifecycle.max_concurrent)),
        shutdown: shutdown_rx,
        trackers: JoinSet::new(),
        last_summary_day: None,
        hb,
        maker_sim,
        rewardsim,
        nearres: nearres_observer,
        reward_selected_day: None,
        last_nearres_lookup: None,
        token_events: HashMap::new(),
        gamma: Arc::new(PaginationState::default()),
    };
    let result = daemon.main_loop(max_cycles).await;
    heartbeat.abort();
    if let Some(watchdog) = watchdog {
        watchdog.abort();
    }
    result
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

/// How often the heartbeat task republishes [`RuntimeStatus`] to SQLite (M7).
///
/// One tiny upsert; at 5 s it is cheaper than a single scan cycle's own telemetry row and
/// keeps the dashboard's "is it alive" question answerable without the dashboard ever
/// touching the daemon. The dashboard treats a status older than
/// [`RUNTIME_STATUS_STALE_SECS`] as "the daemon is not reporting".
const STATUS_PUBLISH_SECS: u64 = 5;

/// Exit code the progress watchdog uses when it gives up on a wedged scan loop (M7.1).
///
/// Distinct and non-zero so a restart policy, a supervisor log or a human can tell a
/// watchdog restart from a crash, a clean shutdown or an OOM kill. 75 is `EX_TEMPFAIL`:
/// "the failure is temporary, try again", which is exactly what it means here.
pub const WATCHDOG_EXIT_CODE: i32 = 75;

/// Loop phases, as published in `runtime_status.last_progress_phase` and named by the
/// watchdog's ERROR line. Labels only — nothing branches on them.
const PHASE_STARTUP: &str = "startup";
const PHASE_DISCOVERY: &str = "discovery";
const PHASE_SEED_BOOKS: &str = "seed_books";
const PHASE_REST_SCAN: &str = "rest_scan";
const PHASE_REST_SWEEP: &str = "rest_sweep";
const PHASE_RESYNC: &str = "resync_stale";
const PHASE_STREAM_DETECT: &str = "stream_detect";
const PHASE_REWARD_SELECT: &str = "reward_select";
const PHASE_REWARD_SAMPLE: &str = "reward_sample";
const PHASE_TICK: &str = "tick";

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

/// Where the scan loop was when it last finished a unit of work (M7.1).
///
/// The point of the record is what it does **not** wait for: it is written when a piece of
/// work *completes*, not when a tick begins, so a loop that is mid-sweep, mid-retry or
/// mid-backoff is described honestly by whoever reads it. `last_at` is monotonic (the
/// watchdog's clock); `last_wall` is its wall-clock twin, because the dashboard renders an
/// age and cannot read another process's `Instant`.
#[derive(Debug, Clone, Copy)]
struct LoopProgress {
    /// `None` until the first unit of work completes. That is also what keeps the watchdog
    /// unarmed through the long first discovery-and-seed.
    last_at: Option<TokioInstant>,
    last_wall: Option<chrono::DateTime<Utc>>,
    phase: &'static str,
    /// Completed full loop iterations.
    ticks: u64,
    last_tick_wall: Option<chrono::DateTime<Utc>>,
}

impl Default for LoopProgress {
    fn default() -> Self {
        Self {
            last_at: None,
            last_wall: None,
            phase: PHASE_STARTUP,
            ticks: 0,
            last_tick_wall: None,
        }
    }
}

/// The parts of a live [`StreamManager`] the heartbeat can read without touching the loop:
/// all shared handles, so the numbers are current even while the loop is blocked.
#[derive(Clone)]
struct StreamRefs {
    books: Arc<BookStore>,
    health: Arc<StreamHealth>,
    shards: usize,
}

fn stream_refs(manager: &StreamManager) -> StreamRefs {
    StreamRefs {
        books: manager.books().clone(),
        health: manager.health().clone(),
        shards: manager.shard_count(),
    }
}

/// The daemon's published state, shared with the heartbeat task (M7.1).
///
/// Before this existed, `runtime_status` was written *between* loop ticks. That was fine
/// while a tick was five seconds; at live scale a REST sweep takes 195 s, and during a DNS
/// outage retries stretched a tick to many minutes — so the dashboard reported
/// `runtime_status::Stale` ("the daemon stopped reporting") about a daemon that was alive
/// and working, the Docker healthcheck went unhealthy, and nothing restarted it.
///
/// The fix is a separation of concerns, not a faster loop: **the heartbeat says the process
/// is alive; [`LoopProgress`] says whether the loop is moving.** Both travel in the same
/// row, and neither can be starved by the other, because the heartbeat task owns its own
/// timer and reads only shared handles.
struct Heartbeat {
    cfg: Arc<Config>,
    store: Arc<Store>,
    alerter: Arc<Alerter>,
    /// M8. Read-only from here: the heartbeat publishes the simulator's live counts so the
    /// dashboard can show open simulations, which exist only in this process's memory.
    maker_sim: Arc<MakerSimulator>,
    /// R1, for the same reason: an in-flight epoch exists only in memory until it closes.
    rewardsim: Arc<RewardSimulator>,
    /// Slow-moving daemon state. A `std` mutex, held for a clone and never across an await.
    status: Mutex<DaemonStatus>,
    stream: Mutex<Option<StreamRefs>>,
    progress: Mutex<LoopProgress>,
}

impl Heartbeat {
    fn new(
        cfg: Arc<Config>,
        store: Arc<Store>,
        alerter: Arc<Alerter>,
        maker_sim: Arc<MakerSimulator>,
        rewardsim: Arc<RewardSimulator>,
        status: DaemonStatus,
    ) -> Self {
        Self {
            cfg,
            store,
            alerter,
            maker_sim,
            rewardsim,
            status: Mutex::new(status),
            stream: Mutex::new(None),
            progress: Mutex::new(LoopProgress::default()),
        }
    }

    /// A poisoned lock here means another task panicked mid-update. The state is still
    /// structurally sound and going silent is strictly worse than carrying on: publishing a
    /// slightly odd status beats publishing none, because "none" is read as "the daemon is
    /// gone".
    fn with<T, U>(mutex: &Mutex<T>, f: impl FnOnce(&mut T) -> U) -> U {
        let mut guard = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    fn update(&self, f: impl FnOnce(&mut DaemonStatus)) {
        Self::with(&self.status, f);
    }

    fn set_stream(&self, refs: Option<StreamRefs>) {
        Self::with(&self.stream, |slot| *slot = refs);
    }

    /// Record that the loop finished a unit of work. Cheap enough to call per book batch,
    /// which is the point: progress must be finer-grained than a tick, or a legitimate
    /// 195 s sweep is indistinguishable from a wedge.
    fn note(&self, phase: &'static str) {
        let now = Utc::now();
        Self::with(&self.progress, |p| {
            p.last_at = Some(TokioInstant::now());
            p.last_wall = Some(now);
            p.phase = phase;
        });
    }

    /// Record a completed loop iteration (which is also progress).
    fn note_tick(&self) {
        let now = Utc::now();
        Self::with(&self.progress, |p| {
            p.last_at = Some(TokioInstant::now());
            p.last_wall = Some(now);
            p.phase = PHASE_TICK;
            p.ticks = p.ticks.saturating_add(1);
            p.last_tick_wall = Some(now);
        });
    }

    fn progress(&self) -> LoopProgress {
        Self::with(&self.progress, |p| *p)
    }

    /// Write one [`RuntimeStatus`] row.
    ///
    /// Best-effort by design: a failed write is logged at debug and nothing else — losing a
    /// dashboard refresh must never disturb the daemon. Each lock is taken and released in
    /// turn (never nested), so this cannot deadlock against the loop.
    fn publish(&self, now: chrono::DateTime<Utc>) {
        let stream = Self::with(&self.stream, |s| s.clone());
        let status = Self::with(&self.status, |s| s.clone());
        let progress = self.progress();

        let stats = stream
            .as_ref()
            .map(|s| s.books.stats.snapshot())
            .unwrap_or_default();
        let alerts = self.alerter.stats();
        let sims = self.maker_sim.stats();
        let rewards = self.rewardsim.stats();
        let row = RuntimeStatus {
            mode: self.cfg.mode.clone(),
            started_at: crate::store::now_str(status.started_at),
            universe_events: status.universe_events,
            universe_markets: status.universe_markets,
            universe_tokens: status.universe_tokens,
            universe_refreshed_at: status.universe_refreshed_at.map(crate::store::now_str),
            universe_refresh_secs: self.cfg.daemon.universe_refresh_secs,
            scan_interval_secs: self.cfg.daemon.scan_interval_secs,
            last_good_discovery_at: status.last_good_discovery_at.map(crate::store::now_str),
            discovery_empty_streak: status.discovery_empty_streak,
            discovery_error: status.discovery_error.clone(),
            stream_enabled: self.cfg.stream.enabled,
            stream_shards: stream.as_ref().map(|s| s.shards as i64).unwrap_or(0),
            stream_shards_connected: stream
                .as_ref()
                .map(|s| i64::from(s.health.live_connections()))
                .unwrap_or(0),
            stream_rest_only: status.stream_rest_only,
            books_total: stream.as_ref().map(|s| s.books.len() as i64).unwrap_or(0),
            books_stale: stream
                .as_ref()
                .map(|s| s.books.stale_count() as i64)
                .unwrap_or(0),
            frames: stats.messages,
            delta_entries_applied: stats.delta_entries_applied,
            frames_unrecognized: stats.frames_unrecognized,
            trade_prints: stats.trade_prints,
            last_progress_at: progress.last_wall.map(crate::store::now_str),
            last_progress_phase: progress.phase.to_string(),
            ticks_completed: progress.ticks,
            last_tick_completed_at: progress.last_tick_wall.map(crate::store::now_str),
            watchdog_stall_secs: self.cfg.daemon.watchdog_stall_secs,
            rest_ok: status.rest_ok,
            rest_failure_streak: status.rest_failure_streak,
            fee_fallback_markets: status.fee_fallback_markets,
            fee_unsupported_formula: status.fee_unsupported_formula,
            markets_with_api_fee: status.markets_with_api_fee,
            telegram_circuit_open: alerts.circuit_open,
            alerts_sent: alerts.sent,
            alerts_failed: alerts.failed,
            alerts_cooldown_held: alerts.cooldown_suppressed,
            maker_sim_enabled: self.maker_sim.enabled(),
            maker_sim_window_secs: self.maker_sim.window_secs(),
            maker_sims_open: sims.open_now as i64,
            maker_sims_opened: sims.opened,
            maker_sims_filled: sims.filled,
            maker_sims_partial: sims.partial,
            maker_sims_unfilled: sims.unfilled,
            maker_sims_untracked: sims.untracked,
            maker_sim_prints_matched: sims.prints_matched,
            maker_sim_print_feed_live: self.maker_sim.print_feed_live(),
            rewardsim_enabled: self.rewardsim.enabled(),
            rewardsim_markets_quoted: rewards.markets_quoted,
            rewardsim_portfolio: self.rewardsim.portfolio_len() as i64,
            rewardsim_samples_scored: rewards.samples_scored,
            rewardsim_samples_lost: rewards.samples_lost,
            rewardsim_fills: rewards.fills,
            rewardsim_epochs_closed: rewards.epochs_closed,
            rewardsim_print_feed_live: rewards.print_feed_live,
            net_floor_default_taker: self
                .cfg
                .net_floor_taker(&crate::types::Category::new("default"))
                .to_string(),
        };
        if let Err(err) = self.store.write_runtime_status(&row, now) {
            tracing::debug!(%err, "could not publish the runtime status row");
        }
    }
}

/// Republish `runtime_status` on its own timer, whatever the scan loop is doing.
///
/// This task touches no scan state and takes no lock the loop can hold for long, so a loop
/// stuck in a 195 s sweep — or in a DNS retry storm — cannot starve it. That is the whole
/// point: from here on, a stale status row means the *process* is gone, and nothing else.
async fn heartbeat_task(hb: Arc<Heartbeat>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(STATUS_PUBLISH_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = ticker.tick() => hb.publish(Utc::now()),
        }
    }
}

/// What the watchdog found when it gave up: the loop's last known position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallReport {
    pub phase: &'static str,
    pub stalled_for_secs: u64,
    pub ticks_completed: u64,
    pub last_progress_at: Option<chrono::DateTime<Utc>>,
}

/// Has the loop stopped making progress? Pure, so every case is a unit test.
///
/// `None` — the honest answer — in two situations that are *not* a wedge:
///
/// * the loop is progressing, however slowly (a book batch counts);
/// * the loop has never completed a unit of work yet. At the live universe scale the first
///   discovery-and-seed is minutes of legitimate work, and killing a daemon for being slow
///   to start is a restart loop, not a recovery.
fn stall_verdict(
    progress: &LoopProgress,
    now: TokioInstant,
    stall_after: Duration,
) -> Option<StallReport> {
    let last = progress.last_at?;
    let stalled_for = now.saturating_duration_since(last);
    (stalled_for >= stall_after).then_some(StallReport {
        phase: progress.phase,
        stalled_for_secs: stalled_for.as_secs(),
        ticks_completed: progress.ticks,
        last_progress_at: progress.last_wall,
    })
}

/// Poll [`stall_verdict`] until it trips or we shut down.
///
/// Returns the report rather than acting on it, so the decision is testable and the
/// process-ending part lives at exactly one call site (see [`run`]).
async fn watchdog_task(
    hb: Arc<Heartbeat>,
    stall_after: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Option<StallReport> {
    // Check often enough to react promptly, rarely enough to be free.
    let every = (stall_after / 10).clamp(Duration::from_millis(50), Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = shutdown.changed() => return None,
            _ = tokio::time::sleep(every) => {}
        }
        if *shutdown.borrow() {
            return None;
        }
        if let Some(report) = stall_verdict(&hb.progress(), TokioInstant::now(), stall_after) {
            return Some(report);
        }
    }
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
    /// M7/M7.1: the shared state a separate task republishes to `runtime_status`.
    hb: Arc<Heartbeat>,
    /// M8: the maker-fill simulator. The daemon opens simulations and persists closed ones;
    /// the stream's socket tasks feed it prints through [`ws::PrintObserver`].
    maker_sim: Arc<MakerSimulator>,
    /// R1: the rewards farming simulator. Same shape as the maker simulator — the loop
    /// samples and persists, the socket tasks feed it prints through the same hook.
    rewardsim: Arc<RewardSimulator>,
    /// R2: the near-resolution observer. No print feed and no timers of its own; it qualifies
    /// markets from the books a scan already fetched.
    nearres: Arc<NearResObserver>,
    /// The UTC day the reward portfolio was last chosen for. Selection is a daily decision,
    /// which is also the epoch's period.
    reward_selected_day: Option<NaiveDate>,
    last_nearres_lookup: Option<Instant>,
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
        // From here the dashboard can tell a running daemon from a stopped one — by the
        // row's age for the process, and by `last_progress_at` for the loop.
        self.transport_changed(stream.as_ref());

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
                        self.hb.update(|s| s.stream_rest_only = true);
                        self.transport_changed(None);
                        continue;
                    }
                },
            };
            if *self.shutdown.borrow() {
                break;
            }

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
                        // A simulation whose book left the universe can never be resolved by
                        // a print again: close it where it stands (M8).
                        if self.maker_sim.enabled() {
                            let live: std::collections::HashSet<TokenId> =
                                universe.token_ids().into_iter().collect();
                            self.maker_sim.retain_tokens(&live, Utc::now());
                        }
                        if let Some(manager) = stream.as_mut() {
                            manager.update_universe(&universe.token_ids());
                        }
                        // A re-plan can change the shard count, which the heartbeat reports.
                        self.transport_changed(stream.as_ref());
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
                    // A pool with no live connection is not a print feed, whatever the
                    // transport pill says: no socket, no prints, no evidence (M8).
                    self.set_print_feed(manager.health().live_connections() > 0);
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
                self.hb.update(|s| s.stream_rest_only = true);
                self.transport_changed(None);
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
                                self.hb.update(|s| s.stream_rest_only = false);
                                self.transport_changed(stream.as_ref());
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
            // Close and persist whatever the maker simulator finished with this tick. Before
            // the summary, so a verdict reached in this tick is in the day it belongs to.
            self.maker_sim_tick(Utc::now()).await;
            // Measurement Phase R rides the same tick: R1 samples its portfolio on its own
            // cadence, R2 follows up the observations whose end date has passed.
            self.rewardsim_tick(&universe, stream.as_ref()).await;
            self.nearres_lookup().await;
            self.maybe_daily_summary(summary_time).await;
            // One full iteration behind us: the loop is not merely alive, it is round-tripping.
            self.hb.note_tick();

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
                trade_prints = stats.trade_prints,
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

        // Every simulation still open gets its verdict written from what the tape had shown
        // by now — usually `maker_unfilled`. Losing them silently would quietly delete the
        // denominator of the fill rate.
        if self.maker_sim.enabled() {
            self.maker_sim.set_print_feed(false);
            self.maker_sim.close_all(Utc::now());
            self.persist_closed_sims().await;
            let sims = self.maker_sim.stats();
            tracing::info!(
                opened = sims.opened,
                filled = sims.filled,
                partial = sims.partial,
                unfilled = sims.unfilled,
                untracked = sims.untracked,
                prints_matched = sims.prints_matched,
                "maker-fill simulation stopping"
            );
        }

        // R1's open epochs get their verdict from what the day had shown by now — the same
        // discipline as the maker simulator, and for the same reason: losing them silently
        // would delete the denominator of the net figure.
        if self.rewardsim.enabled() {
            self.rewardsim.set_print_feed(false);
            self.rewardsim.close_all(Utc::now());
            self.persist_reward_rows().await;
            let rewards = self.rewardsim.stats();
            tracing::info!(
                markets_quoted = rewards.markets_quoted,
                samples_scored = rewards.samples_scored,
                samples_lost = rewards.samples_lost,
                fills = rewards.fills,
                epochs_closed = rewards.epochs_closed,
                "rewards farming simulation stopping"
            );
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
                    self.hb.publish(Utc::now());
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
        self.hb.update(|s| {
            s.rest_ok = ok;
            s.rest_failure_streak = if ok {
                0
            } else {
                s.rest_failure_streak.saturating_add(1)
            };
        });
    }

    /// Point the heartbeat at the current stream pool (or at none) and publish at once.
    ///
    /// Called on every transport state change, so the dashboard never waits out a heartbeat
    /// interval to learn about one. The periodic publication is the task's job.
    fn transport_changed(&self, stream: Option<&StreamManager>) {
        // M8. The simulator eats trade prints, and trade prints only exist on the socket. A
        // pool that is (re)built gets the observer installed on its fresh `BookStore`;
        // handing detection to REST turns the feed off, which flags every open simulation as
        // having a hole in its window rather than letting it close as an honest non-fill.
        match stream {
            Some(manager) => {
                manager
                    .books()
                    .set_print_observer(Some(self.print_readers()));
                self.set_print_feed(true);
            }
            None => self.set_print_feed(false),
        }
        self.hb.set_stream(stream.map(stream_refs));
        self.hb.publish(Utc::now());
    }

    /// The two readers of the trade tape, behind one `BookStore` hook.
    ///
    /// M8 asks whether the queue in front of a resting arbitrage leg traded out; R1 asks
    /// whether a print crossed the in-band quote we would have had resting. Both need every
    /// print, and the store holds a single observer slot — so the fanout is the observer.
    fn print_readers(&self) -> Arc<dyn PrintObserver> {
        Arc::new(PrintFanout::new(vec![
            self.maker_sim.clone(),
            self.rewardsim.clone(),
        ]))
    }

    /// Tell both print-driven measurements whether a feed is running. Turning it off marks
    /// their open windows as holes rather than letting them close as honest zeros.
    fn set_print_feed(&self, live: bool) {
        self.maker_sim.set_print_feed(live);
        self.rewardsim.set_print_feed(live);
    }

    /// Close whatever the maker simulator has finished with, and persist it.
    ///
    /// Called from the loop, never from a socket task: prints are applied where they arrive
    /// (cheaply, under one mutex), and the SQLite writes their verdicts imply happen here.
    async fn maker_sim_tick(&mut self, now: chrono::DateTime<Utc>) {
        if !self.maker_sim.enabled() {
            return;
        }
        self.maker_sim.expire(now);
        self.persist_closed_sims().await;
    }

    async fn persist_closed_sims(&mut self) {
        for sim in self.maker_sim.drain_closed() {
            match self.store.record_maker_sim(&sim) {
                Ok(id) => tracing::info!(
                    maker_sim_id = id,
                    opportunity_id = sim.opportunity_id,
                    status = sim.status.as_str(),
                    legs = format!("{}/{}", sim.legs_filled, sim.legs_total),
                    prints_observed = sim.prints_observed,
                    pnl_lower_bound = %sim.pnl_lower_bound.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
                    no_print_feed = sim.no_print_feed,
                    "maker-fill simulation closed (simulated — no order was ever placed)"
                ),
                Err(err) => tracing::warn!(
                    %err,
                    opportunity_id = sim.opportunity_id,
                    "could not persist a maker-fill simulation"
                ),
            }
            self.alerter.events().append(
                "maker_sim_close",
                json!({
                    "opportunity_id": sim.opportunity_id,
                    "status": sim.status.as_str(),
                    "legs_total": sim.legs_total,
                    "legs_filled": sim.legs_filled,
                    "net_maker_total": sim.net_maker_total,
                    "pnl_lower_bound": sim.pnl_lower_bound,
                    "legging_exposure": sim.legging_exposure,
                    "prints_observed": sim.prints_observed,
                    "time_to_fill_ms": sim.time_to_fill_ms,
                    "no_print_feed": sim.no_print_feed,
                    "event_slug": sim.event_slug,
                    "kind": sim.kind,
                    "category": sim.category,
                    "opened_at": crate::store::now_str(sim.opened_at),
                    "closed_at": crate::store::now_str(sim.closed_at),
                    "legs": sim.legs,
                }),
            );
        }
    }

    /// Start a maker-fill simulation for a newly detected maker-only opportunity.
    fn open_maker_sim(
        &self,
        id: i64,
        op: &Opportunity,
        books: &BookMap,
        now: chrono::DateTime<Utc>,
    ) {
        match self.maker_sim.open(id, op, books, now) {
            Opened::Yes { legs, evicted } => {
                if let Some(evicted) = &evicted {
                    tracing::warn!(
                        opportunity_id = evicted.opportunity_id,
                        limit = self.cfg.maker_sim.max_concurrent,
                        "maker-sim capacity reached — evicting the oldest simulation as \
                         untracked (a missing measurement, not a non-fill)"
                    );
                }
                self.alerter.events().append(
                    "maker_sim_open",
                    json!({
                        "opportunity_id": id,
                        "legs": legs,
                        "window_secs": self.cfg.maker_sim.window_secs,
                        "net_maker_total": op.net_maker_total,
                        "event_slug": op.event_slug,
                        "kind": op.kind.as_str(),
                        "category": op.category.as_str(),
                        "print_feed_live": self.maker_sim.print_feed_live(),
                    }),
                );
            }
            // Nothing here is an error; each is a reason a construction is not the kind of
            // thing this measurement applies to. `NoVisibleLevel` is the one worth a line:
            // it means the book moved between detection and this call.
            Opened::No(NotOpened::NoVisibleLevel) => tracing::debug!(
                opportunity_id = id,
                "no maker simulation: the bid level we would have joined is not in the book \
                 we detected on"
            ),
            Opened::No(_) => {}
        }
    }

    // -----------------------------------------------------------------------------
    // R1 — rewards farming simulation
    // -----------------------------------------------------------------------------

    /// One tick of the rewards simulator: roll the epoch if the UTC day turned, re-select the
    /// portfolio if it is due, take a scoring sample if one is due, and persist whatever
    /// closed.
    async fn rewardsim_tick(&mut self, universe: &Universe, stream: Option<&StreamManager>) {
        if !self.rewardsim.enabled() {
            return;
        }
        let now = Utc::now();
        // The epoch is the venue's: one UTC day, closing at 00:00. Rolling it is what makes
        // a day's row final.
        let rolled = self.rewardsim.roll_epoch(now);
        if rolled {
            self.persist_reward_rows().await;
        }
        if rolled || self.reward_selected_day != Some(now.date_naive()) {
            self.reselect_reward_portfolio(universe, now).await;
        }
        if self.rewardsim.sample_due(now) {
            let tokens = self.rewardsim.portfolio_tokens();
            if !tokens.is_empty() {
                if let Some(books) = self.books_for(&tokens, stream, PHASE_REWARD_SAMPLE).await {
                    let scored = self.rewardsim.sample(&books, now);
                    tracing::debug!(
                        markets = self.rewardsim.portfolio_len(),
                        scored,
                        books = books.len(),
                        "rewards sample taken (simulated — no quote is posted)"
                    );
                }
            }
        }
        self.persist_reward_rows().await;
    }

    /// Books for a handful of tokens: the stream's own state when it has all of them,
    /// otherwise one REST batch. The reward portfolio is ~40 tokens, so this is cheap either
    /// way — and a *partial* stream snapshot is not used, because a missing book would read
    /// as a lost sample when the data was simply somewhere else.
    async fn books_for(
        &self,
        tokens: &[TokenId],
        stream: Option<&StreamManager>,
        phase: &'static str,
    ) -> Option<BookMap> {
        if let Some(manager) = stream {
            let snapshot = manager.books().snapshot_of(tokens);
            if snapshot.len() == tokens.len() {
                return Some(snapshot);
            }
        }
        match self.fetch_books_noting_progress(tokens, phase).await {
            Ok(books) => Some(books),
            Err(err) => {
                tracing::warn!(%err, tokens = tokens.len(), phase, "book fetch failed");
                None
            }
        }
    }

    /// Choose the day's reward portfolio and log it.
    ///
    /// Recomputed daily, which is also the epoch's period: a market that stays in the
    /// portfolio keeps its epoch (re-selection is not a new day), and one that drops out has
    /// its epoch closed where it stands.
    async fn reselect_reward_portfolio(&mut self, universe: &Universe, now: chrono::DateTime<Utc>) {
        let candidates = self.reward_candidates(universe).await;
        if candidates.is_empty() {
            tracing::warn!(
                events = universe.events.len(),
                "no reward-eligible candidates found — the rewards simulator has nothing to \
                 quote today (check the CLOB rewards endpoint and Gamma's clobRewards field)"
            );
            self.reward_selected_day = Some(now.date_naive());
            return;
        }
        let tokens: Vec<TokenId> = candidates
            .iter()
            .flat_map(|c| [c.yes_token.clone(), c.no_token.clone()])
            .collect();
        let Some(books) = self.books_for(&tokens, None, PHASE_REWARD_SELECT).await else {
            // No books, no honest ranking. Keep yesterday's portfolio rather than choosing
            // one from stale prices, and try again on the next tick.
            return;
        };
        let cfg = self.rewardsim.config().clone();
        let mut scores: Vec<CandidateScore> = candidates
            .iter()
            .filter_map(|c| {
                rewardsim::evaluate_candidate(
                    c,
                    &books,
                    &cfg,
                    self.rewardsim.markout_history(&c.condition_id),
                )
            })
            .collect();
        let chosen = rewardsim::select_portfolio(&mut scores, &cfg);

        let capital: Decimal = chosen.iter().map(|s| s.quote.capital).sum();
        let expected_gross: Decimal = chosen.iter().map(|s| s.expected_gross_paid).sum();
        let expected_net: Decimal = chosen.iter().map(|s| s.expected_net).sum();
        tracing::info!(
            day = %now.date_naive(),
            candidates = candidates.len(),
            scored = scores.len(),
            chosen = chosen.len(),
            capital_committed = %usd(capital),
            capital_cap = %cfg.capital_cap_usd,
            expected_gross_per_day = %usd(expected_gross),
            expected_net_per_day = %usd(expected_net),
            "rewards portfolio chosen for the day (simulated)"
        );
        for score in &chosen {
            tracing::info!(
                event = %score.candidate.event_slug,
                condition = %score.candidate.condition_id,
                source = score.candidate.source,
                pool_daily = %score.candidate.params.pool_daily,
                max_spread_cents = %score.candidate.params.max_spread_cents,
                min_size = %score.candidate.params.min_size,
                quote_cents = %score.quote.s_cents,
                shares = %score.quote.shares,
                capital = %usd(score.quote.capital),
                share_of_pool = %score.share.round_dp(4),
                expected_gross_per_day = %usd(score.expected_gross_paid),
                expected_net_per_day = %usd(score.expected_net),
                "rewards portfolio market"
            );
        }
        self.alerter.events().append(
            "rewards_portfolio",
            json!({
                "day": now.date_naive().to_string(),
                "candidates": candidates.len(),
                "chosen": chosen.len(),
                "capital_committed": capital,
                "capital_cap": cfg.capital_cap_usd,
                "expected_gross_per_day": expected_gross,
                "expected_net_per_day": expected_net,
                "markets": chosen.iter().map(|s| json!({
                    "condition_id": s.candidate.condition_id,
                    "event_slug": s.candidate.event_slug,
                    "source": s.candidate.source,
                    "pool_daily": s.candidate.params.pool_daily,
                    "max_spread_cents": s.candidate.params.max_spread_cents,
                    "min_size": s.candidate.params.min_size,
                    "quote_spread_cents": s.quote.s_cents,
                    "shares": s.quote.shares,
                    "capital": s.quote.capital,
                    "mid": s.mid,
                    "competing_score": s.competing_score,
                    "share": s.share,
                    "expected_gross_per_day": s.expected_gross_paid,
                    "expected_net_per_day": s.expected_net,
                    "markout_history": s.markout_history,
                })).collect::<Vec<_>>(),
            }),
        );

        self.rewardsim.set_portfolio(&chosen, now);
        self.reward_selected_day = Some(now.date_naive());
        // Markets that just left the portfolio closed their epochs inside `set_portfolio`.
        self.persist_reward_rows().await;
    }

    /// The reward-eligible candidate set: the CLOB rewards endpoint where it answers, the
    /// per-market Gamma fields where it does not.
    ///
    /// Both sources are recorded per candidate (`source`), because they can disagree and a
    /// number whose provenance is unknown is not evidence.
    async fn reward_candidates(&mut self, universe: &Universe) -> Vec<RewardCandidate> {
        let max_pages = self.cfg.rewardsim.max_candidate_pages;
        let fetched = {
            let clob = ClobClient::new(&self.http, &self.cfg);
            clob.fetch_reward_markets(max_pages).await
        };
        let reward_markets = match fetched {
            Ok(markets) => {
                self.note_rest(true);
                markets
            }
            Err(err) => {
                self.note_rest(false);
                // TODO(verify-live): this endpoint has never been reached from a dev
                // container. On the Pi it either works or this line names it every day.
                tracing::warn!(
                    %err,
                    "the CLOB /sampling-markets endpoint did not answer — falling back to \
                     Gamma's per-market rewards fields for the candidate set"
                );
                Vec::new()
            }
        };
        let by_condition: HashMap<&str, &crate::clob::RewardMarket> = reward_markets
            .iter()
            .map(|m| (m.condition_id.as_str(), m))
            .collect();

        let mut out: Vec<RewardCandidate> = Vec::new();
        for event in &universe.events {
            for market in &event.markets {
                let (params, source) = match by_condition.get(market.condition_id.as_str()) {
                    Some(rm) => (
                        reward_params(rm.max_spread_cents, rm.min_size, rm.daily_rate),
                        "sampling-markets",
                    ),
                    None => (
                        reward_params(
                            market.trading.rewards_max_spread,
                            market.trading.rewards_min_size,
                            market.trading.rewards_daily_rate,
                        ),
                        "gamma",
                    ),
                };
                let Some(params) = params else { continue };
                out.push(RewardCandidate {
                    condition_id: market.condition_id.clone(),
                    event_slug: event.slug.clone(),
                    question: market.question.clone(),
                    category: event.category.as_str().to_string(),
                    yes_token: market.yes_token().clone(),
                    no_token: market.no_token().clone(),
                    params,
                    source,
                });
            }
        }
        // The pool is the first-order term in the ranking, so a truncated candidate set
        // should keep the fattest pools rather than an arbitrary slice.
        out.sort_by(|a, b| {
            b.params
                .pool_daily
                .cmp(&a.params.pool_daily)
                .then_with(|| a.condition_id.cmp(&b.condition_id))
        });
        out.truncate(self.cfg.rewardsim.max_candidates);
        out
    }

    /// Persist closed epochs and matured fills.
    async fn persist_reward_rows(&mut self) {
        for epoch in self.rewardsim.drain_closed() {
            match self.store.record_reward_epoch(&epoch) {
                Ok(true) => tracing::info!(
                    day = %epoch.day,
                    event = %epoch.event_slug,
                    condition = %epoch.condition_id,
                    samples = format!("{}/{}", epoch.samples_scored, epoch.samples_expected),
                    samples_lost = epoch.samples_lost,
                    gross = %usd(epoch.gross_usd),
                    gross_paid = %usd(epoch.gross_paid_usd),
                    fills = epoch.fills,
                    markout_short = %usd(epoch.markout_short_usd),
                    markout_long = %usd(epoch.markout_long_usd),
                    fills_pending = epoch.fills_pending,
                    net = %usd(epoch.net_usd),
                    no_print_feed = epoch.no_print_feed,
                    "rewards epoch closed (simulated — no quote was ever posted)"
                ),
                Ok(false) => tracing::debug!(
                    day = %epoch.day,
                    condition = %epoch.condition_id,
                    "a reward epoch for this market and day is already recorded — the schema \
                     refused the second verdict, which is what it is for"
                ),
                Err(err) => tracing::warn!(%err, "could not persist a reward epoch"),
            }
            self.alerter.events().append(
                "rewards_epoch",
                json!({
                    "day": epoch.day.to_string(),
                    "condition_id": epoch.condition_id,
                    "event_slug": epoch.event_slug,
                    "pool_daily": epoch.params.pool_daily,
                    "capital": epoch.capital,
                    "samples_scored": epoch.samples_scored,
                    "samples_lost": epoch.samples_lost,
                    "gross_usd": epoch.gross_usd,
                    "gross_paid_usd": epoch.gross_paid_usd,
                    "fills": epoch.fills,
                    "markout_short_usd": epoch.markout_short_usd,
                    "markout_long_usd": epoch.markout_long_usd,
                    "fills_pending": epoch.fills_pending,
                    "net_usd": epoch.net_usd,
                    "no_print_feed": epoch.no_print_feed,
                }),
            );
        }
        let now = Utc::now();
        for fill in self.rewardsim.drain_fills() {
            if let Err(err) = self.store.record_reward_fill(&fill, now) {
                tracing::warn!(%err, "could not persist a simulated reward fill");
            }
        }
    }

    // -----------------------------------------------------------------------------
    // R2 — near-resolution observation
    // -----------------------------------------------------------------------------

    /// Qualify markets from the books a scan has just fetched. Pure observation: an entry is
    /// recorded once, at the first moment its executable ask was in the band.
    fn nearres_scan(&mut self, universe: &Universe, books: &BookMap) {
        if !self.nearres.enabled() {
            return;
        }
        let fees = FeeModel::new(self.cfg.fees.clone());
        let (qualified, counters) = self.nearres.qualify(universe, books, &fees, Utc::now());
        if qualified.is_empty() {
            return;
        }
        for q in &qualified {
            match self.store.record_nearres_observation(q) {
                Ok(true) => {}
                Ok(false) => tracing::debug!(
                    token = %q.token_id,
                    "this token already has an observation row — a market qualifies once"
                ),
                Err(err) => tracing::warn!(%err, "could not persist a near-resolution observation"),
            }
            self.alerter.events().append(
                "nearres_qualified",
                json!({
                    "token_id": q.token_id.as_str(),
                    "condition_id": q.condition_id,
                    "event_slug": q.event_slug,
                    "question": q.question,
                    "outcome": q.outcome,
                    "category": q.category,
                    "ask": q.ask,
                    "ask_size": q.ask_size,
                    "ask_depth": q.ask_depth,
                    "best_bid": q.best_bid,
                    "fee_per_share": q.fee_per_share,
                    "net_if_won": q.net_if_won(),
                    "hours_to_resolution": q.hours_to_resolution,
                    "end_date": crate::store::now_str(q.end_date),
                }),
            );
        }
        tracing::info!(
            qualified = counters.qualified,
            examined = counters.tokens_examined,
            outside_band = counters.outside_price_band,
            no_book = counters.no_book,
            too_far_out = counters.too_far_out,
            tracking = self.nearres.open_count(),
            "near-resolution observations recorded (observation only — no order implied)"
        );
    }

    /// Follow the observations whose end date has passed to their resolution.
    async fn nearres_lookup(&mut self) {
        if !self.nearres.enabled() {
            return;
        }
        let interval = Duration::from_secs(self.cfg.nearres.resolution_poll_secs.max(1));
        if self
            .last_nearres_lookup
            .is_some_and(|last| last.elapsed() < interval)
        {
            return;
        }
        let now = Utc::now();
        // A batch per poll: the study is measured in days, so there is no reason to hammer.
        let due = self
            .nearres
            .due_for_lookup(now, self.cfg.nearres.resolution_batch_size);
        self.last_nearres_lookup = Some(Instant::now());
        if due.is_empty() {
            return;
        }
        let batch_size = self.cfg.nearres.resolution_batch_size;
        let fetched = {
            let gamma = GammaClient::with_state(&self.http, &self.cfg, self.gamma.clone());
            gamma.fetch_markets_by_condition(&due, batch_size).await
        };
        let markets = match fetched {
            Ok(markets) => {
                self.note_rest(true);
                markets
            }
            Err(err) => {
                self.note_rest(false);
                tracing::warn!(
                    %err,
                    conditions = due.len(),
                    "near-resolution lookup failed — those observations stay open (an \
                     unanswered lookup is not an outcome)"
                );
                return;
            }
        };
        let facts: Vec<nearres::ResolutionFacts> = markets
            .iter()
            .filter_map(nearres::resolution_facts)
            .collect();
        let settled = self.nearres.apply_lookups(&facts, now);
        if settled.is_empty() {
            tracing::debug!(
                conditions = due.len(),
                returned = markets.len(),
                "no near-resolution observation settled this pass"
            );
            return;
        }
        let (mut won, mut lost, mut undetermined) = (0usize, 0usize, 0usize);
        for (q, resolution) in &settled {
            match resolution.outcome {
                nearres::ResolutionOutcome::Won => won += 1,
                nearres::ResolutionOutcome::Undetermined => undetermined += 1,
                _ => lost += 1,
            }
            if let Err(err) = self.store.record_nearres_resolution(resolution) {
                tracing::warn!(%err, "could not persist a near-resolution verdict");
            }
            self.alerter.events().append(
                "nearres_resolved",
                json!({
                    "token_id": resolution.token_id.as_str(),
                    "condition_id": resolution.condition_id,
                    "event_slug": q.event_slug,
                    "outcome": resolution.outcome.as_str(),
                    "payout": resolution.payout,
                    "ask": q.ask,
                    "fee_per_share": q.fee_per_share,
                    "hours_to_payout": resolution.hours_to_payout,
                    "uma_status": resolution.uma_status,
                    "disputed": resolution.disputed,
                }),
            );
        }
        tracing::info!(
            settled = settled.len(),
            won,
            lost,
            undetermined,
            still_open = self.nearres.open_count(),
            "near-resolution observations settled"
        );
    }

    /// Fetch books, reporting progress on every batch that lands.
    ///
    /// The batch — not the sweep — is the unit of progress the watchdog counts. A full sweep
    /// of the live universe is ~1 500 batches over ~195 s, and a watchdog that only saw
    /// completed sweeps could not tell that from a wedge.
    async fn fetch_books_noting_progress(
        &self,
        tokens: &[TokenId],
        phase: &'static str,
    ) -> Result<BookMap, crate::http::ApiError> {
        let hb = self.hb.clone();
        ClobClient::new(&self.http, &self.cfg)
            .fetch_books_batched(tokens, move |_| hb.note(phase))
            .await
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
                let empty = is_empty_discovery(&err);
                let detail = error_chain(&err);
                self.hb.update(|s| {
                    if empty {
                        s.discovery_empty_streak = s.discovery_empty_streak.saturating_add(1);
                    }
                    s.discovery_error = Some(detail);
                });
                // A failed pass is still a completed unit of work: the loop is alive and
                // asking, which is exactly what the watchdog must not kill.
                self.hb.note(PHASE_DISCOVERY);
                return Err(err);
            }
        };
        let now = Utc::now();
        let events = universe.events.len() as i64;
        let markets = universe.market_count() as i64;
        let tokens = universe.token_count() as i64;
        let with_api_fee = stats.markets_with_api_fee as i64;
        // "Fell back to the category table" is everything the API did not state a usable
        // rate for — including the markets whose stated formula this build cannot price.
        let fallback = stats
            .markets_kept
            .saturating_sub(stats.markets_with_api_fee) as i64;
        let unsupported = stats.markets_unsupported_fee_formula as i64;
        self.hb.update(|s| {
            s.discovery_empty_streak = 0;
            s.discovery_error = None;
            s.last_good_discovery_at = Some(now);
            s.universe_refreshed_at = Some(now);
            s.universe_events = events;
            s.universe_markets = markets;
            s.universe_tokens = tokens;
            s.markets_with_api_fee = with_api_fee;
            s.fee_fallback_markets = fallback;
            s.fee_unsupported_formula = unsupported;
        });
        self.hb.note(PHASE_DISCOVERY);
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
        let books = match self
            .fetch_books_noting_progress(&tokens, PHASE_REST_SCAN)
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
        // R2 rides the books this sweep already paid for.
        self.nearres_scan(universe, &books);
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
                    // M8: one simulation per *row*, so a construction re-detected at the same
                    // quotes does not open a second phantom order at the same price.
                    self.open_maker_sim(recorded.id(), op, books, now);
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
        match self
            .fetch_books_noting_progress(&tokens, PHASE_SEED_BOOKS)
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

        self.hb.note(PHASE_STREAM_DETECT);
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
        match self.fetch_books_noting_progress(&stale, PHASE_RESYNC).await {
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

        let hb = self.hb.clone();
        let fetched = ClobClient::new(&self.http, &self.cfg)
            .fetch_books_batched(&tokens, |batch| {
                let (d, s) =
                    manager
                        .books()
                        .count_divergence(batch.books, per_batch, batch.requested_at);
                diverged += d;
                sampled += s;
                // ~1 500 batches over ~195 s at live scale: the sweep is progress all the
                // way through, not a 195 s silence with a result at the end.
                hb.note(PHASE_REST_SWEEP);
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

        // The full sweep is the one place with every book in hand, which is exactly what R2
        // needs: a 96–99¢ ask anywhere in the universe, not only where a gap was detected.
        self.nearres_scan(universe, &books);
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
/// * `trade_prints` — `last_trade_price` frames: recognized, never applied to a book. They
///   used to inflate `frames_unrecognized` (~4 000 per 5.1 M messages), which is the one
///   counter that has to mean "the wire format drifted" and nothing else.
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
            trade_prints_total = stats.trade_prints,
            trade_prints_usable_total = stats.trade_prints_usable,
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
        // Applied to no book on purpose. It sits next to `frames_unrecognized` so the two
        // are read together: trade prints climbing is normal, unrecognized climbing is a
        // wire-format break. `trade_prints_usable` is the subset carrying a token, a price
        // and a size — the only prints the maker simulator can treat as evidence, so a gap
        // between the two is a gap in that measurement (M8).
        trade_prints = stats.trade_prints.saturating_sub(since.trade_prints),
        trade_prints_usable = stats
            .trade_prints_usable
            .saturating_sub(since.trade_prints_usable),
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
/// Fold a market's three reward fields into [`RewardParams`], or refuse.
///
/// All three are required and all three must be positive: a market with a band but no pool
/// pays nothing, and one with a pool but no band cannot be scored. "The API said nothing" is
/// never turned into a zero or a default here — it simply is not a candidate.
fn reward_params(
    max_spread_cents: Option<Decimal>,
    min_size: Option<Decimal>,
    pool_daily: Option<Decimal>,
) -> Option<RewardParams> {
    let max_spread_cents = max_spread_cents.filter(|v| *v > Decimal::ZERO)?;
    let min_size = min_size.filter(|s| *s > Decimal::ZERO)?;
    let pool_daily = pool_daily.filter(|p| *p > Decimal::ZERO)?;
    Some(RewardParams {
        max_spread_cents,
        min_size,
        pool_daily,
    })
}

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
    /// M8 — the simulated maker fills. Kept as its own struct, and rendered in its own
    /// section, because merging it into either P&L number above would destroy the one thing
    /// it is for: the three figures have three different bases.
    pub maker_sim: MakerSimSummary,
    /// R1 — rewards farming, net of markout, against its pre-committed kill-line.
    pub rewards: RewardsSummary,
    /// R2 — the near-resolution study, against its two pre-committed kill-lines.
    pub nearres: NearResSummary,
}

/// The maker-fill simulator's day, aggregated (M8).
///
/// Read this next to [`DailySummary::pnl_taker`] and
/// [`DailySummary::pnl_maker_hypothetical`] and never inside them:
///
/// * `pnl_taker` — opportunities we sized as a **taker** whose fills were still on the book
///   at the first re-poll. Observed in the book, not in the tape.
/// * `pnl_lower_bound` — maker-only constructions whose every leg's queue *demonstrably*
///   traded through, us included, from trade prints. A floor.
/// * `pnl_maker_hypothetical` — the same maker constructions assuming every resting leg is
///   crossed. A ceiling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MakerSimSummary {
    /// Whether the simulator ran at all. `false` = the section reports "not measured", which
    /// is not the same statement as a 0% fill rate.
    pub enabled: bool,
    /// Simulations opened on this day that reached a verdict.
    pub opened: usize,
    pub filled: usize,
    pub partial: usize,
    pub unfilled: usize,
    pub untracked: usize,
    /// `filled / (filled + partial + unfilled)`. Untracked rows are excluded from both parts
    /// — a capacity eviction is a missing measurement, not a non-fill.
    pub fill_rate_pct: Option<Decimal>,
    /// **The lower bound.** Sum of `net_maker_total` over fully filled simulations only.
    pub pnl_lower_bound: Decimal,
    /// Capital the partials would have left sitting in half-built positions.
    pub legging_exposure: Decimal,
    pub median_time_to_fill_ms: Option<i64>,
    /// Verdicts reached in a window that contained no trade-print feed (REST-only). These
    /// are holes in the measurement, and the report says so rather than counting them as
    /// evidence that quotes do not fill.
    pub no_print_feed: usize,
    /// Trade prints that touched a level we were queued at.
    pub prints_observed: i64,
}

/// Aggregate the day's closed simulations. Pure, so the arithmetic is unit-testable.
pub fn summarize_maker_sims(enabled: bool, rows: &[MakerSimRow]) -> MakerSimSummary {
    let mut out = MakerSimSummary {
        enabled,
        opened: rows.len(),
        ..MakerSimSummary::default()
    };
    let mut fill_times: Vec<i64> = Vec::new();
    for row in rows {
        match row.status.as_str() {
            "maker_filled" => out.filled += 1,
            "maker_partial" => out.partial += 1,
            "maker_untracked" => out.untracked += 1,
            _ => out.unfilled += 1,
        }
        // Money is credited from the row's own column, never re-derived from the status: the
        // daemon decided what this simulation pays, once, and this only adds it up.
        out.pnl_lower_bound += row.pnl_lower_bound.unwrap_or(Decimal::ZERO);
        out.legging_exposure += row.legging_exposure.unwrap_or(Decimal::ZERO);
        out.prints_observed += row.prints_observed;
        if row.no_print_feed {
            out.no_print_feed += 1;
        }
        if let Some(ms) = row.time_to_fill_ms {
            fill_times.push(ms);
        }
    }
    fill_times.sort_unstable();
    out.median_time_to_fill_ms = quantile(&fill_times, 50);
    let resolved = out.filled + out.partial + out.unfilled;
    out.fill_rate_pct = (resolved > 0).then(|| {
        (Decimal::from(out.filled) * Decimal::ONE_HUNDRED / Decimal::from(resolved)).round_dp(1)
    });
    out
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
        maker_sim: MakerSimSummary::default(),
        // Phase R's sections are filled in by `emit_daily_summary`, which has the database.
        // Empty here means "not measured", which is what an empty section prints.
        rewards: rewardsim::summarize_epochs(
            false,
            day,
            &[],
            &crate::config::RewardSimConfig::default(),
        ),
        nearres: nearres::summarize(false, 0, 0, &[], &crate::config::NearResConfig::default()),
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

    /// The M8 section. Its own heading, its own numbers, and a statement of what the rule
    /// refuses to credit — because a fill rate is only meaningful next to the assumption
    /// that produced it.
    fn maker_sim_markdown(&self) -> String {
        let m = &self.maker_sim;
        let mut out = String::from("\n## Maker fills (simulated, last-in-queue)\n\n");
        if !m.enabled {
            out.push_str(
                "- **not measured**: `maker_sim.enabled = false`. This is an absence of \
                 measurement, not a fill rate of zero, and no lower bound is claimed below.\n",
            );
            return out;
        }
        if m.opened == 0 {
            out.push_str(
                "- no maker-only opportunity reached a verdict today, so there is nothing to \
                 bound. A day with no maker-only rows produces no simulations.\n",
            );
            return out;
        }
        out.push_str(&format!(
            "- sims opened: **{}** · filled: {} · partial: {} · unfilled: {} · untracked: {}\n",
            m.opened, m.filled, m.partial, m.unfilled, m.untracked
        ));
        out.push_str(&format!(
            "- fill rate: {} (untracked sims are excluded from both sides — a capacity \
             eviction is a missing measurement, not a non-fill)\n",
            m.fill_rate_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "n/a".into())
        ));
        out.push_str(&format!(
            "- **maker P&L lower bound (filled sims only): ${:.2}**\n",
            usd(m.pnl_lower_bound)
        ));
        out.push_str(&format!(
            "- legging exposure from partials: ${:.2} — capital the filled legs would have \
             committed to positions whose remaining legs never came\n",
            usd(m.legging_exposure)
        ));
        out.push_str(&format!(
            "- median time to fill: {}\n- trade prints observed at a level we were queued at: {}\n",
            fmt_opt_ms(m.median_time_to_fill_ms),
            m.prints_observed
        ));
        if m.no_print_feed > 0 {
            out.push_str(&format!(
                "- **{} sim(s) closed with no trade-print feed** (REST-only fallback for part \
                 or all of the window). Those windows contain no evidence either way and \
                 should not be read as non-fills.\n",
                m.no_print_feed
            ));
        }
        out.push_str(
            "\nThe rule: when a maker-only opportunity is detected we record the resting buy \
             each leg implies, at the best bid the maker economics were computed from, and \
             assume we are **last in that queue** — behind the entire size visible at that \
             price. A leg fills only once subsequent SELL prints at or through our price \
             total that whole queue *plus* our own size. Cancels ahead of us are never \
             credited, and a book improving past our level without prints is not a fill. \
             Every one of those refusals pushes the number down, which is why it is a lower \
             bound and not an estimate. What it still cannot see: our own order would itself \
             have changed the queue (it is information, and it can attract, deter or be \
             stepped in front of), partial fills are counted as no fill at all, and no order \
             was ever placed — this is arithmetic over prints that arrived anyway.\n",
        );
        out
    }

    /// The R1 section: the day's simulated rewards farming, net of adverse selection, and
    /// the running mean against the kill-line that was written down before any of it ran.
    fn rewards_markdown(&self) -> String {
        let r = &self.rewards;
        let mut out = String::from("\n## Rewards farming (simulated)\n\n");
        if !r.enabled {
            out.push_str(
                "- **not measured**: `rewardsim.enabled = false`. An absence of measurement, \
                 not a net of zero.\n",
            );
            return out;
        }
        match &r.day {
            Some(day) => {
                out.push_str(&format!(
                    "- epoch for {}: **{} market(s)**, ${:.2} of simulated quoting capital\n\
                     - gross (advertised pools, before the $1/day/market floor): ${:.2}\n\
                     - gross **paid** (after the floor): ${:.2}\n\
                     - markout on simulated fills ({} fills): ${:.2}\n\
                     - **portfolio net: ${:.2}/day**\n",
                    day.day,
                    day.markets,
                    usd(day.capital),
                    usd(day.gross_before_floor),
                    usd(day.gross_paid),
                    day.fills,
                    usd(day.markout),
                    usd(day.net),
                ));
                if day.markets_with_holes > 0 {
                    out.push_str(&format!(
                        "- **{} market(s) closed with a hole in the trade-print feed**: their \
                         markout is an absence of measurement, not an absence of adverse \
                         selection.\n",
                        day.markets_with_holes
                    ));
                }
            }
            None => {
                let last = match r.window.last() {
                    Some(d) => format!("the most recent closed epoch day is {}", d.day),
                    None => "no epoch has closed yet".to_string(),
                };
                out.push_str(&format!(
                    "- no closed epoch for {}. Epochs close at **00:00 UTC**, as the venue's \
                     do, so the day a report is generated *inside* has none yet — {last}.\n",
                    self.day
                ));
            }
        }

        // "n/a" must not become "n/a/day": a missing mean is not a rate.
        let mean = |v: Option<Decimal>| {
            v.map(|m| format!("${:.2}/day", usd(m)))
                .unwrap_or_else(|| "n/a".into())
        };
        out.push_str(&format!(
            "- {}-day window: {} day(s) measured · mean gross paid {} · **mean net {}** \
             at a ${:.2} cap\n",
            r.window_days,
            r.window.len(),
            mean(r.mean_gross_per_day),
            mean(r.mean_net_per_day),
            usd(r.capital_cap),
        ));
        if r.days_with_holes > 0 {
            out.push_str(&format!(
                "- {} of those day(s) contain a market whose print feed had a hole.\n",
                r.days_with_holes
            ));
        }

        // The kill-line, stated plainly, in the terms it was pre-committed in.
        out.push_str(&format!(
            "\n**KILL-LINE ({}): {}** — pre-committed: net ≤ ${:.2}/day at ${:.2} across {} \
             days is dead.\n",
            self.day,
            r.verdict.label(),
            usd(r.kill_line),
            usd(r.capital_cap),
            r.window_days,
        ));
        match &r.verdict {
            RewardKillLine::Pass { mean_net } => out.push_str(&format!(
                "The {}-day mean net is ${:.2}/day, above the line. The strategy survives this \
                 test; it has not been shown to make money at live-fill accuracy.\n",
                r.window_days,
                usd(*mean_net)
            )),
            RewardKillLine::Fail { mean_net } => out.push_str(&format!(
                "The {}-day mean net is ${:.2}/day, at or below the line. **On the \
                 pre-committed rule this instrument is dead** — no live probe follows from it.\n",
                r.window_days,
                usd(*mean_net)
            )),
            RewardKillLine::Provisional {
                mean_net,
                days_measured,
                needed,
                would_fail,
            } => {
                out.push_str(&format!(
                    "Only {days_measured} of {needed} days are measured, so there is no verdict \
                     yet. A day the daemon did not run is a **missing** day, not a zero day, \
                     and is left out of the mean rather than counted as no earnings.\n"
                ));
                if let (Some(mean), Some(fails)) = (mean_net, would_fail) {
                    out.push_str(&format!(
                        "Running mean so far: ${:.2}/day — on today's evidence the window would \
                         {}.\n",
                        usd(*mean),
                        if *fails { "**FAIL**" } else { "pass" }
                    ));
                }
            }
        }
        out.push_str(
            "\nHow the number is built, and what it refuses to assume: our score is the \
             documented quadratic `((v−s)/v)² × shares` on a two-sided quote (`v` in cents \
             from the size-cutoff-adjusted midpoint), aggregated by the published `Q_min` \
             rule; our share of a sample is that against the in-band score resting on the \
             live book, with the NO side mirrored into YES coordinates and **added**, because \
             ignoring it would shrink the competition and flatter us. The day's denominator \
             is 1 440 samples whether or not we were up, so downtime costs score exactly as \
             it would live. The advertised pool is a **configured cap, not a payout**, and \
             the $1/day/market floor is applied — a market that would pay $0.40 pays nothing. \
             Adverse selection is measured, not assumed: a SELL print crossing our in-band buy \
             fills us at our own limit, and the markout is `mid(t+5m) − fill`, signed. **Net = \
             gross paid + markout.** Nothing was quoted and no order was ever placed.\n",
        );
        out
    }

    /// The R2 section: what near-certain outcomes actually did, and whether there are enough
    /// of them to say anything yet.
    fn nearres_markdown(&self) -> String {
        let n = &self.nearres;
        let mut out = String::from("\n## Near-resolution observation\n\n");
        if !n.enabled {
            out.push_str(
                "- **not measured**: `nearres.enabled = false`. An absence of measurement, not \
                 a loss rate of zero.\n",
            );
            return out;
        }
        out.push_str(&format!(
            "- qualified (executable ask in band, end date inside the horizon): **{}** · \
             still open: {} · resolved: {}\n\
             - won: {} · lost: {} · split: {} · undetermined: {} · disputed: {}\n",
            n.qualified, n.open, n.resolutions, n.won, n.lost, n.split, n.undetermined, n.disputed
        ));
        if n.resolutions == 0 {
            out.push_str(
                "- nothing has resolved yet, so there is no loss rate and no yield. The \
                 observations above are entries that existed on the book, nothing more.\n",
            );
            return out;
        }
        out.push_str(&format!(
            "- realised loss rate at executable prices: **{:.2}%** of {} resolution(s)\n",
            n.loss_rate.unwrap_or(Decimal::ZERO) * Decimal::ONE_HUNDRED,
            n.resolutions,
        ));
        if let Some((low, high)) = n.loss_rate_ci_pct {
            out.push_str(&format!(
                "- 95% Wilson interval on that rate: {low:.2}% – {high:.2}%\n"
            ));
        }
        out.push_str(&format!(
            "- capital (one share per observation): ${:.2} · payout ${:.2} · fees ${:.4} · \
             **net ${:.4}**\n\
             - mean entry ask {} · mean time to payout {} h · recycling period used {} h\n\
             - annualised net yield at that recycling speed: **{}**\n",
            usd(n.capital),
            usd(n.payout),
            n.fees,
            n.net,
            n.mean_ask
                .map(|a| format!("{a}"))
                .unwrap_or_else(|| "n/a".into()),
            n.mean_hours_to_payout
                .map(|h| format!("{h}"))
                .unwrap_or_else(|| "n/a".into()),
            n.cycle_hours
                .map(|h| format!("{h}"))
                .unwrap_or_else(|| "n/a".into()),
            n.annual_yield_pct
                .map(|y| format!("{y}%/yr"))
                .unwrap_or_else(|| "n/a".into()),
        ));
        out.push_str(&format!(
            "\n**KILL-LINE ({}): {}** — pre-committed: a loss rate worse than 1-in-{}, or an \
             annualised net yield below {}%/yr, is dead.\n",
            self.day,
            n.verdict.label(),
            n.kill_line_loss_one_in,
            n.kill_line_annual_yield_pct,
        ));
        match &n.verdict {
            nearres::KillLine::Pass => {
                out.push_str("Both tests pass at a sample size large enough to distinguish them.\n")
            }
            nearres::KillLine::Fail(why) => out.push_str(&format!(
                "{why}. **On the pre-committed rule this instrument is dead.**\n"
            )),
            nearres::KillLine::NotEnoughData {
                resolutions,
                needed,
            } => out.push_str(&format!(
                "**{resolutions} resolution(s) is too small a sample for the loss-rate test.** \
                 Distinguishing a 1-in-40 loss rate from a 1-in-10 one needs on the order of \
                 {needed}; below that the Wilson interval above is the honest statement and \
                 the verdict is neither a pass nor a fail. Do not read the point estimate as \
                 the answer.\n"
            )),
        }
        out.push_str(
            "\nWhat this is: pure observation of other people's markets. No quote, no queue \
             position, no fill model — the ask recorded is the **executable** best ask that \
             was on the book at the moment the market entered the band, and the outcome is \
             whatever the API later stated. A market whose outcome the API does not state is \
             `undetermined` and is excluded from every number above rather than counted as a \
             win. Time to payout is measured at the resolution poll's granularity, so it is \
             an upper bound by at most one poll interval.\n",
        );
        out
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

        out.push_str(&self.maker_sim_markdown());
        // Measurement Phase R. Deliberately after the Phase A sections and never merged with
        // them: these are different instruments answering different questions, and the whole
        // reason the arbitrage scanner keeps running is that it costs nothing to leave on.
        out.push_str(&self.rewards_markdown());
        out.push_str(&self.nearres_markdown());

        out.push_str("\n## Simulated P&L\n\n");
        out.push_str(&format!(
            "- taker (credited only for taker-sized fills): **${:.2}**\n",
            usd(self.pnl_taker)
        ));
        if self.maker_sim.enabled {
            out.push_str(&format!(
                "- maker, simulated lower bound (last-in-queue, filled sims only): **${:.2}**\n",
                usd(self.maker_sim.pnl_lower_bound)
            ));
        } else {
            out.push_str(
                "- maker, simulated lower bound: not measured (`maker_sim.enabled = false`)\n",
            );
        }
        out.push_str(&format!(
            "- maker view (hypothetical — assumes every resting leg is crossed): ${:.2}\n",
            usd(self.pnl_maker_hypothetical)
        ));
        if self.maker_sim.enabled {
            out.push_str(&format!(
                "\nThree numbers, three bases, never added together. The maker truth lies \
                 between the other two: **${:.2} (lower bound) ≤ the real maker P&L ≤ ${:.2} \
                 (if always filled)** — the bound is what the tape proves, the hypothetical is \
                 what perfect fills would pay, and the taker figure (${:.2}) is a different \
                 construction entirely.\n",
                usd(self.maker_sim.pnl_lower_bound),
                usd(self.pnl_maker_hypothetical),
                usd(self.pnl_taker),
            ));
        } else {
            out.push_str(
                "\nWith the simulator off there is only a ceiling here: the maker view assumes \
                 every resting leg is crossed and nothing bounds it from below.\n",
            );
        }

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
    let sims = store
        .maker_sims_for_day(day)
        .context("could not read the day's maker-fill simulations")?;
    let mut summary = summarize(day, &rows, scan, cfg.risk.per_trade_cap_usd);
    summary.maker_sim = summarize_maker_sims(cfg.maker_sim.enabled, &sims);
    summary.rewards = rewards_summary(cfg, store, day)?;
    summary.nearres = nearres_summary(cfg, store)?;
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
        "pnl_maker_lower_bound": summary.maker_sim.pnl_lower_bound,
        "pnl_maker_hypothetical": summary.pnl_maker_hypothetical,
        "maker_sims_opened": summary.maker_sim.opened,
        "maker_sims_filled": summary.maker_sim.filled,
        "maker_sim_fill_rate_pct": summary.maker_sim.fill_rate_pct,
        "rewards_net_per_day": summary.rewards.day.as_ref().map(|d| d.net),
        "rewards_gross_paid_per_day": summary.rewards.day.as_ref().map(|d| d.gross_paid),
        "rewards_mean_net_per_day": summary.rewards.mean_net_per_day,
        "rewards_kill_line": summary.rewards.verdict.label(),
        "nearres_resolutions": summary.nearres.resolutions,
        "nearres_loss_rate": summary.nearres.loss_rate,
        "nearres_annual_yield_pct": summary.nearres.annual_yield_pct,
        "nearres_kill_line": summary.nearres.verdict.label(),
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

/// Build the R1 section's inputs: every closed epoch in the kill-line window.
///
/// The window starts `kill_line_window_days − 1` days before the reported day, so a report
/// for day D covers D−13 … D at the shipped 14-day setting. Days with no closed epoch simply
/// do not appear — they are missing measurements, not zero-earning days, and
/// [`rewardsim::summarize_epochs`] keeps them out of the mean on purpose.
fn rewards_summary(cfg: &Config, store: &Store, day: NaiveDate) -> Result<RewardsSummary> {
    let span = i64::from(cfg.rewardsim.kill_line_window_days.max(1)).saturating_sub(1);
    let from = day
        .checked_sub_signed(chrono::Duration::days(span))
        .unwrap_or(day);
    let rows = store
        .reward_epochs_since(from)
        .context("could not read the reward epochs")?;
    let window: Vec<ClosedEpochSummary> = rows
        .into_iter()
        // `reward_epochs_since` has no upper bound (there is nothing after today to read),
        // but a report generated for a past date must not borrow evidence from its future.
        .filter(|row| row.day <= day)
        .map(|row| ClosedEpochSummary {
            day: row.day,
            capital: row.capital,
            gross_usd: row.gross_usd,
            gross_paid_usd: row.gross_paid_usd,
            markout_short_usd: row.markout_short_usd,
            net_usd: row.net_usd,
            fills: row.fills,
            no_print_feed: row.no_print_feed,
        })
        .collect();
    Ok(rewardsim::summarize_epochs(
        cfg.rewardsim.enabled,
        day,
        &window,
        &cfg.rewardsim,
    ))
}

/// Build the R2 section's inputs.
///
/// The study is cumulative rather than daily on purpose: a 1-in-40 loss rate needs every
/// resolution the soak has ever seen, and slicing it by day would guarantee the sample is
/// always too small to say anything.
fn nearres_summary(cfg: &Config, store: &Store) -> Result<NearResSummary> {
    let resolved = store
        .nearres_resolved()
        .context("could not read the near-resolution verdicts")?;
    let qualified = store
        .nearres_observation_count()
        .context("could not count the near-resolution observations")?
        .max(0) as usize;
    let open = qualified.saturating_sub(resolved.len());
    Ok(nearres::summarize(
        cfg.nearres.enabled,
        qualified,
        open,
        &resolved,
        &cfg.nearres,
    ))
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

    // -- M7.1: the heartbeat, the loop-progress record and the watchdog -----------------

    /// A heartbeat over a real (temporary) database, with nothing else running.
    fn heartbeat_fixture(tag: &str) -> (PathBuf, Arc<Heartbeat>, Arc<Store>) {
        let dir = std::env::temp_dir().join(format!(
            "polyarb-hb-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.storage.database_path = dir.join("polyarb.sqlite").display().to_string();
        cfg.storage.log_dir = dir.join("logs").display().to_string();
        cfg.storage.report_dir = dir.join("reports").display().to_string();
        cfg.alerts.telegram_api_base = "http://127.0.0.1:1".into();

        let store = Arc::new(Store::open(Path::new(&cfg.storage.database_path)).expect("store"));
        let events = Arc::new(EventLog::open(Path::new(&cfg.storage.log_dir)).expect("log"));
        let alerter = Arc::new(Alerter::new(&cfg.alerts, events));
        let maker_sim = Arc::new(MakerSimulator::new(&cfg.maker_sim));
        let rewardsim = Arc::new(RewardSimulator::new(&cfg.rewardsim));
        let hb = Arc::new(Heartbeat::new(
            Arc::new(cfg),
            store.clone(),
            alerter,
            maker_sim,
            rewardsim,
            DaemonStatus {
                started_at: Utc::now(),
                rest_ok: true,
                ..DaemonStatus::default()
            },
        ));
        (dir, hb, store)
    }

    /// The bug this milestone exists for: during a DNS outage the loop tick stretched to
    /// minutes, and because the status row was written *between* ticks the dashboard
    /// reported `runtime_status::Stale` — "the daemon stopped reporting" — about a daemon
    /// that was alive and working.
    ///
    /// The row must now keep coming while the loop is blocked, and it must say so: the
    /// heartbeat is current, the loop's own timestamp is not.
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_publishes_while_the_scan_loop_is_blocked() {
        let (dir, hb, store) = heartbeat_fixture("blocked");

        // One completed unit of work, and then the loop wedges.
        hb.note(PHASE_REST_SWEEP);
        let stopped_at = hb.progress().last_wall.expect("progress was recorded");
        let blocked = tokio::spawn(std::future::pending::<()>());

        let (_tx, rx) = watch::channel(false);
        let task = tokio::spawn(heartbeat_task(hb.clone(), rx));
        tokio::time::sleep(Duration::from_secs(STATUS_PUBLISH_SECS * 3)).await;

        let row = store
            .read_runtime_status()
            .expect("read")
            .expect("the heartbeat must publish without the loop's help");
        assert_eq!(row.status.last_progress_phase, PHASE_REST_SWEEP);
        assert_eq!(
            row.status.last_progress_at,
            Some(crate::store::now_str(stopped_at)),
            "the loop's timestamp must lag — that is the whole signal"
        );
        assert_eq!(
            row.status.ticks_completed, 0,
            "no full iteration ever completed"
        );
        assert_eq!(row.status.watchdog_stall_secs, 600);

        // …and it is really republishing, not a single write at startup: a state change made
        // while the loop is still blocked reaches the row on the next beat.
        hb.update(|s| s.universe_events = 4_242);
        tokio::time::sleep(Duration::from_secs(STATUS_PUBLISH_SECS * 2)).await;
        let row = store.read_runtime_status().expect("read").expect("row");
        assert_eq!(row.status.universe_events, 4_242);
        assert_eq!(
            row.status.last_progress_at,
            Some(crate::store::now_str(stopped_at)),
            "and the loop is still, correctly, reported as stopped"
        );

        task.abort();
        blocked.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stall_verdict_ignores_a_slow_loop_and_an_unstarted_one() {
        let now = TokioInstant::now();
        let stall = Duration::from_secs(600);

        // Never completed anything: unarmed, however long ago the process started. At live
        // scale the first discovery-and-seed is minutes of legitimate work.
        assert_eq!(stall_verdict(&LoopProgress::default(), now, stall), None);

        let progress = |ago_secs: u64, phase| LoopProgress {
            last_at: Some(now - Duration::from_secs(ago_secs)),
            last_wall: Some(Utc::now()),
            phase,
            ticks: 7,
            last_tick_wall: None,
        };
        // Slow: a full REST sweep reports per batch, so "a while ago" is still progress.
        assert_eq!(
            stall_verdict(&progress(599, PHASE_REST_SWEEP), now, stall),
            None
        );

        let report = stall_verdict(&progress(601, PHASE_REST_SWEEP), now, stall)
            .expect("past the threshold, this is a wedge");
        assert_eq!(report.phase, PHASE_REST_SWEEP);
        assert_eq!(report.ticks_completed, 7);
        assert!(report.stalled_for_secs >= 600);
    }

    /// The watchdog task itself, on the tokio clock: it trips on a stalled loop, and never
    /// on a slow one or on a daemon that has not finished its first unit of work.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_trips_on_a_stall_only() {
        let (dir, hb, _store) = heartbeat_fixture("watchdog");
        let stall = Duration::from_secs(120);

        // (a) Unarmed before the first unit of work — the long first discovery-and-seed.
        let (_tx, rx) = watch::channel(false);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3_600),
                watchdog_task(hb.clone(), stall, rx)
            )
            .await
            .is_err(),
            "a daemon still doing its first discovery must never be restarted for it"
        );

        // (b) Slow but progressing: a book batch every 5 s, an hour of it, no trip.
        hb.note(PHASE_SEED_BOOKS);
        let ticker = {
            let hb = hb.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    hb.note(PHASE_REST_SWEEP);
                }
            })
        };
        let (_tx2, rx2) = watch::channel(false);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3_600),
                watchdog_task(hb.clone(), stall, rx2)
            )
            .await
            .is_err(),
            "a 195 s sweep reports progress throughout; only silence is a stall"
        );

        // (c) The loop wedges — and now it trips, naming where it stopped.
        ticker.abort();
        let (_tx3, rx3) = watch::channel(false);
        let report = tokio::time::timeout(
            Duration::from_secs(3_600),
            watchdog_task(hb.clone(), stall, rx3),
        )
        .await
        .expect("the watchdog must answer within the hour")
        .expect("a wedged loop must trip it");
        assert_eq!(report.phase, PHASE_REST_SWEEP);
        assert!(report.stalled_for_secs >= stall.as_secs());
        assert!(report.last_progress_at.is_some());

        // (d) Shutdown wins: a daemon told to stop is not a daemon to kill.
        let (tx4, rx4) = watch::channel(false);
        let quiet = tokio::spawn(watchdog_task(hb.clone(), stall, rx4));
        tx4.send(true).expect("shutdown");
        assert_eq!(quiet.await.expect("join"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    // -- M8: the maker-fill section --------------------------------------------------

    fn sim_row(
        id: i64,
        status: &str,
        pnl: Option<Decimal>,
        exposure: Option<Decimal>,
    ) -> MakerSimRow {
        MakerSimRow {
            id,
            opportunity_id: id,
            event_slug: format!("e{id}"),
            kind: "binary_yes_no".into(),
            category: "politics".into(),
            legs_total: 2,
            legs_filled: match status {
                "maker_filled" => 2,
                "maker_partial" => 1,
                _ => 0,
            },
            net_maker_total: dec!(7.00),
            pnl_lower_bound: pnl,
            legging_exposure: exposure,
            prints_observed: 4,
            time_to_fill_ms: (status == "maker_filled").then_some(12_000 * id),
            status: status.into(),
            no_print_feed: false,
            opened_at: "2026-07-26T09:00:00.000Z".into(),
            closed_at: "2026-07-26T10:00:00.000Z".into(),
        }
    }

    /// The aggregation, hand-computed.
    ///
    /// Two filled at $7.00 each → **$14.00** lower bound. One partial → $47.00 exposure and
    /// nothing credited. Fill rate is 2 of 4 *resolved* — the untracked eviction is in
    /// neither part — so **50%**. Fill times 12 000 and 24 000 ms → nearest-rank p50 = 12 000.
    #[test]
    fn the_maker_sim_summary_counts_credits_and_excludes_untracked() {
        let rows = vec![
            sim_row(1, "maker_filled", Some(dec!(7.00)), None),
            sim_row(2, "maker_filled", Some(dec!(7.00)), None),
            sim_row(3, "maker_partial", None, Some(dec!(47.00))),
            sim_row(4, "maker_unfilled", None, None),
            sim_row(5, "maker_untracked", None, None),
        ];
        let m = summarize_maker_sims(true, &rows);
        assert_eq!(m.opened, 5);
        assert_eq!((m.filled, m.partial, m.unfilled, m.untracked), (2, 1, 1, 1));
        assert_eq!(m.pnl_lower_bound, dec!(14.00));
        assert_eq!(m.legging_exposure, dec!(47.00));
        assert_eq!(m.fill_rate_pct, Some(dec!(50.0)));
        assert_eq!(m.median_time_to_fill_ms, Some(12_000));
        assert_eq!(m.prints_observed, 20);

        // Off is not zero: a disabled simulator claims no bound at all.
        let off = summarize_maker_sims(false, &[]);
        assert!(!off.enabled);
        assert_eq!(off.fill_rate_pct, None);
        assert_eq!(off.pnl_lower_bound, Decimal::ZERO);
    }

    /// (h) The report shows three P&L numbers with three labels, never merged, plus the one
    /// sentence that orders them.
    #[test]
    fn the_report_keeps_the_lower_bound_the_truth_and_the_hypothetical_apart() {
        let rows = vec![row(
            1,
            "binary_yes_no",
            "politics",
            dec!(0.01),
            "filled_simulated",
            Some(1_000),
            dec!(50),
        )];
        let mut s = summarize(day(), &rows, ScanTotals::default(), dec!(50));
        s.maker_sim = summarize_maker_sims(
            true,
            &[
                sim_row(1, "maker_filled", Some(dec!(7.00)), None),
                sim_row(2, "maker_partial", None, Some(dec!(47.00))),
                sim_row(3, "maker_unfilled", None, None),
            ],
        );
        let md = s.to_markdown();

        // The section, with its counts and its money.
        assert!(md.contains("## Maker fills (simulated, last-in-queue)"));
        assert!(md.contains("sims opened: **3** · filled: 1 · partial: 1 · unfilled: 1"));
        assert!(md.contains("fill rate: 33.3%"));
        assert!(md.contains("**maker P&L lower bound (filled sims only): $7.00**"));
        assert!(md.contains("legging exposure from partials: $47.00"));
        assert!(md.contains("median time to fill: 12.0s"));

        // Three numbers, three labels, in the one place they appear together.
        assert!(md.contains("taker (credited only for taker-sized fills): **$1.00**"));
        assert!(md
            .contains("maker, simulated lower bound (last-in-queue, filled sims only): **$7.00**"));
        assert!(
            md.contains("maker view (hypothetical — assumes every resting leg is crossed): $2.00")
        );
        assert!(
            md.contains("**$7.00 (lower bound) ≤ the real maker P&L ≤ $2.00 (if always filled)**")
        );
        // The assumption that produced the bound is stated wherever the bound is.
        assert!(md.contains("last in that queue"));
        assert!(md.contains("Cancels ahead of us are never credited"));

        // A day with the simulator off says so instead of reporting a 0% fill rate.
        let mut off = summarize(day(), &rows, ScanTotals::default(), dec!(50));
        off.maker_sim = summarize_maker_sims(false, &[]);
        let md = off.to_markdown();
        assert!(md.contains("**not measured**: `maker_sim.enabled = false`"));
        assert!(
            !md.contains("**maker P&L lower bound (filled sims only)"),
            "a simulator that never ran must not report a bound of $0.00"
        );

        // A window with no print feed is flagged in the report rather than counted as
        // evidence that quotes do not fill.
        let mut blind = summarize(day(), &rows, ScanTotals::default(), dec!(50));
        let mut sim = sim_row(1, "maker_unfilled", None, None);
        sim.no_print_feed = true;
        blind.maker_sim = summarize_maker_sims(true, &[sim]);
        assert!(blind
            .to_markdown()
            .contains("**1 sim(s) closed with no trade-print feed**"));
    }

    /// R1's section states the day's net, the running mean, and the kill-line verdict in the
    /// words it was pre-committed in — including the case that matters most, an incomplete
    /// window, which is neither a pass nor a fail.
    #[test]
    fn the_rewards_section_prints_net_the_running_mean_and_a_plain_kill_line() {
        let cfg = crate::config::RewardSimConfig {
            kill_line_window_days: 3,
            ..crate::config::RewardSimConfig::default()
        };
        let epoch = |d: &str, net: Decimal, gross: Decimal| ClosedEpochSummary {
            day: NaiveDate::parse_from_str(d, "%Y-%m-%d").expect("day"),
            capital: dec!(1960),
            gross_usd: gross + dec!(1),
            gross_paid_usd: gross,
            markout_short_usd: net - gross,
            net_usd: net,
            fills: 9,
            no_print_feed: false,
        };
        let today = NaiveDate::parse_from_str("2026-09-03", "%Y-%m-%d").expect("day");

        // A complete window, under the line: the report must say the strategy is dead in
        // those words, not hedge it.
        let mut s = summarize(today, &[], ScanTotals::default(), dec!(50));
        s.day = today;
        s.rewards = rewardsim::summarize_epochs(
            true,
            today,
            &[
                epoch("2026-09-01", dec!(0.10), dec!(3)),
                epoch("2026-09-02", dec!(0.40), dec!(3)),
                epoch("2026-09-03", dec!(0.70), dec!(4)),
            ],
            &cfg,
        );
        let md = s.to_markdown();
        assert!(md.contains("## Rewards farming (simulated)"));
        assert!(md.contains("**portfolio net: $0.70/day**"));
        assert!(md.contains("gross **paid** (after the floor): $4.00"));
        assert!(md.contains("gross (advertised pools, before the $1/day/market floor): $5.00"));
        assert!(md.contains("**mean net $0.40/day**"));
        assert!(md.contains("**KILL-LINE (2026-09-03): FAIL**"));
        assert!(md.contains("**On the pre-committed rule this instrument is dead**"));
        // The caveats that make the number worth reading travel with it.
        assert!(md.contains("configured cap, not a payout"));
        assert!(md.contains("1 440 samples whether or not we were up"));
        assert!(md.contains("Net = gross paid + markout"));

        // An incomplete window: no verdict, the running mean, and which way it points.
        let mut s = summarize(today, &[], ScanTotals::default(), dec!(50));
        s.day = today;
        s.rewards = rewardsim::summarize_epochs(
            true,
            today,
            &[epoch("2026-09-03", dec!(3.00), dec!(4))],
            &cfg,
        );
        let md = s.to_markdown();
        assert!(md.contains("**KILL-LINE (2026-09-03): NO VERDICT YET**"));
        assert!(md.contains("Only 1 of 3 days are measured"));
        assert!(md.contains("missing** day, not a zero day"));
        assert!(md.contains("the window would pass"));

        // A day whose epoch has not closed yet says so rather than printing zeros.
        let mut s = summarize(today, &[], ScanTotals::default(), dec!(50));
        s.day = today;
        s.rewards = rewardsim::summarize_epochs(
            true,
            today,
            &[epoch("2026-09-02", dec!(3.00), dec!(4))],
            &cfg,
        );
        let md = s.to_markdown();
        assert!(md.contains("no closed epoch for 2026-09-03"));
        assert!(md.contains("the most recent closed epoch day is 2026-09-02"));

        // Switched off: an absence of measurement, never a net of zero.
        let mut s = summarize(today, &[], ScanTotals::default(), dec!(50));
        s.rewards = rewardsim::summarize_epochs(false, today, &[], &cfg);
        let md = s.to_markdown();
        assert!(md.contains("**not measured**: `rewardsim.enabled = false`"));
        assert!(!md.contains("portfolio net:"));
    }

    /// R2's section prints the count first, refuses a verdict on a small sample, and says why
    /// in the report rather than leaving a reader to notice.
    #[test]
    fn the_near_resolution_section_refuses_a_verdict_on_a_small_sample() {
        use crate::nearres::{ResolutionOutcome, ResolvedObservation};
        let cfg = crate::config::NearResConfig {
            min_resolutions_for_verdict: 40,
            ..crate::config::NearResConfig::default()
        };
        let obs = |outcome: ResolutionOutcome| ResolvedObservation {
            token_id: "t".into(),
            event_slug: "e".into(),
            category: "politics".into(),
            ask: dec!(0.97),
            fee_per_share: dec!(0.001164),
            payout: match outcome {
                ResolutionOutcome::Won => Some(Decimal::ONE),
                ResolutionOutcome::Lost => Some(Decimal::ZERO),
                _ => None,
            },
            outcome,
            hours_to_payout: dec!(20),
            disputed: false,
        };

        let mut s = summarize(day(), &[], ScanTotals::default(), dec!(50));
        let rows: Vec<ResolvedObservation> = (0..5).map(|_| obs(ResolutionOutcome::Won)).collect();
        s.nearres = nearres::summarize(true, 9, 4, &rows, &cfg);
        let md = s.to_markdown();
        assert!(md.contains("## Near-resolution observation"));
        assert!(md.contains("still open: 4 · resolved: 5"));
        assert!(md.contains("realised loss rate at executable prices: **0.00%** of 5"));
        assert!(md.contains("95% Wilson interval"));
        assert!(md.contains("**KILL-LINE"));
        assert!(md.contains("NO VERDICT"));
        assert!(md.contains("**5 resolution(s) is too small a sample"));
        assert!(md.contains("Do not read the point estimate as the answer."));

        // Nothing resolved: no rate, no yield, and no pretending otherwise.
        let mut s = summarize(day(), &[], ScanTotals::default(), dec!(50));
        s.nearres = nearres::summarize(true, 3, 3, &[], &cfg);
        let md = s.to_markdown();
        assert!(md.contains("nothing has resolved yet"));
        assert!(!md.contains("realised loss rate"));

        // Switched off.
        let mut s = summarize(day(), &[], ScanTotals::default(), dec!(50));
        s.nearres = nearres::summarize(false, 0, 0, &[], &cfg);
        assert!(s
            .to_markdown()
            .contains("**not measured**: `nearres.enabled = false`"));
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
                trade_prints: 500,
                trade_prints_usable: 480,
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
            trade_prints: 1_500,
            trade_prints_usable: 1_460,
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
            // Trade prints, next to the counter they used to inflate — and the subset the
            // maker simulator can actually use as evidence of a fill.
            "trade_prints=1000",
            "trade_prints_usable=980",
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
        assert!(log.contains("trade_prints_total=1500"), "{log}");
        assert!(log.contains("trade_prints_usable_total=1460"), "{log}");
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
