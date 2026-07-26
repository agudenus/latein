//! polyarb — Polymarket arbitrage scanner.
//!
//! Phase A, milestones M1 (`markets`) and M2 (`scan`). Read-only: this binary never
//! signs, never places orders, and needs no wallet or API credentials.

mod clob;
mod config;
mod costs;
mod detect;
mod gamma;
mod http;
mod risk;
mod types;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;

use crate::clob::ClobClient;
use crate::config::Config;
use crate::costs::FeeModel;
use crate::gamma::{DiscoveryStats, GammaClient};
use crate::http::HttpClient;
use crate::risk::RiskLimits;
use crate::types::{BookMap, Category, Opportunity, Universe};

#[derive(Debug, Parser)]
#[command(
    name = "polyarb",
    version,
    about = "Polymarket arbitrage scanner (dry-run only — never places orders)"
)]
struct Cli {
    /// Path to the TOML config (default: config/default.toml, or $POLYARB_CONFIG).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Verbose logging (equivalent to RUST_LOG=polyarb=debug).
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover the tracked market universe and fetch one batch of order books.
    Markets {
        /// Override scan.max_events for this run.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// How many events to list individually.
        #[arg(long, default_value_t = 20, value_name = "N")]
        show: usize,
    },
    /// Run the detectors once over freshly fetched books and print opportunities.
    Scan {
        /// Override scan.max_events for this run.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Emit opportunities as JSON instead of a human report.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let mut cfg = Config::load(cli.config.as_deref()).context("failed to load configuration")?;

    match cli.command {
        Command::Markets { limit, show } => {
            if let Some(n) = limit {
                cfg.scan.max_events = n;
            }
            run_markets(&cfg, show).await
        }
        Command::Scan { limit, json } => {
            if let Some(n) = limit {
                cfg.scan.max_events = n;
            }
            run_scan(&cfg, json).await
        }
    }
}

fn init_tracing(verbose: bool) {
    let default = if verbose {
        "polyarb=debug"
    } else {
        "polyarb=info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Shared M1 ingestion: discover the universe, then fetch every book in batches.
async fn ingest(cfg: &Config) -> Result<(Universe, DiscoveryStats, BookMap)> {
    let http = HttpClient::new(&cfg.api).context("failed to build the HTTP client")?;

    tracing::info!(url = %cfg.api.gamma_base_url, "discovering markets");
    let (universe, stats) = GammaClient::new(&http, cfg)
        .fetch_universe()
        .await
        .with_context(|| {
            format!(
                "market discovery via the Gamma API at {} failed",
                cfg.api.gamma_base_url
            )
        })?;

    let tokens = universe.token_ids();
    tracing::info!(
        events = universe.events.len(),
        markets = universe.market_count(),
        tokens = tokens.len(),
        "fetching order books"
    );
    let books = ClobClient::new(&http, cfg)
        .fetch_books(&tokens)
        .await
        .with_context(|| {
            format!(
                "order book fetch via the CLOB API at {} failed",
                cfg.api.clob_base_url
            )
        })?;

    Ok((universe, stats, books))
}

async fn run_markets(cfg: &Config, show: usize) -> Result<()> {
    let (universe, stats, books) = ingest(cfg).await?;
    let fees = FeeModel::new(cfg.fees.clone());

    println!(
        "Tracked universe ({}, mode={})",
        cfg.api.gamma_base_url, cfg.mode
    );
    println!(
        "  events:  {} kept / {} seen  ({} inactive or closed)",
        stats.events_kept, stats.events_seen, stats.events_inactive
    );
    println!(
        "  markets: {} kept / {} seen  ({} inactive, {} unusable token ids)",
        stats.markets_kept, stats.markets_seen, stats.markets_inactive, stats.markets_unusable
    );
    println!(
        "  negrisk events: {} of {}",
        universe.neg_risk_event_count(),
        universe.events.len()
    );

    let tokens = universe.token_ids();
    let with_asks = books.values().filter(|b| b.best_ask().is_some()).count();
    let two_sided = books.values().filter(|b| b.spread().is_some()).count();
    println!(
        "  books:   {} returned / {} requested  ({with_asks} with a live ask, {two_sided} quoted two-sided)",
        books.len(),
        tokens.len(),
    );

    // Category breakdown, with the fee tier each one will be costed at.
    let mut by_category: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for event in &universe.events {
        let entry = by_category
            .entry(event.category.as_str().to_string())
            .or_default();
        entry.0 += 1;
        entry.1 += event.markets.len();
    }
    println!("\n  category        fee     net floor   events  markets");
    for (name, (events, markets)) in &by_category {
        let category = Category::new(name.clone());
        println!(
            "  {:<14}  {:<6}  {:<9}   {:>6}  {:>7}",
            name,
            format!("{:.2}", fees.rate_for(&category)),
            format!("{:.4}", cfg.net_floor_for(&category)),
            events,
            markets
        );
    }

    if show > 0 {
        println!("\n  events (first {show}):");
        for event in universe.events.iter().take(show) {
            let priced = event
                .markets
                .iter()
                .filter(|m| books.contains_key(m.yes_token()) && books.contains_key(m.no_token()))
                .count();
            let n = event.markets.len();
            println!(
                "    [{}] {:<12} {n:>2} market{} ({priced} fully priced)  {}",
                if event.neg_risk { "negrisk" } else { "binary " },
                event.category.as_str(),
                if n == 1 { " " } else { "s" },
                truncate(&event.title, 60),
            );
        }
    }

    Ok(())
}

async fn run_scan(cfg: &Config, json: bool) -> Result<()> {
    let (universe, _stats, books) = ingest(cfg).await?;
    let opportunities = detect::scan(cfg, &universe, &books);

    if json {
        let out = serde_json::to_string_pretty(&opportunities)
            .context("failed to serialise opportunities")?;
        println!("{out}");
        return Ok(());
    }

    let limits = RiskLimits::from_config(&cfg.risk);
    println!(
        "Scanned {} events / {} markets / {} books — {} opportunit{} \
         (mode={}, per-trade cap ${:.2})",
        universe.events.len(),
        universe.market_count(),
        books.len(),
        opportunities.len(),
        if opportunities.len() == 1 { "y" } else { "ies" },
        cfg.mode,
        limits.per_trade_cap_usd(),
    );
    if opportunities.is_empty() {
        println!("\nNo gap survived the executable-side, fee and depth filters.");
        return Ok(());
    }

    for (i, op) in opportunities.iter().enumerate() {
        print_opportunity(i + 1, op);
    }
    Ok(())
}

fn print_opportunity(index: usize, op: &Opportunity) {
    println!("\n─────────────────────────────────────────────────────────────────────────");
    println!(
        "#{index}  {}  [{}]  {}  fee_rate={:.2}{}",
        op.kind.label(),
        op.label.as_str(),
        op.category,
        op.fee_rate,
        if op.maker_only { "  MAKER-ONLY" } else { "" }
    );
    println!("      {}", truncate(&op.event_title, 70));
    println!("      event: {}", op.event_slug);

    println!(
        "\n      {:<38} {:>8} {:>8} {:>10} {:>12}",
        "leg", "bid", "ask", "vwap", "ask depth"
    );
    for leg in &op.legs {
        println!(
            "      {:<38} {:>8} {:>8} {:>10} {:>12}",
            truncate(&format!("{} · {}", leg.outcome, leg.question), 38),
            leg.best_bid
                .map(|b| format!("{b:.4}"))
                .unwrap_or_else(|| "—".to_string()),
            format!("{:.4}", leg.best_ask),
            format!("{:.4}", leg.vwap),
            format!("{:.2}", leg.ask_depth),
        );
    }

    println!(
        "\n      per share (payout ${:.2} per share-set):",
        op.payout
    );
    println!(
        "        gross gap  (executable asks)  {:>12}",
        signed(op.gross_gap)
    );
    println!(
        "        slippage to size              {:>12}",
        signed(-op.slippage_cost)
    );
    println!(
        "        taker fee                     {:>12}",
        signed(-op.fee_taker)
    );
    println!(
        "        = net as TAKER                {:>12}",
        signed(op.net_taker)
    );
    match (op.spread_cost, op.net_maker) {
        (Some(spread), Some(net_maker)) => {
            println!(
                "        spread not crossed            {:>12}",
                signed(spread)
            );
            println!(
                "        = net as MAKER (fee-free)     {:>12}",
                signed(net_maker)
            );
        }
        _ => println!("        maker side: no bid on every leg — not quotable"),
    }

    println!(
        "\n      size {:.2} shares/leg · capital ${:.2} · net taker ${:.2}{}",
        op.executable_size,
        op.capital_required,
        op.net_taker_total,
        op.net_maker_total
            .map(|n| format!(" · net maker ${n:.2}"))
            .unwrap_or_default()
    );

    for flag in &op.resolution_flags {
        println!("      ! {flag}");
    }
}

fn signed(v: Decimal) -> String {
    // Negating an exact zero would render as "-0.000000".
    let v = if v == Decimal::ZERO { Decimal::ZERO } else { v };
    format!("{v:+.6}")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_both_subcommands() {
        use clap::CommandFactory;
        Cli::command().debug_assert();

        let cli = Cli::try_parse_from(["polyarb", "scan", "--json"]).expect("scan parses");
        assert!(matches!(cli.command, Command::Scan { json: true, .. }));

        let cli = Cli::try_parse_from(["polyarb", "markets", "--limit", "10"]).expect("markets");
        assert!(matches!(
            cli.command,
            Command::Markets {
                limit: Some(10),
                ..
            }
        ));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("ééééé", 3), "éé…");
    }
}
