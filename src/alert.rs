//! Alerting and the structured event log.
//!
//! Two sinks, one call site:
//!
//! * **JSONL event log** (`logs/events.jsonl`) — always written, one JSON object per line:
//!   opportunities, lifecycle outcomes, daily summaries. This is the record of truth for
//!   the soak review; Telegram is a convenience on top of it.
//! * **Telegram** (send-only Bot API over `reqwest`) — optional. Credentials come from
//!   `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` and *only* from the environment
//!   (CLAUDE.md). When they are unset, or the API is unreachable, delivery degrades to
//!   the JSONL log plus an INFO line; it never panics and never retry-spams — repeated
//!   failures open a circuit breaker that re-probes after a cool-off.
//!
//! The bot token must never reach a log line, a JSONL record, or an error message.
//! [`redact`] scrubs it from anything we emit, and transport errors are stripped of their
//! URL (which embeds the token) before they are formatted.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use serde_json::json;

use crate::config::AlertConfig;
use crate::types::Opportunity;

/// Telegram rejects messages longer than 4096 characters.
const TELEGRAM_MAX_CHARS: usize = 4000;

// ---------------------------------------------------------------------------------
// JSONL event log
// ---------------------------------------------------------------------------------

/// Append-only JSONL sink. Each line is `{"ts", "event", ...payload}`.
pub struct EventLog {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl EventLog {
    /// Open `<dir>/events.jsonl` for appending, creating `dir` if needed.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event. Failures are logged, never propagated: losing a log line must
    /// not take the daemon down.
    pub fn append(&self, event: &str, payload: serde_json::Value) {
        let mut line = json!({
            "ts": crate::store::now_str(chrono::Utc::now()),
            "event": event,
        });
        if let (Some(obj), serde_json::Value::Object(extra)) = (line.as_object_mut(), payload) {
            obj.extend(extra);
        }
        let text = match serde_json::to_string(&line) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(%err, event, "could not serialise event for the JSONL log");
                return;
            }
        };
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(err) = writeln!(file, "{text}").and_then(|()| file.flush()) {
            tracing::warn!(%err, path = %self.path.display(), "could not write the JSONL log");
        }
    }
}

// ---------------------------------------------------------------------------------
// Alerter
// ---------------------------------------------------------------------------------

/// Whether an alert may be dropped by the anti-spam spacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Opportunity alerts — rate-limited so a busy scan cannot flood the chat.
    Routine,
    /// Daily summaries and start/stop notices — always attempted.
    Important,
}

/// Why an alert was or was not delivered. Recorded on every JSONL line so the soak review
/// can tell "no alert" from "alert suppressed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    NoCredentials,
    Disabled,
    RateLimited,
    CircuitOpen,
    Failed(String),
}

impl Delivery {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Sent => "sent",
            Self::NoCredentials => "no_credentials",
            Self::Disabled => "disabled",
            Self::RateLimited => "rate_limited",
            Self::CircuitOpen => "circuit_open",
            Self::Failed(reason) => reason,
        }
    }
}

struct Telegram {
    client: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
}

#[derive(Debug, Default)]
struct Circuit {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    last_sent: Option<Instant>,
    sent: u64,
    failed: u64,
    suppressed: u64,
}

/// Delivery counters, for the shutdown log line and the daily summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlertStats {
    pub sent: u64,
    pub failed: u64,
    pub suppressed: u64,
    pub circuit_open: bool,
}

pub struct Alerter {
    cfg: AlertConfig,
    events: std::sync::Arc<EventLog>,
    telegram: Option<Telegram>,
    circuit: Mutex<Circuit>,
}

impl Alerter {
    /// Build from config plus the environment. Missing credentials are a normal,
    /// non-fatal state: everything still lands in the JSONL log.
    pub fn new(cfg: &AlertConfig, events: std::sync::Arc<EventLog>) -> Self {
        let token = env_secret("TELEGRAM_BOT_TOKEN");
        let chat_id = env_secret("TELEGRAM_CHAT_ID");
        let telegram = match (token, chat_id) {
            (Some(token), Some(chat_id)) => reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.request_timeout_secs))
                .build()
                .map_err(|err| {
                    // `err` here is a builder error and carries no URL, but redact anyway.
                    tracing::warn!(error = %err, "could not build the Telegram client");
                })
                .ok()
                .map(|client| Telegram {
                    client,
                    api_base: cfg.telegram_api_base.trim_end_matches('/').to_string(),
                    token,
                    chat_id,
                }),
            _ => None,
        };

        if telegram.is_none() {
            tracing::info!(
                "Telegram credentials not set (TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID); \
                 alerts will be written to the JSONL log only"
            );
        }

        Self {
            cfg: cfg.clone(),
            events,
            telegram,
            circuit: Mutex::new(Circuit::default()),
        }
    }

    pub fn events(&self) -> &EventLog {
        &self.events
    }

    pub fn stats(&self) -> AlertStats {
        let c = self.lock();
        AlertStats {
            sent: c.sent,
            failed: c.failed,
            suppressed: c.suppressed,
            circuit_open: c.open_until.is_some_and(|until| Instant::now() < until),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Circuit> {
        self.circuit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send `text`, then record the attempt (content included) in the JSONL log.
    ///
    /// `payload` is merged into the JSONL line so machine-readable fields survive even
    /// when the human-readable message is all that reaches Telegram.
    pub async fn notify(
        &self,
        event: &str,
        text: &str,
        priority: Priority,
        mut payload: serde_json::Value,
    ) -> Delivery {
        let delivery = self.deliver(text, priority).await;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("alert_text".into(), json!(text));
            obj.insert("delivery".into(), json!(delivery.as_str()));
        }
        self.events.append(event, payload);
        match &delivery {
            Delivery::Sent => tracing::info!(event, "alert delivered to Telegram"),
            // The JSONL log is the fallback channel, and this INFO line makes the content
            // visible in the console log too.
            other => tracing::info!(event, delivery = other.as_str(), alert = %text, "alert"),
        }
        delivery
    }

    async fn deliver(&self, text: &str, priority: Priority) -> Delivery {
        if !self.cfg.enabled {
            return Delivery::Disabled;
        }
        let telegram = match &self.telegram {
            Some(t) => t,
            None => return Delivery::NoCredentials,
        };

        // Decide under the lock, then release it before awaiting.
        let now = Instant::now();
        {
            let mut c = self.lock();
            if let Some(until) = c.open_until {
                if now < until {
                    c.suppressed += 1;
                    return Delivery::CircuitOpen;
                }
                // Cool-off elapsed: let this one through as the re-probe.
                c.open_until = None;
                c.consecutive_failures = 0;
                tracing::info!("Telegram circuit breaker re-probing after cool-off");
            }
            if priority == Priority::Routine {
                let spacing = Duration::from_secs(self.cfg.min_seconds_between_alerts);
                if let Some(last) = c.last_sent {
                    if now.duration_since(last) < spacing {
                        c.suppressed += 1;
                        return Delivery::RateLimited;
                    }
                }
            }
        }

        let result = telegram.send(text).await;
        let mut c = self.lock();
        match result {
            Ok(()) => {
                c.consecutive_failures = 0;
                c.last_sent = Some(Instant::now());
                c.sent += 1;
                Delivery::Sent
            }
            Err(reason) => {
                c.failed += 1;
                c.consecutive_failures += 1;
                if c.consecutive_failures >= self.cfg.failure_circuit_break {
                    c.open_until =
                        Some(Instant::now() + Duration::from_secs(self.cfg.circuit_reprobe_secs));
                    tracing::warn!(
                        failures = c.consecutive_failures,
                        cool_off_secs = self.cfg.circuit_reprobe_secs,
                        "Telegram unreachable — opening the circuit breaker; \
                         alerts continue in the JSONL log"
                    );
                }
                Delivery::Failed(format!("failed:{reason}"))
            }
        }
    }

    /// Threshold gate for opportunity alerts: the best *net per share* the construction
    /// offers must clear `alert_min_net_per_share`.
    pub fn passes_threshold(&self, op: &Opportunity) -> bool {
        alertable_net(op) >= self.cfg.alert_min_net_per_share
    }
}

impl Telegram {
    async fn send(&self, text: &str) -> Result<(), String> {
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.token);
        let body = json!({
            "chat_id": self.chat_id,
            "text": truncate_chars(text, TELEGRAM_MAX_CHARS),
            "disable_web_page_preview": true,
        });
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            // `without_url` strips the token-bearing URL from the error before it is
            // formatted; `redact` is the belt to that braces.
            .map_err(|err| self.scrub(&err.without_url().to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(self.scrub(&format!(
            "http {} {}",
            status.as_u16(),
            truncate_chars(&body, 200)
        )))
    }

    fn scrub(&self, message: &str) -> String {
        redact(message, &self.token)
    }
}

/// Replace every occurrence of `secret` with `***`. Empty secrets are ignored so this can
/// be called unconditionally.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

fn env_secret(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The net-per-share number an alert should be judged on: a maker-only opportunity has no
/// meaningful taker net, so its maker net is the honest figure.
pub fn alertable_net(op: &Opportunity) -> Decimal {
    if op.maker_only {
        op.net_maker.unwrap_or(Decimal::ZERO)
    } else {
        op.net_taker
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

// ---------------------------------------------------------------------------------
// Message formatting (pure — unit-tested)
// ---------------------------------------------------------------------------------

/// Human-readable opportunity alert. Plain text: no Markdown/HTML parse mode, so no
/// escaping bugs and no risk of a market title breaking the message.
pub fn format_opportunity(op: &Opportunity) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[DRY-RUN] {} · {} · {}\n{}\n",
        op.kind.label(),
        op.label.as_str(),
        op.category,
        truncate_chars(&op.event_title, 90),
    ));
    for leg in &op.legs {
        out.push_str(&format!(
            "  {} {}  ask {:.4} vwap {:.4} x {:.2}\n",
            truncate_chars(&leg.question, 48),
            leg.outcome,
            leg.best_ask,
            leg.vwap,
            leg.size,
        ));
    }
    out.push_str(&format!(
        "per share: gross {:+.4}  slip -{:.4}  fee -{:.4}  net taker {:+.4}",
        op.gross_gap, op.slippage_cost, op.fee_taker, op.net_taker,
    ));
    if let Some(net_maker) = op.net_maker {
        out.push_str(&format!("  net maker {net_maker:+.4}"));
    }
    out.push_str(&format!(
        "\nsize {:.2} · capital ${:.2} · net taker ${:.2}",
        op.executable_size, op.capital_required, op.net_taker_total,
    ));
    if let Some(total) = op.net_maker_total {
        out.push_str(&format!(" · net maker ${total:.2}"));
    }
    if let Some((tracked, total)) = op.partial_coverage {
        // The single most important caveat on the message: without it a reader sees a
        // NegRisk sweep summing under $1 and assumes it is a lock.
        out.push_str(&format!(
            "\nNOT risk-free: {tracked} of {total} outcomes covered — {} outcome(s) of this \
             event were dropped in discovery and are unhedged. Relative value, not arbitrage.",
            total.saturating_sub(tracked)
        ));
    }
    if op.maker_only {
        out.push_str("\nMAKER-ONLY: taker net is below the floor; requires resting fills.");
    }
    if op.conversion_required {
        out.push_str("\nNO-side: payout needs the NegRisk conversion path (not implemented).");
    }
    out.push_str("\nNo order was placed (dry-run).");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::sample_opportunity;
    use rust_decimal_macros::dec;

    fn cfg() -> AlertConfig {
        AlertConfig::default()
    }

    #[test]
    fn opportunity_format_is_plain_text_and_marked_dry_run() {
        let op = sample_opportunity();
        let text = format_opportunity(&op);
        assert!(text.starts_with("[DRY-RUN]"));
        assert!(text.contains("binary YES+NO"));
        assert!(text.contains("politics"));
        assert!(text.contains("net taker +0.0305"));
        assert!(text.contains("net maker +0.0700"));
        assert!(text.contains("No order was placed (dry-run)."));
        assert!(!text.contains("MAKER-ONLY"));
    }

    #[test]
    fn maker_only_and_conversion_caveats_are_stated() {
        let mut op = sample_opportunity();
        op.maker_only = true;
        op.conversion_required = true;
        let text = format_opportunity(&op);
        assert!(text.contains("MAKER-ONLY"));
        assert!(text.contains("NegRisk conversion"));
    }

    /// The whole point of `redact`: a token must not survive into anything we emit.
    #[test]
    fn the_bot_token_never_appears_in_output() {
        let token = "8123456789:AAH-SECRET-TOKEN-VALUE";
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api_base: "https://api.telegram.org".into(),
            token: token.to_string(),
            chat_id: "-100123".into(),
        };
        let leaked = format!(
            "error sending request for url (https://api.telegram.org/bot{token}/sendMessage)"
        );
        let scrubbed = telegram.scrub(&leaked);
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("***"));

        assert_eq!(redact("nothing to do", ""), "nothing to do");

        // And the formatted alert body never carries credentials at all.
        let text = format_opportunity(&sample_opportunity());
        assert!(!text.contains(token));
        assert!(!text.to_ascii_lowercase().contains("token"));
    }

    #[test]
    fn threshold_uses_the_maker_net_for_maker_only_rows() {
        let mut cfg = cfg();
        cfg.alert_min_net_per_share = dec!(0.05);
        let events = std::sync::Arc::new(
            EventLog::open(&std::env::temp_dir().join("polyarb-test-threshold")).expect("log"),
        );
        let alerter = Alerter::new(&cfg, events);

        let mut op = sample_opportunity(); // net_taker 0.0305, net_maker 0.07
        assert!(!alerter.passes_threshold(&op), "taker net is below 0.05");
        op.maker_only = true;
        assert!(alerter.passes_threshold(&op), "maker net 0.07 clears 0.05");
        assert_eq!(alertable_net(&op), dec!(0.07));
    }

    #[tokio::test]
    async fn without_credentials_alerts_fall_back_to_the_jsonl_log() {
        let dir = std::env::temp_dir().join(format!("polyarb-test-jsonl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = std::sync::Arc::new(EventLog::open(&dir).expect("log"));
        let mut cfg = cfg();
        cfg.telegram_api_base = "http://127.0.0.1:1".into();
        let alerter = Alerter {
            cfg,
            events: events.clone(),
            telegram: None, // as if the env vars were unset
            circuit: Mutex::new(Circuit::default()),
        };

        let delivery = alerter
            .notify(
                "opportunity",
                "hello",
                Priority::Routine,
                serde_json::json!({"id": 7}),
            )
            .await;
        assert_eq!(delivery, Delivery::NoCredentials);

        let text = std::fs::read_to_string(events.path()).expect("read log");
        let line: serde_json::Value =
            serde_json::from_str(text.lines().next().expect("one line")).expect("json");
        assert_eq!(line["event"], "opportunity");
        assert_eq!(line["alert_text"], "hello");
        assert_eq!(line["delivery"], "no_credentials");
        assert_eq!(line["id"], 7);
        assert!(line["ts"].as_str().expect("ts").ends_with('Z'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repeated_failures_open_the_circuit_and_stop_the_retry_spam() {
        let dir = std::env::temp_dir().join(format!("polyarb-test-circuit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = std::sync::Arc::new(EventLog::open(&dir).expect("log"));
        let mut cfg = cfg();
        cfg.failure_circuit_break = 2;
        cfg.min_seconds_between_alerts = 0;
        cfg.request_timeout_secs = 1;
        // Port 1 is closed: every send fails fast without leaving the machine.
        cfg.telegram_api_base = "http://127.0.0.1:1".into();
        let alerter = Alerter {
            telegram: Some(Telegram {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(1))
                    .build()
                    .expect("client"),
                api_base: cfg.telegram_api_base.clone(),
                token: "111:SECRET".into(),
                chat_id: "42".into(),
            }),
            cfg,
            events: events.clone(),
            circuit: Mutex::new(Circuit::default()),
        };

        for _ in 0..2 {
            let d = alerter
                .notify("t", "x", Priority::Important, json!({}))
                .await;
            assert!(matches!(d, Delivery::Failed(_)), "got {d:?}");
        }
        // Circuit is now open: further alerts are suppressed rather than retried.
        let d = alerter
            .notify("t", "x", Priority::Important, json!({}))
            .await;
        assert_eq!(d, Delivery::CircuitOpen);

        let stats = alerter.stats();
        assert_eq!(stats.failed, 2);
        assert_eq!(stats.suppressed, 1);
        assert!(stats.circuit_open);

        // Nothing leaked into the log.
        let text = std::fs::read_to_string(events.path()).expect("read log");
        assert!(!text.contains("111:SECRET"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Exercises the real send path against a local stub of the Bot API: the message goes
    /// out as a `sendMessage` POST, and the token appears only in the URL we call — never
    /// in the returned status or anything we log.
    #[tokio::test]
    async fn a_successful_send_posts_send_message_and_reports_sent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut data = Vec::new();
            loop {
                let mut chunk = vec![0u8; 4096];
                match tokio::time::timeout(Duration::from_millis(300), socket.read(&mut chunk))
                    .await
                {
                    Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => {
                        data.extend_from_slice(&chunk[..n]);
                        if String::from_utf8_lossy(&data).contains("chat_id") {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&data).to_string());
            let body = r#"{"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let dir = std::env::temp_dir().join(format!("polyarb-test-send-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = std::sync::Arc::new(EventLog::open(&dir).expect("log"));
        let alerter = Alerter {
            telegram: Some(Telegram {
                client: reqwest::Client::new(),
                api_base: format!("http://{addr}"),
                token: "8123:SECRET".into(),
                chat_id: "-100999".into(),
            }),
            cfg: cfg(),
            events: events.clone(),
            circuit: Mutex::new(Circuit::default()),
        };

        let delivery = alerter
            .notify("opportunity", "hello world", Priority::Routine, json!({}))
            .await;
        assert_eq!(delivery, Delivery::Sent);
        assert_eq!(alerter.stats().sent, 1);

        let request = rx.await.expect("captured request");
        assert!(request.starts_with("POST /bot8123:SECRET/sendMessage"));
        assert!(request.contains("\"chat_id\":\"-100999\""));
        assert!(request.contains("hello world"));

        // The JSONL record keeps the content but never the credentials.
        let log = std::fs::read_to_string(events.path()).expect("log");
        assert!(log.contains("hello world"));
        assert!(log.contains("\"delivery\":\"sent\""));
        assert!(!log.contains("8123:SECRET"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn routine_alerts_are_rate_limited_but_summaries_are_not() {
        let dir = std::env::temp_dir().join(format!("polyarb-test-rate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = std::sync::Arc::new(EventLog::open(&dir).expect("log"));
        let mut cfg = cfg();
        cfg.min_seconds_between_alerts = 3_600;
        let alerter = Alerter {
            telegram: Some(Telegram {
                client: reqwest::Client::new(),
                api_base: "http://127.0.0.1:1".into(),
                token: "t".into(),
                chat_id: "c".into(),
            }),
            cfg,
            events,
            circuit: Mutex::new(Circuit::default()),
        };
        // Pretend a message just went out.
        alerter.lock().last_sent = Some(Instant::now());

        assert_eq!(
            alerter.notify("o", "a", Priority::Routine, json!({})).await,
            Delivery::RateLimited
        );
        // An Important alert is attempted regardless (and fails: nothing is listening).
        assert!(matches!(
            alerter
                .notify("s", "b", Priority::Important, json!({}))
                .await,
            Delivery::Failed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
