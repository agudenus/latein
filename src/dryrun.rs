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

use crate::alert::{format_opportunity, AlertStats, Alerter, EventLog, Priority};
use crate::clob::ClobClient;
use crate::config::{Config, REPORTED_CATEGORIES};
use crate::detect;
use crate::gamma::GammaClient;
use crate::http::HttpClient;
use crate::store::{
    CycleStats, LifecycleOutcome, LifecycleStatus, OpportunityRow, ScanTotals, Store,
};
use crate::types::{BookMap, Opportunity, Side, TokenId, Universe};
use crate::ws::{DirtyBatch, StreamManager, DIVERGENCE_SAMPLE};

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
        token_events: HashMap::new(),
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

struct Daemon {
    cfg: Arc<Config>,
    http: Arc<HttpClient>,
    store: Arc<Store>,
    alerter: Arc<Alerter>,
    slots: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    trackers: JoinSet<()>,
    last_summary_day: Option<NaiveDate>,
    /// token id → index into `universe.events`. Rebuilt on every universe refresh; this is
    /// what turns "this book moved" into "re-evaluate exactly this event".
    token_events: HashMap<TokenId, usize>,
}

impl Daemon {
    async fn main_loop(&mut self, max_cycles: Option<u64>) -> Result<()> {
        // Startup must prove connectivity: a daemon that cannot see any market is not
        // "quietly idle", it is broken.
        let mut universe = self.fetch_universe().await?;
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

        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.cfg.daemon.scan_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let refresh_after = Duration::from_secs(self.cfg.daemon.universe_refresh_secs);
        let resync_after = Duration::from_secs(self.cfg.stream.resync_interval_secs);
        let stale_after = Duration::from_secs(self.cfg.stream.stale_after_secs);
        let mut last_sweep = Instant::now();
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
                    // The pool is gone (fallback or shutdown): stop selecting on it.
                    None => { stream = None; continue; }
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
                        if let Some(manager) = stream.as_mut() {
                            manager.update_universe(&universe.token_ids());
                        }
                    }
                    Err(err) => {
                        // Keep trading off the last known universe rather than going blind.
                        tracing::warn!(%err, "universe refresh failed — keeping the previous one");
                    }
                }
                universe_fetched = Instant::now();
            }

            // The stream can be given up on mid-run; from then on this is the M3 daemon.
            if stream
                .as_ref()
                .is_some_and(|manager| manager.health().fallen_back())
            {
                tracing::error!(
                    "falling back to REST polling for the rest of this process — detection \
                     latency returns to the {} s scan interval",
                    self.cfg.daemon.scan_interval_secs
                );
                if let Some(manager) = stream.take() {
                    manager.stop().await;
                }
            }

            match stream.as_ref() {
                Some(manager) => {
                    // Cheap every tick: anything the stream has not refreshed recently.
                    self.resync_stale(manager, &universe, stale_after).await;
                    // Slow and thorough: the whole universe over REST, a divergence
                    // cross-check, and a full detection pass as the integrity net.
                    if last_sweep.elapsed() >= resync_after {
                        last_sweep = Instant::now();
                        self.full_sweep(manager, &universe).await;
                    }
                }
                None => self.scan_cycle(&universe).await,
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

    async fn fetch_universe(&self) -> Result<Universe> {
        let (universe, stats) = GammaClient::new(&self.http, &self.cfg)
            .fetch_universe()
            .await
            .with_context(|| {
                format!(
                    "market discovery via the Gamma API at {} failed",
                    self.cfg.api.gamma_base_url
                )
            })?;
        let partial_negrisk = universe
            .events
            .iter()
            .filter(|e| e.neg_risk && !e.coverage_complete())
            .count();
        tracing::info!(
            events = universe.events.len(),
            markets = universe.market_count(),
            negrisk_events = universe.neg_risk_event_count(),
            partial_negrisk_events = partial_negrisk,
            markets_seen = stats.markets_seen,
            dropped_markets = stats.markets_dropped(),
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

        let opportunities = detect::scan(&self.cfg, universe, &books);
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
    async fn seed_stream_books(&self, manager: &StreamManager, universe: &Universe) {
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
                manager.books().apply_rest(&tokens, &books, Instant::now());
            }
            Err(err) => tracing::warn!(
                %err,
                "could not seed book state over REST — detection waits for stream snapshots"
            ),
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
        let opportunities = detect::scan(&self.cfg, &subset, &books);
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
        });
    }

    /// Targeted REST re-fetch of books the stream has not refreshed (or has flagged
    /// unverified). Usually a no-op, and then it costs nothing at all.
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
                manager
                    .books()
                    .stats
                    .resynced
                    .fetch_add(books.len() as u64, std::sync::atomic::Ordering::Relaxed);
                manager.books().apply_rest(&stale, &books, Instant::now());
            }
            Err(err) => tracing::warn!(%err, stale = stale.len(), "stale-book resync failed"),
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
    async fn full_sweep(&mut self, manager: &StreamManager, universe: &Universe) {
        let started = Instant::now();
        let tokens = universe.token_ids();
        let books = match ClobClient::new(&self.http, &self.cfg)
            .fetch_books(&tokens)
            .await
        {
            Ok(books) => books,
            Err(err) => {
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

        let diverged = manager.books().count_divergence(&books, DIVERGENCE_SAMPLE);
        if diverged > 0 {
            tracing::warn!(
                diverged,
                sampled = books.len().min(DIVERGENCE_SAMPLE),
                "locally streamed books disagreed with REST at the top of book — the REST \
                 view wins; investigate before trusting stream-only detection"
            );
        }
        manager.books().apply_rest(&tokens, &books, Instant::now());

        let opportunities = detect::scan(&self.cfg, universe, &books);
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
        });
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

        if self.alerter.passes_threshold(op) {
            self.alerter
                .notify(
                    "opportunity",
                    &format_opportunity(op),
                    Priority::Routine,
                    payload,
                )
                .await;
        } else {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("delivery".into(), json!("below_threshold"));
            }
            self.alerter.events().append("opportunity", payload);
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
                "\n## Alerts\n\n- sent: {} · failed: {} · suppressed: {} · circuit open: {}\n",
                alerts.sent, alerts.failed, alerts.suppressed, alerts.circuit_open
            ));
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
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
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
        });
        format!("ws://{addr}/ws/market")
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
        cfg.stream.stale_after_secs = 60;
        // Long enough that no full REST sweep runs inside the test window: what is
        // detected here was detected from the stream, and nothing else.
        cfg.stream.resync_interval_secs = 3_600;
        cfg.stream.fallback_after_failures = 5;
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
}
