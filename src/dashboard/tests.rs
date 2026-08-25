//! M7 tests.
//!
//! Two things are under test here that are not ordinary rendering concerns:
//!
//! * **the page cannot act** — the route table is `GET`-only, the HTML carries no form,
//!   no `<button>` and no submit control, and a `POST` to any route is refused. That is
//!   the owner's requirement ("strictly read-only") turned into an assertion.
//! * **the break state fires for exactly the right reasons** — every trigger from the
//!   handoff, and every deliberate non-trigger (a quiet socket, a fee fallback, an alert
//!   cooldown, an honest REST fallback), because a page that cries blind at a quiet market
//!   would be trained away within a day.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{Duration, TimeZone, Utc};
use rust_decimal_macros::dec as d;
use tower::ServiceExt;

use super::state::{self, BreakReason, RUNTIME_STATUS_STALE_SECS};
use super::*;
use crate::store::{
    CycleStats, LifecycleOutcome, LifecycleStatus, RuntimeStatus, RuntimeStatusRecord,
};
use crate::types::{Category, Label, OpportunityKind};

// ---------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------

/// The shipped `dashboard.loop_stall_secs`. Every derivation test states it explicitly, so
/// a change to the default cannot silently change what these tests mean.
const LOOP_STALL_SECS: u64 = 300;

fn healthy_status() -> RuntimeStatus {
    RuntimeStatus {
        mode: "dry-run".into(),
        started_at: crate::store::now_str(Utc::now() - Duration::hours(30)),
        universe_events: 312,
        universe_markets: 1_240,
        universe_tokens: 2_480,
        universe_refreshed_at: Some(crate::store::now_str(Utc::now() - Duration::minutes(4))),
        universe_refresh_secs: 600,
        scan_interval_secs: 5,
        last_good_discovery_at: Some(crate::store::now_str(Utc::now() - Duration::minutes(4))),
        discovery_empty_streak: 0,
        discovery_error: None,
        stream_enabled: true,
        stream_shards: 4,
        stream_shards_connected: 4,
        stream_rest_only: false,
        books_total: 1_240,
        books_stale: 0,
        frames: 4_800_000,
        delta_entries_applied: 1_900_000,
        frames_unrecognized: 0,
        trade_prints: 4_012,
        // A loop that finished something a moment ago: alive *and* moving.
        last_progress_at: Some(crate::store::now_str(Utc::now() - Duration::seconds(2))),
        last_progress_phase: "rest_sweep".into(),
        ticks_completed: 8_412,
        last_tick_completed_at: Some(crate::store::now_str(Utc::now() - Duration::seconds(4))),
        watchdog_stall_secs: 600,
        rest_ok: true,
        rest_failure_streak: 0,
        fee_fallback_markets: 7,
        fee_unsupported_formula: 1,
        markets_with_api_fee: 1_233,
        telegram_circuit_open: false,
        alerts_sent: 41,
        alerts_failed: 0,
        alerts_cooldown_held: 2,
        // M8: a simulator that is running, tracking a handful of queues, with a live feed.
        maker_sim_enabled: true,
        maker_sim_window_secs: 3_600,
        maker_sims_open: 6,
        maker_sims_opened: 51,
        maker_sims_filled: 3,
        maker_sims_partial: 4,
        maker_sims_unfilled: 38,
        maker_sims_untracked: 0,
        maker_sim_prints_matched: 219,
        maker_sim_print_feed_live: true,
        // Measurement Phase R: present in the row, read by nothing on this page yet.
        rewardsim_enabled: true,
        rewardsim_markets_quoted: 6,
        rewardsim_portfolio: 4,
        rewardsim_samples_scored: 2_880,
        rewardsim_samples_lost: 12,
        rewardsim_fills: 37,
        rewardsim_epochs_closed: 8,
        rewardsim_print_feed_live: true,
        net_floor_default_taker: "0.005".into(),
    }
}

fn record(status: RuntimeStatus) -> RuntimeStatusRecord {
    RuntimeStatusRecord {
        updated_at: Utc::now(),
        status,
    }
}

/// A temp directory that removes itself, so a failing test cannot leave a database behind.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "polyarb-dash-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn db(&self) -> std::path::PathBuf {
        self.0.join("polyarb.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Seed a database the way the daemon would, then hand back a *read-only* app over it.
///
/// The writer connection is dropped before the reader opens, which is exactly the
/// deployment shape: two processes, one file, and the reader holding no write handle.
fn seeded_app(tag: &str) -> (TempDir, Arc<App>) {
    let dir = TempDir::new(tag);
    let path = dir.db();
    let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
    {
        let store = crate::store::Store::open(&path).expect("open");
        let books = crate::types::BookMap::new();

        // Three opportunities, deliberately different in every way the UI must keep apart:
        // a maker-positive/taker-negative row, a maker-only row, and a plain taker row.
        let mut op = crate::store::tests::sample_opportunity();
        op.event_slug = "nyc-mayor-2026".into();
        op.event_title = "NYC mayoral race — winner".into();
        op.kind = OpportunityKind::NegRiskYesSide;
        op.category = Category::new("politics");
        op.net_taker = d!(0.0031);
        op.net_maker = Some(d!(0.0118));
        op.capital_required = d!(18.40);
        let first = store
            .record_opportunity(&op, &books, now - Duration::minutes(30), Some(384))
            .expect("insert")
            .id();
        store
            .record_lifecycle(
                first,
                &LifecycleOutcome {
                    status: LifecycleStatus::FilledSimulated,
                    repolls: 3,
                    first_repoll_available: Some(true),
                    persistence_ms: Some(6_800),
                    simulated_pnl_taker: Some(d!(0.31)),
                    simulated_pnl_maker: Some(d!(1.18)),
                    resolved_at: now - Duration::minutes(29),
                },
            )
            .expect("lifecycle");

        let mut second = crate::store::tests::sample_opportunity();
        second.event_slug = "epl-title-26".into();
        second.event_title = "Premier League — title winner".into();
        second.kind = OpportunityKind::NegRiskNoSide;
        second.label = Label::RelativeValue;
        second.category = Category::new("sports");
        second.fee_rate = d!(0.05);
        second.legs[0].best_ask = d!(0.41);
        second.maker_only = true;
        second.net_taker = d!(-0.0018);
        second.net_maker = Some(d!(0.0043));
        second.capital_required = d!(4.10);
        let id = store
            .record_opportunity(&second, &books, now - Duration::minutes(20), None)
            .expect("insert")
            .id();
        store
            .record_lifecycle(
                id,
                &LifecycleOutcome {
                    status: LifecycleStatus::Vanished,
                    repolls: 1,
                    first_repoll_available: Some(false),
                    persistence_ms: None,
                    simulated_pnl_taker: None,
                    simulated_pnl_maker: None,
                    resolved_at: now - Duration::minutes(19),
                },
            )
            .expect("lifecycle");

        let mut third = crate::store::tests::sample_opportunity();
        third.event_slug = "isr-lbn-q4".into();
        third.event_title = "Israel–Lebanon ceasefire by Q4".into();
        third.category = Category::new("geopolitics");
        third.fee_rate = d!(0);
        third.legs[0].best_ask = d!(0.42);
        third.net_taker = d!(0.0088);
        third.net_maker = Some(d!(0.0088));
        third.capital_required = d!(6.20);
        store
            .record_opportunity(&third, &books, now - Duration::minutes(10), Some(610))
            .expect("insert");

        // M8: two closed maker-fill simulations against the maker-only row — one whose every
        // leg's queue traded through (credited, $0.74) and one that never filled a leg
        // (credited nothing). The lower bound the hero shows is the first alone, and it is
        // deliberately a different number from the $1.18 hypothetical above: the two must
        // never be able to pass for each other.
        let sim = |status: crate::makersim::SimStatus,
                   pnl: Option<rust_decimal::Decimal>,
                   opened: chrono::DateTime<Utc>| {
            crate::makersim::ClosedSim {
                opportunity_id: id,
                event_slug: "epl-title-26".into(),
                kind: "neg_risk_no_side".into(),
                category: "sports".into(),
                legs: vec![crate::makersim::PlacedLeg {
                    token_id: crate::types::TokenId::new("1001"),
                    price: d!(0.41),
                    visible_size: d!(120),
                    our_size: d!(100),
                    volume_through: d!(220),
                    prints_observed: 3,
                    filled_at: pnl.map(|_| opened + Duration::seconds(90)),
                    fill_ms: pnl.map(|_| 90_000),
                }],
                legs_total: 1,
                legs_filled: usize::from(pnl.is_some()),
                net_maker_total: d!(0.74),
                pnl_lower_bound: pnl,
                legging_exposure: None,
                prints_observed: 3,
                time_to_fill_ms: pnl.map(|_| 90_000),
                status,
                no_print_feed: false,
                opened_at: opened,
                closed_at: opened + Duration::minutes(60),
            }
        };
        store
            .record_maker_sim(&sim(
                crate::makersim::SimStatus::MakerFilled,
                Some(d!(0.74)),
                now - Duration::minutes(20),
            ))
            .expect("filled sim");
        store
            .record_maker_sim(&sim(
                crate::makersim::SimStatus::MakerUnfilled,
                None,
                now - Duration::minutes(18),
            ))
            .expect("unfilled sim");

        store
            .record_cycle(
                &CycleStats {
                    events: 312,
                    markets: 1_240,
                    books: 2_480,
                    opportunities: 118,
                    new_opportunities: 3,
                    duration_ms: 900,
                    failed: false,
                    gaps_detected: 4_182,
                    fee_survivors: 386,
                },
                now - Duration::minutes(30),
            )
            .expect("cycle");
        store
            .write_runtime_status(&healthy_status(), Utc::now())
            .expect("status");
    }

    let store = crate::store::Store::open_read_only(&path).expect("read-only open");
    let mut cfg = Config::default();
    // The seeded rows are timestamped relative to a fixed day; widen the window so the
    // test does not depend on when it runs.
    cfg.dashboard.window_hours = 24 * 365 * 20;
    (dir, Arc::new(App::new(Arc::new(cfg), Arc::new(store))))
}

async fn get(app: Arc<App>, path: &str) -> (StatusCode, String) {
    let response = router(app)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

// ---------------------------------------------------------------------------------
// The API snapshot
// ---------------------------------------------------------------------------------

#[tokio::test]
async fn api_state_reports_money_as_strings_never_as_json_numbers() {
    let (_dir, app) = seeded_app("money");
    let (status, body) = get(app, "/api/state").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");

    // A JSON number here would mean an f64 in a money path — the one thing this codebase
    // does not allow. Assert on the parsed types, not on the text.
    assert!(json["hero"]["net_edge_maker"].is_string());
    assert!(json["hero"]["median_net_maker_bps"].is_string());
    assert!(json["hero"]["median_net_taker_bps"].is_string());
    for opportunity in json["opportunities"].as_array().expect("array") {
        for field in [
            "gross_pct",
            "spread_pct",
            "net_maker_bps",
            "net_taker_bps",
            "size_usd",
        ] {
            let value = &opportunity[field];
            assert!(
                value.is_string() || value.is_null(),
                "{field} must be a string (money never crosses as a number), got {value}"
            );
        }
    }
    for category in json["categories"].as_array().expect("array") {
        let bps = &category["median_net_maker_bps"];
        assert!(
            bps.is_string() || bps.is_null(),
            "category bps must be a string"
        );
    }
    assert_eq!(json["hero"]["net_edge_maker"], "$1.18");
}

#[tokio::test]
async fn api_state_orders_newest_first_and_keeps_maker_and_taker_apart() {
    let (_dir, app) = seeded_app("order");
    let (_, body) = get(app, "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let rows = json["opportunities"].as_array().expect("array");

    let slugs: Vec<&str> = rows
        .iter()
        .map(|r| r["event_slug"].as_str().unwrap())
        .collect();
    assert_eq!(
        slugs,
        vec!["isr-lbn-q4", "epl-title-26", "nyc-mayor-2026"],
        "newest first, decided server-side"
    );

    // The maker-only row is the proof: its maker net is positive and its taker net is not,
    // and there is no field anywhere that adds the two bases together.
    let maker_only = rows
        .iter()
        .find(|r| r["event_slug"] == "epl-title-26")
        .expect("maker-only row");
    assert_eq!(maker_only["net_maker_bps"], "+43");
    assert_eq!(maker_only["net_taker_bps"], "-18");
    assert_eq!(maker_only["net_taker_positive"], false);
    assert!(maker_only["maker_only"].as_bool().unwrap());
    assert_eq!(maker_only["label"], "rel-value");

    let flat = body.to_lowercase();
    for forbidden in ["net_total", "net_combined", "net_blended"] {
        assert!(
            !flat.contains(forbidden),
            "no field may merge the two bases"
        );
    }
}

#[tokio::test]
async fn a_verdict_is_reported_terminally_and_never_promoted() {
    let (_dir, app) = seeded_app("verdict");
    let (_, body) = get(app, "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let rows = json["opportunities"].as_array().expect("array");

    let vanished = rows
        .iter()
        .find(|r| r["event_slug"] == "epl-title-26")
        .expect("row");
    assert_eq!(vanished["verdict"], "vanished");
    assert_eq!(vanished["verdict_display"], "vanished");

    let filled = rows
        .iter()
        .find(|r| r["event_slug"] == "nyc-mayor-2026")
        .expect("row");
    assert_eq!(filled["verdict"], "filled_simulated");
    assert_eq!(filled["verdict_display"], "filled·sim");

    // The dashboard reads the daemon's verdict verbatim: `vanished` counts as vanished in
    // the funnel and is never folded into the survivor number.
    assert_eq!(json["hero"]["survivors"], 1);
    let held = json["funnel"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "held")
        .expect("held stage");
    assert_eq!(held["count"], 1);
    assert_eq!(json["funnel"]["vanished"], 1);
}

#[tokio::test]
async fn the_funnel_says_which_stage_it_does_not_measure() {
    let (_dir, app) = seeded_app("funnel");
    let (_, body) = get(app, "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let stages = json["funnel"]["stages"].as_array().expect("stages");
    assert_eq!(stages.len(), 5);

    let detected = &stages[0];
    assert_eq!(detected["key"], "detected");
    assert_eq!(detected["count"], 4_182);
    assert_eq!(detected["basis"], "pass");

    // The spread stage is not instrumented, and says so rather than carrying a number.
    let spread = &stages[1];
    assert_eq!(spread["key"], "spread");
    assert_eq!(spread["instrumented"], false);
    assert!(spread["count"].is_null());
    assert!(spread["note"]
        .as_str()
        .unwrap()
        .contains("Not instrumented"));

    assert_eq!(stages[2]["count"], 386);
    assert_eq!(stages[3]["count"], 118);
    // The last stage counts rows, not passes, and labels itself accordingly.
    assert_eq!(stages[4]["basis"], "row");
}

// ---------------------------------------------------------------------------------
// Break-state derivation
// ---------------------------------------------------------------------------------

#[test]
fn a_healthy_daemon_is_not_blind() {
    assert_eq!(
        state::derive_break(Some(&record(healthy_status())), Utc::now(), LOOP_STALL_SECS),
        None
    );
}

#[test]
fn empty_discovery_takes_the_page() {
    // Three clean passes yielding nothing, with the previous universe still held.
    let mut status = healthy_status();
    status.discovery_empty_streak = 3;
    assert_eq!(
        state::derive_break(Some(&record(status)), Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::EmptyDiscovery)
    );

    // And immediately when there is no universe at all — a daemon that has never got one
    // is blind on its first empty pass, not on its third.
    let mut cold = healthy_status();
    cold.universe_events = 0;
    cold.discovery_empty_streak = 1;
    assert_eq!(
        state::derive_break(Some(&record(cold)), Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::EmptyDiscovery)
    );

    // One empty pass with a universe still in hand is not yet a takeover: detection keeps
    // running against the last good universe.
    let mut once = healthy_status();
    once.discovery_empty_streak = 1;
    assert_eq!(
        state::derive_break(Some(&record(once)), Utc::now(), LOOP_STALL_SECS),
        None
    );
}

#[test]
fn both_transports_down_is_an_outage_and_says_so() {
    let mut status = healthy_status();
    status.stream_shards_connected = 0;
    status.rest_ok = false;
    status.rest_failure_streak = 3;
    assert_eq!(
        state::derive_break(Some(&record(status)), Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::TransportsDown)
    );
}

#[test]
fn all_books_past_staleness_takes_the_page() {
    let mut status = healthy_status();
    status.books_stale = status.books_total;
    assert_eq!(
        state::derive_break(Some(&record(status)), Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::AllBooksStale)
    );

    // Some stale books are a rail warning, not a takeover.
    let mut partial = healthy_status();
    partial.books_stale = 41;
    assert_eq!(
        state::derive_break(Some(&record(partial)), Utc::now(), LOOP_STALL_SECS),
        None
    );
}

#[test]
fn a_missing_or_stale_runtime_status_is_itself_a_break() {
    assert_eq!(
        state::derive_break(None, Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::NoStatus)
    );

    let stale = RuntimeStatusRecord {
        updated_at: Utc::now() - Duration::seconds(RUNTIME_STATUS_STALE_SECS + 5),
        status: healthy_status(),
    };
    assert_eq!(
        state::derive_break(Some(&stale), Utc::now(), LOOP_STALL_SECS),
        Some(BreakReason::StatusStale)
    );
}

/// M7.1. The heartbeat runs in its own task, so a fresh status row proves the *process* is
/// alive and nothing more. These are the four answers that separation has to produce.
#[test]
fn a_stalled_loop_is_its_own_break_and_a_slow_one_is_not() {
    let now = Utc::now();
    let with_progress = |secs: i64| {
        let mut s = healthy_status();
        s.last_progress_at = Some(crate::store::now_str(now - Duration::seconds(secs)));
        s
    };

    // (a) Slow but progressing. A full REST sweep at live scale takes ~195 s and reports on
    // every book batch, so this is the normal shape of a busy daemon — never a takeover.
    assert_eq!(
        state::derive_break(Some(&record(with_progress(200))), now, LOOP_STALL_SECS),
        None
    );

    // (b) Stalled: nothing finished for longer than the threshold, while the heartbeat kept
    // publishing. That is the state the old single-timestamp design could not express.
    assert_eq!(
        state::derive_break(
            Some(&record(with_progress(LOOP_STALL_SECS as i64 + 60))),
            now,
            LOOP_STALL_SECS
        ),
        Some(BreakReason::LoopStalled)
    );

    // (c) A daemon gone is still a daemon gone: a stale row outranks the loop verdict,
    // because a status we cannot trust says nothing about a loop.
    let gone = RuntimeStatusRecord {
        updated_at: now - Duration::seconds(RUNTIME_STATUS_STALE_SECS + 5),
        status: with_progress(LOOP_STALL_SECS as i64 + 60),
    };
    assert_eq!(
        state::derive_break(Some(&gone), now, LOOP_STALL_SECS),
        Some(BreakReason::StatusStale)
    );

    // (d) Switched off.
    assert_eq!(
        state::derive_break(
            Some(&record(with_progress(LOOP_STALL_SECS as i64 + 600))),
            now,
            0
        ),
        None
    );

    // Before the first unit of work there is nothing but the daemon's own start to measure
    // from — which is the right reference: starting up is fine, never getting going is not.
    let mut starting = healthy_status();
    starting.last_progress_at = None;
    starting.started_at = crate::store::now_str(now - Duration::seconds(30));
    assert_eq!(
        state::derive_break(Some(&record(starting.clone())), now, LOOP_STALL_SECS),
        None
    );
    starting.started_at =
        crate::store::now_str(now - Duration::seconds(LOOP_STALL_SECS as i64 * 2));
    assert_eq!(
        state::derive_break(Some(&record(starting)), now, LOOP_STALL_SECS),
        Some(BreakReason::LoopStalled)
    );
}

/// The stalled page has to read differently from the blind ones: the transports are fine
/// and their numbers are live (the heartbeat reads the pool directly), so the pill must not
/// claim BLIND and the copy must name the loop.
#[tokio::test]
async fn the_stalled_loop_page_names_the_loop_and_keeps_the_transport_pill_honest() {
    let dir = TempDir::new("stalled");
    let path = dir.db();
    let mut status = healthy_status();
    status.last_progress_at = Some(crate::store::now_str(
        Utc::now() - Duration::seconds(LOOP_STALL_SECS as i64 + 120),
    ));
    status.last_progress_phase = "rest_sweep".into();
    {
        let store = crate::store::Store::open(&path).expect("open");
        store
            .write_runtime_status(&status, Utc::now())
            .expect("status");
    }
    let store = crate::store::Store::open_read_only(&path).expect("read-only");
    let app = Arc::new(App::new(Arc::new(Config::default()), Arc::new(store)));
    let (code, html) = get(app, "/").await;

    assert_eq!(code, StatusCode::OK);
    assert!(html.contains("dryrun::LoopStalled"));
    assert!(html.contains("data-break=\"loopstalled\""));
    assert!(
        html.contains("rest_sweep"),
        "the page must name where the loop stopped"
    );
    assert!(
        html.contains("watchdog"),
        "the page must say what is about to happen to the process"
    );
    assert!(
        html.contains("STREAMING · 4 shards") && !html.contains(">BLIND<"),
        "the sockets are fine and their counts are live — claiming BLIND would misdiagnose it"
    );
}

#[test]
fn the_deliberate_non_triggers_stay_non_triggers() {
    let now = Utc::now();

    // A quiet socket. Frames have stopped arriving but every shard is connected and no
    // book was invalidated: this is what most of the universe looks like most of the time,
    // and M6.1 exists precisely because silence was once read as staleness.
    let mut quiet = healthy_status();
    quiet.frames = 4_800_000;
    quiet.delta_entries_applied = 0;
    quiet.books_stale = 0;
    assert_eq!(
        state::derive_break(Some(&record(quiet)), now, LOOP_STALL_SECS),
        None
    );

    // A fee-model fallback is a warn dot in the rail, not an outage.
    let mut fees = healthy_status();
    fees.fee_fallback_markets = 1_240;
    fees.fee_unsupported_formula = 12;
    assert_eq!(
        state::derive_break(Some(&record(fees)), now, LOOP_STALL_SECS),
        None
    );

    // An alert cooldown means the message was held, not that the measurement was lost.
    let mut cooldown = healthy_status();
    cooldown.alerts_cooldown_held = 96;
    cooldown.telegram_circuit_open = true;
    assert_eq!(
        state::derive_break(Some(&record(cooldown)), now, LOOP_STALL_SECS),
        None
    );

    // An honest REST fallback: the socket is gone, REST demonstrably works. Degraded
    // latency, working detection — the pill changes, the page does not.
    let mut fallback = healthy_status();
    fallback.stream_shards_connected = 0;
    fallback.stream_rest_only = true;
    fallback.rest_ok = true;
    assert_eq!(
        state::derive_break(Some(&record(fallback)), now, LOOP_STALL_SECS),
        None
    );

    // REST failing on its own, while the stream still pushes, is a degraded integrity net.
    let mut rest_flaky = healthy_status();
    rest_flaky.rest_ok = false;
    rest_flaky.rest_failure_streak = 9;
    assert_eq!(
        state::derive_break(Some(&record(rest_flaky)), now, LOOP_STALL_SECS),
        None
    );
}

// ---------------------------------------------------------------------------------
// The page itself
// ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_page_renders_the_evidence_console() {
    let (_dir, app) = seeded_app("html");
    let (status, html) = get(app, "/").await;
    assert_eq!(status, StatusCode::OK);

    // The standing assurance that no order path is live.
    assert!(html.contains("DRY-RUN · LOCKED"));
    assert!(html.contains("polyarb"));

    // Every funnel label, including the one that admits it is not measured.
    for label in [
        "Where the gaps die",
        "Gaps detected",
        "Survive bid–ask spread",
        "Survive taker fee curve",
        "Fillable at walked size",
        "Still there on re-poll",
    ] {
        assert!(html.contains(label), "missing funnel label: {label}");
    }
    assert!(html.contains("not instrumented"));

    // Maker and taker are separate columns, and stay separate.
    assert!(html.contains("Net mk") && html.contains("Net tk"));
    assert!(html.contains("filled·sim") && html.contains("vanished"));
    // The detector names are the Rust ones.
    assert!(html.contains("negrisk-yes") && html.contains("negrisk-no"));
    // The label the repo insists on not forgetting.
    assert!(html.contains("true-arb") && html.contains("rel-value"));
}

/// (i) M8 — the simulated lower bound reaches the page as a *string*, next to the
/// hypothetical it bounds, with both labels visible; and the rail says how many simulations
/// are in flight.
#[tokio::test]
async fn the_maker_lower_bound_travels_as_a_string_and_is_labelled_apart() {
    let (_dir, app) = seeded_app("makersim");
    let (_, body) = get(app.clone(), "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");

    // Money, so a string — never a JSON number, and never parsed back in the page.
    assert!(json["hero"]["maker_lower_bound"].is_string());
    assert_eq!(json["hero"]["maker_lower_bound"], "$0.74");
    assert!(json["hero"]["maker_fill_rate_pct"].is_string());
    assert_eq!(json["hero"]["maker_fill_rate_pct"], "50%");
    // The hypothetical is still its own field: the two are never merged into one number.
    assert!(json["hero"]["net_edge_maker"].is_string());
    assert_ne!(
        json["hero"]["maker_lower_bound"],
        json["hero"]["net_edge_maker"]
    );

    let sims = json["pipeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Maker sims")
        .expect("the rail names the simulator");
    assert_eq!(sims["value"], "6 open · fill rate 50%");
    assert_eq!(sims["dot"], "ok");

    let (_, html) = get(app, "/").await;
    assert!(html.contains("Simulated maker P&amp;L"));
    assert!(
        html.contains("sim lower bound (last in queue)"),
        "the bound must say what assumption produced it"
    );
    assert!(
        html.contains("if always filled"),
        "the hypothetical must keep its own label right beside it"
    );
    assert!(html.contains("Maker sims"));
}

#[tokio::test]
async fn a_crypto_zero_always_carries_its_caveat() {
    let (_dir, app) = seeded_app("crypto");
    let (_, body) = get(app.clone(), "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let crypto = json["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "crypto")
        .expect("crypto is always listed, including at zero");
    assert_eq!(crypto["count"], 0);
    assert_eq!(crypto["rate_display"], "0.07");
    assert_eq!(json["crypto_zero"], true);

    // A category's count is every opportunity in it, not just the ones with a quotable
    // maker side — otherwise the tile silently shrinks to "rows we could take a median of".
    let sports = json["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "sports")
        .expect("sports");
    assert_eq!(
        sports["count"], 1,
        "the maker-only row still counts as an opportunity"
    );

    let (_, html) = get(app, "/").await;
    assert!(
        html.contains("means not looked at properly, not no edge"),
        "a blank crypto cell that reads as 'no edge' is a wrong conclusion the dashboard \
         would be causing"
    );
}

#[tokio::test]
async fn the_page_has_no_control_surface_at_all() {
    let (_dir, app) = seeded_app("readonly");
    let (_, html) = get(app.clone(), "/").await;

    for forbidden in [
        "<button",
        "<form",
        "<input",
        "<select",
        "<textarea",
        "method=\"post\"",
    ] {
        assert!(
            !html.to_lowercase().contains(forbidden),
            "the dashboard must carry no control: found {forbidden}"
        );
    }
    // The two links the break state offers are navigation, and the page says why there is
    // nothing else.
    let (_blind_dir, blind) = blind_app("readonly-blind");
    let (_, break_html) = get(blind, "/").await;
    assert!(break_html.contains("read-only · nothing to pause, the daemon cannot trade"));
    assert!(!break_html.to_lowercase().contains("<button"));
}

#[tokio::test]
async fn every_route_is_a_get_and_nothing_else_is_routed() {
    let (_dir, app) = seeded_app("routes");
    assert!(
        ROUTES.iter().all(|(method, _)| *method == "GET"),
        "a non-GET route would make this page something other than a monitor"
    );

    for (_, path) in ROUTES {
        let (status, _) = get(app.clone(), path).await;
        assert_eq!(status, StatusCode::OK, "GET {path}");

        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let response = router(app.clone())
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(*path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} must not be routed"
            );
        }
    }

    // Nothing outside the table exists.
    let (missing, _) = get(app, "/api/orders").await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_assets_are_embedded_and_reference_nothing_external() {
    let (_dir, app) = seeded_app("assets");
    for (_, path) in ROUTES.iter().filter(|(_, p)| p.starts_with("/assets/")) {
        let (status, body) = get(app.clone(), path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty(), "{path} is embedded in the binary");
        assert!(
            !body.contains("http://") && !body.contains("https://"),
            "{path} must not reach out to a third party from an operator's monitor"
        );
    }
    let (_, html) = get(app, "/").await;
    assert!(!html.contains("//fonts.googleapis.com"));
    assert!(!html.contains("unpkg.com"));
}

/// An app whose store has no `runtime_status` row at all → the page must take itself over.
fn blind_app(tag: &str) -> (TempDir, Arc<App>) {
    let dir = TempDir::new(tag);
    let path = dir.db();
    {
        crate::store::Store::open(&path).expect("open");
    }
    let store = crate::store::Store::open_read_only(&path).expect("read-only");
    (
        dir,
        Arc::new(App::new(Arc::new(Config::default()), Arc::new(store))),
    )
}

#[tokio::test]
async fn the_break_state_takes_over_the_whole_page() {
    let (_dir, app) = blind_app("break");
    let (status, html) = get(app, "/").await;
    assert_eq!(status, StatusCode::OK);

    assert!(html.contains("runtime_status::Missing"));
    assert!(html.contains("BLIND"));
    // The pill is on every screen state, including this one.
    assert!(html.contains("DRY-RUN · LOCKED"));
    // …and none of the clean numbers are shown next to it.
    assert!(!html.contains("Where the gaps die"));
    assert!(html.contains("data-break=\"nostatus\""));
}

/// A window with no surviving opportunity must say what was measured, never conclude that
/// there is no edge — and the crypto caveat is the same rule at category scale.
#[tokio::test]
async fn an_empty_window_reports_a_measurement_not_a_finding() {
    let dir = TempDir::new("empty");
    let path = dir.db();
    {
        let store = crate::store::Store::open(&path).expect("open");
        store
            .write_runtime_status(&healthy_status(), Utc::now())
            .expect("status");
    }
    let store = crate::store::Store::open_read_only(&path).expect("read-only");
    let app = Arc::new(App::new(Arc::new(Config::default()), Arc::new(store)));

    let (status, html) = get(app.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("None above the 0.5% net floor in the last 24h"));
    assert!(html.contains("not a finding about the market"));
    assert!(
        !html.to_lowercase().contains("no edge here"),
        "an empty window is not evidence of absence"
    );

    // The funnel still renders, with honest zeros and no percentages it cannot justify.
    let (_, body) = get(app, "/api/state").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["hero"]["survivors"], 0);
    assert!(json["hero"]["median_net_maker_bps"].is_null());
    assert!(json["funnel"]["vanished_pct"].is_null());
    assert_eq!(json["crypto_zero"], true);
}
