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

use std::collections::BTreeMap;
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
use crate::config::Config;
use crate::detect;
use crate::gamma::GammaClient;
use crate::http::HttpClient;
use crate::store::{
    CycleStats, LifecycleOutcome, LifecycleStatus, OpportunityRow, ScanTotals, Store,
};
use crate::types::{BookMap, Opportunity, Side, TokenId, Universe};

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
}

impl Daemon {
    async fn main_loop(&mut self, max_cycles: Option<u64>) -> Result<()> {
        // Startup must prove connectivity: a daemon that cannot see any market is not
        // "quietly idle", it is broken.
        let mut universe = self.fetch_universe().await?;
        let mut universe_fetched = Instant::now();

        // If we start after today's summary time, do not immediately fire a partial-day
        // summary; wait for tomorrow's.
        let summary_time = self.cfg.daily_summary_time()?;
        if Utc::now().time() >= summary_time {
            self.last_summary_day = Some(Utc::now().date_naive());
        }

        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.cfg.daemon.scan_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let refresh_after = Duration::from_secs(self.cfg.daemon.universe_refresh_secs);
        let mut cycles = 0u64;

        loop {
            tokio::select! {
                _ = self.shutdown.changed() => break,
                _ = ticker.tick() => {}
            }
            if *self.shutdown.borrow() {
                break;
            }

            if universe_fetched.elapsed() >= refresh_after {
                match self.fetch_universe().await {
                    Ok(fresh) => {
                        universe = fresh;
                        universe_fetched = Instant::now();
                    }
                    Err(err) => {
                        // Keep trading off the last known universe rather than going blind.
                        tracing::warn!(%err, "universe refresh failed — keeping the previous one");
                        universe_fetched = Instant::now();
                    }
                }
            }

            self.scan_cycle(&universe).await;
            self.maybe_daily_summary(summary_time).await;

            cycles += 1;
            if max_cycles.is_some_and(|max| cycles >= max) {
                tracing::info!(cycles, "reached the configured cycle limit — stopping");
                break;
            }
        }

        self.drain_trackers().await;
        let stats = self.alerter.stats();
        tracing::info!(
            cycles,
            alerts_sent = stats.sent,
            alerts_failed = stats.failed,
            alerts_suppressed = stats.suppressed,
            "polyarb daemon stopped"
        );
        Ok(())
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
        tracing::info!(
            events = universe.events.len(),
            markets = universe.market_count(),
            negrisk_events = universe.neg_risk_event_count(),
            dropped_markets = stats.markets_unusable + stats.markets_inactive,
            "market universe refreshed"
        );
        Ok(universe)
    }

    /// One scan tick: fetch every tracked book, detect, persist, alert, track.
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
        let now = Utc::now();
        let mut new_opportunities = 0i64;

        for op in &opportunities {
            match self.store.record_opportunity(op, &books, now) {
                Ok(recorded) if recorded.is_new() => {
                    new_opportunities += 1;
                    self.on_new_opportunity(recorded.id(), op).await;
                }
                Ok(recorded) => {
                    // Same construction at the same quotes: update `last_seen_at`, never
                    // duplicate the row.
                    tracing::debug!(id = recorded.id(), "opportunity still on the book");
                }
                Err(err) => tracing::warn!(%err, "could not persist an opportunity"),
            }
        }

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

    fn record_cycle(&self, stats: CycleStats) {
        if let Err(err) = self.store.record_cycle(&stats, Utc::now()) {
            tracing::warn!(%err, "could not record scan-cycle stats");
        }
    }

    /// Log, alert (if it clears the threshold), and start lifecycle tracking.
    async fn on_new_opportunity(&mut self, id: i64, op: &Opportunity) {
        let mut payload = json!({
            "id": id,
            "dedupe_key": crate::store::dedupe_key(op),
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
    /// Net per share, as constructed (maker net for maker-only rows).
    pub net_p50: Option<Decimal>,
    pub net_p90: Option<Decimal>,
    pub net_max: Option<Decimal>,
    pub persistence_p50_ms: Option<i64>,
    pub persistence_p90_ms: Option<i64>,
    pub persistence_max_ms: Option<i64>,
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
    let mut nets: Vec<Decimal> = Vec::with_capacity(rows.len());
    let mut persistences: Vec<i64> = Vec::new();
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
    }

    nets.sort();
    persistences.sort_unstable();
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
        net_p50: quantile(&nets, 50),
        net_p90: quantile(&nets, 90),
        net_max: nets.last().copied(),
        persistence_p50_ms: quantile(&persistences, 50),
        persistence_p90_ms: quantile(&persistences, 90),
        persistence_max_ms: persistences.last().copied(),
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

    /// Minimal HTTP/1.1 stub for the two Polymarket endpoints. Returns its base URL.
    async fn mock_api() -> String {
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
                        MOCK_EVENTS
                    } else {
                        MOCK_BOOKS
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

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
