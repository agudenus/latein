# Research: Execution Costs and the Arb Filter (Practitioner Guide)

Source: practitioner guide provided by the owner (author traded Polymarket arbitrage starting with no coding background; describes turning an unprofitable bot profitable). Captured 2026-07-23.

Core thesis: **detected gaps are not tradable profit.** Most of what a scanner flags as arbitrage dies to three sequential taxes — bid-ask spread, taker fees, and misclassification (relative value dressed as arb). The fix is a hard pre-trade filter plus a maker-first execution posture.

## 1. The spread kills ~3 of 4 detected gaps

Evidence cited: [Cao, *Essays on prediction markets* (NZAE, 2014)](https://www.nzae.org.nz/wp-content/uploads/2014/05/Cao.pdf) — a tick-by-tick arbitrage count in a prediction market (iPredict, NZ):

- Ignoring the bid-ask spread, arbitrage "existed" ~**90%** of the time.
- Adding the spread collapsed that to **24.5%**, and average profit per trade fell from ~1.4¢ to ~0.6¢ — **below the risk-free rate** (doing nothing paid better).

Mechanism: detectors typically compute gaps on mid or last price, but execution buys at the ask and sells at the bid. If the gap is smaller than the spread you must cross, the opportunity never existed. **Design rule: all gap math must be computed on the executable side of the book (ask for buys, bid for sells), never mid/last.**

## 2. The fee curve kills what's left near 50/50

2026 Polymarket charges taker fees per share of `rate × p × (1 − p)` (Kalshi-style curve), which **peaks exactly at p = 0.50** — where arb gaps cluster, because that's where uncertainty and volatility are highest. Category rates as given by the guide:

| Category | Taker fee rate (guide) | **Verified rate (official docs, 2026-07)** |
|---|---|---|
| crypto | 0.072 | **0.07** |
| economics, culture, weather, other | 0.05 | **0.05** ✓ |
| finance, politics, tech (+ mentions) | 0.04 | **0.04** ✓ |
| sports | 0.03 | **0.05** (guide was wrong) |
| geopolitics | 0.0 | **0.0** ✓ (fee-free both sides) |

> **✅ Verified 2026-07-23** against [docs.polymarket.com/trading/fees](https://docs.polymarket.com/trading/fees) via adversarially-verified research (see `market-opportunity-and-strategy-report.md`): formula confirmed as `fee = C × feeRate × p × (1−p)`, makers confirmed fee-free, and makers additionally earn **rebates funded by taker fees** plus **daily liquidity rewards** (market-level qualification params `min_incentive_size` / `max_incentive_spread` queryable via the CLOB API). Use the verified column in code; re-check rates periodically as they are set by the protocol and can change.

For a two-leg arb both legs share the same `p(1−p)`, so the pair pays **2×** the one-leg fee. Worked example: a crypto market at 50/50 → fee term = 2 × 0.072 × 0.25 = **3.6¢ per share pair** — a 2¢ "arb" is dead before the spread is even counted.

**Makers pay zero fees.** That's the single biggest lever: a gap that dies as a taker can be profitable captured with resting orders.

## 3. Much of the remainder is not arb at all

A price gap between two related markets is only true arbitrage if the combined position **locks $1.00 at resolution regardless of outcome**. If profit depends on prices converging again, it's **relative value** — a directional bet in an arb costume, and it can lose. The scanner must classify every opportunity as one or the other and never report relative value as risk-free. (This matches the whale finding in `polymarket-paradox-manipulation-whales.md`: dislocations can persist for weeks and resolve the wrong way.)

The guide's practical observation: the real money is less in textbook clean arb and more in **mistakes** — overreactions, bad multi-outcome pricing, one leg lagging news. Those are +EV trades, not risk-free ones, and should be labeled accordingly.

## 4. The filter (reference implementation)

Run every detected gap through this before it becomes an order:

```python
# 2026 Polymarket taker fee rates — VERIFY against official docs before use
# fee per share on a leg = rate * p * (1 - p)
FEE_RATES = {
    "crypto":      0.072,
    "economics":   0.05,
    "culture":     0.05,
    "weather":     0.05,
    "other":       0.05,
    "finance":     0.04,
    "politics":    0.04,
    "tech":        0.04,
    "sports":      0.03,
    "geopolitics": 0.0,   # still free both sides
}

def is_real_arb(gap, price, category, spread, taker=True, floor=0.005):
    """
    gap      - how far the price sum is off $1, per share (e.g. 0.03)
    price    - price of one leg; the other sits near (1 - price)
    spread   - bid-ask you cross on entry, per share
    taker    - True if you cross the book, False if you rest as maker
    floor    - minimum net you'll bother with, per share
    """
    rate = FEE_RATES.get(category, 0.05)

    # both legs share the same p*(1-p), so the pair fee is 2x one leg
    fee_per_share = 2 * rate * price * (1 - price) if taker else 0.0

    # a taker crosses the spread on entry; a maker does not
    spread_cost = spread if taker else 0.0

    net = gap - fee_per_share - spread_cost
    return net > floor
```

Notes for our implementation:
- The maker path (`taker=False`) zeroes both fee and spread cost but introduces **fill risk** (the gap can close before both legs rest and fill, leaving a one-legged directional position). The filter says *whether* an edge exists; a maker-execution model must additionally handle partial-fill/legging risk.
- `floor` (min net per share) belongs in config, not code.
- This composes with depth-awareness from the NBA paper: net-per-share must be multiplied by *executable size from the book*, and size-weighted average fill prices should replace top-of-book prices for anything beyond tiny size.

## 5. Strategy posture: detect wide, filter hard, capture as maker

1. **Detect wide** — scan everything; detection is cheap.
2. **Filter hard** — spread on executable prices, fee curve by category, true-arb vs relative-value classification, depth-adjusted sizing, minimum net floor.
3. **Capture as maker** where possible — rest orders instead of crossing, accepting fill risk in exchange for zero fees and zero spread cost.

## Design consequences for the scanner

- Gap detection on executable book sides only (asks for buy legs), never mid/last price.
- A per-market **fee model** (category → rate, formula `rate × p × (1−p)`, maker = 0) applied to every opportunity before reporting; rates fetched/configurable, not hardcoded.
- Every reported opportunity carries: gross gap, spread cost, fee cost (taker), **net as taker**, **net as maker**, executable size, and a **true-arb vs relative-value** label.
- Fee-free categories (currently geopolitics) and low-fee categories (sports) deserve scanning priority for taker-style capture.
- Report the honest headline: net expected profit in dollars at executable size — not the raw gap.
