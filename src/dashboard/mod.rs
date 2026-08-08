//! M7 — the read-only soak dashboard.
//!
//! ## Read-only by construction, not by convention
//!
//! `polyarb dashboard` is a **separate process** from `polyarb run`. It shares no memory,
//! no channel and no lock with the daemon; it opens the same SQLite file with
//! `SQLITE_OPEN_READ_ONLY` (WAL allows concurrent readers) and serves `GET` routes only.
//! There is no `POST` route, no form, and no control on the page — because there is
//! nothing to control: the daemon has no signing code, no wallet, and `mode` is locked to
//! `dry-run`. The absence of a kill switch is the point, not an omission.
//!
//! ## How the page stays live
//!
//! `GET /` renders the whole page server-side. `GET /api/state` returns the same snapshot
//! as JSON, which the page polls every `dashboard.poll_interval_ms` and applies to the
//! cells whose values changed. Money never crosses that boundary as a number: every
//! monetary field is a `Decimal` rendered to a string in `state.rs`, and the JavaScript
//! never parses one back.
//!
//! ## What it can show that SQLite alone cannot
//!
//! Stream shard counts, the REST role, the Telegram breaker — all of that lives in daemon
//! memory. The daemon republishes it to a single `runtime_status` row every few seconds
//! (see [`crate::store::RuntimeStatus`]), which is what lets a reader distinguish "the
//! market is quiet" from "the scanner is blind" — the whole point of the break state.

pub mod render;
pub mod state;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use chrono::Utc;

use crate::config::Config;
use crate::store::{Store, DASHBOARD_MIN_SCHEMA};

/// Every route this server has, as data, so the "read-only" claim is testable rather than
/// merely stated. If a method other than `GET` ever appears here, the test fails.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/"),
    ("GET", "/api/state"),
    ("GET", "/assets/nocturne-styles.css"),
    ("GET", "/assets/app.css"),
    ("GET", "/assets/app.js"),
];

/// Nocturne, vendored so the binary is self-contained and the container needs no docs/
/// directory (which `.dockerignore` excludes). The upstream `@import` of Google Fonts is
/// removed: the page must make no external request, so Inter is a preference in the font
/// stack and `system-ui` does the work when it is absent.
const NOCTURNE_CSS: &str = include_str!("assets/nocturne-styles.css");
const APP_CSS: &str = include_str!("assets/app.css");
const APP_JS: &str = include_str!("assets/app.js");

pub struct App {
    cfg: Arc<Config>,
    store: Arc<Store>,
}

impl App {
    pub fn new(cfg: Arc<Config>, store: Arc<Store>) -> Self {
        Self { cfg, store }
    }
}

/// The router. Split out so tests can drive it without binding a port.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/assets/nocturne-styles.css", get(css_nocturne))
        .route("/assets/app.css", get(css_app))
        .route("/assets/app.js", get(js_app))
        .with_state(app)
}

/// `polyarb dashboard`.
pub async fn serve(cfg: Config) -> Result<()> {
    let addr = cfg.dashboard.socket_addr()?;
    let path = std::path::PathBuf::from(&cfg.storage.database_path);
    let store = Store::open_read_only(&path).with_context(|| {
        format!(
            "could not open {} for reading. The dashboard never creates or migrates a \
             database — start `polyarb run` against it first.",
            path.display()
        )
    })?;
    let version = store.schema_version()?;
    anyhow::ensure!(
        version >= DASHBOARD_MIN_SCHEMA,
        "{} is at schema v{version}, but the dashboard needs v{DASHBOARD_MIN_SCHEMA} \
         (runtime_status + funnel counters + maker_sims). Run `polyarb run` once to \
         migrate it — the \
         dashboard opens the file read-only and cannot migrate anything itself.",
        path.display()
    );

    let app = Arc::new(App::new(Arc::new(cfg), Arc::new(store)));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind dashboard.bind = {addr}"))?;
    tracing::info!(
        %addr,
        db = app.store.path(),
        routes = %ROUTES
            .iter()
            .map(|(method, path)| format!("{method} {path}"))
            .collect::<Vec<_>>()
            .join(", "),
        "dashboard listening — read-only (SQLITE_OPEN_READ_ONLY, the routes above and \
         nothing else, no control surface: the daemon cannot trade)"
    );
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received — stopping the dashboard");
        })
        .await
        .context("the dashboard server stopped with an error")?;
    Ok(())
}

// ---------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------

async fn index(State(app): State<Arc<App>>) -> Response {
    match state::build(&app.cfg, &app.store, Utc::now()) {
        Ok(snapshot) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            render::page(&snapshot),
        )
            .into_response(),
        Err(err) => store_error(err),
    }
}

async fn api_state(State(app): State<Arc<App>>) -> Response {
    match state::build(&app.cfg, &app.store, Utc::now()) {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(err) => store_error(err),
    }
}

/// A read failure is reported as a read failure. The alternative — rendering a page with
/// zeroes — is the failure mode this whole screen exists to prevent.
fn store_error(err: crate::store::StoreError) -> Response {
    tracing::warn!(%err, "dashboard could not read the store");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("could not read the store: {err}\n"),
    )
        .into_response()
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // Assets are compiled into the binary, so their lifetime is the process's.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn css_nocturne() -> Response {
    asset(NOCTURNE_CSS, "text/css; charset=utf-8")
}

async fn css_app() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8")
}

async fn js_app() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

#[cfg(test)]
mod tests;
