# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project Overview

**Polymarket Arbitrage Finder** — a tool that scans Polymarket (a prediction market platform) to detect and surface arbitrage opportunities.

The core idea: prediction market prices represent implied probabilities. When related outcomes are mispriced relative to each other (e.g. the YES/NO prices of a binary market sum to less than $1.00, or mutually exclusive outcomes across a multi-outcome event sum to less than 100%), a risk-free or low-risk profit opportunity exists. This tool finds those situations.

## Current Status

**Early stage — no code yet.** The repository was repurposed from a previous project. The owner is providing research material about Polymarket and arbitrage strategies, which informs the architecture and implementation. Do not assume implementation details described below are final until code exists.

Research collected so far lives in `docs/research/`:
- `awesome-prediction-market-tools.md` — survey of the prediction-market tool ecosystem: existing arbitrage tools (competitive landscape), data/API providers, and open-source projects worth studying (Polymarket JB Bot, PMXT, TREMOR, pykalshi).

## Domain Context

Key concepts Claude should know when working here:

- **Polymarket** is a crypto-based prediction market on Polygon. Markets resolve via UMA oracle. Outcome shares trade between $0.00 and $1.00 and pay out $1.00 if correct.
- **Binary markets**: YES + NO shares. If YES + NO ask prices sum < $1.00, buying both locks in profit at resolution.
- **Multi-outcome (negative-risk) events**: mutually exclusive outcomes. If the sum of all YES prices < $1.00 (or NO-side equivalents > threshold), arbitrage may exist.
- **CLOB API**: Polymarket runs a central limit order book. Public REST/WebSocket APIs expose markets, order books, and prices (see https://docs.polymarket.com).
- **Practical frictions** that any arbitrage math must account for: order book depth/slippage, gas/transaction fees, resolution risk, capital lockup until resolution, and API rate limits.

## Planned Scope (subject to change once research is provided)

1. **Market data ingestion** — fetch markets and order books from Polymarket's public APIs.
2. **Arbitrage detection** — scan for intra-market (YES/NO sum), cross-outcome (negative-risk), and potentially cross-platform mispricings.
3. **Opportunity reporting** — surface opportunities with expected profit, required capital, depth-adjusted sizing, and risk notes.

Execution/trading automation is out of scope unless the owner explicitly requests it.

## Conventions

- Tech stack is not yet decided; propose one when implementation begins and confirm with the owner if the choice is significant.
- Never commit API keys, private keys, or wallet credentials. Use environment variables and keep a `.env` in `.gitignore`.
- Research documents provided by the owner should be stored under `docs/research/` and treated as the source of truth for strategy details.
