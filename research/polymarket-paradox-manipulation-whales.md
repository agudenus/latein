# Paper Summary: The Polymarket Paradox — Manipulation, Whale Concentration, and Predictive Accuracy

- **Paper:** Muhammad Noraiz Abid — *The Polymarket Paradox: Manipulation, Whale Concentration, and Predictive Accuracy in the World's Largest Prediction Market*
- **Links:** [SSRN abstract 6670638](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6670638) · [ResearchGate](https://www.researchgate.net/publication/404352194_The_Polymarket_Paradox_Manipulation_Whale_Concentration_and_Predictive_Accuracy_in_the_World's_Largest_Prediction_Market)
- **Published:** April 28, 2026
- **Note:** SSRN downloads are blocked by this session's network policy, so this summary was compiled from search results and secondary coverage rather than a full read of the PDF. Verify specifics against the original before relying on them.

## What the paper studies

The paper documents a paradox: Polymarket is simultaneously (a) a genuinely accurate forecasting instrument and (b) a venue that has hosted the largest documented price-impact/manipulation events in prediction market history. It uses verified anchor probabilities across **five episodes spanning June 2024 – July 2025** and **ten resolved binary markets** with publicly verifiable election-eve probabilities.

Scale context: Polymarket processed roughly **$9 billion** in trading volume in 2024, with **$3.7+ billion** on the 2024 US Presidential Election market alone.

## Key findings

### Predictive accuracy (the "good" side)
- Polymarket assigned ≥50% probability to the eventual winner in **7 of 10** resolved markets studied, including **6 of 8 battleground states** in the 2024 election.
- Its election-eve **57%** probability for Donald Trump scored a **Brier loss of 0.185**, against polling aggregates that had the race at roughly 50/50 — i.e. the market beat the polls.

### Whale concentration and price impact (the "bad" side)
- A single French trader ("Théo") operating **11 linked wallets** accumulated roughly **$85 million** in profit via concentrated directional bets on Trump.
- His buying pushed Polymarket's Trump probability **10–15 percentage points above competitor platforms** during October 2024 — the largest documented price-impact event in prediction market history. Prices on the largest, most liquid market can be moved by one actor and stay dislocated from other venues for weeks.
- His strategy: accumulate over months, buying into panic-selling dips on negative news, without revealing total position size.

### Oracle/governance risk (the "ugly" side)
- Two UMA oracle resolution disputes in 2025 saw markets totaling **$250+ million** settle through governance votes traders alleged were captured by token whales:
  - **Ukraine mineral deal market (March 2025):** "Will Ukraine agree to Trump's mineral deal before April?" resolved YES despite no deal existing; an actor wielding ~**25% of UMA voting power** forced the false resolution on a ~$7M market. Odds moved 9% → 100%.
  - **Zelenskyy suit market (July 2025):** resolved NO (not a suit) via governance vote despite expert opinion the outfit met the definition — a resolution-ambiguity dispute rather than a factual one.
- Aftermath (context beyond the paper): Polymarket deployed **Managed Optimistic Oracle V2** in November 2025, restricting resolution proposals to 37 pre-approved addresses while keeping disputes open.

## Implications for this project

1. **Cross-platform divergence ≠ arbitrage.** The Théo episode shows Polymarket can trade 10–15 points away from Kalshi/other venues for *weeks*. Divergence between platforms is only true arbitrage if both legs pay out on identical resolution criteria at overlapping times; otherwise it's a (possibly whale-distorted) relative-value bet. Our cross-platform detector must distinguish these.
2. **Resolution risk is a first-class risk, not a footnote.** "Risk-free" arbitrage assumes markets resolve correctly and consistently. The UMA episodes show resolution itself can be captured or ambiguous. Practical mitigations for the scanner: flag markets with subjective/ambiguous resolution criteria, markets in active UMA dispute, and pairs whose two legs have subtly different resolution wording (deadline, source, definition).
3. **Whale flow is signal.** Concentrated one-sided flow can create the very dislocations we scan for — and also means the "cheap" side may be cheap for a reason. Whale/insider tracking (a whole tool category in our ecosystem survey) can be a useful confidence input for ranking opportunities.
4. **Intra-Polymarket combinatorial arbitrage is more robust to this** than cross-platform arbitrage: both legs resolve under the same oracle and criteria, so resolution-consistency risk mostly cancels. This reinforces the NBA paper's direction (see `arbitrage-analysis-polymarket-nba.md`).
5. **Liquidity concentration cuts both ways:** headline markets are deep but efficient; long-tail markets are inefficient but shallow and more exposed to manipulation and resolution ambiguity.

## Related work surfaced during research

- [The Anatomy of a Decentralized Prediction Market: Microstructure Evidence from the Polymarket Order Book](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6658364) (Dubach) — order book microstructure.
- [Who Wins and Who Loses in Prediction Markets? Evidence from Polymarket](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6443103) (Akey, Grégoire, Harvie, Martineau).
- [The Anatomy of a Blockchain Prediction Market: Polymarket in the 2024 U.S. Presidential Election](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6336679) (Yang & Tsang).
- [Manipulation in Prediction Markets: An Agent-based Modeling Experiment](https://arxiv.org/html/2601.20452) — simulation of manipulation dynamics.
- [Manipulation, Insider Information, and Regulation in Leveraged Event-Linked Markets](https://arxiv.org/pdf/2605.10486).
