# Soak verdict — 2026-08-08 → 2026-08-18 (Phase A go/no-go: NO-GO)

Decision taken 2026-08-25 with the owner, on 11 of 12 soak days of daily reports
(2026-08-12 missing from the record, 2026-08-09 was a full-day network outage reported
honestly as zero). This document is the permanent record of what the soak measured, the
decision it forced, and the lessons every future strategy in this repo must not re-learn.

## The question the soak was built to answer

Detection was never the bottleneck — the scanner surfaced ~700–3,600 gap constructions
per day. The June–July analysis had already established: taker capture of those gaps nets
~$11/day (not viable) and the maker "if-always-filled" view nets $600–4,000/day (fantasy
ceiling). Everything hinged on one number nobody publishes: **do passive resting orders at
the detected levels actually get filled?** M8 (the last-in-queue maker-fill simulator over
live trade prints) was built to bound that from below, defensibly.

## The answer

| day (UTC) | maker sims | filled | partial | unfilled | maker P&L LB | taker P&L | hypothetical |
|---|---:|---:|---:|---:|---:|---:|---:|
| 08-07 (pre-M8) | — | — | — | — | — | $11.00 | $632.96 |
| 08-08 | 367 | 0 | 5 | 362 | $0.00 | $4.30 | $903.54 |
| 08-09 | outage | — | — | — | $0.00 | $0.00 | $0.00 |
| 08-10 | 631 | 0 | 11 | 620 | $0.00 | $2.62 | $2,099.23 |
| 08-11 | 785 | 0 | 17 | 768 | $0.00 | $4.74 | $2,212.32 |
| 08-13 | 753 | 0 | 17 | 736 | $0.00 | $5.29 | $2,155.74 |
| 08-14 | 704 | 0 | 10 | 694 | $0.00 | $11.08 | $2,185.89 |
| 08-15 | 593 | 0 | 14 | 579 | $0.00 | $35.00 | $1,821.25 |
| 08-16 | 341 | 1 | 14 | 326 | $4.95 | $3.91 | $1,174.74 |
| 08-17 | 862 | 0 | 37 | 825 | $0.00 | $28.62 | $2,768.18 |
| 08-18 | 1,058 | 4 | 48 | 1,006 | $6.79 | $117.06 | $4,029.38 |
| **total** | **6,094** | **5** | **173** | **5,916** | **$11.74** | **$223.62** | **~$19,983** |

- **Maker full-fill rate: 0.08%** (5 of 6,094, last-in-queue accounting; `no_print_feed`
  measurement holes were 45 sims total — not the explanation).
- **Partials outnumber full fills 35:1.** A partially-filled basket is not an arbitrage,
  it is an unhedged directional position; live, the partials would have stranded ~$2,051
  of capital in one-sided exposure over the soak. Partial-fill legging is the *most likely
  daily outcome* of the maker strategy, not an edge case.
- Median time-to-fill on the two days with any fills: **~33–37 minutes** of resting
  exposure.
- Maker P&L lower bound: **≈ $1.30/day** — at the taker floor, nowhere near the ceiling.
- Taker P&L simulated ≈ $20/day, but it (a) assumes taking every opportunity, committing
  $15k–35k/day against real capital of $500–2k with lockup until resolution, and
  (b) credits fills at detection-time prices despite detection latency of minutes
  (p50 stream-detection latency ran 4–40 minutes across the soak at 55k–79k markets
  scanned). Scaled honestly it collapses toward the known $11/day floor.

**Verdict: NO-GO on Phase B execution.** No wallet, no order path. The dry-run discipline
did its job: the strategy was falsified by tape evidence before it could lose real money.

## Lessons — the mistakes this repo must not repeat

1. **A detectable gap is not a fillable gap.** Our detected constructions exist *because*
   nobody trades at those prices. Any future strategy must state, before building, why
   flow will reach our orders — and must measure fills (or their economic equivalent)
   from real tape in dry-run before any execution code is written. Detection volume is
   vanity; fills are truth.
2. **Passive capture at stale levels fails quietly, via partials.** The failure mode is
   not "no profit", it is "accidental unhedged inventory". Every multi-leg passive
   strategy must count a partial fill as a risk event, never rounding error.
3. **We cannot win latency races.** Minutes-scale detection (home connection, Pi,
   55k+ market universe) forecloses every strategy whose edge decays in seconds:
   fresh-gap sniping, crypto 5-minute markets, news trading. Strategies must be chosen so
   that being minutes late does not matter.
4. **Small capital changes the strategy space.** $500–2k with lockup-until-resolution
   cannot harvest a wide opportunity stream; simulated P&L that assumes unlimited
   parallel commitment is a lie at our size. Evaluate everything per-dollar-locked-per-day.
5. **Honest accounting was the project's best decision.** Three-number P&L (proven lower
   bound ≤ truth ≤ hypothetical), last-in-queue pessimism, `no_print_feed` holes reported
   as holes, outage days reported as zeros — this is what made the no-go cheap. Keep the
   discipline for every future measurement.
6. **Operational: home-network DNS is the recurring single point of failure** (three
   incidents in three weeks; containers additionally cache resolver config from startup —
   `docker compose restart` after any network change). Any always-on strategy needs a
   fallback DNS on the Pi and the self-healing already built (heartbeat, watchdog,
   discovery-never-fatal).
7. **Regime matters.** The soak ran in the August 2026 off-season. The historical profit
   pool (AFT 2025: ~72% of realized arb = NegRisk rebalancing) is concentrated around
   elections. A quiet-regime falsification does not falsify the election regime — but the
   burden of proof is on the regime, hence the planned late-October re-soak before the
   Nov 2026 midterms, using the same M8 machinery.

## What survives the no-go

- The entire measurement stack: scanner, cost model, depth-walking, WS streaming, M8
  print-based fill simulation, dashboard, reports. Any pivot reuses it.
- The one economically inverted fact: **we proved our quotes don't get hit.** For
  spread-capture that is fatal; for Polymarket's liquidity-rewards program (makers are
  paid daily for resting two-sided quotes, filled or not) it is close to the *desired*
  outcome. The pivot analysis (see `strategy-pivot-2026-08.md` when it lands) starts
  there, plus regime-timed NegRisk and near-resolution capture as candidates.
