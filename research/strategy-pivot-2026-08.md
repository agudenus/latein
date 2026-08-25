# Strategy pivot — post no-go plan (2026-08-25)

Owner directive after the Phase A no-go (`soak-2026-08-verdict.md`): research Polymarket
strategies, make one work, or find another logic — without repeating the proven mistakes.
This document synthesizes the two research reports commissioned for that decision
(`liquidity-rewards-2026-08.md`, `retail-strategy-survey-2026-08.md`) into a concrete,
measurable plan.

## The honest top line (set expectations first)

Nothing found in either report credibly returns more than **single-digit dollars per day
on $500–2,000** for an operator with our constraints (no informational edge, minutes-scale
latency, capital locked until resolution). Base rate: ~0.5% of Polymarket wallets have
ever cleared $1,000 total profit; cumulative liquidity-provider rewards ever paid are
$12.86M across 66,567 wallets (top 1% ≥ $1,563 *all-time*). The realistic success case at
$2k is **$2–6/day gross, −$1 to +$3/day net**. Anyone promising more at this size is
selling something. The plan below is therefore built the same way Phase A was: **measure
first at zero capital, with kill-lines written down before the measurement starts.**

## The trap the research caught (a lesson-1 near-miss)

The intuitive pivot — "our quotes never filled, and liquidity rewards pay for quotes
whether or not they fill" — is **wrong**, and the rewards deep-dive killed it: reward
scoring only credits quotes within ~1.5–5.5¢ of the midpoint. In-band quotes DO get
filled; that is the point of the subsidy. **Rewards are approximately the market price of
adverse selection.** One directly comparable open-source bot was live-tested at our
capital size and retired for exactly this reason (sub-$1/day floor + toxic fills, with
logged toxicity running 27–175% of gross rewards). So the decisive number is **net of
markout**, never gross rewards — a gross-only simulation would repeat soak lesson #1 in
new clothes.

## What was ruled out, and why it stays ruled out

- **October midterm NegRisk re-soak — cancelled.** Decisive evidence: in Nov 2024,
  intra-Polymarket arb half-lives *fell from hours to under a minute* as election volume
  arrived. High volume feeds the fast, not us. (Supersedes the verdict doc's lesson #7
  hope: the regime change makes fills *harder* for a slow actor, not easier.)
- **Cross-platform Kalshi arb**: two different adjudicators is not an arbitrage; capital
  split across venues; ruled out.
- **News-latency trading**: a directional-competence business we are not in.
- **Longshot-bias harvesting**: collapses into near-resolution capture with worse capital
  velocity, or into crypto (excluded, highest fee tier + latency).
- **Crypto 5-minute markets**: unchanged — fee tier 0.07 and a latency race.

## The plan: Measurement Phase R ("Soak 2") — zero capital, two instruments, ~14 days

Both instruments run in the existing dry-run daemon on the Pi, reusing the streaming and
print machinery Phase A built. No wallet, no keys, no orders — same guarantees as before.

### R1 — Liquidity-rewards farming simulator

Simulate our two-sided in-band quotes on reward-eligible markets and compute, from public
data only:

1. **Gross**: our per-market per-minute Q-score under the verified formula
   (`S = ((v−s)/v)² × shares`, both-sides `Q_min` rule, daily 00:00 UTC epoch), and our
   pool share as `our score ÷ (pool competition)` — competition measured from the live
   book as in-band resting notional (`market_competitiveness` from
   `/rewards/user/markets` where available as cross-check). Parameters
   (`min_incentive_size` in shares, `max_incentive_spread` in cents) from the CLOB
   rewards endpoints (`/sampling-markets`, `/rewards/markets`); mind the $1/day/market
   payout floor.
2. **Net**: adverse selection measured with the M8 print machinery *in reverse* — when
   observed prints cross our simulated in-band quote, that is a fill we would have taken;
   markout = mid(t+5min) and mid(t+1h) minus fill price, signed. Net = gross rewards −
   markout losses on simulated fills. Also track the paired-quote structure (YES-bid +
   NO-bid banking `2s` per merged pair) as the inventory-neutral quoting mode.
3. Market selection: maximize expected net per dollar of committed quote notional under
   a $2,000 cap; report the chosen portfolio daily.

**Kill-line (pre-committed): simulated net ≤ $1/day at $2,000 across 14 days → dead.**
(Survey's independent kill-line — gross < $3/day — is subsumed: if gross fails, net fails.)

### R2 — Near-resolution capture observation study

Pure logging, no simulation of our own behavior needed: for every market whose best
executable ask sits in **96–99¢** with time-to-resolution under ~72h, record the
executable ask + depth, entry-time metadata, realized outcome, time-to-payout, and any
dispute/UMA flags (dispute rate is rising — track it explicitly). Compute the realized
loss rate at executable prices and the capital-recycling yield after the (tiny, favorable)
fee at those prices.

**Kill-line (pre-committed): loss rate worse than 1-in-40 at executable prices, or
annualized net yield below ~15% at realistic recycling speed → dead.**

### R3 — Split-and-hold floor (no build)

Verified 4.00% APR holding reward (~$0.22/day at $2k, no directional risk, exit anytime).
Not a strategy — the benchmark every strategy must beat. Recorded here so the final
decision compares against it and against "do nothing".

## Decision rule after Soak 2 (~2026-09-10)

- R1 or R2 clears its kill-line → design a micro-scale live probe ($200–400, not $2k)
  for the survivor, with the owner's explicit approval, following every wallet/security
  rule in CLAUDE.md.
- Both die → **stop, and say so**: Polymarket does not pay at this capital and latency;
  the honest options are the 4% floor or walking away with the tooling and the lessons.

## Security warning (owner-relevant, from the rewards report)

At least one search-prominent "Polymarket liquidity rewards bot" repository
(`polymarket-liquidity-rewards-bot/*`) is a plagiarized README wrapping a Windows `.exe`
that asks for your private key. **Never run third-party trading binaries; never enter the
wallet key anywhere but the deployment secret store we control.** The only code reference
worth reading is `warproxxx/poly-maker`, as reading material.
