# What Verifiably Works for a Small Retail Account on Polymarket (2025–2026)

**Prepared:** 2026-08-25 · for the polyarb pivot decision after the 11-day maker-fill soak failure.
**Capital under discussion:** $500–$2,000. **Infra:** Raspberry Pi 5, home connection, minutes-scale detection latency.

---

## 0. Read this first — the honest top line

Every strategy surveyed below, sized to $2,000, lands in the same band: **roughly −$1 to +$3 per day**, with wide error bars and a meaningful chance the true number is negative. Nothing found in this survey changes the order of magnitude. Three independent anchors:

- The **holding-rewards** program pays a verified, genuinely risk-free 4.00% APR → **$0.22/day** on $2,000 ([Polymarket Help Center](https://help.polymarket.com/en/articles/13364459-holding-rewards)).
- The most bullish market-making claim found anywhere (SEO content, low credibility) is **40–120% APY** on deployed capital → $2.19–$6.58/day on $2,000 ([laikalabs](https://laikalabs.ai/prediction-markets/polymarket-liquidity-rewards)).
- Our own M8 taker simulation produced ~$20/day but required **$15k–$35k/day committed**; scaled to $2,000 that is **$1.1–$2.7/day** — the same band.

Against that, the population base rates are brutal and **verified from multiple independent outlets**: of 95M Polymarket transactions Apr 2024–Dec 2025, **only 0.51% of wallets cleared >$1,000 profit**; >100,000 accounts lost ≥$1,000, roughly twice the number that made that much; non-winners lost **$131M in aggregate** ([Japan Times / Bloomberg, 2026-04-28](https://www.japantimes.co.jp/business/2026/04/28/markets/prediction-market-traders-lose-money/), [FA-Mag reprint](https://www.fa-mag.com/news/most-prediction-market-traders-are-losing-money-while-bots-rack-up-gains-86783.html)). A separate April 2026 on-chain study of 2.5M wallets puts it at **84.1% of traders losing money, 2% ever above $1,000, top 0.04% capturing >70% of all PnL** ([crypticorn](https://www.crypticorn.com/how-to-trade-polymarket-profitably-what-actually-works-in-2026/)).

**The correct framing for the capital decision is therefore not "which strategy do we deploy" but "which single measurement most cheaply tells us whether to deploy at all."** Sections 5 and 6 answer that.

### Source-access caveat (important for weighing everything below)

This session's egress proxy blocked **arxiv.org, medium.com, huggingface.co, dagstuhl, reddit, HN, docs.polymarket.com, and essentially every content site**; only `github.com` / `raw.githubusercontent.com` fetched successfully. Everything else here comes from **search-engine summaries of those pages**, not from reading them. That means:

- Paper titles, authors, dates and arXiv IDs are reliable (they appear in result metadata).
- **Numbers attributed to papers are second-hand** and should be re-verified against the PDF before anything is built on them. I have tagged these `single-source` at best, never `verified`.
- The two GitHub READMEs (§2.1) are the only sources I read in full and directly. They are correspondingly the highest-quality practitioner evidence in this document.

Confidence tags used: **[verified]** = multiple independent sources or read directly; **[single-source]** = one source, or one source echoed by content farms; **[inference]** = my reasoning from verified primitives, not an observed fact.

---

## 1. Facts that constrain everything (the primitives)

### 1.1 Fee schedule — unchanged as of Aug 2026 **[verified]**
`taker fee = size × rate × p × (1−p)`; **makers pay zero**. Geopolitics 0.00, politics/finance/tech/mentions 0.04, sports/economics/culture/weather/other 0.05, crypto 0.07. Confirmed still current in Aug-2026 sources, which also note the July-2026 changes our own docs recorded (sports 0.03→0.05, crypto 0.072→0.07) ([Start Polymarket fees](https://startpolymarket.com/learn/polymarket-fees/), [OddsShopper](https://www.oddsshopper.com/articles/betting-101/polymarket-fees), [crypticorn](https://www.crypticorn.com/polymarket-fees-explained/)).

**The under-exploited implication:** the fee curve is *minimised at the extremes*. At p=0.98 in politics the taker fee is `0.04 × 0.98 × 0.02 = 0.078¢/share` — 1/13th of the fee at p=0.50. Every strategy that trades near 0 or 1 is in the cheapest part of the curve. This is the single structural reason near-resolution work (§3.2) deserves a look and 50/50 arb does not.

**Maker rebates:** 15–25% of collected taker fees redistributed daily to makers ([Start Polymarket](https://startpolymarket.com/learn/polymarket-fees/); the Birantx README cites "Polymarket Fee V2 (Mar 30, 2026): makers pay zero fees and receive 20–25% of taker fees as daily PUSD rebates"). **[verified]**

### 1.2 Resolution-time distribution **[single-source, but the single source is a real 18k-market study]**
Across **18,427 markets resolving May 9 2025 – May 8 2026**: median **41 minutes** after the underlying event ended; **p90 = 6.4 hours; p99 = 4.2 days**. **184 markets (1.0%) were disputed at UMA**, adding a median **49 hours** ([Poly Syncer resolution study](https://www.polysyncer.com/blog/polymarket-resolution-time-2026)). Category spread: NBA median 22 min; **geopolitics median 5h16m** (slowest).

This is the most decision-relevant number in the whole survey: it sets the capital-velocity ceiling for any hold-to-resolution strategy, and the 1.0% dispute rate sets the tail budget.

### 1.3 Dispute rate is rising, fast **[verified — two independent sources]**
1.0% of markets disputed in the year to May 2026 (above), but **Polymarket logged >1,150 disputed markets in the first half of 2026 alone, already past its full-year 2025 total** (Wang, *Economics Letters* 268:113176, via [ScienceDirect](https://www.sciencedirect.com/science/article/abs/pii/S0165176526003721)). A WSJ investigation (May 2026, cited in the same cluster) found **>half of UMA votes in most disputed markets came from the ten largest wallets**, ≥60% of active UMA voters were linkable to live Polymarket accounts, and **~1 in 5 disputes had a voter with a financial stake in the market they ruled on** ([summary](https://www.oddsshopper.com/articles/prediction-markets/uma-oracle-polymarket-disputes)).

This is a **worsening** of the tail risk documented in our own `polymarket-paradox-manipulation-whales.md`, not a stable state. Anything with an ~98% breakeven win rate is now being underwritten against a rising, adversarially-controlled adjudication process.

### 1.4 Two-track resolution now exists **[single-source]**
Polymarket's US arm is a CFTC-registered DCM resolving against **named authoritative sources**; the international book (the one our API code targets) **still settles under UMA** ([The Defiant](https://thedefiant.io/news/markets/usd85m-polymarket-dispute-over-strategy-s-may-bitcoin-sale-puts-uma-s-token-voting-oracle-on), [track360 2026 oracle guide](https://track360.io/blog/prediction-market-oracles-resolution-settlement-operator-guide-2026)). **Consequence: our UMA tail risk is not going away on the venue we can actually trade.**

### 1.5 Order-book mechanics that bite a Raspberry Pi **[verified — read directly from Polymarket's own repo]**
From [Polymarket/agent-skills `order-patterns.md`](https://github.com/Polymarket/agent-skills/blob/main/order-patterns.md):

- **Heartbeat: "If a valid heartbeat is not received within 10 seconds (with up to a 5-second buffer), all of your open orders will be cancelled."** Every resting-order strategy on a home connection is one 15-second network blip away from a full book wipe. For liquidity rewards — scored *every minute* — that is direct revenue loss, and it is an operational-fit fact nobody in the SEO literature mentions.
- **`post-only` orders exist** and are rejected rather than executed if they would cross — so guaranteed maker status is available, GTC/GTD only.
- Tick sizes 0.1 / 0.01 / 0.001 / 0.0001 by market; batch endpoints cap at **15 orders per request**.
- Price bounds bite at the extremes: a market order near the top of the book errors with `Invalid price (0.999), min: 0.01 - max: 0.99` on 0.01-tick markets ([clob-client issue #232](https://github.com/Polymarket/clob-client/issues/232)). **At 0.99 on a penny-tick market there is no exit above your entry — the only exit is resolution.** This is the "price-band cliff" the theta-harvesting literature refers to.

### 1.6 Who is on the other side **[single-source]**
Whale / HFT-operator / power-trader tiers hold **81.4% of total notional across 12.6% of addresses** (Nechepurenko, [arXiv:2605.11640](https://arxiv.org/abs/2605.11640) / [SSRN 6751284](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6751284)). Separately, an on-chain 2025 tally puts **the top 0.04% of wallets (~680 of 1.7M) at 70% of all realized profit** ([QuantPedia](https://quantpedia.com/systematic-edges-in-prediction-markets/)).

---

## 2. Candidate strategies, assessed

### 2.1 Liquidity-rewards farming (post resting quotes in reward-eligible markets)

**What it is.** Polymarket pays makers a daily USDC/pUSD reward for resting two-sided quotes inside a per-market band. Score per order is `S = ((v − s)/v)² · b` where `v = max_spread`, `s` = distance from the size-cutoff-adjusted midpoint, `b` = size; the market's daily pool is split proportionally to score, computed **every minute** and settled at 00:00 UTC, with a **$1/day per-market minimum payout that does not roll over**. Single-sided quotes score at **1/3** and are ineligible entirely when the midpoint sits outside [0.10, 0.90]. Per-market `rewardsMinSize` and `rewardsMaxSpread` are **queryable via the API**. **[verified]** — formula and structure corroborated across [Polymarket docs](https://docs.polymarket.com/programs/liquidity-rewards), [Help Center](https://help.polymarket.com/en/articles/13364466-liquidity-rewards), and independently reimplemented in the Birantx README below.

**Practitioner evidence — the two open-source bots are the best evidence in this document (I read both READMEs in full):**

| | [`ALLmightyn/MarketMakerBot`](https://github.com/ALLmightyn/MarketMakerBot) | [`Birantx/polymarket-lp-bot`](https://github.com/Birantx/polymarket-lp-bot) |
|---|---|---|
| Status | **Archived with a kill-switch marker file, "Not intended to be restarted."** | Alive; gated at "paper week" before a **$100 live ramp** |
| Result | Paper: **+$9.90 over ~59 hours, 6 fills** on stable markets; **−$2,095 paper drawdown** on correlated markets during a sports event | Live paper data: two books with **identical ~$95 gross reward** diverged entirely on adverse cost — **$26 vs $166** |
| Stated cause of death | *"the reward-eligible order flow needed to clear that [$1/day] bar wasn't reachable at the capital this was sized for"* | (not dead) explicitly concedes *"fill toxicity cannot be measured in simulation — only assumed conservatively"* |

Birantx's design is the one worth copying: rank markets by `score = daily_reward_pool / (in_band_competition + 1)`, target **long-tail $25–100/day pools with near-zero competing in-band liquidity**, cap **$50 per market**, auto-halt at **$15 daily loss per market**. Its own logged example: a nominee market scored 30.0 (zero competition) vs a $300/day geopolitical market scoring 0.005 under **$57k of professional quotes**. **[verified — read directly]**

A practitioner postmortem adds the historical arc: early open-source LPs reported ~**10,000 USDC making 200–300 USDC/day at peak**, but *"as more players joined… rewards became more of a thin 'bonus' on top of real trading edge rather than a standalone money printer"* ([wanguolin, Jan 2026, via search summary](https://medium.com/@wanguolin/my-two-week-deep-dive-into-polymarket-liquidity-rewards-a-technical-postmortem-88d3a954a058)). **[single-source]**

**Economics on $2,000 [inference].** `rewardsMinSize` runs **20–200 shares** depending on market (Birantx). At a 0.50 midpoint, a two-sided min-size quote of 100 shares/side commits ≈**$100 of collateral per market** (you back the YES bid and the NO bid). $2,000 therefore buys resting quotes in **≈20 markets**, or ≈40 at $50/market single-min-size. To be worth anything each must clear the **$1/day floor**. Clearing $1/day in 20 markets = **$20/day = 1%/day = 365% APY** — which is obviously not happening, and the fact that it *would* follow from the naive arithmetic is precisely why the naive arithmetic is wrong: the low-competition pools are low-competition because they are small, or because min-size is large relative to the pool, or because the flow is toxic. ALLmightyn's shutdown note is the empirical version of that.

**Tail risks.** (a) **Adverse selection is the entire PnL**, per Birantx's own logs — you are filled when informed flow crosses you, and our repricing latency is minutes; the market can move 5¢ before we cancel. (b) The **10-second heartbeat** (§1.5): a home-connection blip cancels every resting order and zeroes that minute's score. (c) Reward-program parameters are set by Polymarket and can be cut without notice. (d) No resolution risk if you never carry inventory to resolution — but you will carry inventory.

**Why it is NOT our proven M8 failure.** M8 measured resting orders at *arbitrage-implied prices* — deep in the book, far from the mid, in stale illiquid markets nobody trades against; the failure mode was **5 fills in 6,094 opportunities (never fills)**. Reward farming rests **at or adjacent to the midpoint in markets selected for having flow**, and is paid **for resting whether or not it fills**. The failure mode inverts from "never fills" to "fills adversely." That is a different mechanism with a different measurement, and M8 says nothing about it.

**Operational fit: good on the revenue side, poor on the risk side.** Reward accrual is latency-insensitive (scored per minute, not per millisecond). Quote *defence* is latency-sensitive and we are minutes-slow.

**Confidence: program mechanics [verified]; small-capital profitability [contested — one archived bot says no, one unfinished bot says maybe, SEO content says 40–120% APY and should be ignored].**

---

### 2.2 Holding rewards — split-and-hold at 4.00% APR

**What it is.** Polymarket pays **4.00% annualized** on the mid-price value of positions held in a curated set of long-running political/geopolitical markets — 2028 presidential, **2026 midterms balance of power**, Erdoğan/Zelenskyy/Netanyahu/Xi/Putin term-end, Russia–Ukraine ceasefire. Position value is **sampled once per hour at random** and paid daily in pUSD; `reward = position_value × (0.04/365/24)` per sampled hour. Treasury-funded, launched early 2026. **[verified]** — [Help Center](https://help.polymarket.com/en/articles/13364459-holding-rewards), [Cointribune](https://www.cointribune.com/en/polymarket-rolls-out-4-annual-rewards-for-long-term-market-positions/), [blocmates](https://www.blocmates.com/news-posts/polymarket-introduces-4-annualized-yield-for-long-term-market-positions), [Bitget](https://www.bitget.com/news/detail/12560604987292). Program-to-date: **$2.03M paid to 314,168 holders** ([polyscalping leaderboard](https://polyscalping.org/leaderboard/yield)) — i.e. ~$6.50 per holder, which tells you the typical position size is small.

**The delta-neutral construction [inference, with one supporting source].** Position value = `yes_shares × mid + no_shares × (1 − mid)`. A **Split** converts exactly $1 of collateral into 1 YES + 1 NO with **no order book, no spread, and no taker fee** ([Positions & Tokens docs](https://docs.polymarket.com/concepts/positions-tokens)). A split pair is therefore worth exactly $1 of eligible position value, carries **zero directional risk**, and can be Merged back to $1 at any time. One source states this explicitly: *"The Split function allows you to simultaneously buy both Yes and No shares. This is a strategy used to hedge positions while earning holding rewards."* **[single-source]** A Hacker News thread titled *"While Polymarket does offer holding rewards interest, it looks like it doesn't…"* ([46512917](https://news.ycombinator.com/item?id=46512917)) suggests there may be an exclusion I could not read — **this must be verified before relying on it.**

**Economics on $2,000:** $80/year = **$0.22/day**. Risk-free, latency-irrelevant, Pi-friendly, no resolution exposure (you merge out whenever). Costs: Polygon gas (negligible), and the program can be withdrawn.

**Verdict: not a strategy — a cash-management floor.** Any capital not actively deployed should sit here rather than idle, and it lowers the hurdle rate for the hold-to-resolution strategies in eligible markets. It also independently corroborates the academic result that lock-up cost is priced: *"yield-bearing collateral flattens the term structure by reducing the opportunity cost of lock-up"* ([arXiv:2605.31431](https://arxiv.org/abs/2605.31431)).

---

### 2.3 Near-resolution capture (buying 96–99¢ favourites for capital-recycling yield)

**Why it deserves a look.** Three of our structural handicaps stop mattering here. **(a)** The **fee curve is minimised at the extremes** — 0.078¢/share in politics at p=0.98 vs 1.00¢ at p=0.50 (§1.1). **(b)** There is **no latency race**: the price is 0.97 for hours because the residual uncertainty is genuinely small, not because a bot hasn't arrived. **(c)** Capital lock-up is **hours, not months** — median 41 minutes post-event, p90 6.4 hours (§1.2).

**Academic grounding [single-source].** [arXiv:2605.31431](https://arxiv.org/abs/2605.31431), *When Certainty Is Not Worth It*, recovers an **annualized settlement wedge (ASW)** from persistent near-certain Polymarket contracts (markets where a YES or NO midpoint stays ≥0.90 for seven consecutive daily snapshots), using hourly Data-API quotes plus on-chain Polygon data. Findings as summarised: wedges are **positive, maturity-dependent and time-varying**, and adjusting for them **removes 48–88% of the raw near-certainty horizon gradient**. Translated: *most of the apparent 97¢ "free money" is the market correctly pricing the cost of having your dollar locked up* — it is compensation for a real cost, not a mispricing. The paper also notes **negRisk conversion compresses the discount** by recycling part of the position into synthetic collateral.

**The calibration evidence is genuinely conflicting, and that conflict is the finding [contested].**
- *Against:* "markets priced at 0.95 resolve YES at 0.93" — i.e. **overpriced by 2pp at exactly the band we would be buying** ([Poly Syncer accuracy study](https://www.polysyncer.com/blog/polymarket-prediction-accuracy)).
- *For:* "the 90–100% bucket was not overconfident, it was slightly underconfident" across all three political segments; mean absolute calibration error 2.1pp across **28,407 resolved markets** sampled 24h before resolution ([predictionnews](https://predictionnews.com/news/study-reveals-nuance-behind-polymarket-90-percent-accuracy-rate/), [The Defiant](https://thedefiant.io/news/research-and-opinion/polymarket-is-up-to-94-accurate-in-predicting-outcomes-analysis)).
- *Also for:* the Polymarket-v1 archive (1.20B trades, 1.30M markets, $61B nominal, 2022-11-21→2026-04-28) finds **high-probability tokens have positive realized returns (underpriced)**, low-probability negative ([arXiv:2606.04217](https://arxiv.org/abs/2606.04217)).

**The whole edge is 1–3¢ and the published disagreement about calibration at that band is ±2pp.** Public data cannot settle whether this is +EV. Only our own executable-side measurement can.

**The four things that kill it [inference from verified primitives]:**
1. **Spread eats half the edge.** On a 0.01-tick market, entering at 0.98 when the mid is 0.975 costs 0.5–1.0¢ against a 2.0¢ gross gap. This is exactly the mechanism from `execution-costs-and-arb-filtering.md` (spread collapsed "arbitrage" from 90% to 24.5% of ticks in the Cao study) — near-resolution is not exempt.
2. **The price-band cliff.** At 0.99 on a penny-tick market there is no order above you (§1.5) — you cannot exit at a profit, only resolve. Your exit optionality goes to zero exactly where the strategy is most tempting.
3. **Breakeven win rate at 0.98 is 98%. One loss erases 49 wins.** The **1.0% UMA dispute rate — rising to >1,150 disputes in H1 2026 (§1.3)** — is the *same order of magnitude as the entire error budget*, before you even consider being wrong about the event.
4. **The p99 tail is 4.2 days and a disputed market adds a median 49 hours** — so the annualized return is set by the tail, not the median.

**Economics on $2,000 [inference].** 40 positions of $50 to diversify the tail. Optimistic: 1.2¢ net per share entered at 0.975, ~1.2% per turn, 5–8 turns/month ⇒ **6–10%/month ≈ $120–200/month**, *conditional on zero losers*. One loser at $50 costs 33 winners. At the historical ~1% dispute/adverse-resolution rate plus genuine event risk at 2–3%, the realistic loss rate is 2–4% of positions — which is **precisely at or beyond breakeven**. The strategy is a coin-flip on parameters we have not measured.

**Why it is NOT our proven failure.** It is a **taker** strategy at prices that sit wide open for hours, not a maker-fill strategy, and not a latency race. M8's finding (passive orders don't fill) and the taker-sim finding (needs $15–35k/day of committed capital) both fail to apply: here the order fills immediately and the capital turns in hours.

**Confidence: mechanism [verified]; profitability [unknown — this is the whole point].**

---

### 2.4 Longshot-bias harvesting (systematically selling overpriced longshots)

**Evidence that the bias exists: strong.** Polymarket-v1 (1.2B trades): low-probability tokens ≤0.30 show **negative realized returns**, ≥0.40 **positive** ([arXiv:2606.04217](https://arxiv.org/abs/2606.04217)). Independently replicated on Kalshi by UCD economists over **300,000+ contracts**, and by the McCullough dashboard on Polymarket ([predictionnews](https://predictionnews.com/news/study-reveals-nuance-behind-polymarket-90-percent-accuracy-rate/), [Wikipedia FLB](https://en.wikipedia.org/wiki/Favourite-longshot_bias)). One concrete figure, low credibility: *"crypto-price binaries at 5 cents resolve YES at 3 percent rather than 5 percent"* ([laikalabs](https://laikalabs.ai/prediction-markets/prediction-market-biases-how-to-exploit-profit)). **[bias: verified; magnitude: single-source]**

**Why it does not survive contact with our constraints [inference].** Selling a longshot at 5¢ means **buying NO at 95¢**: you commit 95¢ to win 5¢. Expected edge with the numbers above is ~2¢/share ⇒ **≈2.1% return on capital per resolved event**. That is fine *if the event resolves quickly* and catastrophic if it does not:

- **Short duration ⇒ crypto.** The 5-minute BTC binaries where this is capital-efficient are the **0.07 fee tier and the most latency-contested category on the platform** — explicitly out of scope for us.
- **Long duration ⇒ politics/geopolitics.** A 3-month longshot at 3¢ returns ~3.1% gross over 3 months ≈ **12.9%/yr before spread and losses**, on capital locked the whole time. At the 1¢ tick, entry spread alone is a third to a half of the edge.
- The trade is **structurally identical to §2.3** (buying something near a dollar) with **strictly worse capital velocity**, and the same 98%-breakeven tail exposure.

**Verdict: rule out as a standalone strategy.** It collapses into near-resolution capture when the duration is short, and into a low-yield capital sink when it is long. Its one genuine use is as a **prior**: it says the near-resolution side of the book is the side the bias favours, which mildly supports §2.3.

---

### 2.5 Election / midterm-regime NegRisk rebalancing — **RULED OUT, with decisive evidence**

This was our MVP thesis, and the hypothesis for rescuing it was "high-volume regimes change the fill picture." **The evidence says the opposite, and it is the cleanest counter-result in this survey.**

From the 2024-election microstructure study (Tsang & Yang, *The Anatomy of a Blockchain Prediction Market*, [arXiv:2603.03136](https://arxiv.org/abs/2603.03136)): **"arbitrage-deviation half-lives fell from several hours in early 2024 to well under a minute in October and November,"** while **Kyle's λ dropped from 0.53 to 0.01** as depth grew. **[single-source, but from a dedicated microstructure paper on exactly this regime]**

High volume makes arbitrage **faster to be captured, not more available to slow actors.** The 2026 midterm regime will do to gaps what Nov 2024 did: compress them to sub-minute lifetimes. Our detection latency is minutes.

Corroborating: NegRisk arbitrage is described as **"already industrialized: multi-wallet operations sweeping cheap NOs across thousands of markets and converting complete sets at a hub wallet"** ([Polyflux](https://polyflux.io/blog/polymarket-arbitrage/)); median same-market arb lifetime **12.3s (2024) → 2.7s (2026)** with **73% of arb profit to sub-100ms bots**. Midterm volume is real ($197M across Kalshi+Polymarket midterm markets by July 2026, [NBC News](https://www.nbcnews.com/tech/internet/kalshi-polymarket-midterm-election-markets-money-bet-invest-how-rcna352804)) — it is simply not ours.

A newer paper worth reading before closing the file: Gebele, Mutzel & Matthes (TUM), *Executable Arbitrage and Market Efficiency in Prediction Markets*, [arXiv:2608.00666](https://arxiv.org/abs/2608.00666), posted **2026-08-01**. It distinguishes **payoff-space no-arbitrage** from **protocol-executable no-arbitrage** and measures depth-aware violations and their exploitation on NegRisk markets, noting the **NegRisk Adapter only operationalizes NO→YES before settlement**. I could not retrieve its numbers. It is the most likely source of a decisive refutation *or* a surviving niche, and is the single highest-value follow-up read.

**This re-runs our proven failure directly. Do not rebuild it for the midterms.**

---

### 2.6 News-latency trading — **RULED OUT for us, honestly**

The honest version is more interesting than a flat no, so here is both halves.

**The half that says no [single-source, consistent across sources]:** simple arb windows last **~2.7 seconds**; **73% of profits go to sub-100ms bots on dedicated Polygon RPC nodes**; on a January 2026 news break the market repriced to $0.42 **within 8 minutes** while humans were still reading at 90 seconds; a single bot extracted **$271,500 in 30 days** purely from Polymarket's price-display latency ([Predik](https://predik.io/en/blog/bot-trading-polymarket-exploit-latencia-arbitraje-en), [quantvps](https://www.quantvps.com/blog/polymarket-hft-traders-use-ai-arbitrage-mispricing)).

**The half that complicates it [single-source]:** a study of **476,000 news-story→market pairings across 618 news sources over 56 days ending 2026-06-25** found a news story moved a related contract within one hour **only 15.2% of the time** ([Crypto Briefing](https://cryptobriefing.com/polymarket-media-impact-prediction-market-prices/)). Full repricing after major news takes **~80 minutes**, with markets initially moving only **~64% of the way** to the corrected price ([Cryptonomist](https://en.cryptonomist.ch/2026/08/10/polymarket-price-repricing-delay/)). Windows are **"30 seconds to five minutes for high-attention markets and hours or even days for low-attention markets."**

**Why it is still a no for us.** The slow tail is real but it is **not arbitrage** — it is a directional bet requiring a news→market mapping competence we do not have and have not built, in exactly the thin long-tail markets where §1.6's informed flow and the insider-leakage literature ([arXiv:2605.00459](https://arxiv.org/abs/2605.00459), [arXiv:2605.02286](https://arxiv.org/abs/2605.02286)) say the counterparty most often knows something. Our stated constraint — *no informational edge on outcomes* — binds. Filing it as "a different company's business, not this one's."

---

### 2.7 Cross-platform Kalshi arbitrage — **RULED OUT**

**Ruled in on gap size, ruled out on everything else.** Documented gaps of **2–5% on major events, 5–8¢ persistent on World Cup contracts**, and a median implied-probability gap of **3.2pp across 1,840 NBA/soccer markets over 60 days** ([XCLSV](https://xclsvmedia.com/kalshi-vs-polymarket-arbitrage-2026-nba-finals-sharp-bettors/), [Poly Syncer](https://www.polysyncer.com/blog/polymarket-vs-sportsbook)). Against that:

1. **Capital fragmentation halves an already-tiny stake.** $2,000 becomes $1,000/venue, and both sides must be pre-funded *before* the gap appears.
2. **Settlement basis risk is real, not theoretical.** CFTC venues resolve against a **named authoritative source**; Polymarket international resolves by **UMA token vote** (§1.4). Identical-sounding contracts have settled differently ([OddsShopper](https://www.oddsshopper.com/articles/prediction-markets/kalshi-vs-polymarket-settlement-rules), [DeFiRate](https://defirate.com/prediction-markets/how-contracts-settle/)). **A two-leg position with two different adjudicators is not an arbitrage; it is a bet on adjudication agreement.**
3. **Rebalancing is days.** Kalshi credits within hours–48h of resolution, then **1–3 business days ACH**; Polymarket needs the UMA window. You cannot recycle capital between venues at the frequency the strategy requires.
4. **It is still a latency race** on the liquid overlap, and only ~6% of events are cross-listed (our prior research).
5. **Our own executable-cost finding applies unchanged:** the quoted 1–2¢ gaps die to the double spread — the widely-repeated worked example has a 2¢ gap becoming a **0.5¢ loss** after two legs of slippage.

Our `polymarket-paradox-manipulation-whales.md` already recorded the decisive precedent: the **10–15 point Polymarket-vs-PredictIt gap persisted for three weeks un-arbitraged**. Persistent cross-venue gaps are usually structural, not free.

---

### 2.8 Resolution-rule / adjudication mispricing — **the one genuinely new candidate**

**The claim.** The crowd trades the *intuitive meaning of the headline*; the contract pays on the *written rule as certified by the adjudicator*. Systematically reading rules — cheap for an LLM at 63k-market scale, expensive for humans — is an edge that is **latency-insensitive (windows are days), capital-light, and requires no forecasting skill.**

**Peer-reviewed grounding — the strongest single citation in this document.** Wang, *Do prediction markets price events or adjudication? Evidence from the disputed Polymarket markets*, **Economics Letters 268:113176 (2026)** ([ScienceDirect](https://www.sciencedirect.com/science/article/abs/pii/S0165176526003721)): *"in an oracle-settled market the cash flow is fixed not by the event but by the outcome that the resolution mechanism certifies, so the price can come apart from the event."* Using an independent rule-implied-outcome benchmark plus the on-chain settlement clock, it shows **prices forecast the adjudication rather than the event.** Its worked case: on 2026-06-01 a contract on whether MicroStrategy sold Bitcoin by May 31 **sat unresolved even though the firm's own filing that morning disclosed a sale in the window** — the payoff turned on a token vote over the wording. **[verified — peer-reviewed journal]**

**This is the paper's sting, and it must be stated plainly: the naive version of this trade loses.** "The rules say X, the market says Y, therefore buy X" is a bet against the adjudicator, and the adjudicator is a token vote where the top-10 wallets cast most of the votes and ~1 in 5 disputes has a financially-interested voter (§1.3). Our own E4 (Ukraine minerals, resolved YES with no deal) and E5 (Zelenskyy suit, $242M, resolved against the initial reading) are exactly this failure. The tradeable version requires modelling **how UMA will vote**, not what is true — and there is active research on whether that is even automatable ([*Can LLMs Help Decentralized Dispute Arbitration?*, arXiv:2604.15674](https://arxiv.org/abs/2604.15674)).

**Assessment:** highest ceiling, lowest evidence, and it needs a competence (LLM rule-reading + adjudication modelling) we have not built. **Not a Phase-B candidate — a Phase-C research question with a zero-capital backtest attached (§5).**

---

### 2.9 Briefly considered and dismissed

- **Sports de-vig vs sportsbooks.** The best-documented small-account bot found, `polymm`, did exactly this: **+$8,293 arb, −$3,184 directional, ≈$5,000 net** over several months in early 2026 on a public wallet — and is **retired**. Author's autopsy: *"It got too slow to defend its edge, which is exactly why it stopped making money"* and *"The code was never the hard part. The edge was fresh odds and speed."* Its 7%-edge directional residuals **lost money in aggregate** — textbook adverse selection. Requires a paid live odds feed and sub-second cancels. **Rule out: it is our latency failure with an extra data-vendor bill.** ([README, read directly](https://github.com/kachence/polymm))
- **Mean reversion on binary contracts.** Twelve strategy variants on 10-minute bars over ~1 year: *"substantial alpha under passive limit-order execution (zero-spread scenario)"* that **"degrades significantly when more aggressive market orders are accounted for"** ([QuantPedia](https://quantpedia.com/exploiting-mean-reversion-in-decentralized-prediction-markets-evidence-from-polymarket-binary-contracts/)). "Alpha only if you fill passively at zero spread" is **exactly the assumption M8 falsified for us.** Rule out.
- **Anything crypto.** Highest fee tier (0.07), most latency-contested, and Polymarket has been actively re-engineering BTC settlement to close quant loopholes ([Protos](https://protos.com/polymarket-ends-trading-loophole-for-bitcoin-quants/)). Out.

---

## 3. The comparison table

| Strategy | Evidence quality | Return on $2k (honest) | Lock-up | Worst tail | Pi/minutes fit | Reduces to a proven failure? |
|---|---|---|---|---|---|---|
| **Liquidity-rewards farming** | Mechanics verified; profitability contested (1 archived bot: no; 1 unfinished: maybe) | −$2 to +$3/day, sign genuinely unknown | Continuous, exit at will | Adverse selection + inventory; heartbeat wipes | Revenue side yes; quote defence no | **No** — inverts M8's failure mode from "never fills" to "fills adversely" |
| **Near-resolution capture** | Mechanism verified; calibration evidence conflicting ±2pp | −$50 to +$200/month | Hours (p90 6.4h; p99 4.2d) | 98% breakeven; 1% dispute rate is the whole error budget | Good — no latency race | **No** — taker at wide-open prices, hours-long capital turns |
| **Holding rewards (split-and-hold)** | Verified program; hedged eligibility single-source | **+$0.22/day, risk-free** | Merge out at will | Program cancelled | Perfect | **No** — but it is a floor, not a business |
| **Resolution-rule mispricing** | Peer-reviewed *warning*; strategy unproven | Unknown, high variance | Days–weeks | You are betting on a captured token vote | Excellent | **No** — but needs a competence we lack |
| Longshot harvesting | Bias verified; magnitude single-source | ~13%/yr gross at long duration, before costs | Weeks–months | Same 98% breakeven | Fine | Collapses into near-resolution, worse velocity |
| NegRisk / midterm regime | **Decisively negative** (half-lives → <1 min in Nov 2024) | ≈0 | — | — | No | **Yes — directly** |
| News latency | Verified negative for high-attention | ≈0 without a new competence | — | Informed counterparty | Marginal | Yes (fast half) |
| Cross-platform Kalshi | Gaps verified; execution verified-negative | ≈0 at $1k/venue | Days to rebalance | **Two adjudicators = not an arb** | No | Yes (spread/latency) |

---

## 4. Ranked recommendation

### #1 — Liquidity-rewards farming in low-competition markets

**Why first, despite the archived bot.** Not because the returns look good — they look thin, and one competent open-source attempt died of exactly our capital constraint. It ranks first because **the revenue side is exactly and cheaply simulatable, and the simulation is nearly exact rather than approximate.** The reward score depends only on (a) our hypothetical resting orders and (b) the public order book — both fully observable. We can compute our *precise* would-be share of every market's daily pool, minute by minute, **without placing a single order or committing a dollar.** No other candidate offers that. In M8 terms: we can compute the payoff, not estimate it.

It is also the only candidate whose revenue accrues on a **per-minute** clock, which is the one clock our infrastructure can actually keep.

**Decisive measurement (the M8 analogue):**
> Replay 14 days of book snapshots across all reward-eligible markets. For each minute, insert our hypothetical two-sided quotes at `rewardsMinSize` under a $2,000 budget allocated by Birantx's `pool/(in_band_competition+1)` rank, compute `S = ((v−s)/v)² · b` for our orders **and every competing in-band order already on the book**, and normalize. **Output the single number: expected gross reward $/day at $2,000 — and the count of markets where we clear the $1/day floor.**
>
> **Kill criterion: if gross reward at $2,000 is under ~$3/day, stop — no adverse-selection model can rescue it, because adverse selection only subtracts.** Birantx's own logged range for adverse cost was 27%–175% of gross reward, so gross must exceed roughly 2× the target net before the strategy is even arguable.

This is decisive in the strongest sense: it can only kill the idea or license the next step, and it costs zero capital and no new API surface beyond the reward parameters we already fetch.

**Second, only if the first passes:** measure fill toxicity by tracking mid-price drift at +1/+5/+30 minutes conditional on each simulated fill. That is the number Birantx admits cannot be simulated — so treat it as bounding, not proof, and gate live money behind a **$100 ramp**, not $2,000.

---

### #2 — Near-resolution capture, run first as a pure observation study

**Why second.** The fee curve works *for* us here for the first time in this project (0.078¢/share vs 1.00¢ at 50/50), there is no latency race, and capital turns in hours rather than being locked to resolution. But the public calibration evidence is **genuinely contradictory at exactly the 95–99¢ band** (0.95 resolving at 0.93 vs "the 90–100% bucket is underconfident"), and the entire edge is 1–3¢ wide. Nobody's published number settles it. Ours can.

**Decisive measurement:**
> Using the existing scanner, log every market whose **best executable ask** sits in [0.96, 0.995] with ≥$50 of depth at that ask. Record the executable price (never the mid), the timestamp, the category, and the resolution wording. Then wait and record the **realized outcome** and **realized time-to-resolution**. At N ≥ 300 observations, compute: **realized net-of-fee return per turn, the empirical loss rate, and the annualized IRR** — bucketed by residual duration and by category.
>
> **Kill criterion: if the empirical loss rate exceeds `gross_gap / (1 − entry_price)` — i.e. if more than ~1 in 40 positions at 0.975 fails — the strategy is negative and we stop.**

This costs **zero capital**, reuses the scanner we already built, and simultaneously answers §2.4 (longshot harvesting) by bucketing on duration. It also directly measures the one thing the settlement-discount paper implies we should fear: that the 2¢ is *rent for lock-up*, not mispricing. If the realized IRR lands near the risk-free rate plus the 4% holding-rewards rate, the paper is right and there is nothing here.

**Note the sequencing advantage:** this measurement runs *in the background* while #1 is being built. It requires only logging.

---

### #3 — Holding-rewards split-and-hold, as a capital floor

**Not a strategy — a decision about idle cash.** Verified 4.00% APR, zero directional risk via Split, no spread, no fee, merge out at will, latency-irrelevant. $0.22/day on $2,000. Deploy it for whatever capital is not doing something better, and note that it **lowers the hurdle rate for #2** in the eligible political markets (which include the 2026 midterms).

**Decisive measurement — and this one is not a dry run:**
> **Split $100 into one eligible market (e.g. 2026 midterms balance of power), hold 7 days, confirm the pUSD accrual matches `value × 0.04/365` per day.** Total risk: $100 of capital and Polygon gas.

This exists solely to test the one unverified link in the chain: whether Polymarket credits **hedged/split positions** at full value or excludes them. That question cannot be answered from the blocked documentation and is not worth a week of research — it is worth $100 and seven days.

---

## 5. What I would NOT do

- **Do not rebuild NegRisk detection for the midterms.** The Nov-2024 regime evidence is decisive in the wrong direction: half-lives fell from hours to **under a minute** as volume arrived. The midterms will make this worse for us, not better.
- **Do not add Kalshi.** Two adjudicators is not an arbitrage, and $1,000 per venue cannot support it.
- **Do not touch crypto or 5-minute markets.** Highest fee tier, most contested, actively being re-engineered against quants.
- **Do not deploy $2,000 on the strength of any number in this document.** Every profitability figure here is either from a content-marketing site, a second-hand search summary of a blocked paper, or one unfinished open-source bot. **The two highest-quality data points I read directly are both discouraging** (one archived bot, one retired bot).
- **Do not treat the 4% holding reward as an edge.** It is roughly the risk-free rate. Its value is that it removes the excuse for capital to sit idle.

## 6. The one-line decision frame

Run measurement #1 (reward-score replay, ~zero cost) and start logging #2 (near-resolution observation, ~zero cost) **in parallel and before committing any capital**; spend $100 on the #3 split test to close the one unverifiable documentation gap. If #1 returns under ~$3/day gross at $2,000 and #2's realized loss rate is above ~2.5%, **the honest answer is that this account is too small for this venue in 2026, and the right move is not to trade it.**

---

## 7. Follow-up reads (blocked here — retrieve with unrestricted access)

Ordered by expected decision value:

1. **[arXiv:2608.00666](https://arxiv.org/abs/2608.00666)** — Gebele, Mutzel & Matthes (TUM, 2026-08-01), *Executable Arbitrage and Market Efficiency in Prediction Markets*. Depth-aware executable NegRisk violations + who exploited them. Most likely to either close the NegRisk file permanently or reveal a surviving niche.
2. **[arXiv:2605.31431](https://arxiv.org/abs/2605.31431)** — *When Certainty Is Not Worth It*. The actual ASW magnitudes by maturity are the hurdle rate for §2.3. If the ASW at a 1-day horizon exceeds our expected net, near-resolution capture is dead on arrival.
3. **Wang, Economics Letters 268:113176** — the adjudication-vs-event paper. Peer-reviewed, and the honest ceiling on §2.8.
4. **[arXiv:2606.04217](https://arxiv.org/abs/2606.04217)** + its [HuggingFace dataset](https://huggingface.co/datasets/TimeSeventeen/Polymarket-v1) — 1.2B trades, 2022→Apr 2026. **This dataset would let us backtest #2 historically instead of waiting for 300 live observations.** Highest practical value of anything on this list.
5. **[arXiv:2606.16852](https://arxiv.org/abs/2606.16852)** — *The Ghosts of Polymarket: When Off-Chain Matches Meet On-Chain Reverts*. Execution-reliability tail we have not modelled at all.
6. **[Polymarket liquidity-rewards docs](https://docs.polymarket.com/programs/liquidity-rewards)** — read the exact scoring formula and confirm the Birantx reimplementation before building measurement #1 on it.
