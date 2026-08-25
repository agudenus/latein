# Polymarket Liquidity Rewards — State of the Program, August 2026

Research for the polyarb pivot: can we earn quoting rewards (where *not* being filled is normal)
instead of spread capture (where our 11-day soak produced 5 fills / 6,094 orders)?

**Research constraint you must know before trusting anything here.** This session's egress proxy
blocks `polymarket.com`, `docs.polymarket.com`, `help.polymarket.com`, `medium.com`, `x.com`,
`reddit.com`, and every third-party dashboard (`polyscalping.org`, `polyield.xyz`, `telonex.io`,
`dune.com`). Only `raw.githubusercontent.com`, `api.github.com` and `pypi.org` were directly
fetchable. Everything not sourced from GitHub raw files below comes from **WebSearch result
summaries of pages I could not open myself**. That is a real degradation: search summaries are
paraphrases, they occasionally blend two sources, and several of the sites being paraphrased are
SEO/affiliate content farms with an incentive to overstate earnings. I flag confidence per claim
and I have deliberately *not* smoothed over the contradictions I found.

---

## 1. Mechanics

### 1.1 Two different programs — do not confuse them

| | **Global Polymarket** (what we'd use) | **Polymarket US** (QCX LLC, CFTC-regulated) |
|---|---|---|
| Docs | docs.polymarket.com/programs/liquidity-rewards | docs.polymarket.us/incentives/liquidity |
| Scoring | Quadratic in distance from adjusted midpoint, linear in size | `discount_factor ^ (ticks from best price) × size` |
| Sampling | ~every minute, random | every second, snapshot-normalised |
| Params | `rewards_min_size`, `rewards_max_spread`, daily rate | `Target Size`, `Discount Factor`, `Time Period Reward` |
| Payout | Daily, ~00:00 UTC, direct to maker address | Within 5 business days of period end + 2 days to credit |
| Schedule published? | No public per-market schedule page found | Yes — live at polymarket.us/rewards |

The US program's formula is structurally identical to Kalshi's Liquidity Incentive Program (same
"Target Size / Discount Factor" vocabulary). It is **not** available to a non-US operator and needs
KYC on a US entity, so it is out of scope for us — but be aware that a large share of the 2026 web
writing about "Polymarket liquidity rewards" is actually describing the US program's per-second
discount-factor scheme. If you see "discount factor ^ ticks", that source is talking about the wrong
venue for us.

*Confidence: verified (two independent doc trees, distinct formulas, corroborated by the CFTC-portal
notice "Market Incentive Program (2026.03.05)" hosted at polymarketexchange.com).*
Sources: <https://docs.polymarket.us/incentives/liquidity>, <https://polymarket.us/rewards>,
<https://help.kalshi.com/en/articles/13823851-liquidity-incentive-program>,
<https://www.polymarketexchange.com/files/notices/Market%20Incentive%20Program%20(2026.03.05).pdf>

### 1.2 The global Q-score formula

Per qualifying order, per sample:

```
S(v, s) = ((v - s) / v)^2 * b
```

- `v` = `rewards_max_spread`, the qualifying half-band, **in cents** from the adjusted midpoint
- `s` = the order's distance from the adjusted midpoint, in cents
- `b` = order size **in shares** (not dollars)
- `s > v` ⇒ score 0. Size below `rewards_min_size` ⇒ score 0.

Side aggregation and normalisation:

```
Q_one  = Σ S(v, spread_bid_i) * BidSize_i          (bid side)
Q_two  = Σ S(v, spread_ask_j) * AskSize_j          (ask side)
Q_min  = max( min(Q_one, Q_two), max(Q_one/c, Q_two/c) )    with c = 3
Q_normal = Q_min / Σ_n (Q_min)_n                   (per sample, across makers)
Q_epoch  = Σ_samples Q_normal
Q_final  = Q_epoch / Σ_n (Q_epoch)_n               (your share of the day's pool)
reward   = Q_final * market_daily_reward_pool
```

Documented worked example (adjusted midpoint 0.50, `max_spread` = 3 cents):
`Q = ((3-1)/3)² × 100 + ((3-2)/3)² × 200 + ((3-1)/3)² × 100`.

Midpoint bands:
- midpoint ∈ [0.10, 0.90] → single-sided quoting still scores, at `Q/3` (the `c = 3` penalty)
- midpoint ∈ [0, 0.10) or (0.90, 1.0] → **must be two-sided**; one-sided scores nothing

The midpoint used is the **size-cutoff-adjusted midpoint**: computed after discarding orders below
`rewards_min_size`, specifically to stop someone pinning a fake midpoint with dust orders.

*Confidence: high for the formula shape and the c=3 penalty (the same formula appears verbatim in
three independent places: search summaries of the official docs; the ported scoring code in
`Birantx/polymarket-lp-bot/src/lpbot/quoting.py`; and a third-party optimizer that claims ±1%
agreement with realised on-chain payouts). Medium for the exact `Q_min` expression — I could not
open the doc page myself and am relying on a search summary reproducing it.*
Sources: <https://docs.polymarket.com/programs/liquidity-rewards>,
<https://raw.githubusercontent.com/Birantx/polymarket-lp-bot/main/src/lpbot/quoting.py>,
<https://opt-markets.com/docs>

### 1.3 The economically important consequence of "size in shares"

Score is linear in **shares**, and the natural two-sided structure is *two bids* — buy-YES at `p`
and buy-NO at `q` — because a filled pair merges back to $1 of USDC (this is what
`warproxxx/poly-maker` does, and it is the only way to quote both sides without pre-holding
inventory). For an in-band pair `p ≈ mid − s`, `q ≈ 1 − mid − s`:

- capital per paired share = `p + q = 1 − 2s` ≈ **$1 per share, independent of market price**
- score per paired share = `((v−s)/v)² × N` on each side ⇒ `Q_min = ((v−s)/v)² × N`

So **score per dollar of quoting capital ≈ ((v−s)/v)², independent of the market's price level**.
There is no "cheap penny-market" edge; the two-sided requirement below $0.10 closes that door
deliberately. And your share of a pool is, to first order:

```
your_daily_reward ≈ pool_per_day × (your_capital / total_in-band_capital) × (your_f / mean_f)
```

which means the single number that decides whether a market is worth quoting is
**`pool_per_day ÷ in-band resting notional` = a daily return on capital.**

Two-sided vs single-sided, per dollar: two-sided gives 3× the score for 2× the capital = 1.5× score
per dollar. Always quote two-sided.

*Confidence: inference (mine), but mechanical — it follows from the documented formula plus the
observed YES-bid/NO-bid structure. Worth re-deriving against live data before sizing on it.*

### 1.4 Epochs, payouts, thresholds

- Scoring samples ~once per minute (random sampling within the minute); epoch = one UTC day.
- Payout daily at ~00:00 UTC, direct to the maker address. Post-CLOB-v2 the collateral token is
  **pUSD**; sources disagree on whether liquidity rewards now land as pUSD or USDC (both are
  claimed by different 2026 write-ups). Assume pUSD, verify on first payout.
- **Minimum $1/day. Below that, nothing is paid and it does not roll over.**
- Rewards are computed **per market, no cross-market netting**.

The `$1` threshold is the single most consequential unknown for our capital size (see §5).
The API schema is ambiguous: the `UserReward` object (per `date` + `condition_id` + `asset_address`)
carries `status ∈ {estimated, closed, below_minimum, paid}` — implying a **per-market** test — but
`UserRewardTotal` carries the same status enum, and the `/rewards/user/total` endpoint is described
as "the wallet total after independent program thresholds", which could mean the test is applied to
the wallet total per program. One retired community bot states flatly it is per market and that this
is why they shut down.

*Confidence: verified for daily/00:00 UTC/$1 minimum/per-market scoring (multiple independent
sources incl. the KuCoin summary of the program rules). **Single-source / genuinely ambiguous** for
whether the $1 test is per-market or per-wallet-per-day. Resolve empirically — it is cheap to.*
Sources: <https://www.kucoin.com/news/flash/polymarket-lp-incentive-mechanism-four-key-insights-and-cost-traps>,
<https://help.polymarket.com/en/articles/13364466-liquidity-rewards>,
<https://github.com/ALLmightyn/MarketMakerBot>

### 1.5 Sponsored rewards (new, and it changes the market universe)

Anyone can top up a market's reward pool. Deposit USDC, choose a duration ($500 over 10 days =
$50/day). **Minimum commitment $0.10/day.** One active sponsorship per market at a time,
cancellable with a refund from the next 00:00 UTC. Sponsored and native rates are tracked
separately in the API (`sponsored_daily_rate`, `native_daily_rate`, `total_daily_rate`;
`sponsored=true|false` query param on the rewards endpoints).

The Feb 17, 2026 "Will Jesus Christ return before 2027?" market is the documented extreme case:
~$70,000 of sponsored rewards was added (reportedly by accident), makers flooded in within seconds,
and Telonex's tick-level study measured the structural break:

| metric | pre (Feb 10 → event) | post (Feb 18–20) |
|---|---|---|
| median top-of-book depth | 38,386 | 900,931 (23.5×) |
| median depth to 25 levels | 1,468,280 | 22,068,638 (15.0×) |
| cost to move price 1¢ up | 19,821 | 155,297 (7.8×) |
| book updates / hour | 684 | 2,631 |
| replenish rate | 27.3% | 49.9% |
| new-cohort median marked PnL | — | $7.52 (vs $30.10 pre-event cohort) |

Telonex also report maker concentration *fell*: HHI down ~4×, top-5 share down ~20pp, active makers
more than doubled. Read that as: **a fat pool attracts enough capital, fast enough, to compete the
per-dollar yield back down.** The new cohort earned a median of $7.52 marked PnL.

*Confidence: verified — I pulled these numbers out of Telonex's own published notebook outputs on
GitHub, not from a summary. The HHI/top-5 claims are from the article summary (single-source).*
Sources: <https://raw.githubusercontent.com/telonex/research/main/research/liquidity_rewards_jesus/liquidity_rewards_jesus.ipynb>,
<https://telonex.io/research/sponsored-liquidity-rewards-jesus-market>,
<https://help.polymarket.com/en/articles/13755867-sponsor-market-rewards>

### 1.6 Where the parameters live in the APIs

**CLOB (`clob.polymarket.com`)** — reward endpoint set confirmed from the official (now archived)
TypeScript client's `endpoints.ts`, and independently from a mirrored OpenAPI spec:

| Endpoint | Auth | Returns |
|---|---|---|
| `GET /sampling-markets?next_cursor=` | none | **only reward-eligible markets**, each with `rewards: {rates: [{asset_address, rewards_daily_rate}], min_size, max_spread}`, plus `neg_risk`, `minimum_order_size`, `minimum_tick_size`, `tokens[]` |
| `GET /sampling-simplified-markets` | none | same universe, trimmed |
| `GET /rewards/markets/current?sponsored=` | none | all currently incentivised markets |
| `GET /rewards/markets/{condition_id}` | none | `rewards_max_spread`, `rewards_min_size`, `rewards_config[] {rate_per_day, total_rewards, start_date, end_date}` |
| `GET /rewards/markets/multi?condition_ids=` | none | batch, ≤500 ids |
| `GET /order-scoring?order_id=` | L2 | `{scoring: bool}` — **is this specific resting order earning right now** |
| `POST /orders-scoring` | L2 | map of order_id → bool |
| `GET /rewards/user?date=` | L2 | per-market earnings for a day, with `status` |
| `GET /rewards/user/total?date=` | L2 | wallet total by collateral asset |
| `GET /rewards/user/percentages` | L2 | latest normalised share per condition |
| `GET /rewards/user/markets?date=&no_competition=` | L2 | per-market earnings **plus `market_competitiveness` and `earning_percentage`** |

`GET /order-scoring` and `GET /rewards/user/markets` are the two that matter most for us:
the first is ground truth that our quote qualifies; the second hands us the competitiveness metric
without us having to reconstruct it from books.

**Gamma (`gamma-api.polymarket.com`)** exposes `rewardsMinSize`, `rewardsMaxSpread`, and
`clobRewards[]` (with `rewardsDailyRate`) on market objects. Summing `rewardsDailyRate` across
eligible Gamma markets is how the public dashboards compute the "live rewards pool" number.

**Unit trap, load-bearing:** `max_spread` is in **cents** (e.g. `3.5`) while book prices are
fractions of a dollar (`0.035`). A ported implementation flagged this explicitly as a 100× bug
class. `min_size` is in **shares**, and is distinct from the CLOB's own `minimum_order_size`
(5 shares) — an order can be legal and still score zero.

Observed live parameter values across public fixtures/captures: `max_spread ∈ {1.5, 3.0, 3.5, 4.5,
5.5}` cents, `min_size ∈ {20, 50, 100, 200}` shares, `rewards_daily_rate ∈ {5, 30, 50, 75, 100, 124,
150, 286, 428, 476, 750, 1667, 2143, 2381, 3333}` USD/day.

Note the **new** official SDK (`Polymarket/ts-sdk`, which replaces the now-archived `clob-client`)
does **not** yet expose the rewards endpoints; `py-clob-client` exposes `order-scoring` and
`sampling-markets` but **not** the `/rewards/*` family. We'd be calling those over plain HTTP.
Also present in a Polymarket-derived OpenAPI mirror but *not* confirmed on Polymarket itself:
`minimum_order_age` (an anti-flicker rule requiring an order to rest N seconds before scoring).
Treat as an unverified possibility.

*Confidence: verified for the endpoint list and payload shapes (read directly from official SDK
source + a mirrored spec + real captured JSON fixtures). Inference for `minimum_order_age` applying
to Polymarket. Unverified live — this session cannot reach the API.*
Sources: <https://raw.githubusercontent.com/Polymarket/clob-client/main/src/endpoints.ts>,
<https://raw.githubusercontent.com/Polymarket/clob-client/main/src/types.ts>,
<https://raw.githubusercontent.com/Polymarket/py-clob-client/main/py_clob_client/endpoints.py>,
<https://raw.githubusercontent.com/kuestcom/prediction-market/main/docs/api-reference/schemas/openapi-clob.json>,
<https://raw.githubusercontent.com/Birantx/polymarket-lp-bot/main/src/lpbot/scoring.py>,
<https://github.com/nautechsystems/nautilus_trader/blob/develop/crates/adapters/polymarket/test_data/clob_market_closed_binary_accepting_true.json>

### 1.7 The adjacent programs that stack (or don't)

- **Maker Rebates** — 25% of collected taker fees rebated to makers daily in pUSD (20% crypto,
  **15% sports** after a Jul 2026 cut from 25%). Allocated *pro rata by your share of maker
  liquidity that actually got taken*, per market. **This requires fills.** Given our soak, treat
  expected rebate income as ~$0 until we're quoting at touch, and even then it is second-order.
- **Holding Rewards** — 3.25% annualised (launched at 4%) on total position value in selected
  long-dated markets, sampled randomly once per hour, paid daily. Relevant only if we end up
  *holding* inventory, which we mostly don't want to.
- **Taker Rebates** — launched May 28, 2026, 7 volume tiers up to 50% back. Irrelevant to us.

*Confidence: verified for existence and mechanics; single-source for the exact 25/20/15 split and
the 3.25% figure.*
Sources: <https://docs.polymarket.com/programs/maker-rebates>,
<https://help.polymarket.com/en/articles/13364471-maker-rebates-program>,
<https://help.polymarket.com/en/articles/13364459-holding-rewards>,
<https://docs.polymarket.com/changelog>

---

## 2. Realistic earnings — and a contradiction I could not resolve

### 2.1 The distribution of what people actually earned

The best hard number I found: cumulative LP rewards paid **$12,866,173 across 66,567 addresses**,
reported 2026-02-17, with the program's reward data going back to **November 2023**.
Percentile thresholds on cumulative earnings:

| percentile | cumulative LP rewards earned, all-time |
|---|---|
| top 1% | ≥ $1,563 |
| top 1,000 wallets | ≥ $927 |
| top 10% | ≥ $49 |
| mean | ~$193 |

Read that table slowly. **Ninety percent of every wallet that has ever earned a Polymarket liquidity
reward has earned less than $49 in total, across 27 months.** The top 1% cutoff — over more than two
years — is $1,563, i.e. under $2/day. This is not a distribution where a $500–$2,000 account quietly
collects a salary. The mass of participants are incidental limit-order placers, and the genuine
earners are far out in a tail that these percentiles don't even show (the mean of $193 sitting ~4×
above the 90th percentile tells you the top fraction of a percent takes most of it).

*Confidence: single-source (one dashboard's on-chain aggregation, republished by several news
aggregators from the same underlying press item). The percentile cutoffs are consistent with the mean/total,
which is a weak internal check. I could not open the dashboard to verify the period definition of
"all-time".*
Sources: <https://www.kucoin.com/news/flash/polymarket-lp-rewards-exceed-12-86m-with-over-66-000-participating-addresses>,
<https://www.odaily.news/en/newsflash/468933>, <https://polyscalping.org/leaderboard>

### 2.2 The contradiction: $5M/month vs $12.86M all-time

Several 2026 guides state Polymarket distributes "a slice of its **$5+ million monthly** rewards
pool" daily. If that were the run-rate, the cumulative total by Feb 2026 would be an order of
magnitude above $12.86M. The two claims cannot both be right.

Most likely reconciliation, in my judgement:
1. The "$5M/month" figure is the sum of **configured** `rewardsDailyRate` across eligible markets —
   a *cap*, not a payout. A dashboard operator's own documentation says exactly this: the live pool
   number "represents the sum of `rewardsDailyRate` from Polymarket's Gamma API across every
   currently-eligible market" and "actual distribution per day can be less if individual markets
   don't reach their LP-activity threshold." Polymarket itself, describing the August 2026 crypto
   program, is reported to be "explicit that these are configured reward caps, not promised payouts."
2. Plus the $12.86M spans 2023–2026 including a much smaller early program, so the *current*
   run-rate is higher than its 27-month average of ~$476k/month.

**Practical consequence for us: never model income off the advertised `rewards_daily_rate`.**
It is an upper bound that is not necessarily paid out. The one honest way to calibrate is to
compare, for markets we actually quote, our realised `/rewards/user` payout against the advertised
pool and our computed Q share.

*Confidence: the contradiction is verified (both claims exist in current sources). The
reconciliation is **inference** — plausible, mechanically supported by the "caps not payouts"
language, but not proven.*
Sources: <https://polyscalping.org/lp-dashboard>, <https://tradoxvps.com/polymarket-liquidity-rewards/>,
<https://laikalabs.ai/prediction-markets/polymarket-liquidity-rewards>

### 2.3 The headline earnings claims, and why I discount them

Circulating numbers, in rough order of credibility:

| Claim | Source type | My assessment |
|---|---|---|
| "$200–300 USDC/day on ~10,000 USDC" | practitioner postmortem, describing an **early/peak period** that the same author says is over | Historical, not current. The author's own conclusion is the opposite of bullish. |
| "$200–800/day on $50K in sports/political markets, 2024–2026" | SEO guide | 0.4–1.6%/day. Unsourced, no methodology, affiliate-monetised site. Discount heavily. |
| "net returns 40–120% APY after adverse selection for a disciplined maker in 2026" | same SEO guide | Same. |
| "roughly target ~10% annualized" | practitioner postmortem | The only earnings claim from someone who ran it and published their reasoning. |
| "a $5,000/day pool with 200+ competing makers ⇒ your share ~0.05% ⇒ ~$2.50/day" | guide, illustrative | Structurally right, and the arithmetic is the one that matters. |

The practitioner postmortem's conclusion, verbatim in substance: *rewards became a thin "bonus" on
top of real trading edge rather than a standalone money printer; unless you have strong independent
alpha, treat liquidity rewards as a bonus, not the main profit engine; target long-dated, calm
markets (e.g. 2028 election markets without imminent catalysts).*

*Confidence: the postmortem is single-source but the author shows their reasoning and reaches an
unflattering conclusion about their own project, which is weak evidence of honesty. The SEO numbers
are **low confidence, treat as marketing**.*
Sources: <https://medium.com/@wanguolin/my-two-week-deep-dive-into-polymarket-liquidity-rewards-a-technical-postmortem-88d3a954a058>,
<https://polymarkets.co.il/en/guide/liquidity-rewards/>, <https://www.alphascope.app/blog/polymarket-liquidity>

### 2.4 The one directly relevant negative result

`ALLmightyn/MarketMakerBot` is a Polymarket liquidity-rewards market maker that was **live-tested
and then retired with a published post-mortem**. Its stated kill reason:

> The Liquidity Rewards program pays out only above a $1/day threshold per market; combined with
> the safe quoting distance required to avoid adverse selection, the reward-eligible order flow
> needed to clear that bar wasn't reachable at the capital this was sized for.

Its own validation data: ~59 hours of paper trading on calm markets gave **+$9.9 with 6 fills**;
a single stress event (a sports-tournament night) produced a **−$2,095 paper drawdown** on
correlated markets. It never left the $10 mainnet smoke-test phase.

That is precisely our proposed strategy, at roughly our proposed capital, killed for exactly the
reason our capital is exposed to. It is the most decision-relevant single artifact in this report.

**Caveat on all the community bots below, including this one:** these are 0-star repositories with
strong stylistic markers of AI-assisted authorship (badge-heavy READMEs, "261 tests / 96% coverage",
narrated build-order steps). Their *reasoning* is checkable and mostly sound; their *reported
numbers* are unaudited and could be fabricated or simulated. I weight the mechanism claims, not the
P&L claims.

*Confidence: the repo and post-mortem text are verified (fetched directly). The numbers inside it
are single-source and unaudited.*
Source: <https://github.com/ALLmightyn/MarketMakerBot>

### 2.5 Competition structure

- The big pools (elections, Fed, headline sports) are camped by automated professionals. One scanner
  observed a **$300/day pool sitting under ~$57,000 of in-band resting notional** — a yield of
  ~0.53%/day *if* that measure is right, which sounds attractive until you remember that yield is
  precisely the market's price for taking adverse selection there.
- The same scanner found **$30/day long-tail pools (nominee markets) with literally zero in-band
  competition**. That is where a small account's share can be large — but a $30/day pool that you
  win 30% of is $9/day, and any other scanner running the same `pool ÷ competition` ranking (a
  trivially obvious heuristic, already open-sourced) arrives at the same market.
- The Jesus-market study is the empirical answer to "does capital chase the pool?": yes, within
  seconds, and concentration *falls* as it does.

*Confidence: single-source for the two scanner observations (one repo's README, one live scan on
2026-06-11). Verified for the Jesus-market dynamics.*
Source: <https://github.com/Birantx/polymarket-lp-bot>

---

## 3. Risks and gotchas

**3.1 Our soak result does not transfer. This is the single biggest analytical trap in the pivot.**
Our 5 fills / 6,094 orders came from resting at *arbitrage-detected* price levels — deep, passive,
far from the touch. Reward scoring requires quoting **within `max_spread` (1.5–5.5 cents) of the
adjusted midpoint**, quadratically penalising distance, so any competitive reward quote sits at or
within a cent or two of the touch. **We will get filled, a lot.** The pivot does not remove fill
risk; it relocates it and pays a subsidy against it. Every "we never get filled" intuition from the
soak must be discarded before sizing.

**3.2 Adverse selection is the whole game, and rewards are roughly its market price.**
Competitive equilibrium says in-band capital flows in until reward yield ≈ expected adverse-selection
cost. If the observed ~0.1–0.5%/day reward yields are real, expect adverse-selection costs of the
same order. The favourable structural fact is the YES-bid + NO-bid pair: post buy-YES at
`mid − s` and buy-NO at `1 − mid − s`; if **both** fill you merge them back to $1 and bank `2s` per
pair (≈2% on a 1-cent quote), a maker-only exit that never crosses the spread. The loss case is
one-sided fills in a market that keeps moving. So our real risk metric is **per-fill markout**, not
fill count — and we can measure it directly with the book-snapshot infrastructure we already have.

**3.3 The $1/day floor is a small-account tax.** If (as I believe) it is per market per day, the
optimal play is *concentration*, not diversification — the opposite of what risk management wants.
$2,000 spread over 10 markets earning $0.40 each pays **$0**. This interacts viciously with 3.2: to
clear $1/day per market you must quote size and/or tightness that raises your adverse-selection
exposure in that market.

**3.4 Uptime is directly monetised.** Per-minute sampling with random within-minute timing means
downtime is a proportional loss of the day's `Q_epoch`. No penalty beyond the missed samples on the
global program (the US program explicitly excludes snapshots), but a bot that restarts, gets
rate-limited, or loses its WebSocket during a fast market both loses score and leaves stale quotes
to be picked off. A dead-man switch / heartbeat is mandatory, not optional.

**3.5 Rule and infrastructure churn is high and recent.** In the last ~6 months: CLOB v2 cutover
(Apr 28, 2026) which **cleared all existing order books** at the maintenance window and swapped the
collateral token to pUSD; Fee Structure V2 (Mar 30, 2026); maker rebates extended to all crypto
(Mar 6); sports taker fee 0.03→0.05 with the **sports maker rebate cut 25%→15%**; crypto
0.072→0.07; taker rebate program launched (May 28); $1M one-off incentive at CLOB v2 launch
($500k of it in the first two hours); another $1M in August 2026 for TWAP-settled crypto markets,
of which **Bitcoin takes $575k and 5-minute markets carry 55% of the pool**. Any model built on
today's parameters has a short half-life, and a *configured* pool can be reconfigured or expire
mid-week (`rewards_config` carries `start_date`/`end_date`).

**3.6 Gaming and sybil.** Columbia researchers estimated ~25% of Polymarket volume may be wash
trading, with capital cycled across many wallets specifically to game future incentives. Polymarket
reserves the right to disqualify wash trading from reward distributions and excludes trades above
98¢ from trading rewards. The reward formula's own defences are the size-cutoff-adjusted midpoint
(anti-dust-pinning) and the quadratic distance penalty. I found **no** published sybil rule for
liquidity rewards specifically, and no documented case of rewards being clawed back from a
liquidity farmer. Our exposure here is mostly reputational/ToS, not strategic — but note that
splitting across wallets to dodge the $1 threshold would be exactly the pattern they police.

**3.7 Security — a live trap in the search results.** The GitHub org
`polymarket-liquidity-rewards-bot/polymarket-liquidity-rewards-bot` markets itself as a
"production-ready" rewards bot; its README instructs you to **download a build from Releases and run
`Polyliquid.exe`**, and its config asks for `PK` (private key). It is a near-verbatim plagiarism of
the legitimate `warproxxx/poly-maker` README with a binary dropper attached. **Do not run it, do not
fetch its releases.** Given our project handles a hot wallet, this is worth a standing rule: no
prebuilt binaries in the wallet path, source only.

*Confidence: 3.1–3.3 are inference from verified mechanics (high confidence in the reasoning).
3.4–3.5 verified from changelog/news. 3.6 verified for the Columbia study and the 98¢ rule,
**absence of evidence** for sybil rules. 3.7 verified by direct inspection of the README.*
Sources: <https://www.coindesk.com/markets/2025/11/07/polymarket-s-trading-volume-may-be-25-fake-columbia-study-finds>,
<https://docs.polymarket.com/changelog>, <https://crypto.news/polymarket-rolls-out-clob-v2-with-1m-liquidity-rewards-to-harden-prediction-markets/>,
<https://tradoxvps.com/polymarket-liquidity-rewards/>,
<https://github.com/polymarket-liquidity-rewards-bot/polymarket-liquidity-rewards-bot>

---

## 4. Tooling

| Project | What it is | What we'd take |
|---|---|---|
| **`warproxxx/poly-maker`** ([repo](https://github.com/warproxxx/poly-maker)) | The credible one. Maker-only MM for **CLOB V2**, political markets, async Python 3.12, typed + mypy strict, 83 tests. Gamma discovery ranked by **reward + rebate income vs. volatility/spread risk**; depth-weighted microprice FV nudged by signed-flow EWMA; reservation price with inventory skew; half-spread `δ = base + c_vol·σ + c_tox·toxicity`; **posts BUY-YES and BUY-NO as two bids that merge to USDC at locked edge `1−p−q`**; regime machine `QUIET` (farm rewards in-band) / `TRENDING` / `EVENT` (pull) / `REDUCE_ONLY` / `HALTED`; exchange heartbeat dead-man switch; per-market + neg-risk-group + total exposure caps; daily-loss kill switch; paper mode; `livetest`/`moneydoctor` self-tests. | **The architecture blueprint.** The regime machine, the YES-bid/NO-bid pair structure, the heartbeat, and the per-fill markout EWMA are all things we'd otherwise invent badly. Its README carries an explicit "this can lose money" warning. |
| **`Birantx/polymarket-lp-bot`** ([repo](https://github.com/Birantx/polymarket-lp-bot)) | Long-tail scanner + quoting engine + paper gate. Ranks by `pool ÷ (in-band competition + 1)`. Documents the **cents-vs-fraction 100× bug**, the **5-share CLOB floor vs `rewards.min_size` (20–200 shares)** distinction, and the **EIP-712 `verifyingContract` must be `CTF_EXCHANGE` for binary / `NEG_RISK_CTF_EXCHANGE` for neg-risk** signing bug. | The `scoring.py` math (portable to Rust in an afternoon) and the three documented bug classes — each of which we would otherwise hit. Reported live scan numbers: unaudited. |
| **`ALLmightyn/MarketMakerBot`** ([repo](https://github.com/ALLmightyn/MarketMakerBot)) | Retired, with post-mortem. `ToxicFlowShield` (pull/widen on informed flow), inventory management, staged paper → $10 smoke → full rollout, `HALT` marker file kill switch. | The negative result (§2.4) and the staged-rollout discipline. Proprietary licence — read, don't copy. |
| **`himyeticapital/polymarket-lp-bot`** ([repo](https://github.com/himyeticapital/polymarket-lp-bot)) | **One-sided** LP + "Didi Flip" state machine: place entry, flip to the opposite side on fill, auto-close on fill to cap loss at spread cost, 30-min fill cooldown, 25% stop-loss, $250 drawdown kill switch, order jitter. | The flip state machine is a genuinely different answer to the one-sided-fill problem than the paired-bid structure. Note one-sided quoting takes the **3× score penalty** and is disqualified entirely outside [0.10, 0.90]. |
| **`Polymarket/clob-client`** (archived) | Official TS client. **Only place with the full `/rewards/*` endpoint surface in official source.** | The endpoint list and response types in §1.6. Archived — migrate targets are `Polymarket/ts-sdk` (no rewards endpoints yet) and `py-clob-client` (`order-scoring` + `sampling-markets` only). |
| **`telonex/research`** ([repo](https://github.com/telonex/research)) | MIT-licensed notebooks over tick data (trades, quotes, `book_snapshot_25`, on-chain fills). | The methodology for measuring depth/impact/replenishment around a reward change — directly reusable as our adverse-selection measurement harness. |
| **opt.markets** ([docs](https://opt-markets.com/docs)) | Closed-source Q-score optimizer: exact Q-score from the public formula, per-market competitor Q-scores, order placement optimiser, on-chain payout verification. Claims **±1% vs actual payouts**. | Evidence the quadratic formula is still live and reconstructable. Not something to depend on. |
| **polyscalping.org / polyield.xyz** | LP dashboards + leaderboards (24h/7d/30d/all-time), live pool = Σ Gamma `rewardsDailyRate`. | Independent cross-check on our realised payouts. Both unreachable from this session. |
| ⚠️ **`polymarket-liquidity-rewards-bot/*`** | **Malware.** Plagiarised README + `.exe` from Releases + asks for `PK`. | Nothing. See §3.7. |

---

## 5. What this is actually worth at $500–$2,000 — with the assumptions on the table

### 5.1 The model

```
daily_reward(market) ≈ pool_per_day × (our_capital / total_in-band_capital) × (our_f / mean_f)
   where f = ((v − s)/v)²,  and paired YES-bid+NO-bid capital ≈ $1 per share quoted
subject to:  daily_reward(market) ≥ $1, else it pays $0
net = Σ daily_reward − adverse_selection_cost + double-fill_edge(2s per merged pair)
```

### 5.2 Stated assumptions

1. `pool ÷ in-band notional` (the reward yield on capital) is **0.05%–0.5%/day** for markets with
   real competition. *Anchored on one scanner observation ($300/day pool under $57k in-band =
   0.53%/day) and one illustrative guide figure (0.05% share of a $5,000 pool). Weak.*
2. Advertised `rewards_daily_rate` is a **cap**, and realised distribution is lower (§2.2). I apply
   a 0.5–1.0× realisation haircut.
3. Two-sided paired-bid quoting, so capital ≈ $1/share and `Q_min` is not penalised by the 3× factor.
4. `min_size` of 100–200 shares in the pools worth quoting ⇒ **~$100–$200 of capital per market**
   just to qualify. $500 ⇒ 2–4 markets; $2,000 ⇒ 10–20 markets, *but see the $1 floor*.
5. The **$1/day floor is per market**. If it turns out to be per wallet per day, every number below
   improves materially and the diversification penalty disappears.
6. No gas cost per order — Polymarket CLOB orders are off-chain signed, settlement gas is not paid by
   the maker. (One community simulator models "$5.76/day gas"; I believe that is simply wrong.)
7. Uptime ≥ 95%, quoting `s ≈ v/2` (f ≈ 0.25) to `s ≈ v/3` (f ≈ 0.44).

### 5.3 The numbers

**$500 of quoting capital**

| | gross rewards/day | after adverse selection |
|---|---|---|
| Pessimistic | $0.00 — spread across 2–3 markets, every one lands under the $1 floor | **−$1 to $0** |
| Central | $0.30–$1.00 in 1–2 concentrated markets; **coin-flip whether it clears $1 at all** | **−$0.50 to +$0.50** |
| Optimistic | $2–$5 if we find a genuinely uncontested long-tail pool and hold it | **$0 to +$3** |

**Honest read: at $500 this is a rounding error at best and the $1/day floor is a live risk of
earning literally zero.** $500 buys information, not income.

**$2,000 of quoting capital**

| | gross rewards/day | after adverse selection |
|---|---|---|
| Pessimistic | $0.50 — capital fragmented, floors eat most markets, or we get run over in one event | **−$5 to −$1** |
| Central | $2–$6 (0.1–0.3%/day) concentrated in 2–4 markets | **−$1 to +$3** |
| Optimistic | $8–$15 (0.4–0.75%/day) in low-competition pools, disciplined quote-pulling | **+$3 to +$8** |

**Central case: roughly $60–$180/month gross, minus an adverse-selection bill of the same order of
magnitude. Expected net somewhere between −$30 and +$90/month, and I would not be surprised by
either tail.** Annualised on $2,000 that is a central estimate of maybe **0–30% APR net**, with a
real probability of negative, against a gross-reward APR that *looks* like 35–110%.

### 5.4 What would move these numbers

Upward: the $1 floor turning out to be per-wallet; finding pools where `pool ÷ in-band notional`
genuinely exceeds 0.5%/day and stays there; measured markout showing our adverse-selection cost is
well under the reward yield in long-dated calm markets; maker rebates being non-trivial once we quote
at touch.

Downward: realisation haircut worse than 0.5×; competitors' `f` higher than ours (they quote tighter,
we get a smaller share than our capital ratio implies); a single tournament-night-style event
(the −$2,095 paper drawdown pattern); a program reconfiguration mid-run.

### 5.5 Recommendation

**Do not deploy $2,000 on this research. Deploy $200–$400 for two weeks to measure four numbers we
cannot get from the internet:**

1. `GET /order-scoring` on every resting order → what fraction of wall-clock our quotes actually
   score (this alone validates or kills the whole thesis, and needs no capital at risk beyond the
   quotes themselves).
2. `GET /rewards/user?date=` realised payout vs. our own computed Q share vs. the advertised pool →
   resolves the "caps not payouts" haircut (§2.2) and the per-market-vs-per-wallet $1 floor (§1.4),
   the two largest unknowns in this report.
3. Per-fill markout at +30s/+120s using our existing book-snapshot infrastructure → the
   adverse-selection number, which is the actual determinant of net P&L and which **nobody publishes**.
4. `pool ÷ in-band notional` measured across the live reward universe (`/rewards/markets/current`
   + `/books`) → replaces assumption 1 with data.

That pilot costs a few hundred dollars of exposure and ~a week of build on top of infrastructure we
already have (Gamma discovery, batched book fetch, SQLite Decimal store, dry-run daemon). It
converts every "single-source" and "inference" label in this document into a measurement. Sizing to
$2,000 before those four numbers exist would be committing capital on the strength of SEO content
farms and three unaudited 0-star repositories — one of which is a private-key stealer and another of
which is a published failure at exactly our capital size.

*Confidence: the model in 5.1 is inference from verified mechanics (sound). The **input assumptions
in 5.2 are the weak link** — assumption 1 in particular rests on essentially two data points. Treat
every dollar figure in 5.3 as an order-of-magnitude bracket, not an estimate.*

---

## Appendix: source ledger by reliability

**Directly fetched and read by me (highest confidence):**
`Polymarket/clob-client` `endpoints.ts` + `types.ts`; `Polymarket/py-clob-client` `endpoints.py` +
`client.py`; `Polymarket/ts-sdk` READMEs + `bindings/src/shared.ts`; `telonex/research` Jesus-market
notebook (including its computed outputs); `Birantx/polymarket-lp-bot` `scoring.py`, `scanner.py`,
`quoting.py`, `cli.py`, README; `warproxxx/poly-maker` README; `ALLmightyn/MarketMakerBot` README;
`himyeticapital/polymarket-lp-bot` README; `polymarket-liquidity-rewards-bot` README (malware);
`kuestcom/prediction-market` mirrored CLOB OpenAPI; `pr6thv3/polymarket-bot` reward-simulator output;
real captured market JSON in `nautechsystems/nautilus_trader`, `Ashwin-Iyer1/PolymarketArbitrage`,
`supergithubo/polyscout`, `agentcourt/adjudication`, `DenisGorbachev/polymarket-client-sdk-ext`.

**Search-summary only, official pages (medium confidence — paraphrase risk):**
docs.polymarket.com/programs/{liquidity-rewards, maker-rebates, taker-rebates}, /changelog,
/api-reference/trading-rate-limits; help.polymarket.com articles 13364466 (liquidity rewards),
13364471 (maker rebates), 13364459 (holding rewards), 13755867 (sponsor market rewards);
docs.polymarket.us/incentives/liquidity; polymarket.us/rewards.

**Search-summary only, third-party (low–medium confidence):**
telonex.io/research article; kucoin/odaily/bitget/phemex news flashes; coindesk (Columbia wash-trade
study); crypto.news + cryptotimes (CLOB v2); pineanalytics.substack.com (fee rollout).

**Search-summary only, SEO/affiliate (low confidence — cited for structure, not for numbers):**
startpolymarket.com, tradoxvps.com, laikalabs.ai, polymarkets.co.il, alphascope.app,
bravadotrade.com, polyscalping.org, polyield.xyz, opt-markets.com, dropstab.com, vpn07.com.

**Practitioner write-ups I could not open (cited via summary, reasoning weighted over numbers):**
medium.com/@wanguolin two-week postmortem; medium.com/mountain-movers rewards calculator + hidden
yield layer.
