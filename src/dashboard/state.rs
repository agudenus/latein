//! The dashboard's data model: one JSON-serialisable snapshot, derived entirely from
//! SQLite.
//!
//! Three rules run through this file.
//!
//! 1. **Money is never a JSON number.** Every dollar, per-share net, percentage and bps
//!    figure is computed in [`Decimal`] and serialised as a string. The page renders those
//!    strings verbatim; it never parses one back into a JavaScript float, and it never
//!    sorts on one (ordering is decided here, server-side).
//! 2. **Maker and taker are never merged.** They travel as separate fields, are aggregated
//!    separately, and no total anywhere mixes the two bases.
//! 3. **A verdict is never revised in the opportunity's favour.** The lifecycle is decided
//!    by the daemon at the first re-poll; this layer only reads it, and `vanished` is
//!    terminal. There is no code path here that could promote one.
//!
//! Where the pipeline does not measure something, the field says so ([`FunnelStage`]'s
//! `instrumented = false`) rather than carrying a plausible number.

use chrono::{DateTime, Duration, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::Serialize;

use crate::config::Config;
use crate::store::{DashboardRow, RuntimeStatus, RuntimeStatusRecord, Store, StoreError};

/// A runtime status older than this means the daemon **process** is not talking to us. Six
/// times the heartbeat's publish cadence.
///
/// Since M7.1 the row is published by a task of its own, decoupled from the scan loop, so
/// this threshold answers exactly one question — is the process there? — and a slow loop can
/// no longer be mistaken for a dead one. "The process is up but its loop is stuck" is a
/// separate verdict with a separate threshold: see [`BreakReason::LoopStalled`] and
/// `dashboard.loop_stall_secs`.
pub const RUNTIME_STATUS_STALE_SECS: i64 = 30;

/// Consecutive empty discovery passes before the break state takes the page over. Matches
/// the daemon's own retry story and the design's copy ("three consecutive passes").
pub const EMPTY_DISCOVERY_BLIND_STREAK: i64 = 3;

/// Consecutive REST failures before REST counts as down rather than flaky.
pub const REST_DOWN_STREAK: i64 = 3;

/// Memory guard on the window query. Far above a realistic day (a live soak writes
/// hundreds of rows a day), and a cap is better than an unbounded read of a file another
/// process is writing.
const MAX_WINDOW_ROWS: usize = 20_000;

// ---------------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DashboardState {
    pub generated_at: String,
    pub poll_interval_ms: u64,
    pub header: Header,
    /// `Some` = the page is taken over. Every clean number below it is meaningless while
    /// this is set, which is the entire reason it exists.
    pub break_state: Option<BreakState>,
    pub hero: Hero,
    pub funnel: Funnel,
    pub opportunities: Vec<OpportunityView>,
    pub opportunity_note: String,
    /// The configured default taker floor as a percentage — the number the empty state
    /// has to quote. "None above the floor" is a measurement; "no edge" is a conclusion
    /// the dashboard has no right to draw.
    pub net_floor_pct: String,
    pub pipeline: Vec<PipelineRow>,
    pub categories: Vec<CategoryRow>,
    /// True when a category tile reads zero for crypto. The caveat below it is mandatory:
    /// a blank cell that reads as "no edge" is a wrong conclusion the dashboard would be
    /// causing.
    pub crypto_zero: bool,
    pub log: Vec<LogLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Header {
    /// Always `dry-run`; the pill is the standing assurance that no order path is live.
    pub mode: String,
    pub mode_pill: String,
    pub transport: Transport,
    pub universe_events: i64,
    pub refresh_secs: u64,
    pub uptime: String,
    pub last_good_ago: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transport {
    /// `streaming` · `rest_fallback` · `rest_only` · `blind`.
    pub state: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hero {
    pub soak_day: u32,
    pub soak_days: u32,
    /// Opportunities whose walked fills were still there at the first re-poll.
    pub survivors: i64,
    /// Simulated net edge at walked size, **maker basis**. Never added to a taker figure.
    pub net_edge_maker: String,
    pub median_net_maker_bps: Option<String>,
    pub median_net_taker_bps: Option<String>,
    pub latency_p50_ms: Option<i64>,
    pub latency_p95_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Funnel {
    pub window_hours: u64,
    pub stages: Vec<FunnelStage>,
    /// Share of resolved opportunities recorded `vanished` — never revised in their favour.
    pub vanished_pct: Option<String>,
    pub vanished: i64,
    /// The window's telemetry predates the M7 counters, wholly or in part.
    pub partial_window: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunnelStage {
    pub key: &'static str,
    pub label: String,
    /// `None` when this pipeline does not measure the stage. Rendered as "not
    /// instrumented", never as a zero or an estimate.
    pub count: Option<i64>,
    /// The same count with thousands separators — what the cell shows. Kept apart from
    /// `count` so a JSON consumer still gets the number.
    pub count_display: Option<String>,
    pub pct: Option<String>,
    /// Bar width as a percentage string, already floored so a tiny survivor stays visible.
    pub bar_pct: Option<String>,
    pub instrumented: bool,
    /// `pass` = counted per construction per detection pass; `row` = distinct opportunity
    /// rows. The two bases cannot be compared, and the panel says which is which.
    pub basis: &'static str,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpportunityView {
    pub id: i64,
    pub event_title: String,
    pub event_slug: String,
    pub category: String,
    /// `fee-free` when the rate is 0, else the decimal.
    pub fee_display: String,
    /// The Rust detector name, lowercased exactly as the design asks: `negrisk-yes`,
    /// `negrisk-no`, `binary`.
    pub detector: String,
    /// `true-arb` / `rel-value`. Two prices disagreeing is not a locked profit and the UI
    /// must not let the operator forget which is which.
    pub label: String,
    pub gross_pct: String,
    pub spread_pct: Option<String>,
    pub net_maker_bps: Option<String>,
    pub net_taker_bps: String,
    pub net_taker_positive: bool,
    pub size_usd: String,
    pub verdict: String,
    pub verdict_display: String,
    pub maker_only: bool,
    pub detected_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineRow {
    /// `ok` or `warn`. There is no `error` tone: a condition that severe takes the page.
    pub dot: &'static str,
    pub name: &'static str,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryRow {
    pub name: String,
    pub rate_display: String,
    pub count: i64,
    pub median_net_maker_bps: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub time: String,
    pub kind: &'static str,
    pub subject: String,
    pub detail: String,
}

/// Why the page is blind. Ordered by how much it invalidates: if we cannot trust the
/// daemon's own status, nothing below it can be trusted either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakReason {
    /// The daemon has never published a runtime status row.
    NoStatus,
    /// It published one, but not recently enough to describe the present. Since M7.1 the
    /// heartbeat is independent of the scan loop, so this means the *process* is gone —
    /// not that it is busy.
    StatusStale,
    /// The heartbeat is current — the process is alive — but the scan loop has finished no
    /// unit of work in a long time. A slow loop is not this: progress is recorded per book
    /// batch, so a 195 s REST sweep reports throughout.
    LoopStalled,
    /// `ApiError::EmptyDiscovery` — passes parse cleanly and yield zero events.
    EmptyDiscovery,
    /// The socket is down *and* REST is failing: a network outage, not a fallback.
    TransportsDown,
    /// Every held book is past its staleness window; detection would run on stale depth.
    AllBooksStale,
}

impl BreakReason {
    /// The Rust identifier the operator can grep for. `2d`'s eyebrow shows this verbatim.
    pub fn eyebrow(self) -> &'static str {
        match self {
            Self::NoStatus => "runtime_status::Missing",
            Self::StatusStale => "runtime_status::Stale",
            Self::LoopStalled => "dryrun::LoopStalled",
            Self::EmptyDiscovery => "ApiError::EmptyDiscovery",
            Self::TransportsDown => "transport::BothDown",
            Self::AllBooksStale => "ws::AllBooksStale",
        }
    }

    /// Whether this break makes the transport pill meaningless.
    ///
    /// For a stalled loop it does not: the heartbeat publishes shard counts straight from
    /// the live pool, so those numbers are current even while the loop is wedged — and
    /// "sockets fine, loop stuck" is precisely the diagnosis worth showing.
    fn blinds_transport(self) -> bool {
        !matches!(self, Self::LoopStalled)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakState {
    pub reason: BreakReason,
    pub eyebrow: &'static str,
    pub headline_html: String,
    pub body_html: String,
    pub cards: Vec<BreakCard>,
    pub log: Vec<LogLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakCard {
    pub label: String,
    pub value: String,
    pub value_tone: &'static str,
    pub suffix: Option<String>,
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------------
// Break-state derivation (pure, so every trigger and non-trigger is a unit test)
// ---------------------------------------------------------------------------------

/// Decide whether the scanner is **blind rather than idle**.
///
/// The triggers are exactly the conditions under which the numbers below would be
/// meaningless, and nothing else. Deliberate non-triggers, each of which has burned this
/// project before:
///
/// * a **quiet socket** — silence is the normal state of most books, never a fault (M6.1);
/// * a **REST fallback** — degraded latency, fully working detection;
/// * a **fee-model fallback** — a `warn` dot in the rail, not an outage;
/// * an **alert cooldown** — the message was held, the measurement was not;
/// * a **slow loop** (M7.1) — a full REST sweep of the live universe takes ~195 s and
///   reports progress on every book batch. Only the *absence* of progress counts, and only
///   after `loop_stall_secs`.
///
/// `loop_stall_secs` of `0` switches the stall verdict off.
pub fn derive_break(
    status: Option<&RuntimeStatusRecord>,
    now: DateTime<Utc>,
    loop_stall_secs: u64,
) -> Option<BreakReason> {
    let Some(record) = status else {
        return Some(BreakReason::NoStatus);
    };
    if (now - record.updated_at) > Duration::seconds(RUNTIME_STATUS_STALE_SECS) {
        return Some(BreakReason::StatusStale);
    }
    let s = &record.status;

    // The row is fresh, so the process is alive. The remaining question — is its loop
    // moving? — is what the heartbeat was split off to make answerable. Before the first
    // unit of work completes there is nothing to measure but the daemon's own start, which
    // is the right reference: a daemon that has been up for ten minutes without finishing
    // anything is wedged, however early it is.
    if loop_stall_secs > 0 {
        let since = s
            .last_progress_at
            .as_deref()
            .and_then(parse_ts)
            .or_else(|| parse_ts(&s.started_at));
        if let Some(since) = since {
            if (now - since) > Duration::seconds(loop_stall_secs as i64) {
                return Some(BreakReason::LoopStalled);
            }
        }
    }

    // A universe of zero events is blind on the first empty pass; a universe we still hold
    // survives a few, because detection keeps running against the last good one.
    let empty_discovery = s.discovery_empty_streak >= EMPTY_DISCOVERY_BLIND_STREAK
        || (s.universe_events == 0 && s.discovery_empty_streak >= 1);
    if empty_discovery {
        return Some(BreakReason::EmptyDiscovery);
    }

    // Both transports down is an outage. The socket being down *alone* is a fallback, and
    // REST failing *alone* while the stream still pushes is a degraded integrity net.
    let ws_down = !s.stream_enabled || s.stream_shards_connected == 0;
    if ws_down && !s.rest_ok && s.rest_failure_streak >= REST_DOWN_STREAK {
        return Some(BreakReason::TransportsDown);
    }

    if s.books_total > 0 && s.books_stale >= s.books_total {
        return Some(BreakReason::AllBooksStale);
    }
    None
}

/// The header's transport pill, which is the break state's own summary when blind.
fn transport(status: Option<&RuntimeStatus>, blind: bool) -> Transport {
    if blind {
        return Transport {
            state: "blind".into(),
            label: "BLIND".into(),
        };
    }
    let Some(s) = status else {
        return Transport {
            state: "blind".into(),
            label: "BLIND".into(),
        };
    };
    if !s.stream_enabled {
        // Configured off, not broken — saying "fallback" here would report a failure that
        // did not happen.
        return Transport {
            state: "rest_only".into(),
            label: "REST POLLING".into(),
        };
    }
    if s.stream_rest_only || s.stream_shards_connected == 0 {
        return Transport {
            state: "rest_fallback".into(),
            label: "REST FALLBACK".into(),
        };
    }
    Transport {
        state: "streaming".into(),
        label: format!("STREAMING · {} shards", s.stream_shards_connected),
    }
}

// ---------------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------------

/// Read the store and build the whole snapshot. The only function the routes call.
pub fn build(
    cfg: &Config,
    store: &Store,
    now: DateTime<Utc>,
) -> Result<DashboardState, StoreError> {
    let record = store.read_runtime_status()?;
    let status = record.as_ref().map(|r| &r.status);
    let reason = derive_break(record.as_ref(), now, cfg.dashboard.loop_stall_secs);

    let window = Duration::hours(cfg.dashboard.window_hours as i64);
    let from = now - window;
    let rows = store.opportunities_since(from, MAX_WINDOW_ROWS)?;
    let scan = store.scan_totals_since(from)?;
    let stored_rows = store.opportunity_row_count()?;
    let first_hour = store.first_scan_hour()?;

    let survivors = rows
        .iter()
        .filter(|r| r.status == "filled_simulated")
        .count() as i64;
    let vanished = rows.iter().filter(|r| r.status == "vanished").count() as i64;

    let hero = Hero {
        soak_day: soak_day(first_hour.as_deref(), status, now),
        soak_days: cfg.dashboard.soak_days,
        survivors,
        // Maker basis, stated in the label and again in the note under it. This is a
        // hypothetical: we never rest an order, so a maker fill is never observed.
        net_edge_maker: money(
            rows.iter()
                .filter(|r| r.status == "filled_simulated")
                .filter_map(|r| r.simulated_pnl_maker)
                .sum::<Decimal>(),
        ),
        median_net_maker_bps: median_bps(&rows, |r| r.net_maker).map(signed),
        median_net_taker_bps: median_bps(&rows, |r| Some(r.net_taker)).map(signed),
        latency_p50_ms: percentile_i64(&latencies(&rows), 50),
        latency_p95_ms: percentile_i64(&latencies(&rows), 95),
    };

    let funnel = funnel(cfg, &scan, &rows, survivors, vanished);
    let opportunities: Vec<OpportunityView> =
        rows.iter().take(cfg.dashboard.max_rows).map(view).collect();
    let categories = categories(cfg, &rows);
    let crypto_zero = categories
        .iter()
        .any(|c| c.name == "crypto" && c.count == 0);

    let opportunity_note = format!(
        "{} events · {} above the {} net floor in the last {}h · {}",
        status.map(|s| s.universe_events).unwrap_or(0),
        rows.len(),
        floor_pct(status),
        cfg.dashboard.window_hours,
        match status {
            Some(s) if s.stream_enabled && !s.stream_rest_only => "dirty-event driven",
            Some(_) => "REST-timer driven",
            None => "no daemon status",
        }
    );

    Ok(DashboardState {
        generated_at: crate::store::now_str(now),
        poll_interval_ms: cfg.dashboard.poll_interval_ms,
        header: Header {
            mode: status
                .map(|s| s.mode.clone())
                .unwrap_or_else(|| cfg.mode.clone()),
            mode_pill: "DRY-RUN · LOCKED".into(),
            transport: transport(status, reason.is_some_and(BreakReason::blinds_transport)),
            universe_events: status.map(|s| s.universe_events).unwrap_or(0),
            refresh_secs: status
                .map(|s| s.universe_refresh_secs)
                .unwrap_or(cfg.daemon.universe_refresh_secs),
            uptime: status
                .and_then(|s| parse_ts(&s.started_at))
                .map(|t| ago(now - t))
                .unwrap_or_else(|| "—".into()),
            last_good_ago: status
                .and_then(|s| s.last_good_discovery_at.as_deref().and_then(parse_ts))
                .map(|t| format!("{} ago", ago(now - t))),
        },
        break_state: reason.map(|reason| break_state(reason, record.as_ref(), &rows, now)),
        hero,
        funnel,
        opportunities,
        opportunity_note,
        net_floor_pct: floor_pct(status),
        pipeline: pipeline(status, stored_rows, now, cfg.dashboard.loop_stall_secs),
        categories,
        crypto_zero,
        log: log_lines(&rows),
    })
}

// ---- funnel ---------------------------------------------------------------------

/// Five stages, and an honest account of which of them this pipeline actually measures.
///
/// Stages 1, 3 and 4 are counted per *construction evaluated, per detection pass* — a gap
/// that survives twelve passes is twelve. Stage 5 counts *distinct opportunities*, because
/// a lifecycle verdict belongs to a row, not to a pass. The two bases cannot be divided
/// into each other, so stage 5's percentage is taken against distinct rows and the panel
/// says so.
///
/// Stage 2 is **not instrumented**, and inventing it would be the exact dishonesty this
/// dashboard exists to avoid: polyarb computes every gap on the executable book side from
/// the start (never mid, never last), so it has no pre-spread population to filter. The
/// spread is already paid inside stage 1. Measuring it would mean adding a mid-price gap
/// count to the detector — see the M7 notes.
fn funnel(
    cfg: &Config,
    scan: &crate::store::WindowScanTotals,
    rows: &[DashboardRow],
    survivors: i64,
    vanished: i64,
) -> Funnel {
    let detected = scan.gaps_detected;
    let distinct = rows.len() as i64;
    let resolved = survivors + vanished;

    let mut stages = vec![
        FunnelStage {
            key: "detected",
            label: "Gaps detected".into(),
            count: Some(detected),
            count_display: Some(thousands(detected)),
            pct: Some("100%".into()),
            bar_pct: Some("100".into()),
            instrumented: true,
            basis: "pass",
            note: "Sum of executable asks below the payout across binary and NegRisk events, \
                   before any cost was charged."
                .into(),
        },
        FunnelStage {
            key: "spread",
            label: "Survive bid–ask spread".into(),
            count: None,
            count_display: None,
            pct: None,
            bar_pct: None,
            instrumented: false,
            basis: "pass",
            note: "Not instrumented. Every gap above is already computed on the sides you \
                   would lift, never mid — so there is no pre-spread population to filter \
                   here, and a number in this row would be invented."
                .into(),
        },
        FunnelStage {
            key: "fee",
            label: "Survive taker fee curve".into(),
            count: Some(scan.fee_survivors),
            count_display: Some(thousands(scan.fee_survivors)),
            pct: Some(pct_of(scan.fee_survivors, detected)),
            bar_pct: bar(scan.fee_survivors, detected),
            instrumented: true,
            basis: "pass",
            note: "fee = shares × rate × p × (1−p), charged with slippage at the smallest \
                   tradeable size; worst exactly at 50/50, where gaps cluster."
                .into(),
        },
        FunnelStage {
            key: "depth",
            label: "Fillable at walked size".into(),
            count: Some(scan.opportunities),
            count_display: Some(thousands(scan.opportunities)),
            pct: Some(pct_of(scan.opportunities, detected)),
            bar_pct: bar(scan.opportunities, detected),
            instrumented: true,
            basis: "pass",
            note: format!(
                "Depth-walked VWAP still clears the category net floor at ≥ {} shares, \
                 inside the ${} per-trade cap.",
                cfg.scan.min_size_shares, cfg.risk.per_trade_cap_usd
            ),
        },
        FunnelStage {
            key: "held",
            label: "Still there on re-poll".into(),
            count: Some(survivors),
            count_display: Some(thousands(survivors)),
            pct: Some(pct_of(survivors, distinct)),
            bar_pct: bar(survivors, distinct.max(1)),
            instrumented: true,
            basis: "row",
            note: format!(
                "filled_simulated, of {distinct} distinct opportunities. The {vanished} that \
                 had gone are recorded vanished and never revised."
            ),
        },
    ];
    // The first row is the 100% baseline, not an achievement; the design dims it.
    if detected == 0 {
        for stage in &mut stages {
            stage.pct = None;
            stage.bar_pct = None;
        }
        // Stage 5 stands on its own denominator and survives an unmeasured stage 1.
        if let Some(last) = stages.last_mut() {
            last.pct = Some(pct_of(survivors, distinct));
            last.bar_pct = bar(survivors, distinct.max(1));
        }
    }

    Funnel {
        window_hours: cfg.dashboard.window_hours,
        stages,
        vanished_pct: (resolved > 0).then(|| pct_of(vanished, resolved)),
        vanished,
        // A window whose telemetry predates M7 has no gap counters for part of its span.
        partial_window: scan.buckets > 0 && detected == 0 && scan.cycles > 0,
    }
}

// ---- opportunity rows ------------------------------------------------------------

fn view(row: &DashboardRow) -> OpportunityView {
    OpportunityView {
        id: row.id,
        event_title: row.event_title.clone(),
        event_slug: row.event_slug.clone(),
        category: row.category.clone(),
        fee_display: if row.fee_rate.is_zero() {
            "fee-free".into()
        } else {
            row.fee_rate.normalize().to_string()
        },
        detector: detector_name(&row.kind).into(),
        label: match row.label.as_str() {
            "true-arb" => "true-arb".into(),
            "relative-value" => "rel-value".into(),
            other => other.to_string(),
        },
        gross_pct: pct_of_payout(row.gross_gap, row.payout),
        spread_pct: row.spread_cost.map(|s| pct_of_payout(s, row.payout)),
        net_maker_bps: row.net_maker.map(|n| signed(bps(n, row.payout))),
        net_taker_bps: signed(bps(row.net_taker, row.payout)),
        net_taker_positive: row.net_taker > Decimal::ZERO,
        size_usd: money(row.capital_required),
        verdict: row.status.clone(),
        verdict_display: verdict_display(&row.status).into(),
        maker_only: row.maker_only,
        detected_at: row.detected_at.clone(),
    }
}

/// The detector names exactly as the design asks — and they are the Rust variants'
/// database strings, not a parallel vocabulary.
fn detector_name(kind: &str) -> &str {
    match kind {
        "binary_yes_no" => "binary",
        "neg_risk_yes_side" => "negrisk-yes",
        "neg_risk_no_side" => "negrisk-no",
        other => other,
    }
}

fn verdict_display(status: &str) -> &str {
    match status {
        "filled_simulated" => "filled·sim",
        other => other,
    }
}

// ---- rail ------------------------------------------------------------------------

fn pipeline(
    status: Option<&RuntimeStatus>,
    stored_rows: i64,
    now: DateTime<Utc>,
    loop_stall_secs: u64,
) -> Vec<PipelineRow> {
    let Some(s) = status else {
        return vec![PipelineRow {
            dot: "warn",
            name: "Daemon",
            value: "no runtime status".into(),
        }];
    };
    let mut rows = Vec::new();

    // The loop's own row (M7.1). The heartbeat proves the process is up; this is the only
    // line that says whether the loop is moving — including while it is merely slow, which
    // is a `warn`, not a takeover.
    let progress_age = s
        .last_progress_at
        .as_deref()
        .and_then(parse_ts)
        .map(|t| now - t);
    rows.push(PipelineRow {
        dot: match progress_age {
            Some(age) if loop_stall_secs > 0 && age.num_seconds() > loop_stall_secs as i64 => {
                "warn"
            }
            Some(_) => "ok",
            None => "warn",
        },
        name: "Scan loop",
        value: match progress_age {
            Some(age) => format!(
                "{} ticks · {} since {}",
                thousands(s.ticks_completed as i64),
                ago(age),
                s.last_progress_phase
            ),
            None => "no progress recorded yet".into(),
        },
    });

    rows.push(if s.stream_enabled {
        PipelineRow {
            dot: if s.stream_shards_connected >= s.stream_shards && s.stream_shards > 0 {
                "ok"
            } else {
                "warn"
            },
            name: "Stream shards",
            value: format!(
                "{}/{} connected",
                s.stream_shards_connected, s.stream_shards
            ),
        }
    } else {
        PipelineRow {
            dot: "ok",
            name: "Stream shards",
            value: "disabled in config".into(),
        }
    });

    // Staleness is explicit invalidation only. A quiet book is not a stale one, so "no
    // updates for Ns" is never surfaced as a fault here.
    rows.push(PipelineRow {
        dot: if s.books_stale == 0 { "ok" } else { "warn" },
        name: "Books maintained",
        value: format!("{} · {} stale", thousands(s.books_total), s.books_stale),
    });

    rows.push(PipelineRow {
        dot: if s.rest_ok { "ok" } else { "warn" },
        name: "REST role",
        value: match (s.stream_enabled, s.stream_rest_only, s.rest_ok) {
            (_, _, false) => format!("failing · {} in a row", s.rest_failure_streak),
            (false, _, true) => "sole transport".into(),
            (true, true, true) => "FALLBACK ACTIVE".into(),
            (true, false, true) => "seed + slow sweep".into(),
        },
    });

    rows.push(PipelineRow {
        dot: if s.discovery_empty_streak == 0 && s.discovery_error.is_none() {
            "ok"
        } else {
            "warn"
        },
        name: "Universe refresh",
        value: format!("{} events", thousands(s.universe_events)),
    });

    rows.push(PipelineRow {
        dot: if s.fee_fallback_markets == 0 {
            "ok"
        } else {
            "warn"
        },
        name: "Fee model fallback",
        value: format!("{} markets → table", thousands(s.fee_fallback_markets)),
    });

    rows.push(PipelineRow {
        dot: "ok",
        name: "Store",
        value: format!("sqlite · {} rows", thousands(stored_rows)),
    });

    rows.push(PipelineRow {
        dot: if s.telegram_circuit_open {
            "warn"
        } else {
            "ok"
        },
        name: "Telegram",
        value: format!(
            "{} · {} cooldown",
            if s.telegram_circuit_open {
                "open"
            } else {
                "closed"
            },
            s.alerts_cooldown_held
        ),
    });
    rows
}

/// Category tiles: everything the window saw, plus the focus categories at zero.
///
/// A zero is a result. Crypto's in particular is an instrumentation gap, and the caveat
/// under this block is mandatory wherever it appears.
fn categories(cfg: &Config, rows: &[DashboardRow]) -> Vec<CategoryRow> {
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<String, Vec<Option<Decimal>>> = BTreeMap::new();
    for name in crate::config::REPORTED_CATEGORIES {
        buckets.entry(name.to_string()).or_default();
    }
    for row in rows {
        buckets
            .entry(row.category.clone())
            .or_default()
            .push(row.net_maker.map(|n| bps(n, row.payout)));
    }

    let fees = crate::costs::FeeModel::new(cfg.fees.clone());
    let mut out: Vec<CategoryRow> = buckets
        .into_iter()
        .map(|(name, nets)| {
            // The count is every opportunity in the category; the median is over the ones
            // that had a maker side at all. A construction with no bid on every leg is not
            // quotable as a maker, and dropping it from the *count* as well would quietly
            // shrink the category to "the rows we could compute a median for".
            let count = nets.len() as i64;
            let mut quotable: Vec<Decimal> = nets.into_iter().flatten().collect();
            quotable.sort();
            let rate = fees.rate_for(&crate::types::Category::new(name.clone()));
            CategoryRow {
                rate_display: if rate.is_zero() {
                    "0".into()
                } else {
                    rate.normalize().to_string()
                },
                count,
                median_net_maker_bps: percentile_dec(&quotable, 50).map(signed),
                name,
            }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The log, derived strictly from stored facts: a detection line per opportunity row and a
/// verdict line per resolved one, merged newest-first. Nothing here is synthesised — the
/// dashboard reads SQLite and only SQLite, so it reports what the daemon wrote down.
fn log_lines(rows: &[DashboardRow]) -> Vec<LogLine> {
    let mut lines: Vec<(String, LogLine)> = Vec::new();
    for row in rows.iter().take(40) {
        lines.push((
            row.detected_at.clone(),
            LogLine {
                time: clock(&row.detected_at),
                kind: "detect",
                subject: detector_name(&row.kind).to_string(),
                detail: format!(
                    "{} net_maker {} @ {}",
                    row.event_slug,
                    row.net_maker
                        .map(|n| format!("{}bps", signed(bps(n, row.payout))))
                        .unwrap_or_else(|| "n/a".into()),
                    money(row.capital_required),
                ),
            },
        ));
        if let Some(resolved) = &row.resolved_at {
            lines.push((
                resolved.clone(),
                LogLine {
                    time: clock(resolved),
                    kind: "repoll",
                    subject: row.status.clone(),
                    detail: match row.persistence_ms {
                        Some(ms) => format!("{} walked fills held {}", row.event_slug, secs(ms)),
                        None => format!("{} no re-poll landed", row.event_slug),
                    },
                },
            ));
        }
    }
    lines.sort_by(|a, b| b.0.cmp(&a.0));
    lines.into_iter().map(|(_, line)| line).take(12).collect()
}

// ---- break state ------------------------------------------------------------------

fn break_state(
    reason: BreakReason,
    record: Option<&RuntimeStatusRecord>,
    rows: &[DashboardRow],
    now: DateTime<Utc>,
) -> BreakState {
    let status = record.map(|r| &r.status);
    let last_good = status
        .and_then(|s| s.last_good_discovery_at.as_deref().and_then(parse_ts))
        .map(|t| format!("{} ago", ago(now - t)))
        .unwrap_or_else(|| "never".into());

    let (headline_html, body_html) = match reason {
        BreakReason::EmptyDiscovery => (
            "Discovery returned <span class=\"num\">0</span> events. The scanner is not quiet — \
             it is blind, and every clean line below it is meaningless."
                .to_string(),
            format!(
                "{} consecutive <code>/events/keyset</code> pass(es) parsed successfully and \
                 yielded an empty <code>events[]</code>. Books are held from the last good pass \
                 ({last_good}); detection runs against the last known universe at best and is \
                 worth nothing at worst. Soak evidence for this window should be excluded from \
                 the daily summary.",
                status.map(|s| s.discovery_empty_streak).unwrap_or(0)
            ),
        ),
        BreakReason::TransportsDown => (
            "Both transports are down. This is a network outage, not a fallback — the daemon \
             is retrying both, forever."
                .to_string(),
            format!(
                "The stream has no live connection and the last {} REST call(s) failed. The \
                 daemon does <em>not</em> claim a REST fallback here: a fallback requires REST \
                 demonstrably working while the socket does not. Books are whatever the last \
                 good pass left behind.",
                status.map(|s| s.rest_failure_streak).unwrap_or(0)
            ),
        ),
        BreakReason::AllBooksStale => (
            "Every held book is past its staleness window. Detection would be pricing depth \
             that no longer exists."
                .to_string(),
            "These books were explicitly invalidated — by a reconnect, an out-of-order delta or \
             a contradictory hash — not merely quiet. Until the resync sweep repairs them, any \
             gap below is computed against depth we cannot vouch for."
                .to_string(),
        ),
        BreakReason::NoStatus => (
            "No daemon has published a runtime status. There is nothing here to trust yet."
                .to_string(),
            "The dashboard reads the same SQLite file the daemon writes, and that file carries \
             no <code>runtime_status</code> row. Either <code>polyarb run</code> has never been \
             started against this database, or it is a different database than the one you \
             think. Rows below, if any, are historical."
                .to_string(),
        ),
        BreakReason::StatusStale => (
            "The daemon stopped reporting. Every number below is the last thing it said, not \
             the present."
                .to_string(),
            format!(
                "The runtime status row is older than {RUNTIME_STATUS_STALE_SECS}s. That row is \
                 written by a heartbeat task with its own timer, independent of the scan loop — \
                 so a slow sweep can no longer cause this. The process is gone, or this database \
                 is not the one it writes."
            ),
        ),
        BreakReason::LoopStalled => (
            "The daemon is alive but its scan loop has stopped moving. Nothing below is being \
             re-measured."
                .to_string(),
            format!(
                "The heartbeat is current, so the process is up and answering. Its loop last \
                 finished a unit of work ({}) {} — and progress is recorded per book batch, not \
                 per sweep, so a slow sweep reports throughout and cannot produce this. It is \
                 stuck: a hung request, a wedged socket read, or a retry storm.{}",
                status
                    .map(|s| s.last_progress_phase.clone())
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "unknown phase".into()),
                status
                    .and_then(|s| s.last_progress_at.as_deref().and_then(parse_ts))
                    .map(|t| format!("{} ago", ago(now - t)))
                    .unwrap_or_else(|| "never".into()),
                match status.map(|s| s.watchdog_stall_secs) {
                    Some(0) | None => " The progress watchdog is disabled \
                                        (<code>daemon.watchdog_stall_secs = 0</code>), so nothing \
                                        will restart it on its own."
                        .to_string(),
                    Some(secs) => format!(
                        " The daemon's own watchdog exits the process after {secs}s without \
                         progress, so a restart policy should be reviving it about now."
                    ),
                },
            ),
        ),
    };

    let status_age = record
        .map(|r| ago(now - r.updated_at))
        .unwrap_or_else(|| "never".into());
    let cards = vec![
        BreakCard {
            label: "Events discovered".into(),
            value: status
                .map(|s| thousands(s.universe_events))
                .unwrap_or_else(|| "—".into()),
            value_tone: if status.map(|s| s.universe_events).unwrap_or(0) == 0 {
                "bad"
            } else {
                "plain"
            },
            suffix: None,
            note: Some(format!("last good discovery {last_good}")),
        },
        BreakCard {
            label: "Transports".into(),
            value: match status {
                Some(s) => format!("{}/{} shards", s.stream_shards_connected, s.stream_shards),
                None => "—".into(),
            },
            value_tone: "plain",
            suffix: None,
            note: Some(match status {
                Some(s) if s.rest_ok => "REST answering".into(),
                Some(s) => format!("REST failing · {} in a row", s.rest_failure_streak),
                None => "unknown".into(),
            }),
        },
        BreakCard {
            label: "Books held".into(),
            value: match status {
                Some(s) => format!("{} fresh", thousands(s.books_total - s.books_stale)),
                None => "—".into(),
            },
            value_tone: "plain",
            suffix: None,
            note: Some(match status {
                Some(s) => format!("{} past staleness", thousands(s.books_stale)),
                None => "unknown".into(),
            }),
        },
        // The fourth card answers the question this particular break raises. For a stalled
        // loop that is the loop's age, not the heartbeat's — the heartbeat is fresh, and
        // showing "3s" under a red headline would read as a contradiction.
        if reason == BreakReason::LoopStalled {
            BreakCard {
                label: "Loop progress".into(),
                value: status
                    .and_then(|s| s.last_progress_at.as_deref().and_then(parse_ts))
                    .map(|t| ago(now - t))
                    .unwrap_or_else(|| "never".into()),
                value_tone: "bad",
                suffix: Some("ago".into()),
                note: Some(match status {
                    Some(s) => format!(
                        "{} ticks completed · heartbeat {} old",
                        thousands(s.ticks_completed as i64),
                        status_age
                    ),
                    None => "no row in runtime_status".into(),
                }),
            }
        } else {
            BreakCard {
                label: "Runtime status".into(),
                value: status_age,
                value_tone: if matches!(reason, BreakReason::NoStatus | BreakReason::StatusStale) {
                    "bad"
                } else {
                    "plain"
                },
                suffix: None,
                note: Some(match status {
                    Some(s) => {
                        format!("alerts sent {} · failed {}", s.alerts_sent, s.alerts_failed)
                    }
                    None => "no row in runtime_status".into(),
                }),
            }
        },
    ];

    let mut log = Vec::new();
    if let Some(s) = status {
        // Always, whatever the reason: where the loop was and how long ago is the first
        // thing to know about any of these states.
        log.push(LogLine {
            time: record.map(|r| clock_dt(r.updated_at)).unwrap_or_default(),
            kind: "loop",
            subject: format!("phase {}", s.last_progress_phase),
            detail: format!(
                "{} ticks · last progress {}",
                thousands(s.ticks_completed as i64),
                s.last_progress_at
                    .as_deref()
                    .and_then(parse_ts)
                    .map(|t| format!("{} ago", ago(now - t)))
                    .unwrap_or_else(|| "never".into())
            ),
        });
        if let Some(err) = &s.discovery_error {
            log.push(LogLine {
                time: record.map(|r| clock_dt(r.updated_at)).unwrap_or_default(),
                kind: "discovery",
                subject: format!("empty streak {}", s.discovery_empty_streak),
                detail: err.clone(),
            });
        }
        log.push(LogLine {
            time: record.map(|r| clock_dt(r.updated_at)).unwrap_or_default(),
            kind: "stream",
            subject: format!(
                "{}/{} shards connected",
                s.stream_shards_connected, s.stream_shards
            ),
            detail: format!(
                "{} frames · {} delta entries applied · {} unrecognized",
                thousands(s.frames as i64),
                thousands(s.delta_entries_applied as i64),
                thousands(s.frames_unrecognized as i64)
            ),
        });
    }
    log.extend(log_lines(rows).into_iter().take(3));
    BreakState {
        reason,
        eyebrow: reason.eyebrow(),
        headline_html,
        body_html,
        cards,
        log,
    }
}

// ---------------------------------------------------------------------------------
// Formatting — every one of these returns a string, and none of them touches an f64
// ---------------------------------------------------------------------------------

/// Basis points against the construction's own payout (`$1`, or `$(N−1)` for a NegRisk
/// NO-side sweep). Dividing by the payout is what makes a two-leg and a nine-leg
/// construction comparable at all.
pub fn bps(value: Decimal, payout: Decimal) -> Decimal {
    if payout.is_zero() {
        return Decimal::ZERO;
    }
    (value / payout * Decimal::from(10_000))
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
}

fn pct_of_payout(value: Decimal, payout: Decimal) -> String {
    if payout.is_zero() {
        return "—".into();
    }
    format!("{:.2}%", value / payout * Decimal::ONE_HUNDRED)
}

fn signed(value: Decimal) -> String {
    if value >= Decimal::ZERO {
        format!("+{value}")
    } else {
        value.to_string()
    }
}

/// Dollars, always to the cent, with thousands separators.
///
/// Rounded and never truncated: `Decimal`'s `{:.2}` truncates, which would print $49.998
/// as $49.99 next to a mean of $50.00 and read as an arithmetic bug (the daily summary
/// makes the same choice). Cents are kept at every magnitude — the shipped per-trade cap
/// is $50, so a real walked size is *mostly* cents, and a column that drops them above
/// $1,000 and keeps them below would jitter between two shapes as values cross.
fn money(value: Decimal) -> String {
    let cents = value.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    let whole = cents.trunc().to_string().parse::<i64>().unwrap_or(0);
    let fraction = (cents.abs().fract() * Decimal::ONE_HUNDRED)
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
    format!("${}.{:02}", thousands(whole), fraction)
}

fn thousands(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

fn pct_of(part: i64, whole: i64) -> String {
    if whole <= 0 {
        return "—".into();
    }
    let pct = Decimal::from(part) * Decimal::ONE_HUNDRED / Decimal::from(whole);
    format!("{}%", pct.round_dp(1).normalize())
}

/// Bar width, floored at 1.2% so a surviving handful is still a visible mark.
fn bar(part: i64, whole: i64) -> Option<String> {
    if whole <= 0 {
        return None;
    }
    let pct = Decimal::from(part) * Decimal::ONE_HUNDRED / Decimal::from(whole);
    let floored = pct.max(Decimal::new(12, 1)).min(Decimal::ONE_HUNDRED);
    Some(floored.round_dp(2).normalize().to_string())
}

fn floor_pct(status: Option<&RuntimeStatus>) -> String {
    status
        .and_then(|s| s.net_floor_default_taker.parse::<Decimal>().ok())
        .map(|f| format!("{}%", (f * Decimal::ONE_HUNDRED).normalize()))
        .unwrap_or_else(|| "configured".into())
}

fn latencies(rows: &[DashboardRow]) -> Vec<i64> {
    let mut out: Vec<i64> = rows.iter().filter_map(|r| r.detection_latency_ms).collect();
    out.sort_unstable();
    out
}

fn median_bps(
    rows: &[DashboardRow],
    pick: impl Fn(&DashboardRow) -> Option<Decimal>,
) -> Option<Decimal> {
    let mut values: Vec<Decimal> = rows
        .iter()
        .filter_map(|r| pick(r).map(|v| bps(v, r.payout)))
        .collect();
    values.sort();
    percentile_dec(&values, 50)
}

/// Nearest-rank percentile, the same rule the daily summary uses.
fn percentile_dec(sorted: &[Decimal], pct: usize) -> Option<Decimal> {
    percentile(sorted, pct)
}

fn percentile_i64(sorted: &[i64], pct: usize) -> Option<i64> {
    percentile(sorted, pct)
}

fn percentile<T: Copy>(sorted: &[T], pct: usize) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * pct).div_ceil(100).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn clock(raw: &str) -> String {
    parse_ts(raw).map(clock_dt).unwrap_or_default()
}

fn clock_dt(ts: DateTime<Utc>) -> String {
    ts.format("%H:%M:%S").to_string()
}

fn secs(ms: i64) -> String {
    format!("{:.1}s", Decimal::from(ms) / Decimal::ONE_THOUSAND)
}

/// `4d 17h` / `17h 4m` / `4m` / `41s`.
fn ago(delta: Duration) -> String {
    let secs = delta.num_seconds().max(0);
    match secs {
        s if s >= 86_400 => format!("{}d {}h", s / 86_400, (s % 86_400) / 3_600),
        s if s >= 3_600 => format!("{}h {}m", s / 3_600, (s % 3_600) / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Which day of the soak this is, counted from the first scan cycle this database ever
/// recorded — not from process start, so a restart does not reset a five-day soak.
fn soak_day(first_hour: Option<&str>, status: Option<&RuntimeStatus>, now: DateTime<Utc>) -> u32 {
    let start = first_hour
        .and_then(|h| parse_ts(&format!("{h}:00:00Z")))
        .or_else(|| status.and_then(|s| parse_ts(&s.started_at)))
        .unwrap_or(now);
    let days = (now.date_naive() - start.date_naive()).num_days().max(0);
    (days + 1).min(u32::MAX as i64) as u32
}
