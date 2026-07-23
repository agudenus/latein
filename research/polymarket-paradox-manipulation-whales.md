# Paper Summary: The Polymarket Paradox — Manipulation, Whale Concentration, and Predictive Accuracy

- **Paper:** Muhammad Noraiz Abid — *The Polymarket Paradox: Manipulation, Whale Concentration, and Predictive Accuracy in the World's Largest Prediction Market* (April 2026)
- **Links:** [SSRN abstract 6670638](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6670638) · [ResearchGate](https://www.researchgate.net/publication/404352194_The_Polymarket_Paradox_Manipulation_Whale_Concentration_and_Predictive_Accuracy_in_the_World's_Largest_Prediction_Market)
- **Status:** summarized from a full read of the PDF (provided by the owner).

## The paradox

Polymarket in 2024–25 was simultaneously:
- the **most accurate major forecaster** of the 2024 US election (best Brier score of any major forecaster), and
- the venue of the **largest documented single-trader price impact** and the **largest oracle-resolution disputes** in prediction market history.

The paper documents both facts with verified, sourced probability anchors (no interpolated or simulated data) and argues they are not contradictory: manipulation that is *directional* (in the trading layer) gets absorbed by profit-seeking counterparties, while manipulation of the *resolution layer* has no counterparty and is the genuinely dangerous kind.

## Data: five anchor episodes (June 2024 – July 2025) + ten resolved markets

- **E1 — Biden withdrawal market** (June–July 21, 2024, $21.1M volume): the cleanest case of Polymarket *leading* the news. 20% baseline → 42% after the CNN debate → 82% on July 2 (first Democratic legislator calls for exit) → 32% after the Trump shooting attempt → 36%→70% in 7.5 hours on July 17 → resolved YES July 21, with the price reflecting Biden's statement **6 minutes before** the first legacy news network confirmed it.
- **E2 — The "Théo" whale spike** (October 7, 2024 onward): a French trader (username **Fredi9999**) operating **11 linked wallets** (confirmed by Chainalysis, published in Bloomberg Law) placed ~**$80M** of directional Trump bets, ultimately earning ~**$85M** profit (largest single account, "Theo4": ~$22M). On Oct 7 Polymarket had Trump at 53.3% vs PredictIt 49%, FiveThirtyEight 45%, Silver Bulletin 45.3%; by Oct 30 Polymarket was at ~67% while polling models sat near 50/50. The gap widened to **10–15 percentage points and persisted for three weeks** until the election. Polymarket's own investigation concluded there was no intent to mislead — "high conviction directional betting"; the trader claimed research-driven bets, including privately commissioned neighbor-method YouGov polls in three battlegrounds.
- **E3 — Election resolution week** (Nov 4–8, 2024): election-eve 57% Trump; resolved correctly.
- **E4 — Ukraine mineral deal market** (March 2025, $7M volume): resolved **YES despite no deal existing**. YES surged 9% → 100% between March 24–25 after a single UMA holder with **5M tokens across 3 accounts cast ~25% of disputed votes**. Polymarket called it an "unprecedented governance attack" and **refused refunds**.
- **E5 — Zelenskyy suit market** (June–July 2025, **$242M volume**): YES hit ~85% when Zelenskyy attended the NATO summit in what media called a suit, then collapsed to **4% over 14 days** through repeated UMA disputes, finally resolving NO. Key ratio: top-10 UMA voters held ~6.5M UMA (~30% of typical vote participation) against a UMA market cap of ~$95M — **the cost of controlling the vote was far below the $242M contested market value**.

## Accuracy results (10 resolved 2024 binary markets)

- Election-eve Brier scores on the presidential outcome: **Polymarket 0.185** (best), Kalshi ~0.203, FiveThirtyEight and Silver Bulletin 0.250, The Economist 0.314 (worst). Mean Brier across the ten markets: **0.213**.
- Polymarket put ≥50% on the eventual winner in **7 of 10** markets, including **6 of 8 battlegrounds** (misses: Michigan, Wisconsin, and the Walz VP pick priced at 23% the day before). All misses were markets where Polymarket favored *Democrats* — the market's famous "Trump bias" was directionally correct in 2024.
- Big caveat the author stresses: this is conditional on a **single binary realization**. "If Trump had not won, every conclusion in this paper about Polymarket's accuracy would be reversed."

## The theoretical framework (why accuracy and manipulation coexist)

Drawing on Hanson & Oprea (2009): a directional manipulator effectively subsidizes liquidity for informed counterparties, so trading-layer manipulation gets absorbed and average accuracy survives (individual prints can still be distorted). The absorption mechanism requires **three boundary conditions**:

1. Manipulation is in the **trading layer**, not the resolution layer (a vote has no counterparty);
2. **Sufficient liquidity** that the manipulator can't exhaust counterparty capital;
3. The outcome is **unambiguous** enough that resolution can't be meaningfully disputed.

E4 and E5 are exactly the cases where conditions 1/3 failed — and that's where traders lost money on positions ground truth would have made winning.

## Findings directly relevant to an arbitrage finder

1. **Persistent cross-venue gaps are real and are NOT free money.** The 10–15 point Polymarket–PredictIt gap lasted three weeks and was *not* arbitraged away. The paper uses the absence of spillover as evidence the move was venue-specific rather than informational. For us: cross-platform "arbitrage" signals must account for why the gap exists (position limits, KYC walls, capital lockup, whale flow) — a wide, persistent gap is often a relative-value bet, not an arb.
2. **Resolution risk is the tail risk that breaks "risk-free."** Both UMA episodes show markets can resolve against ground truth (E4) or on ambiguous definitions (E5). The scanner should flag: subjective resolution wording, markets in active UMA dispute, and any cross-market pair whose legs could resolve inconsistently. Intra-Polymarket combinatorial arbitrage (both legs on one oracle) mostly cancels this risk — but E4 shows even a single market can settle "wrongly."
3. **Don't trust volume/liquidity dashboards.** The paper adopts findings that Polymarket on-chain volume is **double-counted** (separate OrderFilled events for maker and taker — Slivkoff/Paradigm 2025) and that **wash trading peaked near 60% of volume** in December 2024 (Sirolly et al. 2025). True October/November 2024 volume may be ~half the dashboard figures. Our liquidity estimates must come from **order book depth**, never from reported volume.
4. **Markets get more efficient as they mature.** Tsang & Yang (2026, cited): Kyle's lambda on the presidential market fell by more than an order of magnitude over ten months. Inefficiencies are more likely in young, thin, long-tail markets — which is also where resolution ambiguity and manipulation risk concentrate. Opportunity and risk live in the same place.
5. **Institutional context is shifting**: Polymarket acquired QCEX (CFTC-licensed exchange/clearinghouse, July 2025), received a CFTC Amended Order (Nov 2025), and restored US access (Dec 2025). The resolution layer is being hardened (see also the Managed Optimistic Oracle V2, Nov 2025, which restricts proposals to vetted addresses). Resolution-risk assumptions should be dated and revisited.

## Notable references to follow up

- Tsang & Yang (2026), *The Anatomy of Polymarket* — [arXiv:2603.03136](https://arxiv.org/abs/2603.03136): full on-chain microstructure of the 2024 election market.
- Rahman, Al-Chami & Clark (2025), *SoK: Market Microstructure for Decentralized Prediction Markets* — [arXiv:2510.15612](https://arxiv.org/abs/2510.15612).
- Sirolly, Ma, Kanoria & Sethi (2025), *Network-based Detection of Wash Trading* — [SSRN 5714122](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=5714122).
- Slivkoff (2025), *Polymarket volume is being double-counted* — [Paradigm Research](https://www.paradigm.xyz/2025/12/polymarket-volume-is-being-double-counted).
- Hanson & Oprea (2009), *A manipulator can aid prediction market accuracy* — the theory behind the absorption mechanism.
