# Paper Summary: Arbitrage Analysis in Polymarket NBA Markets

- **Paper:** Jiaxin Yang, Guang Cheng, HaoXuan Zou (UCLA) — *Arbitrage Analysis in Polymarket NBA Markets*
- **Links:** [arXiv:2605.00864](https://arxiv.org/abs/2605.00864) · [SSRN](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6624718) · [UCLA copy](http://www.stat.ucla.edu/~guangcheng/Arbitrage%20Analysis%20Cheng.pdf)
- **Published:** April/May 2026
- **Note:** arxiv.org is blocked by this session's network policy, so this summary was compiled from search results and secondary coverage (notably ["Arbitrage Is Real. The Money Isn't."](https://arielcalista.substack.com/p/arbitrage-is-real-the-money-isnt) by Ariel Calista) rather than a full read of the PDF. Verify exact formulas against the original before implementing.

## What the paper studies

A systematic empirical analysis of algorithmic arbitrage in Polymarket's **NBA game markets**. The authors reconstruct continuous market states from **75+ million limit order book snapshots across 173 games** and measure the frequency, duration, and profitability of two arbitrage classes:

1. **Single-market arbitrage** — within one binary market, the combined ask price of YES + NO tokens falls below $1.00, so buying both sides locks in a risk-free profit at resolution.
2. **Combinatorial arbitrage** — across *related but separately traded* markets for the same game, e.g. Moneyline vs. Point Spread. A team covering a large spread is mathematically guaranteed to also win the moneyline, but Polymarket's smart contracts do not link or cross-margin these order books, so logically dependent markets can be transiently mispriced against each other. This includes the **"Middle"** construction, where the position can pay out on *both* legs ($2.00 total) if the final margin lands in a specific window.

A single NBA game spawns roughly **45 distinct markets** on fully isolated order books, which is what creates the structural room for cross-market mispricings.

## Key quantitative findings

| Finding | Number |
|---|---|
| Single-market (YES+NO < $1) executable in-game episodes | **7** total across 173 games |
| Median duration of those episodes | **3.6 seconds** |
| Combinatorial arbitrage active episodes | **290**, overwhelmingly in the final minutes of live play |
| Median return on combinatorial execution | **101 basis points (~1%)** |
| Episodes constrained by order book depth | **76.9%** |
| Average executable size in constrained episodes | **~14.8 shares** (often can't deploy even $100) |
| "Middle" double payout ($2.00) realized | **Never** — every combinatorial pair in the dataset resolved to the baseline $1.00 |

## Conclusions

- Polymarket's NBA markets show **profound microstructural efficiency**: pure single-market arbitrage is essentially extinct (7 fleeting episodes, seconds-long).
- The real (if modest) inefficiency lives in **cross-market/combinatorial** relationships between logically dependent markets — but it clusters in the chaotic final minutes of live games.
- Executable profit is **structurally bounded by liquidity**: shallow books cap positions at retail scale (~15 shares), so risk-free extraction cannot scale to institutional size.
- The theoretical jackpot of the Middle strategy is a mirage in practice; the realistic outcome is the ~1% locked baseline return.

## Implications for this project

1. **Don't build around single-market YES/NO arbitrage as the main product** — at least in high-attention sports markets it is nearly nonexistent and lasts seconds, requiring latency we can't achieve with polling.
2. **Cross-market logical-dependency arbitrage is the promising direction**: detect markets whose outcomes are logically linked (moneyline/spread/totals for the same game; nested thresholds; mutually exclusive outcome sets) and check for price incoherence between them. This generalizes beyond sports to any Polymarket event with related markets.
3. **Depth-aware math is mandatory, not optional**: the paper shows 77% of opportunities can't absorb even $100. Our scanner must compute executable size by walking the order book, and report depth-adjusted profit — a headline "1% arb" on 15 shares is ~$0.15.
4. **Timing matters**: opportunities cluster around high-volatility moments (live-game endgames, breaking news). Scan cadence and websocket subscriptions matter more than broad coverage.
5. **Set expectations honestly**: median ~101 bps per opportunity, at retail size. This is a tool for finding scraps of free money and for market research — not a money printer.

## Related work surfaced during research

- [*Unravelling the Probabilistic Forest: Arbitrage in Prediction Markets*](https://arxiv.org/abs/2508.03474) (AFT 2025) — broader treatment of arbitrage across logically related prediction markets; likely the theoretical companion to read next.
- [Finding free money between Kalshi and Polymarket](https://www.yashkothari.ca/writing/polymarket-kalshi-arbitrage) (Yash Kothari) — practitioner writeup of cross-platform arbitrage.
- Turbine blog: [Why You Can't Spot Arbitrage Fast Enough on Prediction Markets](https://www.turbinefi.com/blog/prediction-market-arbitrage-latency-speed-2026) — latency arms race context.
