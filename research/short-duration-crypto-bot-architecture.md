# Research: Bot Architecture for Short-Duration Crypto Markets (BTC Up/Down 5m)

Source: practitioner article provided by the owner (author analyzes trading activity of high-PnL bots in Polymarket's short-duration crypto markets; promotional material and referral links stripped). Captured 2026-07-26.

**Credibility caveats:** the "100+ bots each making $50K+/month" claim is unverified marketing; the cited example wallets are public but we have not audited them. Treat the *architecture* as the valuable content, not the PnL claims. Also note: **crypto carries Polymarket's highest taker fee (0.07)** — worst category for taker-style capture (see `market-opportunity-and-strategy-report.md`), which makes this article's maker/inventory techniques the only way its strategies survive fees.

## Where the edge is claimed to come from

Not predicting BTC — **repricing faster than the book**. When BTC moves on external exchanges, stale resting orders on Polymarket's CLOB briefly price the old state. The gap between "what the outcome is worth now" and "what's still resting in the book" is the edge. Inputs a bot tracks: BTC vs. the window's opening price, momentum over the last seconds, time remaining, book depth on both sides, combined executable cost of equal Up+Down quantities, own average entry, unhedged inventory, resting-order age, and deviations across related markets (current 5m vs next 5m vs 15m, ETH/SOL equivalents), plus the exact resolution price feed.

## The decision chain

`market data → signal → fair value → executable edge → position structure → execution → risk`

- **Signals → fair value**: Bayesian updating of P(Up) given signals; key warning is **double-counting correlated signals** (volume spike, aggressive buys, bid-side depth, ETH/SOL co-movement are often the *same* event) — features must be scored on marginal information added.
- **Fair value ≠ PnL**: net edge = fair value − expected VWAP fill − fees − slippage − safety buffer. Evaluate at **volume-weighted executable size**, not top-of-book (echoes our execution-costs doc). A "9-point edge" typically shrinks to 5–6 after honest deductions.
- **Cross-market structure**: compute an independent fair value per related market first (different opening prices/time remaining can fully justify a gap), then z-score the spread between related markets against its history to flag dislocations.

## Five position structures observed in profitable systems

1. **Temporal arbitrage** — build the Up and Down legs at *different times* (e.g. Down at 29¢ after a spike, Up at 51¢ after reversion → 80¢ pair cost, $1 payout). Risk: the second leg never gets cheap; mitigate by building in small paired blocks.
2. **Hedged directional** — matched Up/Down inventory core plus a small directional excess sized to current fair-value edge. Risk: matched pairs acquired above $1.00 combined must be recovered by the excess.
3. **Inventory market making** — manage matched/excess inventory across many related markets simultaneously; sell near-certain winners early (e.g. 98¢) to recycle capital; buy 1–2¢ tail protection. Risk: pair cost basis above $1 loses even when "neutral."
4. **Near-resolution capture** — buy the near-certain side at 98–99¢ just before resolution. High win rate but **negatively skewed**: one wrong 99¢ entry erases dozens of 1¢ wins (final-second moves, wrong feed, wrong opening price, resolution-rule misunderstanding).
5. **Dynamic rotation** — flip exposure toward whichever side currently has net edge. Risk: over-rotation in noise pays spread/slippage repeatedly; require new-signal strength > cost of switching.

## Execution lessons (the part that kills naive bots)

- **Legging failure mode**: one leg fills, the book moves, the "arb" becomes an unhedged position. Needs explicit policy: how long to wait, how fast to walk the limit price, max unhedged size, when to cut the first leg at a loss.
- **Inventory-adjusted reservation price** (Avellaneda-Stoikov style): willingness-to-pay falls as same-side inventory grows; bid the other side harder to rebalance.
- **Order types on the CLOB**: GTC, GTD (auto-expire), FOK, FAK, and post-only (guarantees maker). Split intended size across multiple resting orders to control average fill, hide size, and allow partial cancels.
- **Maker vs taker is situational**: passively saving 1¢ can forfeit the whole opportunity when the market is repricing fast — the execution engine trades price improvement against fill probability continuously.

## Risk management

- **Fractional Kelly** (~25%) as the sizing baseline, then hard caps on top: max size per market, max exposure per underlying (BTC/ETH/SOL), max unhedged inventory, daily loss limit, max correlated positions, and an automatic **kill switch on data-quality degradation**.
- **Correlation trap**: BTC 5m + BTC 15m + ETH 5m positions are often one trade in disguise during a broad move.
- Backtests must model fees, realistic spread, slippage, partial fills, latency, and cancels — "a strategy that only makes money when every order fills at the best visible price is not ready for production."

## The six-layer production stack

1. **Data** — external prices, resolution feed, CLOB books, trades, own orders/fills
2. **Signals** — momentum, volatility, book imbalance, trade flow, related-market deviations
3. **Pricing** — independent fair values per market (opening price + time remaining)
4. **Position logic** — one of the five structures above
5. **Execution & risk** — limit orders, partial-fill management, sizing, exposure limits, kill switches
6. **Research** — backtesting, paper trading, trade-cycle reconstruction, loss forensics (where LLMs/AI belong; the live loop stays deterministic: *receive data → calculate → check risk → submit orders*)

## Relevance to our project

- **Confirms our planned architecture** almost 1:1 (scanner → cost filter → dry-run → execution with caps and kill switch), and adds the missing execution-layer detail: legging policy, reservation pricing, order-type selection, post-only usage, and size-splitting.
- **Temporal arbitrage and inventory MM are the natural "v2" of maker-side capture** identified in `market-opportunity-and-strategy-report.md` — same fee logic (maker = free + rebates), applied statefully over time rather than to a single snapshot.
- **Short-duration crypto is a specialist arena**: highest fee tier (0.07), fastest repricing, latency-sensitive, and requires an external low-latency price feed. Not the MVP — but the architecture patterns generalize to our NegRisk/sports focus.
- Reusable code sketches from the article: order-book imbalance feature, Bayesian fair-value update, executable-edge calculator, cross-market z-score, fractional Kelly sizing.
