# Design: `polyarb-crypto` — short-duration BTC/ETH Up-or-Down engine

**Status: plan, not a commitment.** Nothing in this document is scheduled. The
politics/NegRisk dry-run soak (`docs/deploy.md` §13) remains the primary evidence base and
the only thing running. This design exists so that *if* the soak review says "the NegRisk
pool is real but capital velocity is the binding constraint", we already know what the
second focus would look like, what it costs, and — more importantly — which parts of it we
should refuse to build.

**Source of truth:** `research/short-duration-crypto-bot-architecture.md` (architecture),
`research/execution-costs-and-arb-filtering.md` and
`research/market-opportunity-and-strategy-report.md` (verified fee schedule). Where this
document departs from the research it says so explicitly, in §12.

---

## Contents

1. [Why crypto at all, and why not](#1-why-crypto-at-all-and-why-not)
2. [Scope and non-goals](#2-scope-and-non-goals)
3. [External market data](#3-external-market-data)
4. [The Polymarket side](#4-the-polymarket-side)
5. [Fair value](#5-fair-value)
6. [Position structures: v1 and later](#6-position-structures-v1-and-later)
7. [Execution and risk](#7-execution-and-risk)
8. [Dry-run design](#8-dry-run-design)
9. [Integration with polyarb](#9-integration-with-polyarb)
10. [Milestones and go/no-go gates](#10-milestones-and-gono-go-gates)
11. [Open questions and `TODO(verify-live)`](#11-open-questions-and-todoverify-live)
12. [Where this design diverges from the research](#12-where-this-design-diverges-from-the-research)

---

## 1. Why crypto at all, and why not

### The case against (read this first)

Crypto is the **worst** category on Polymarket by every metric we have measured:

- **Highest taker fee: 0.07.** `fee = shares × rate × p × (1 − p)`. At 50/50 that is
  1.75¢ per share, so a two-leg Up+Down pair bought as a taker pays **3.5¢ per $1 of
  payout**. A pair must be assembled for ≤ 96.5¢ before it breaks even. In geopolitics the
  same pair pays zero.
- **Fastest repricing on the platform.** The research's own claim is that the edge is "not
  predicting BTC — repricing faster than the book". That is a latency race, and the
  measured cohort winning it operates sub-100ms
  (`market-opportunity-and-strategy-report.md` §3). We do not, and will not.
- **Not in the realized-profit evidence.** The AFT 2025 measurement of ~$40M realized
  arbitrage put NegRisk rebalancing at ~72% and single-condition rebalancing at ~26%,
  concentrated in politics and sports. Short-duration crypto does not appear as a measured
  pool at all. Everything we have on it is one unverified practitioner article.

### The one case for

**Capital velocity.** A NegRisk politics rebalance locks capital until resolution —
possibly November 2026. A 5-minute BTC window returns capital 288 times a day. A strategy
earning 0.5¢ per share-pair on 50 shares in 5 completed windows a day is $1.25/day on ~$25
of working capital turned over repeatedly; the same $25 sitting in a politics leg until the
midterms earns its gap once. Small, frequent, independent bets are also statistically
tractable in a way that a handful of election arbs are not: **288 windows/day/series gives
us a sample size that can actually reject a bad strategy in weeks rather than years.**

That is the entire argument, and it is an argument about *measurement and turnover*, not
about edge. It justifies building a recorder and a simulator. It does not justify sending
an order.

### The consequence for the design

Because we lose every race and pay the worst fee, the only structures that can work are the
ones where **someone else crosses the spread to us** and where **being slow costs a bounded
amount rather than the whole edge**. That single constraint determines almost every choice
below.

---

## 2. Scope and non-goals

### In scope (eventually)

| Item | Phase |
|---|---|
| Recording external price feeds + Polymarket books for the 5m/15m series | C0 |
| A calibrated fair-value model for `P(Up)` per window | C1 |
| Maker-side temporal arbitrage (paired-block accumulation), **dry-run** | C2 |
| The same, live, behind an explicit flag with hard caps | C3 |
| Inventory market-making across neighbouring windows | C4 |

### Explicit non-goals

- **No HFT taker sniping.** We will not build a system whose profitability depends on
  reacting to a Binance tick faster than another bot. We would need sub-100ms end-to-end
  including EIP-712 signing and a Polygon-side CLOB round trip; our realistic floor is
  200–400ms with a WebSocket build (§4.3). Any strategy that dies at 400ms is out of scope
  by construction, not by scheduling.
- **No cross-platform legs initially.** No Kalshi, no CEX hedge, no perp leg. Cross-venue
  adds capital fragmentation, a second failure domain, and genuine resolution-criteria
  divergence (`polymarket-paradox-manipulation-whales.md`). If the intra-Polymarket engine
  cannot make money, a second venue will not rescue it.
- **No taker entries in v1 at all.** Not "prefer maker" — *only* maker. See §7.2.
- **No directional alpha.** We are not forecasting BTC. The fair-value model exists to
  price a quote and to measure adverse selection, not to take a view. Any structure whose
  P&L depends on the model being right about direction is deferred until the model has
  demonstrably beaten the market mid out-of-sample (§5.4).
- **No SOL or long-tail underlyings** in v1. BTC and ETH only — the two with a plausible
  reference feed and plausible depth.
- **No wallet or key code** until C3, and then only under the Phase B rules already written
  in `CLAUDE.md`.

---

## 3. External market data

### 3.1 The feed

**Primary: Binance public WebSocket.** Free, no API key, no account.

```
wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/btcusdt@aggTrade/ethusdt@bookTicker/ethusdt@aggTrade
```

- `@bookTicker` — best bid/ask on every change. This is the fair-value input: we price off
  the **mid of the reference book**, not off last trade, for the same reason the scanner
  prices off executable sides rather than mid — last trade is a stale point sample and
  jumps between bid and ask, injecting spurious "moves" into the model.
- `@aggTrade` — trade prints, used for the volatility estimator and for the
  double-counting analysis in §5.3.
- Combined-stream endpoint so one socket carries both underlyings; one socket, one failure
  domain, one heartbeat.

**Geo-restriction caveat:** `binance.com` blocks US-originating requests. The soak VPS in
`docs/deploy.md` is Hetzner (Germany), which is fine. If the host ever moves to a US
region the feed must switch to Binance.US or Coinbase — **and that is not a transparent
substitution**, because it changes the reference price (see basis risk below).

**Secondary (health only, not pricing): Coinbase `wss://ws-feed.exchange.coinbase.com`,
`ticker` channel for `BTC-USD`/`ETH-USD`.** Its only job is to answer "is Binance lying or
stalled?" — see §3.3. It never feeds the model, because mixing two sources into one price
would smear the basis into our fair value.

### 3.2 Basis risk and the resolution source

This is the single most dangerous unknown in the whole design.

> **`TODO(verify-live)`: what exactly resolves a Polymarket "Bitcoin Up or Down — 5
> minute" market?** Candidates the engine must distinguish between: Binance `BTCUSDT`
> spot; a Chainlink or Pyth oracle price; a specific exchange's 1-minute candle close; a
> time-weighted or median-of-sources price. Each gives a *different* answer in exactly the
> situations that matter — near the money, in the last seconds.

Three sub-questions that are each individually fatal if wrong:

1. **Which price series**, and is it the same for BTC and ETH, and the same for the 5m and
   15m series? (Different series may well resolve differently.)
2. **What is the window's opening price** — the tick at `T0`, the close of the preceding
   candle, or the first print after `T0`? Our model's `d` is measured *from* this number.
   Being wrong by one tick is irrelevant at `τ = 300s` and decisive at `τ = 5s`.
3. **What happens on an exact tie** (close == open)? "Up" is presumably strictly greater,
   so a tie resolves Down — but as `τ → 0` the entire probability mass concentrates at the
   tie point, so this rule is worth several cents in the last seconds and *all* of the
   near-resolution-capture edge.

**Hard design rule.** The engine keeps a config-declared mapping
`series_slug → resolution_source`, and **refuses to quote any series whose resolution
source is not explicitly declared and empirically verified**. Verification is a C0 gate
(§10): reconstruct the resolution of ≥ 200 already-settled windows from our recorded feed
and confirm we predict the settled outcome 100% of the time, including at least 20 windows
that settled within 0.02% of the open. Anything less than 100% means we do not know what we
are trading, and the project stops there.

Basis risk, stated plainly: if Polymarket resolves on source X and we price on Binance
`BTCUSDT`, then every quote we place carries an unhedgeable error equal to the X-vs-Binance
basis. Near the money that basis is worth far more than our whole per-share edge.

### 3.3 Feed health monitoring

Tracked continuously, per stream:

| Signal | Definition | Threshold (config) |
|---|---|---|
| `staleness_ms` | now − last message timestamp | warn 500ms, **halt 2000ms** |
| `clock_skew_ms` | local clock vs exchange event time, EWMA | **halt > 1000ms** |
| `gap` | missed sequence / update id discontinuity | **halt on any gap** |
| `divergence_bps` | \|Binance mid − Coinbase mid\| / mid | warn 5bps, **halt 25bps** |
| `reconnects_5m` | socket reconnects in trailing 5 min | **halt > 3** |
| `spread_bps` | reference book spread (a proxy for venue stress) | warn > 5bps |

**The kill-switch-on-stale-data rule.** Any `halt` condition immediately:

1. **Cancels every resting order** (live mode) or marks every simulated quote as withdrawn
   at that instant (dry-run), with the reason recorded.
2. Blocks all new quoting until the condition has been clear for `feed.recovery_secs`
   (default 30s) *and* a fresh book snapshot has been fetched from Polymarket.
3. Emits a `crypto_feed_halt` alert through the existing `src/alert.rs` path.
4. Increments a counter that itself trips the daily kill switch if it exceeds
   `risk.max_feed_halts_per_day`.

Halting is fail-safe in one direction only: **we can always stop quoting, but we cannot
always cancel in time.** So the resting-order design (§7) must assume that a feed outage
leaves our orders exposed for the duration of the outage plus a cancel round trip. That is
another argument for quoting far from the money: a quote 20¢ out of the money survives a
5-second blackout; a quote at 50¢ does not.

---

## 4. The Polymarket side

### 4.1 Discovering the series

The existing `src/gamma.rs` paginates `/events?active=true&closed=false` and builds the
universe. The 5m/15m crypto series break three of its assumptions:

- **Churn.** A 5-minute series produces 288 events/day. The scanner's
  `universe_refresh_secs = 600` would miss two entire windows per refresh. The crypto
  engine needs its own discovery cadence (~30s) scoped to a handful of series slugs, *not*
  a full `/events` sweep.
- **Volume.** These events would dominate `scan.max_events = 2000` and starve the politics
  scanner of pagination budget. The crypto discovery path must be a **separate, filtered
  query** (by tag/slug prefix — `TODO(verify-live)`: the correct Gamma filter parameter for
  a series, whether `tag_id`, `slug` prefix, or a `series` endpoint) and must not touch the
  scanner's universe.
- **Forward listing.** Temporal structures across neighbouring windows require the *next*
  window to be listed and quotable before the current one closes.
  **`TODO(verify-live)`: how far ahead is the next window listed, and is it order-book
  enabled before its start time?** If windows are only listed at `T0`, the "current 5m vs
  next 5m" structure from the research does not exist and v2 loses one of its legs.

Also unknown and structurally important: **`TODO(verify-live)`: are the Up/Down windows
plain binary conditions (two tokens, one `conditionId`) or NegRisk events?** Everything in
§6 assumes plain binary — `ask(Up) + ask(Down) < $1.00` is the pair. If they are NegRisk,
the NO-side construction and the adapter conversion path from `src/detect.rs` apply and the
capital math changes.

### 4.2 Book tracking: WebSocket vs REST

| | REST (`POST /books`, today) | CLOB WebSocket |
|---|---|---|
| Freshness | one snapshot per poll cycle | push on change |
| Realistic cadence | 1–5s (rate limits, batch size 100) | ~continuous |
| Effort | zero — `src/clob.rs` exists | new client, reconnect/resync logic, sequence handling |
| Fill simulation | coarse: we see the level shrink, not why | can distinguish trades from cancels (§8) |

**Recommendation: REST for C0/C1, WebSocket required before C2.**

The reason is not speed, it is **fill-simulation honesty**. The pessimistic maker fill model
in §8 needs to know whether a price level disappeared because it *traded* or because it was
*cancelled*. A REST snapshot cannot tell the difference, so a REST-only simulator must
treat every disappearance as a cancel (credit nothing) — which is so pessimistic it would
never credit a fill and the soak would produce no signal at all. The market-channel trade
stream is what makes the dry run informative.

> `TODO(verify-live)`: CLOB WSS endpoint, channel names, subscription payload, whether the
> market channel is public (no auth) and what its message schema is. Assume
> `wss://ws-subscriptions-clob.polymarket.com/ws/market` until confirmed.

### 4.3 The latency budget, and what it rules out

Honest end-to-end accounting for a *future* WebSocket build, from an EU VPS:

| Stage | Optimistic | Realistic |
|---|---|---|
| Binance event → our socket | 10 ms | 30–60 ms |
| Decode, model, decision | < 1 ms | 1–3 ms |
| Order sign (EIP-712) + POST to CLOB | 60 ms | 150–300 ms |
| CLOB accept → book update | ? | ? (`TODO(verify-live)`) |
| **Total reaction** | **~80 ms** | **~200–400 ms** |

Today, with the REST scanner architecture, the equivalent number is **2,000–5,000 ms**.

What this means, quantitatively. Model `P(Up) = Φ(d)` with
`d = ln(S/S_open) / (σ√τ)` (§5). Over a reaction window `Δt`, the 1-sigma repricing is

```
ΔP(1σ) = φ(d) · √(Δt / τ)
```

— and note that **σ cancels out entirely**. The exposure of a stale quote depends only on
how close to the money it is (`φ(d)`) and on the ratio of our reaction time to the time
remaining. That makes it directly computable:

**1σ adverse repricing of a resting quote, in cents:**

| | `d = 0` (50¢) | `d = 1` (84¢) | `d = 1.5` (93¢) | `d = 2` (98¢) |
|---|---|---|---|---|
| `Δt = 3s`, `τ = 300s` | 3.99¢ | 2.42¢ | 1.30¢ | 0.54¢ |
| `Δt = 3s`, `τ = 60s` | 8.92¢ | 5.41¢ | 2.90¢ | 1.21¢ |
| `Δt = 3s`, `τ = 30s` | 12.61¢ | 7.65¢ | 4.09¢ | 1.71¢ |
| `Δt = 0.3s`, `τ = 60s` | 2.82¢ | 1.71¢ | 0.92¢ | 0.38¢ |
| `Δt = 0.3s`, `τ = 30s` | 3.99¢ | 2.42¢ | 1.30¢ | 0.54¢ |

Three conclusions, all of which shape v1:

1. **At-the-money quoting is not available to us.** A quote at 50¢ with a minute left is
   wrong by ~9¢ one-sigma over a 3-second reaction. No spread we could charge covers that.
2. **A 10× latency improvement buys about 3×** (the `√` is unforgiving). Going from REST to
   WebSocket is worth doing, but it does not change the qualitative answer, so we should
   not treat "get faster" as the strategy.
3. **Deep, early quotes are survivable.** `d = 2`, `τ = 300s` is 0.54¢ of 1σ exposure —
   a number a few cents of quote discount can genuinely cover.

That table *is* the strategy selection argument. Everything in §6 follows from it.

---

## 5. Fair value

### 5.1 The v1 model, concretely

One model, one input path, no ensemble:

```
d   = ln(S_t / S_open) / (σ_1s · √τ)
P(Up) = Φ(d)
P(Down) = 1 − P(Up)
```

| Symbol | Meaning | Source |
|---|---|---|
| `S_open` | the window's opening reference price | recorded at `T0` from the declared resolution source (§3.2) |
| `S_t` | current reference mid | Binance `@bookTicker` mid |
| `τ` | seconds remaining to the settlement instant | window metadata, clock-skew corrected |
| `σ_1s` | per-second log-return volatility | EWMA estimator, §5.2 |
| `Φ`, `φ` | standard normal CDF / PDF | — |

Deliberate simplifications, each of which is a claim we are making:

- **Zero drift.** We assume the reference price is a martingale over a 5-minute horizon.
  Any non-zero drift term is a directional forecast and belongs to a structure we are not
  building in v1.
- **Lognormal, constant σ over the window.** Wrong in detail (vol clusters, jumps), and
  the calibration in §5.4 is how we find out whether it is wrong enough to matter. The
  known failure mode is that a Gaussian under-prices the tails, so our model will say a
  large reversal is less likely than it is — which makes us over-confident exactly at the
  deep quote levels we intend to use. **Mitigation: a tail-widening fudge is *not* applied
  blind; instead the quote buffer in §7.3 is set from empirical markout, not from the
  model's own confidence.**
- **No order-book input.** The Polymarket book is deliberately *not* an input to fair
  value. It is what we are trying to price against; feeding it back in would make the model
  chase the very quotes we are evaluating.

### 5.2 The volatility estimator

`σ_1s` = EWMA of squared 1-second log returns of the reference mid, half-life 300s, with:

- a floor and cap from config (`fair_value.sigma_floor_bps`, `sigma_cap_bps`) so a quiet
  minute cannot collapse `σ` to zero and drive `|d| → ∞` (which would print 99.9¢
  confidences and invite exactly the near-resolution disaster in §6.2);
- a blend with a slower estimator (half-life 3600s) at a config weight, so a single spike
  does not dominate;
- **a hard rule that `σ` is estimated from the *previous* window and frozen at `T0`** for
  the current window, or updated only on a schedule. Estimating `σ` from the same price
  path we are evaluating creates a subtle look-ahead that would make backtests look better
  than live.

### 5.3 The double-counting warning, taken seriously

The research is explicit that volume spikes, aggressive buy flow, bid-side depth imbalance
and ETH/SOL co-movement are frequently *the same event* observed four ways, and that naive
Bayesian chaining of them over-updates. Our response is stronger than "be careful":

**v1 has exactly one signal.** Price versus open, scaled by vol and time. No momentum
term, no book imbalance, no cross-asset term.

Any additional feature must clear all four of these before it enters the model:

1. Fitted **jointly** with the existing features (one logistic regression on
   `d` plus candidates), never as an independent multiplicative update.
2. Measured on **out-of-sample** recorded windows — a separate time period, not a random
   split, because these series are autocorrelated within a session.
3. Improves **log-loss** against the v1 baseline by a margin exceeding the standard error
   of the improvement.
4. Reported with its **pairwise correlation** to every existing feature, and with its
   marginal contribution after orthogonalisation. A feature that adds nothing after
   orthogonalisation is rejected regardless of its standalone `R²`.

### 5.4 Calibration plan

Calibration runs entirely offline on the C0 recordings — no orders, no risk.

**Data:** for every recorded window, the full reference tick series, the Polymarket book
snapshots/updates, and the realised settlement.

**Metrics**, all bucketed by `τ` (5 bins) and by `|d|` (5 bins), because a model that is
calibrated on average and badly miscalibrated in the last 30 seconds is a model that will
lose money in exactly the region where we would trade:

- **Brier score** and **log-loss**, model vs. two benchmarks: the Polymarket **mid** at the
  same instant, and the trivial 50/50.
- **Reliability diagram** — predicted vs. realised frequency in 10 probability buckets.
- **Markout of the model against the market**: if we had taken the market's price whenever
  the model disagreed by more than `x`, what is the realised P&L per share, gross of fees?

**The gate that matters (C1):** the model must beat the Polymarket mid on out-of-sample
log-loss, in the `(τ, |d|)` region we intend to quote in. If it does not, that is not
necessarily fatal — a pure structural strategy (buy both legs below $1 combined) does not
need model alpha — but it *is* fatal for anything that sizes on model edge, including
fractional Kelly (§7.4) and every directional structure. That distinction is recorded
explicitly in the C1 write-up rather than being quietly ignored.

---

## 6. Position structures: v1 and later

The research describes five structures. We adopt one, flag one, and defer three.

### 6.1 v1: temporal arbitrage — paired-block accumulation (maker-only)

**The structure.** Rest post-only bids on *both* Up and Down, each well out of the money,
each substantially below the model's fair value for that side. A move in either direction
makes the losing side cheap and fills that bid; a reversion later in the window fills the
other. Two legs bought at different times, combined cost below $1, payout exactly $1.

The research's own worked example: Down at 29¢ after a spike, Up at 51¢ after reversion —
80¢ pair cost, $1 payout, 20¢ gross. Costed honestly at crypto's 0.07 rate:

| | as taker | as maker |
|---|---|---|
| Leg 1 fee (29¢) | `0.07 × 0.29 × 0.71` = 1.44¢ | 0 |
| Leg 2 fee (51¢) | `0.07 × 0.51 × 0.49` = 1.75¢ | 0 |
| Spread crossed (assume 2¢ each) | 4.00¢ | 0 |
| **Net on a 20¢ gross pair** | **12.8¢** | **20.0¢** |

Even as a taker this survives — the fee curve is not what kills the 80¢ pair. What kills it
is that 80¢ pairs are not sitting on the ask waiting to be lifted; they only exist if you
are the resting order that someone else hits, twice, at two different times. Hence
maker-only.

**Why this structure and not another.** It is the only one of the five where our two
structural disadvantages are neutralised:

- We never cross, so the 0.07 fee never applies (§7.2).
- We quote deep (`|d| ≥ 1.5` at entry), so the §4.3 table says a 3-second staleness costs
  us ~1–4¢ of adverse selection rather than ~9–13¢.
- We are not racing anyone: our order sits there and either gets hit or does not.

**What it actually is.** Not arbitrage. Until the second leg fills, we hold a naked
directional position bought at a discount. It becomes true arbitrage at the moment the pair
completes below $1 and not one second earlier. The engine must label it exactly as
`src/types.rs` already does: `Label::RelativeValue` while unpaired, `Label::TrueArb` only
once the pair is complete and `Σ cost < payout`. Reporting an unpaired leg as arbitrage
would be the same lie the scanner already refuses to tell about partial NegRisk sweeps.

**Strict unhedged caps** (the whole risk of the structure lives here):

| Cap | Default | Rationale |
|---|---|---|
| `max_unpaired_shares_per_window` | 25 | one leg's worth of a minimum-size block |
| `max_unpaired_notional_per_window_usd` | 10 | 25 shares × ~40¢ |
| `max_unpaired_notional_per_underlying_usd` | 25 | across 5m + 15m of the same coin |
| `max_unpaired_notional_total_usd` | 40 | correlated bucket, §7.5 |
| `block_size_shares` | 5–10 | accumulate in small paired blocks, per the research |
| `stop_opening_at_tau_secs` | 60 | no *new* unpaired risk in the last minute |

The last one deserves emphasis: §4.3 shows adverse selection scales as `√(Δt/τ)`, so late
quoting is where a slow participant gets destroyed. We stop opening at `τ = 60s` and stop
quoting entirely at `τ = 20s`.

### 6.2 v1, behind its own default-off flag: near-resolution capture

The research lists it as high-win-rate; it also flags the negative skew. Both are true, and
the numbers are stark.

**Breakeven win rate**, including the (small, because the fee curve collapses at the edges)
taker fee:

| Entry | Taker fee/share (0.07) | All-in cost | Breakeven win rate | Losses per 100 wins that erase the profit |
|---|---|---|---|---|
| 99¢ | 0.069¢ | 99.07¢ | **99.07%** | 1 loss erases **106** wins |
| 98¢ | 0.137¢ | 98.14¢ | **98.14%** | 1 loss erases **53** wins |
| 95¢ | 0.333¢ | 95.33¢ | **95.33%** | 1 loss erases **20** wins |

**And then adverse selection on top.** The `ΔP(1σ)` formula applies to *taker* entries too,
in the other direction: at 99¢ (`d ≈ 2.33`, `φ = 0.027`) with `τ = 10s` and a 3-second
reaction, 1σ of repricing is **1.46¢** — larger than the 0.93¢ of gross edge. We are not
lifting a fresh 99¢ ask; we are lifting an ask that a faster participant has already
decided not to cancel. **That selection is systematically against us**, and it is on top of
the skew, not instead of it.

**Therefore:**

- Flag `crypto.near_resolution.enabled`, **default `false`**, separate from the main
  engine flag. Enabling it in live mode requires the same explicit opt-in ceremony as live
  mode itself.
- It cannot be enabled at all until the §3.2 resolution-source verification is 100% and the
  tie rule is confirmed. "Wrong feed, wrong opening price, resolution-rule
  misunderstanding" are three of the four loss modes the research names, and all three are
  eliminated only by that gate.
- **It is fully backtestable for free, before any code that places an order.** Taker fills
  are certain — you lift the ask — so the C0 recordings alone answer the question: for
  every recorded 98–99¢ print in the last 60 seconds, did that side actually settle? That
  measurement, on thousands of windows, is worth more than any amount of reasoning. If the
  empirical realised win rate is below the breakeven column above, the structure is deleted
  from the design rather than gated.
- Hard per-window notional cap regardless (`near_resolution.max_notional_per_window_usd`,
  default 5), and a hard floor on entry price (never above `max_entry_price`, default 0.98,
  because 99¢ needs a 99.07% win rate we have no way to demonstrate).

### 6.3 Later (v2, C4): inventory market-making across neighbouring windows

Quote both sides of several related markets at once (BTC 5m current + next, BTC 15m, ETH
5m), managing matched vs. excess inventory with an inventory-adjusted reservation price;
recycle capital by selling near-certain winners early; buy cheap tail protection.

Deferred because it requires, in order: the WebSocket book feed, a demonstrated positive
markout on the simpler structure, cross-window fair values that are independently computed
(different opens, different `τ` — the research is explicit that a raw price gap between
related windows is usually *justified*), and a z-score history of the inter-window spread
that only exists after months of recordings.

### 6.4 Deferred indefinitely

- **Hedged directional** (matched core + directional excess) — requires proven model alpha
  (§5.4 gate). Without it the "excess" is a coin flip paying 0.07 to enter.
- **Dynamic rotation** — repeatedly flipping sides pays spread and fee every flip; at our
  latency the new-signal strength can never exceed the switching cost reliably. This is the
  structure most likely to look good in a naive backtest and lose money live.

---

## 7. Execution and risk

### 7.1 Reservation-price quoting

Per side, per window, per tick:

```
fair        = model P(side)                                   §5.1
reservation = fair − q · γ · σ_P² · τ                          inventory skew (A-S style)
quote_px    = reservation − δ_adverse − δ_edge
```

- `q` — signed inventory in *this* side's direction, in shares, normalised by
  `max_unpaired_shares_per_window`. Same-side inventory pushes our bid down; the opposite
  side's bid moves up, which is the rebalancing pressure the research describes.
- `γ` — risk aversion, config. Starts high (quote timidly) and is only lowered on evidence.
- `σ_P` — the standard deviation of `P` over the remaining window ≈ `φ(d)` (a convenient
  identity: the probability's own diffusion scale near `d`).
- **`δ_adverse`** — the staleness buffer, computed from the §4.3 identity, not guessed:
  `δ_adverse = z · φ(d) · √(Δt_react / τ)` with `z` from config (default 2.0) and
  `Δt_react` measured live as the observed p95 feed→decision→ack latency, not assumed.
  This term is what makes quoting near the money automatically uneconomic instead of
  merely discouraged: at `d = 0`, `τ = 60s` it demands a 17.8¢ discount and no such quote
  will ever be worth placing.
- **`δ_edge`** — the minimum profit we require per share, config, analogous to
  `scan.net_floor`.

A quote is only placed if `quote_px` is at or better than one tick inside the current best
bid *and* clears the minimum order size. Otherwise no quote — silence is a valid output.

### 7.2 Order types: post-only, always

- **`post-only` on every order, without exception, in v1.** It guarantees maker status,
  which is the entire economic basis of the strategy: zero fee (vs. 1.75¢/share at the
  money), zero spread crossed, and eligibility for the maker rebate. If a post-only order
  would cross, it is rejected and **we simply do not trade** — there is no taker fallback
  path in v1 code. Not a policy; an absent code path, the same way the scanner has no order
  path at all today.
- **GTD, expiring at `window_close − expiry_margin_secs`.** Nothing may rest into the next
  window: the next window has a different `S_open`, so a stale order is priced off the
  wrong reference and is pure gift.
- **Max resting age** (`quote.max_age_secs`, default 5–10s): re-quote rather than leave a
  stale price. Cancel/replace is free in fee terms and is our main defence against
  staleness, subject to rate limits.
- **Size splitting**: intended size is split across `quote.blocks` resting orders at
  laddered prices. Controls average fill, hides size, and lets us cancel partially when
  `d` moves.

**A tension we should state rather than paper over.** The strategy report notes that
Polymarket's **liquidity rewards** subsidise makers, with per-market qualification
parameters `min_incentive_size` and `max_incentive_spread` queryable from the API. Those
rewards require quoting **within** a maximum spread of the mid — i.e. *near* the money.
Our latency requires quoting **far** from the money. **We almost certainly do not qualify,
and v1 counts zero reward income in its EV.** If the C0 recordings show
`max_incentive_spread` is wide enough on these series for a deep quote to qualify, that is
a pleasant surprise to be measured, not an assumption to be planned on.
(`TODO(verify-live)`: the actual reward parameters on the 5m/15m crypto series.)

### 7.3 Legging policy

The named failure mode: one leg fills, the book moves, and the "arb" is now a naked
position. The policy is explicit and boring:

1. **Both bids rest simultaneously from the start.** We do not "leg in" deliberately; we
   place both and let the path decide which fills first.
2. **On first fill**, the unfilled side's quote is re-priced against the new fair value and
   the *pair budget*: we will pay at most `pair_budget − cost_of_leg_1` for the second leg,
   where `pair_budget` (default 0.97) is the maximum total pair cost we accept. This is the
   mechanism that guarantees a completed pair is profitable.
3. **We do not chase.** The second-leg quote walks *down* toward its own fair value over
   time, never up beyond the pair budget. If the market never comes back, the pair does not
   complete — that is the accepted risk of the structure, bounded by the unpaired caps.
4. **At `τ = stop_opening_at_tau_secs`** we stop adding new blocks. At
   `τ = force_flat_tau_secs` (default 20s) we stop quoting entirely.
5. **We do not cut the first leg at market by default.** Round-tripping an unpaired leg
   costs the spread plus 0.07 on the exit, which usually exceeds the position's remaining
   expected loss. The default is to hold to resolution *within the cap* — which is exactly
   why the cap must be small enough that a total loss of the unpaired inventory is boring.
   A post-only exit quote may be placed; a market exit requires `risk.allow_taker_exit`
   (default false) and exists only as an emergency valve tied to the kill switch.
6. **Every leg fill records the contemporaneous fair value**, so markout (§8.2) can tell us
   whether we are being systematically picked off — the metric that decides the whole
   project.

### 7.4 Sizing: caps first, Kelly second

The research proposes fractional Kelly (~25%) as the sizing baseline with hard caps on
top. **We invert that ordering.** Kelly sizing on an uncalibrated probability is a formula
for losing money faster, and until §5.4's gate passes we have no demonstrated edge to size
against.

```
size = min(
    hard_cap_shares,                        # always binding in v1
    depth_available_at_our_price,           # never quote more than the level can absorb
    kelly_fraction · bankroll / price       # only if the model has passed the §5.4 gate
)
```

- `kelly_fraction = 0.25 · edge/odds`, computed from model edge, **clamped to zero unless
  `fair_value.calibrated = true`** in config, which is set by a human after reading the C1
  report, never automatically.
- Bankroll for the crypto engine is a *separate*, explicitly configured figure — not the
  wallet balance — so a shared wallet cannot silently upsize the crypto book.

### 7.5 Hard caps and the correlated-exposure rule

| Cap | Default | Scope |
|---|---|---|
| `max_notional_per_window_usd` | 25 | one 5m or 15m market |
| `max_notional_per_underlying_usd` | 50 | all BTC windows together |
| `max_unpaired_notional_total_usd` | 40 | the correlated bucket, below |
| `max_open_windows` | 4 | BTC 5m + BTC 15m + ETH 5m + ETH 15m |
| `daily_loss_limit_usd` | 25 | realised + marked unpaired, UTC day |
| `max_consecutive_losing_windows` | 8 | trips the kill switch |
| `max_feed_halts_per_day` | 20 | data quality degradation |

**The correlated-exposure rule.** BTC 5m, BTC 15m and ETH 5m unpaired positions taken
during one broad move are one trade wearing three hats. The engine therefore computes a
single signed effective exposure:

```
E_effective = | E_btc5m + E_btc15m + ρ · (E_eth5m + E_eth15m) |
```

with `ρ` (config, default **0.85** — BTC/ETH intraday return correlation is typically
0.8–0.9; `TODO(verify)` from our own recordings) and `E_x` the *signed* unpaired notional
(positive = long Up). `E_effective` is checked against
`max_unpaired_notional_total_usd` **before every order**, and it is the number the kill
switch watches. A design that only caps per-market exposure has no cap at all during the
moves that matter.

### 7.6 The kill switch

A single `KillSwitch` type, checked at the top of every decision cycle and before every
order, trips on:

- any §3.3 feed halt condition;
- `daily_loss_limit_usd` breached (realised + marked-to-fair unpaired);
- `max_consecutive_losing_windows`;
- CLOB order-reject rate or HTTP error rate above threshold in a trailing window;
- clock skew, or a window whose `S_open` we failed to record;
- **an unexpected fill** — any fill that does not match an order we believe we have open.
  This is the "we do not understand our own state" condition and it is unconditionally
  fatal;
- the presence of a `kill-switch` file at a configured path (the manual, no-SSH-needed
  stop), and a config flag.

Tripped ⇒ cancel everything, quote nothing, alert, and require a **manual restart**. No
automatic recovery from a tripped kill switch, ever.

### 7.7 Live-mode gating (identical to Phase B)

- `mode = "live"` alone is insufficient; `crypto.live_enabled = true` is a second,
  independent flag, and the binary logs the full effective cap set at startup.
- Keys and CLOB API credentials **only** from environment variables
  (`POLYMARKET_PRIVATE_KEY`, `POLYMARKET_API_KEY`/`SECRET`/`PASSPHRASE`), never in config,
  logs, or errors. `src/alert.rs::redact` is already the pattern for this.
- Dedicated hot wallet, funded with loss-tolerable capital only.
- The engine refuses to *read* credentials at all when not in live mode, so a dry-run
  process cannot be one config typo away from trading.
- First live period is time-boxed and size-boxed by the C3 gate (§10).

---

## 8. Dry-run design

### 8.1 What the dry run is

The full engine runs — feeds, model, quoting decisions, cancels, risk checks — and instead
of sending orders it writes **intended orders** to the store with their exact timestamps
and prices, then simulates their fate against the recorded market data. Same code path,
one substitution at the boundary. This mirrors Phase A, where `src/dryrun.rs` runs the real
detectors and only the order path is absent.

### 8.2 Honest maker fill simulation

This is the part that decides whether the soak is worth anything, and the default assumption
everywhere is **pessimism**.

**Queue position: we are always LAST.** When we place a simulated post-only buy at price
`p`, we record `Q0` = the total size already resting at level `p` at that instant. We are
behind all of it. Then:

- We accumulate `V` = observed volume that has **traded** at price `p` or better on our
  side since placement, taken from the CLOB market-channel trade stream.
- **A fill is credited only once `V > Q0`**, and then only for `min(our_size, V − Q0)`
  shares. In practice this means our order fills only when the level **fully trades
  through** — the level being *emptied* is not enough, it must be emptied *by trades*.
- **Cancellations ahead of us are never credited.** If the level shrinks without a
  corresponding trade print, `Q0` is *not* reduced. Real queue position improves when
  people cancel ahead of you; we deliberately refuse ourselves that benefit, because we
  cannot verify it from public data and because assuming it is how simulators flatter
  themselves.
- If the trade/cancel distinction is unavailable (REST-only fallback), **nothing is ever
  credited** — and the run is marked as producing no fill evidence, rather than producing
  optimistic evidence.
- Self-impact is ignored in the pessimistic direction only: we do not model our own quote
  attracting flow.

**Every simulated fill records:**

| Field | Why |
|---|---|
| `fair_value_at_fill` | the model's `P` at the fill instant |
| `reference_price_at_fill` | `S_t`, for reconstruction |
| `markout_1s`, `markout_5s`, `markout_30s` | fair value *after* the fill minus fill price |
| `markout_resolution` | the settled outcome minus fill price — the only one that is real money |
| `queue_ahead_at_placement` (`Q0`) | so we can re-run with different queue assumptions |
| `latency_ms` | measured decision→(would-be) ack |

**Markout is the headline metric, not P&L.** If our simulated fills have negative markout
at +5s, we are being adversely selected and the strategy is dead no matter how the pair
arithmetic looks — because the pair arithmetic assumes we bought below fair value, and
negative markout is the direct measurement that we did not. A soak that reports positive
simulated P&L *and* negative markout is reporting luck.

### 8.3 The soak metrics that would justify going live

Minimum **4 UTC weeks** of continuous dry-run on BTC 5m + 15m and ETH 5m + 15m. At 288
windows/day for the 5m series and 96 for the 15m, that is ~21,000 windows — enough to make
the following statistics mean something, which is precisely the reason to be in this market
at all (§1).

**Part A — did the engine work?** (mirrors `docs/deploy.md` §13 Part A)

- [ ] Feed uptime ≥ 99.5% per week; total halt time < 0.5%; zero unexplained gaps.
- [ ] `S_open` recorded for ≥ 99.9% of windows; **zero** windows quoted without one.
- [ ] Resolution prediction from our recorded feed matches actual settlement on **100%** of
      settled windows (the §3.2 gate, continuously re-checked).
- [ ] Zero kill-switch trips from "unexpected fill" or state-inconsistency causes.

**Part B — is there anything there?**

- [ ] **Simulated maker fill rate** > 0 and stable — under the last-in-queue rule this is
      expected to be low; if it is *zero*, our quotes are too deep to ever be hit and the
      strategy is a theory with no fills, which is a clean NO-GO.
- [ ] **Pair completion rate**: of windows where leg 1 filled, the share where leg 2 also
      filled within the pair budget. A completion rate below ~40% makes the structure a
      lottery-ticket buyer rather than an arbitrageur.
- [ ] **Distribution of completed pair cost** — p50 and p90 must sit meaningfully below
      $1.00 (say p50 ≤ 0.95), not at 0.99.
- [ ] **Markout at +5s and +30s ≥ 0** on simulated fills, in aggregate and in each `τ`
      bucket. Negative markout in the late-`τ` buckets with positive markout early is a
      strong instruction to raise `stop_opening_at_tau_secs`, not to go live.
- [ ] **Markout to resolution** positive on unpaired legs — the honest measure of the
      lottery tickets.

**Part C — is it worth the risk?**

- [ ] Simulated net P&L positive in **at least 3 of the 4 weeks**, not one anomalous week.
- [ ] Positive across at least two distinct volatility regimes (bucket the weeks by
      realised `σ`; a strategy that only works in one regime will meet the other one live).
- [ ] Return on the *working* capital that was actually deployed, annualised honestly,
      exceeding what the same capital does in the NegRisk book — because this engine only
      exists to beat that alternative.
- [ ] The absolute number matters to the owner after discounting it by at least half for
      everything a dry run cannot simulate: real queue position, our own market impact,
      partial fills, API rejects, and the days the bot is broken.

Anything less is a NO-GO, and a NO-GO here is a good outcome: it costs four weeks of a
process running and saves a hot wallet.

---

## 9. Integration with polyarb

### 9.1 Same binary, new module — recommended

```
src/crypto/
  mod.rs         engine wiring, the run loop, mode gating
  series.rs      window discovery, window metadata, S_open capture
  feed.rs        Binance/Coinbase WS clients, health monitor, kill-switch hooks
  book.rs        CLOB WS book tracking (falls back to src/clob.rs REST)
  fair_value.rs  §5 model + volatility estimator
  quote.rs       §7 reservation pricing, order construction, legging policy
  sim.rs         §8 pessimistic fill simulation, markout accounting
  risk.rs        crypto caps, correlated exposure, kill switch
  record.rs      raw feed/book capture for backtests
```

New subcommands, alongside `markets` / `scan` / `run` / `report`:

| Command | Does |
|---|---|
| `polyarb crypto record` | C0: subscribe, record, monitor health. Places nothing, decides nothing. |
| `polyarb crypto calibrate --from <dir>` | C1: offline model fitting and calibration report. |
| `polyarb crypto run` | C2+: the engine. Refuses any mode but `dry-run` unless both live flags are set. |
| `polyarb crypto report [--date]` | daily crypto summary through the existing report path. |

**Why the same binary.** The crypto engine reuses `types.rs` (Decimal domain types,
`OrderBook`, `Label`), `costs.rs` (the verified fee curve — one fee table, not two),
`config.rs` (loading, env overrides, validation), `store.rs` (SQLite, Decimal-as-TEXT),
`alert.rs` (Telegram + JSONL fallback + circuit breaker), and `http.rs`. Duplicating those
into a separate crate before we know the engine is worth building would double the surface
that has to stay correct, and the first thing to rot would be the fee table. One binary
also means one Docker image and one deployment, which `docs/deploy.md` already covers.

**Two concrete costs, both manageable:**

1. **The loop cadences differ** (5s scanner vs. sub-second crypto). Solution: the crypto
   engine is its own set of tokio tasks with its own intervals; it does not share the scan
   loop. `polyarb run` and `polyarb crypto run` are separate processes in the compose file,
   not one process doing both, so a crypto crash cannot take down the politics soak.
2. **`HttpClient` throttling is per-instance.** `src/http.rs` holds `next_allowed` in a
   `Mutex` inside each `HttpClient`, so two clients means two independent request budgets
   against the *same* Polymarket hosts — an easy way to earn 429s and degrade the politics
   soak. **Whatever else changes, the request budget must become per-host and shared**
   (a small shared limiter behind an `Arc`), or the two processes must be given explicitly
   partitioned budgets in config. This is a prerequisite, not a nice-to-have.

**Migration path, and the trigger for it.** If the engine ever needs a genuinely different
runtime — sub-100ms hot path, no allocation in the decision loop, a different async
strategy — promote to a Cargo workspace: `crates/polyarb-core` (types, costs, config,
store, alert, http), `crates/polyarb` (scanner), `crates/polyarb-crypto`. The trigger is
explicit: **when the crypto decision path's p99 latency budget is being spent inside our
own process rather than on the wire.** Until then, splitting is premature.

### 9.2 Config additions

A new `[crypto]` section, `enabled = false` by default, validated by the existing
`Config::validate` (which must reject a crypto config that is enabled while `mode` is not
dry-run and `crypto.live_enabled` is not set):

```toml
[crypto]
enabled = false
live_enabled = false                 # second, independent live gate
underlyings = ["btc", "eth"]
series = ["btc-5m", "btc-15m", "eth-5m", "eth-15m"]   # TODO(verify-live): real slugs
bankroll_usd = 100                   # NOT the wallet balance

[crypto.feed]
binance_ws_url = "wss://stream.binance.com:9443/stream"
coinbase_ws_url = "wss://ws-feed.exchange.coinbase.com"
staleness_halt_ms = 2000
divergence_halt_bps = 25
clock_skew_halt_ms = 1000
recovery_secs = 30

[crypto.series_resolution]           # §3.2: unmapped series are never quoted
"btc-5m" = "TODO-VERIFY"

[crypto.fair_value]
sigma_halflife_secs = 300
sigma_slow_halflife_secs = 3600
sigma_floor_bps = 2
sigma_cap_bps = 200
calibrated = false                   # set by a human after reading the C1 report

[crypto.quote]
min_abs_d = 1.5                      # never quote closer to the money than this
delta_edge = 0.02                    # minimum per-share edge, cents
adverse_z = 2.0                      # z in delta_adverse
gamma = 0.5                          # inventory risk aversion
max_age_secs = 8
blocks = 3
block_size_shares = 5
pair_budget = 0.97
stop_opening_at_tau_secs = 60
force_flat_tau_secs = 20

[crypto.risk]
max_notional_per_window_usd = 25
max_notional_per_underlying_usd = 50
max_unpaired_notional_total_usd = 40
max_open_windows = 4
daily_loss_limit_usd = 25
max_consecutive_losing_windows = 8
eth_btc_correlation = 0.85
allow_taker_exit = false
kill_switch_file = "data/crypto/KILL"

[crypto.near_resolution]
enabled = false                      # §6.2 — negative skew, default off
max_entry_price = 0.98
max_notional_per_window_usd = 5

[crypto.sim]
recordings_dir = "data/crypto/recordings"
credit_cancels_ahead = false         # never set true; present so the pessimism is explicit
```

New env overrides following the existing `POLYARB_*` convention:
`POLYARB_CRYPTO_ENABLED`, `POLYARB_CRYPTO_LIVE_ENABLED`, `POLYARB_CRYPTO_BANKROLL_USD`,
`POLYARB_CRYPTO_DAILY_LOSS_LIMIT_USD`. **No secret ever appears in this file** — the same
test that asserts `config/default.toml` grows no credential fields must cover the crypto
section.

### 9.3 Storage: migration v2

`src/store.rs` migrations are append-only ("never edit a shipped migration"), so this is a
new `(2, "crypto engine", SCHEMA_V2)` entry. All money columns are `TEXT` Decimal strings;
no `REAL` anywhere.

| Table | Contents |
|---|---|
| `crypto_windows` | series, window_id, `t0`, `t_close`, `s_open` TEXT, `s_close` TEXT, resolution_source, settled_outcome, our verdict vs. actual |
| `crypto_quotes` | intended orders: ts, window_id, token_id, side, `price` TEXT, `size` TEXT, post_only, block_index, `q0_queue_ahead` TEXT, status (`resting`/`cancelled`/`expired`/`filled`), reason |
| `crypto_fills` | quote_id, ts, `price`, `size`, `fair_value_at_fill`, `reference_price_at_fill`, markouts at 1s/5s/30s/resolution, `latency_ms` |
| `crypto_positions` | per window: paired shares, unpaired shares by side, `pair_cost`, realised P&L, label (`true_arb` once paired, `relative_value` before) |
| `crypto_feed_health` | minute buckets: ticks, max gap ms, halts, max divergence bps, reconnects |
| `crypto_daily` | the daily aggregate the report renders from |

**Raw tick data does not go in SQLite.** Two `bookTicker` streams generate on the order of
10⁵–10⁶ messages/day; recordings belong on disk as compressed newline-JSON under
`crypto.sim.recordings_dir`, one file per series per UTC hour, with SQLite holding only
aggregates and decisions. Disk budget: measure in C0 before committing to a retention
policy; assume ~0.5–2 GB/week uncompressed until measured, and note that
`docs/deploy.md`'s VPS sizing does not currently account for it.

---

## 10. Milestones and go/no-go gates

Effort figures are for one developer working with an LLM, and exclude soak wall-clock time.

### C0 — Recorder and resolution verification (~4–6 days work, 2+ weeks soaking)

Build: Binance + Coinbase WS clients, feed health monitor, crypto series discovery via
Gamma, window metadata + `S_open` capture, REST book snapshotting, disk recorder,
`polyarb crypto record`. **No model, no quotes, no orders.**

**Gate — all must pass:**

- [ ] Resolution source identified for every series in scope, and our recorded feed
      predicts the settled outcome on **100% of ≥ 200 settled windows**, including ≥ 20 that
      settled within 0.02% of the open. *This is the project's single most important gate.*
- [ ] Tie rule confirmed empirically (find or construct the case).
- [ ] Feed uptime ≥ 99.5% over 2 weeks; halt behaviour verified by deliberately killing the
      socket.
- [ ] Measured book depth on the 5m/15m series at the price levels we intend to quote. If
      the book is 20 shares deep at `d = 1.5`, the strategy has no capacity and we stop.
- [ ] Tick size, minimum order size, and NegRisk-vs-binary structure confirmed.
- [ ] Recording volume and disk growth measured.

**Free bonus at this gate:** the near-resolution-capture backtest (§6.2) — measurable from
recordings alone, no trading code required.

### C1 — Fair value and calibration (~3–4 days)

Build: model, volatility estimator, offline calibration harness, `polyarb crypto calibrate`
and its report.

**Gate:**

- [ ] Reliability diagram, Brier and log-loss, bucketed by `τ` and `|d|`, versus market mid
      and versus 50/50.
- [ ] An explicit written verdict on whether the model beats market mid in the quoting
      region. If not, `fair_value.calibrated` stays `false` and every model-edge-sized
      structure is off the table — the design continues with structure-only sizing or
      stops.
- [ ] The empirical `√(Δt/τ)` adverse-selection relationship from §4.3 confirmed against
      recorded data (it is a testable prediction; if it does not hold, the model is wrong in
      a way that matters).

### C2 — Quote engine in dry-run (~8–12 days, 4+ weeks soaking)

Build: CLOB WebSocket book tracking, reservation-price quoting, legging policy, risk caps,
correlated exposure, kill switch, pessimistic fill simulation, markout accounting, crypto
daily report, migration v2.

**Gate:** the full §8.3 soak checklist. Explicitly: a low fill rate is informative, a *zero*
fill rate is a NO-GO, and negative markout is a NO-GO regardless of simulated P&L.

### C3 — Live, time-boxed and size-boxed (~8–12 days + review)

Build: CLOB authentication, EIP-712 order signing, order placement/cancel, reconciliation
against the exchange's view of our orders, live-mode gating, live risk enforcement.

**Gate before enabling:**

- [ ] Owner sign-off in writing, on the C2 report, with the caps read out loud.
- [ ] `crypto.bankroll_usd` ≤ $100 for the first two weeks, `max_notional_per_window_usd`
      ≤ $10.
- [ ] Dedicated hot wallet, funded only with loss-tolerable capital, key in the environment
      only.
- [ ] A dry-run and a live process running **side by side** for the first two weeks, so the
      simulator's predictions can be compared against real fills. Any material divergence
      (real fill rate far above simulated, or realised markout far below simulated) stops
      live trading and sends us back to C2 with a corrected simulator.
- [ ] Kill switch tested live, including the file-based manual stop.

### C4 — Inventory market-making (not estimated)

Only opened if C3 shows positive *realised* markout over ≥ 4 weeks. Estimating it now would
be false precision.

**Total, if every gate passes: roughly 4–6 weeks of build and 8–10 weeks of wall clock.**
The gates are designed so that the most likely outcomes are cheap: C0 alone (~1 week of
work) can kill the project outright on the resolution-source or depth findings, and it is
useful data even then.

---

## 11. Open questions and `TODO(verify-live)`

Nothing below is answerable from the development container; every one is a C0 task.

**Resolution and settlement**

1. **Resolution source per series** — Binance `BTCUSDT`? Chainlink? Pyth? a candle close?
   Different per series? *(blocking; §3.2)*
2. **Opening price definition** — tick at `T0`, previous candle close, or first print after
   `T0`? *(blocking)*
3. **Tie rule** — does close == open resolve Down? *(blocking for §6.2)*
4. **Settlement instant** — instantaneous tick or end-of-candle? Determines the true `τ` in
   the final minute.
5. **UMA involvement** — are these auto-resolved from a price feed or subject to the normal
   dispute path? A disputable 5-minute market has a resolution-risk profile nothing in this
   design accounts for.

**Market structure**

6. **Are the windows plain binary or NegRisk?** *(§4.1)*
7. **Tick size** — 0.01 throughout, or 0.001 in the tails? Decides whether deep quoting has
   any price resolution at all.
8. **Minimum order size** — shares and/or notional. The scanner assumes ~5 shares; if the
   crypto minimum is larger, `block_size_shares` and every cap must move.
9. **Forward listing** — how early is the next window quotable? *(§4.1; gates v2)*
10. **Gamma filter** for a series — `tag_id`, slug prefix, or a dedicated endpoint?
11. **Book depth** at `|d| ≥ 1.5` price levels, and how it varies with `τ`. Capacity
    question; if there is no depth there is no strategy.
12. **Series inventory** — do BTC 15m / ETH 15m actually exist, and are hourly/daily
    variants relevant?

**Fees and subsidies**

13. **Liquidity reward parameters** (`min_incentive_size`, `max_incentive_spread`) on these
    series — do deep quotes qualify? *(§7.2; assume no)*
14. **Maker rebate** mechanics and rate on crypto specifically.
15. **Crypto taker rate still 0.07?** Protocol parameter, re-check periodically. (Verified
    2026-07.)

**Plumbing**

16. **CLOB WebSocket** endpoint, channels, auth requirements, message schema, and whether
    the public market channel distinguishes trades from cancels. *(blocking for §8.2)*
17. **Rate limits** for orders/cancels per second — directly caps `quote.max_age_secs` and
    the re-quote strategy.
18. **CLOB accept→book-update latency**, measurable only with a live order.
19. **Post-only / GTD support and semantics** — confirmed by docs, unverified by us.
20. **Binance geo-restriction** on the chosen host region, and the Binance.US/Coinbase
    fallback's basis versus the resolution source.

**Model**

21. **BTC/ETH intraday return correlation** measured on our own data, to replace the 0.85
    default in §7.5.
22. **Whether `ΔP(1σ) = φ(d)√(Δt/τ)` holds empirically** — the design's central quantitative
    claim, and cheap to falsify.

---

## 12. Where this design diverges from the research

The research doc is the primary source and is followed closely on architecture (the
six-layer stack, the decision chain, legging policy, reservation pricing, post-only,
size-splitting, kill switches). Seven deliberate departures:

1. **Five position structures → one, plus one flagged off.** The research presents temporal
   arbitrage, hedged directional, inventory MM, near-resolution capture and dynamic rotation
   as a menu. We adopt only temporal arbitrage for v1, gate near-resolution capture behind a
   default-off flag, defer inventory MM to v2, and defer hedged directional and dynamic
   rotation indefinitely. *Reason:* the §4.3 latency table and the 0.07 fee tier leave only
   the structures where someone else crosses to us and where our staleness exposure is
   bounded. Rotation in particular is the structure most likely to backtest well and lose
   money live.

2. **"Maker vs. taker is situational" → maker-only, as an absent code path.** The research
   says the execution engine should trade price improvement against fill probability
   continuously and cross when the market is repricing fast. That is correct advice for
   someone fast enough to know the market is repricing. At 0.07 and 2–5 second reaction, a
   deliberate cross is a decision to pay 1.75¢/share to be late. v1 has no taker path at
   all.

3. **Multi-signal Bayesian updating → one signal in v1.** The research describes Bayesian
   updating over momentum, volume, book imbalance and cross-asset co-movement, with a
   warning about double-counting correlated signals. We take the warning to its conclusion:
   v1 uses price-versus-open and vol only, zero drift, and any new feature must clear the
   four-part joint-fit/out-of-sample/log-loss/orthogonalisation test in §5.3. This is more
   conservative than the source, on the source's own logic.

4. **Fractional Kelly as sizing baseline → Kelly as a ceiling that is off by default.**
   The research proposes ~25% Kelly as the baseline with hard caps on top. We invert it:
   hard caps size everything in v1, and Kelly is clamped to zero until the model has beaten
   market mid out-of-sample (§5.4). Kelly on an uncalibrated probability sizes up a
   non-existent edge.

5. **Near-resolution capture reframed from "high win rate" to "systematically
   adversely-selected and negatively skewed".** The research flags the skew; we add the
   arithmetic (99.07% breakeven win rate at 99¢; one loss erases 106 wins) and the
   observation that our taker fills at 99¢ are selected against us by faster participants
   who chose not to cancel. It stays in the design only because it is **free to falsify from
   C0 recordings** — and it is deleted rather than gated if the empirical win rate misses
   the breakeven.

6. **Maker subsidies are not counted in EV.** The strategy report establishes that maker
   capture is positively subsidised (zero fees + rebates + daily liquidity rewards). We keep
   the zero-fee benefit, which is unconditional, but explicitly exclude liquidity rewards
   from v1 economics: rewards require quoting *within* `max_incentive_spread` of mid, and
   our latency forces us to quote far from it. Naming that tension is more useful than
   quietly assuming the subsidy.

7. **The article's PnL claims are load-bearing nowhere.** "100+ bots each making $50K+/month"
   informs no sizing, no capacity estimate and no gate. Every number in this design derives
   from the verified fee schedule, the `Φ`/`φ` model, or a measurement scheduled in C0.
   Consistent with the research doc's own credibility caveat.

One thing we take from the research *more* seriously than it states: its remark that
"a strategy that only makes money when every order fills at the best visible price is not
ready for production" becomes the last-in-queue fill rule in §8.2 — the single most
consequential design decision in the document, and the one most likely to make the soak
report disappointing. That is the intent.
