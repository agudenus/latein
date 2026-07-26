# Research: Awesome Prediction Market Tools

Source: https://github.com/aarora4/Awesome-Prediction-Market-Tools (curated directory, ~600 stars, 19 categories)
Captured: 2026-07-23

This document extracts the sections most relevant to building a Polymarket arbitrage finder: the competitive landscape (existing arbitrage tools), data/API providers we could build on, and adjacent analytics tools.

## Existing arbitrage tools (competitive landscape)

| Tool | Link | What it does |
|---|---|---|
| ArbBets | https://getarbitragebets.com | AI-driven platform identifying arbitrage and +EV opportunities across Polymarket, Kalshi, and sportsbooks |
| Eventarb | https://www.eventarb.com | Free cross-platform arbitrage calculator/alerts for Polymarket, Kalshi, Robinhood |
| Polytrage | https://t.me/polytrage | Telegram alert channel; automated arbitrage signals every 15 minutes with bid-ask spreads, guaranteed-profit calculations, direct trading links |
| PolyScalping | https://polyscalping.org | Scans all Polymarket markets every 60 seconds; Telegram alerts, ROI calculations, filtering by spread/volume/liquidity/category |
| Polymarket JB Bot | https://t.me/polymarket_jb_bot | Open-source Telegram bot: arbitrage alerts, order book depth analysis, market-closing scanner, three-tier signal filtering |
| Prediction Hunt | https://predictionhunt.com | Cross-exchange comparison and arbitrage detection across Kalshi, Polymarket, PredictIt; 5-minute refresh |

Takeaways:
- The space is crowded for **alert-style tooling** (Telegram bots, 15s–5min refresh cadences). Differentiation comes from scan frequency, depth-aware profit math, and cross-platform coverage.
- Cross-platform (Polymarket ↔ Kalshi/PredictIt/sportsbooks) arbitrage is a common angle in addition to intra-Polymarket YES/NO and negative-risk arbitrage.
- Polymarket JB Bot is open source — worth studying for implementation ideas.

## Data & API providers

| Tool | Link | What it offers |
|---|---|---|
| Dome | https://domeapi.io | Unified APIs/SDKs for real-time + historical prediction market data across platforms |
| Marketlens | https://marketlens.trade | Tick-level historical Polymarket order book/trade data; Python SDK + backtesting REST API |
| PolyRouter | https://polyrouter.io | Normalized data from Kalshi, Polymarket, Limitless via a single API key |
| Probalytics | https://probalytics.io | Polymarket + Kalshi infrastructure; REST API, ClickHouse SQL, 200–500M orderbook updates/day at 1ms resolution, Parquet/S3 bulk export |
| PMXT | https://github.com/qoery-com/pmxt | Open-source API for prediction market data across multiple exchanges |
| pykalshi | https://github.com/ArshKA/kalshi-client | Python client for Kalshi: WebSocket streaming, rate limiting, local orderbook management |
| Goldsky | https://goldsky.com | Blockchain data infrastructure powering Polymarket's real-time data processing |
| TREMOR | https://github.com/sculptdotfun/tremor | Open-source data terminal for Polymarket/Kalshi: SQL analytics, 140K+ markets |
| Adjacent News | https://adj.news | Prediction-market-driven news with data/trading APIs |

Takeaways:
- For an MVP, Polymarket's **own public CLOB API** (docs.polymarket.com) is free and sufficient for intra-Polymarket arbitrage.
- For **cross-platform** arbitrage, PolyRouter/PMXT (normalized multi-exchange data) avoid writing one client per exchange; pykalshi covers Kalshi directly.
- For **backtesting** strategies, Marketlens (tick-level history) and Probalytics (high-resolution orderbook history) are the relevant sources.
- Open-source references to study: PMXT, TREMOR, pykalshi, Polymarket JB Bot.

## Adjacent tools worth knowing (context)

- **Analytics platforms**: Polymarket Analytics, Polysights, Hashdive, Betmoar, Parsec, Synthesis (live orderbooks + cross-market price comparison across Polymarket/Kalshi/Limitless) — useful to see what metrics traders already get.
- **Dune dashboards**: sealaunch (trending topics), filarm (activity/volume), dunedata (cross-platform volume/OI), alexmccullough (Polymarket accuracy analysis).
- **Trading terminals/aggregators**: Converge (cross-venue aggregator with built-in cross-venue arbitrage detection, zero added fees), Sharpe Terminal, Rainmaker (arbitrage + copy-trading terminal).
- **DeFi integrations**: Gondor (borrow against Polymarket positions), Robin (delta-neutral yield) — relevant to capital-efficiency questions later.
- **Education**: PolyNoob (strategy encyclopedia), PolymarketGuide (resolution precedent database — relevant to resolution risk assessment).

## Full category list in the source repo

AI Agents, APIs, Aggregator, Alerts, Analytics Tools, Arbitrage tools, Dashboards, Data, DeFi, Educational Resources, Extensions, Funds, Infrastructure, News, Official, Others, Parlays, Portfolio Tracking, Trading Bots.

## Implications for this project

1. **MVP scope**: intra-Polymarket arbitrage (YES/NO sum and negative-risk multi-outcome) using the free official CLOB API — no paid data needed.
2. **Differentiators to consider**: depth-adjusted (slippage-aware) profit calculations, faster scan cadence than the 60s–15min competitors, and honest fee/risk accounting.
3. **Natural v2**: cross-platform arbitrage (Kalshi first, via pykalshi or PolyRouter), which most commercial tools treat as the main event.
4. **Study before building**: Polymarket JB Bot, PMXT, and TREMOR source code.
