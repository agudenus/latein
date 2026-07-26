# Deep-Research Report: Where the Opportunities Are & Which Strategy Wins

Produced 2026-07-23 by a multi-agent research run (5 search angles → 23 sources → 111 extracted claims → 25 adversarially verified: **16 confirmed, 1 refuted, 8 unverified** due to a mid-run usage limit; synthesis written by the lead session). Confidence labels: ✅ = confirmed 3-0 (or 2-0) against primary sources; ⚠️ = plausible but verification incomplete; sourced but treat as unconfirmed.

## 1. The 2026 fee schedule — verified, and it corrects our earlier numbers ✅

From [official docs](https://docs.polymarket.com/trading/fees) and [help center](https://help.polymarket.com/en/articles/13364478-trading-fees) (all 3-0 confirmed):

- Formula: `fee = C × feeRate × p × (1−p)` — peaks at p=$0.50, exactly where arb gaps cluster. **Makers never pay fees.**
- **Verified per-category taker rates** (these **correct** `execution-costs-and-arb-filtering.md`):

| Category | feeRate | Max fee per 100 shares (p=0.50) |
|---|---|---|
| **Geopolitics** | **0.00 — fee-free** | $0.00 |
| Politics, Finance, Tech, Mentions | 0.04 | $1.00 |
| Sports, Economics, Culture, Weather, Other | 0.05 | $1.25 |
| Crypto | 0.07 | $1.75 |

  (Prior guide said crypto 0.072 and sports 0.03 — both wrong; sports is 0.05.)
- A claim that *all* categories including geopolitics now carry fees was **refuted 0-3**: geopolitics remains fee-free, the only category where taker execution costs zero platform fee. ✅
- Taker fees fund a **daily Maker Rebates Program** (makers get a cut of taker fees they're matched against) plus a tiered **Taker Rebate Program**. ✅ So maker-side capture is *positively subsidized*, not merely free.
- **Liquidity rewards** pay makers daily at midnight UTC ($1 minimum payout), and each market's qualifying parameters (`min_incentive_size`, `max_incentive_spread`) are **queryable via the CLOB/Markets API** — the scanner can compute, per market, exactly which resting quotes earn the subsidy. ✅

## 2. Where arbitrage money was actually made — the decisive evidence ✅

The AFT 2025 study ([arXiv:2508.03474](https://arxiv.org/abs/2508.03474), IMDEA — year-long, Apr 2024–Apr 2025, 10,237 markets / 86M transactions) measured **realized** (executed, not theoretical) arbitrage profit on Polymarket: **~$40M total**, split:

| Strategy | Realized profit | Share |
|---|---|---|
| **NegRisk multi-outcome rebalancing** (esp. buying NO across election outcomes) | **~$28.9M** | ~72% |
| **Single-condition YES/NO rebalancing** (dominated by sports) | **~$10.58M** | ~26% |
| **Combinatorial** (logically dependent market pairs) | **~$95K** | ~0.24% |

Both the total and the breakdown were confirmed 3-0. Supporting detail (⚠️ unverified — verifier agents hit the usage limit, source is the same paper): of 13 logically dependent pairs identified around the 2024 election, only 5 ever produced realized profit; and category-wise, single-condition profit concentrated in **sports** while NegRisk profit concentrated in **politics/elections**.

This **reweights our earlier conclusion** from the UCLA NBA paper. Combinatorial arb is real (290 episodes, ~101 bps median, ✅ confirmed) but it is a *rounding error at platform scale* and carries legging/middle risk. The dollars are in the two *pure* intra-market forms — which also have the **highest win ratio**, since a completed rebalance locks $1.00 regardless of outcome with no cross-market resolution risk.

## 3. Single-market arb: real money pool, brutal speed game ✅/⚠️

- In NBA in-game markets: essentially extinct for slow actors — 7 episodes / 173 games, median lifetime 3.6s. ✅
- Practitioner data (⚠️, Medium analysis of Q3 2025–Q1 2026 order books): average same-market arb lifetime fell to **~2.7 seconds**, ~73% of profits captured by sub-100ms bots, median gap ~0.3%.
- Yet the AFT study shows $10.58M was realized in a year, mostly in sports — the pool refills continuously during live games; capture just requires being fast **or being the maker** (resting orders that others cross).

## 4. Cross-platform: bigger gaps, worse plumbing ⚠️

All cross-platform claims ended unverified (usage limit), but the sourced picture ([arXiv:2601.01706](https://arxiv.org/abs/2601.01706)): persistent execution-aware deviations of ~2–4% between semantically equivalent markets — larger than intra-Polymarket gaps — but only ~6% of events are cross-listed, capital must sit fragmented across venues, and resolution criteria can genuinely diverge (documented Kalshi-vs-Polymarket settlement divergence on an identical-seeming market). Consistent with our whale/paradox research: treat as a later, carefully-filtered addition — not MVP.

## 5. Maker capture is the strongest confirmed edge ✅

Three independent confirmed facts stack: makers pay **zero** fees; makers **receive rebates** funded by taker fees; makers earn **daily liquidity rewards** with API-queryable qualification parameters. Practitioner corroboration (⚠️): an open-source maker-side sports bot ("polymm") reports ~$5k net over a few months in early 2026 with an on-chain-verifiable wallet. The cost: fill risk (legging) — which is smallest in NegRisk rebalancing where each leg independently reduces risk rather than creating a middle.

## 6. Recommendation: scanner MVP focus

**Primary: intra-Polymarket NegRisk (multi-outcome) rebalancing — politics/elections first, maker-preferred execution.**
Largest realized pool (~$28.9M/yr) ✅, true risk-free structure (single oracle, mutually exclusive outcomes), politics fee tier is low (0.04) and irrelevant when captured maker-side, and multi-leg books move asynchronously — mispricings arise on every news shock. 2026 midterms (Nov 2026) are the catalyst that recreates the 2024 conditions.

**Secondary: single-condition YES/NO rebalancing in sports** (second pool, $10.58M/yr ✅) — enter as maker where possible; taker only when net-positive through the verified 0.05 fee at the executable price.

**Opportunistic: geopolitics** — the only fee-free category ✅, so marginal taker gaps that die elsewhere survive there; scan it with a lower net-floor threshold. Balance against its higher resolution-ambiguity risk (see `polymarket-paradox-manipulation-whales.md`).

**Deprioritized: combinatorial** (tiny realized pool, legging risk — keep detection cheap but don't build execution first) and **cross-platform** (later phase, resolution-criteria matching required).

**Win-ratio ranking** (risk-free-ness × evidence): NegRisk rebalancing ≥ single-condition rebalancing > maker-captured spreads > combinatorial > cross-platform > mistake-driven +EV (highest return potential, but directional — not arb).

## Sources (primary)

- [Polymarket fee docs](https://docs.polymarket.com/trading/fees) · [Trading fees (help)](https://help.polymarket.com/en/articles/13364478-trading-fees) · [Maker Rebates Program](https://help.polymarket.com/en/articles/13364471-maker-rebates-program) · [Liquidity rewards](https://docs.polymarket.com/market-makers/liquidity-rewards)
- [AFT 2025: Unravelling the Probabilistic Forest](https://arxiv.org/abs/2508.03474) ([PDF](https://suarez-tangil.networks.imdea.org/papers/2025aft-arbitrage.pdf))
- [UCLA NBA arbitrage study](https://arxiv.org/abs/2605.00864)
- [Cross-platform deviations study](https://arxiv.org/abs/2601.01706)
- Practitioner (unverified): [polymm](https://github.com/kachence/polymm), [polymarket-arbitrage](https://github.com/ImMike/polymarket-arbitrage), [BTC cross-platform bot](https://github.com/CarlosIbCu/polymarket-kalshi-btc-arbitrage-bot), Medium practitioner analyses, [settlement-divergence writeup](https://defirate.com/prediction-markets/how-contracts-settle/)
