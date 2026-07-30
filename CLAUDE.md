# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project Overview

**Polymarket Arbitrage Finder** — a tool that scans Polymarket (a prediction market platform) to detect and surface arbitrage opportunities.

The core idea: prediction market prices represent implied probabilities. When related outcomes are mispriced relative to each other (e.g. the YES/NO prices of a binary market sum to less than $1.00, or mutually exclusive outcomes across a multi-outcome event sum to less than 100%), a risk-free or low-risk profit opportunity exists. This tool finds those situations.

## Current Status

**Phase A complete and soaking.** The dry-run scanner (M1–M6: ingestion, detectors, cost model, daemon, packaging, WebSocket streaming) is built, tested, and running on the owner's machine collecting go/no-go evidence. Phase B (execution/live trading) is designed but NOT built — it requires an explicit owner decision after the soak review. The research below informed the architecture and remains the strategy source of truth.

Research collected so far lives in `research/`:
- `awesome-prediction-market-tools.md` — survey of the prediction-market tool ecosystem: existing arbitrage tools (competitive landscape), data/API providers, and open-source projects worth studying (Polymarket JB Bot, PMXT, TREMOR, pykalshi).
- `arbitrage-analysis-polymarket-nba.md` — summary of arXiv:2605.00864 (UCLA): single-market YES/NO arbitrage is nearly extinct (7 episodes, ~3.6s median); combinatorial arbitrage between logically dependent markets (moneyline vs. spread) is the real opportunity (~101 bps median) but is depth-constrained to retail size. Key design consequences: depth-aware profit math is mandatory, and cross-market logical-dependency detection matters more than single-market scanning.
- `polymarket-paradox-manipulation-whales.md` — summary of SSRN 6670638 (Abid, full PDF read): Polymarket is accurate on average but exposed to whale price impact (single trader moved the 2024 election market 10–15 points vs. other venues for three weeks, un-arbitraged) and oracle/governance capture (UMA disputes settled $250M+ of markets controversially, one against ground truth). Key design consequences: cross-platform divergence is not automatically arbitrage; resolution risk must be flagged per market; intra-Polymarket combinatorial arbitrage is more robust than cross-platform; liquidity must be measured from order book depth, never reported volume (volume is double-counted and was up to ~60% wash trading).
- `execution-costs-and-arb-filtering.md` — practitioner guide (owner-provided): most detected gaps die to bid-ask spread (a tick study found 90% "arbitrage" collapses to 24.5% once spreads are counted), then to Polymarket's taker fee curve (`rate × p × (1−p)`, peaking at 50/50 exactly where gaps cluster; makers pay zero), and much of the rest is relative value mislabeled as arb. Key design consequences: compute gaps on executable book sides (never mid/last); apply a per-category fee model to every opportunity; label true-arb vs relative-value; report net-after-costs at executable size, as taker and as maker; prefer maker-side capture (with legging risk handled). Fee rates verified against official docs 2026-07: geopolitics 0 (fee-free), politics/finance/tech/mentions 0.04, sports/economics/culture/weather/other 0.05, crypto 0.07.
- `market-opportunity-and-strategy-report.md` — deep-research synthesis (adversarially verified, 2026-07): realized-arb evidence (AFT 2025, ~$40M/yr) shows NegRisk multi-outcome rebalancing is ~72% of realized profit and combinatorial only ~0.24%; maker capture is positively subsidized (zero fees + rebates + daily liquidity rewards with API-queryable qualification params). **MVP focus decision: intra-Polymarket NegRisk rebalancing (politics first), secondary single-condition YES/NO rebalancing in sports, opportunistic fee-free geopolitics; combinatorial and cross-platform deprioritized.**
- `short-duration-crypto-bot-architecture.md` — practitioner architecture guide (owner-provided) for BTC Up/Down 5m-style markets: the decision chain (data → signal → fair value → executable edge → position structure → execution → risk), five position structures (temporal arb, hedged directional, inventory MM, near-resolution capture, rotation), execution detail (legging policy, reservation pricing, order types, post-only, size-splitting), and risk framework (fractional Kelly + hard caps + kill switch). Crypto is the highest-fee tier and latency-sensitive — not the MVP, but the execution/risk patterns apply to our chosen focus.

## Domain Context

Key concepts Claude should know when working here:

- **Polymarket** is a crypto-based prediction market on Polygon. Markets resolve via UMA oracle. Outcome shares trade between $0.00 and $1.00 and pay out $1.00 if correct.
- **Binary markets**: YES + NO shares. If YES + NO ask prices sum < $1.00, buying both locks in profit at resolution.
- **Multi-outcome (negative-risk) events**: mutually exclusive outcomes. If the sum of all YES prices < $1.00 (or NO-side equivalents > threshold), arbitrage may exist.
- **CLOB API**: Polymarket runs a central limit order book. Public REST/WebSocket APIs expose markets, order books, and prices (see https://docs.polymarket.com).
- **Practical frictions** that any arbitrage math must account for: order book depth/slippage, gas/transaction fees, resolution risk, capital lockup until resolution, and API rate limits.

## Planned Scope

1. **Market data ingestion** — fetch markets and order books from Polymarket's public APIs.
2. **Arbitrage detection** — scan for intra-market (YES/NO sum), cross-outcome (negative-risk), and combinatorial mispricings, filtered through the execution-cost model in `research/execution-costs-and-arb-filtering.md`.
3. **Opportunity reporting** — surface opportunities with expected profit, required capital, depth-adjusted sizing, and risk notes.
4. **Execution** — the owner has explicitly requested trading automation (2026-07): the bot will trade detected arbs with a dedicated wallet funded by the owner. Execution must ship behind a dry-run mode first, with per-trade and total exposure caps, and live trading only enabled by an explicit config flag.

## Wallet & Security Rules (non-negotiable)

- The trading wallet is a **dedicated hot wallet funded only with capital the owner can afford to lose** — never a main wallet.
- Private keys and API credentials live **only in environment variables / deployment secrets** (`.env` is gitignored). Never in code, config files, logs, chat, or commits.
- Every execution path must respect: max order size, max total exposure, max open positions, and a global kill switch.
- Default mode is **dry-run** (log intended orders, don't send). Live mode requires an explicit opt-in flag.

## Codebase Layout

Rust binary crate `polyarb` (Phase A: scanner + dry-run only; no wallet/key code exists).

- `src/main.rs` — CLI: `markets` (universe discovery), `scan` (one-shot detection), `run` (dry-run daemon; refuses any mode other than `dry-run`), `report [--date]` (daily summary).
- `src/gamma.rs` / `src/clob.rs` / `src/http.rs` — Gamma `/events` discovery, CLOB batch book fetch, shared throttled/retrying transport. The live `/events/keyset` envelope (`{$schema, events[], next_cursor}`, verified 2026-07-30) is parsed alongside the legacy bare-array and `{data}` shapes; a discovery pass that yields zero events is an `ApiError::EmptyDiscovery` naming the count and the endpoint, never a silent empty universe.
- `src/types.rs` — Decimal-only domain types (**no f64 in any money path — enforced convention**), including `MarketFees` (Gamma's per-market `feesEnabled`/`feeType`/`feeSchedule`).
- `src/costs.rs` — verified fee curve + cost decomposition (`net_maker = gross_gap + spread_cost` identity). `breakdown` takes **one taker rate per leg**; `FeeModel::resolve` prefers the market's API-stated rate (`feesEnabled=false` ⇒ 0; `feeSchedule.rate` with exponent 1 ⇒ that rate) and falls back to the category table when the API says nothing or states a formula this build does not implement (counted, and warned once per refresh).
- `src/detect.rs` — binary, NegRisk YES-side, NegRisk NO-side detectors; depth-walked joint sizing.
- `src/risk.rs` — per-trade cap (Phase A scope).
- `src/store.rs` — SQLite persistence; money stored as TEXT Decimal strings, never REAL.
- `src/ws.rs` — M6 streaming market data: CLOB WebSocket market channel, sharded connection pool, defensively parsed frames, locally maintained books with staleness/resync, debounced dirty-event detection. Every wire shape is `TODO(verify-live)` (the container cannot reach the endpoint). M6.1: staleness comes from explicit invalidation or a *shard disconnect*, never from silence (quiet ≠ stale); the REST-only fallback requires WS failing **while REST demonstrably works** (both down = network outage = retry both forever) and is re-probed every `stream.reprobe_interval_secs` so it lasts only until the socket proves itself again.
- `src/dryrun.rs` — daemon loop (REST-timer *and* stream-triggered detection through one shared `process_opportunities` path), opportunity lifecycle (filled_simulated vs vanished decided by first re-poll, never revised), daily summaries incl. detection-latency p50/p95.
- `src/alert.rs` — Telegram send-only alerts with JSONL fallback + circuit breaker; token never logged. M6.1 adds an alert-level cooldown per `(event, kind, side)` (row-level dedupe unchanged — different economics is still its own measurement row; only the *message* is held back, and it is still logged with `delivery = "cooldown"`).
- `config/default.toml` — all knobs, including `[stream]` (enabled by default); secrets only via env (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`).
- `tests/fixtures/` — recorded API-shape fixtures (live Polymarket APIs unreachable from dev container; `TODO(verify-live)` markers track unverified API shape assumptions).

Quality gate for every change: `cargo build && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`.

## Conventions

- Tech stack is not yet decided; propose one when implementation begins and confirm with the owner if the choice is significant.
- Never commit API keys, private keys, or wallet credentials. Use environment variables and keep a `.env` in `.gitignore`.
- Research documents provided by the owner should be stored under `research/` and treated as the source of truth for strategy details.
