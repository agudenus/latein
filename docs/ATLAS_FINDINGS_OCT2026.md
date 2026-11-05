# Atlas KvK history: findings, October 2026

Source: ks-atlas admin export received 2026-10-24, KvK 1 to 19, 23,436
kingdom-side rows, 2,368 kingdoms, 72 byes. No power data. The raw file is
kept out of git (`data/atlas/`, gitignored) because this repo is public and
the dataset belongs to the admin. Everything below is reproducible with:

```
python3 scripts/ingest_atlas.py     # -> data/matchups.csv (gitignored)
python3 scripts/era_leak.py         # era aware leak test
python3 scripts/derive_pools.py     # -> data/pools_by_kvk.json
python3 scripts/pool_tests.py       # pool sizes, mixing, rematch gaps, record
python3 scripts/backtest_record.py  # Stage 2 backtest, record only
python3 scripts/score_kvk19.py      # scores the pre-registered predictions
```

## Data quality

Clean. Every matchup appears from both sides with mirrored W/L, no
duplicates, no self matches. `kvk_date` is blank for KvK 12 to 17, but every
present date fits KvK n = 2025-05-24 + 28(n-1), so dates are filled from
that. `kvk_date` is the matchmaking Saturday.

## 1. The published groups are not the matchmaking pools

The decisive era aware test (CLAUDE.md section 7 step 2) **failed as
stated**. Scored against the boundaries in force on each date, leak is not
near zero. It is exactly zero in some KvKs and around 40 to 65% in others.

Deriving each KvK's real pools directly from the matchups (a cut wherever no
matchup crosses a kingdom number, no group map assumed) shows why:

- Every pool is a **union of adjacent cohort blocks**. The published groups
  and blocks are the building units, plus unpublished blocks of a similar
  size (for example 116-175, 176-235, 418-463, 464-505, 506-545, 546-587).
- **Which neighbours get pooled together changes every KvK.** K377's pool:

| KvK | Date | Pool containing K377 |
|---|---|---|
| 15 | 2026-06-20 | K26 to K417 |
| 16 | 2026-07-18 | K116 to K587 |
| 17 | 2026-08-15 | K236 to K758 |
| 18 | 2026-09-12 | K236 to K417 (group alone) |
| 19 | 2026-10-10 | K116 to K417 |

- Pools hold a median of 50 to 75 kingdoms. The oldest cohorts get pools of
  up to about 520.
- The published Neighboring Kingdom Leaderboard merges change the **smallest
  unit**: after a merge, the two blocks are never split again. They do not
  fix the pool.

So Stage 1 becomes two layers. The unit (published group) is known. Which
neighbouring units join it in a given KvK is **the new open problem**. The
earlier "walls exist but move" conclusion was half right. The walls are
real, but they move every cycle, not only when a merge is announced.

The distance decay model is still wrong. Pools are hard edged, and the
smooth curve came from pooling across many pool layouts.

## 2. Inside a pool, cohort barely matters

Across 17 pools spanning two or more published units, 598 of 1,289 pairings
crossed units, against 633 expected under uniform pairing (94% of uniform).
In KvK 19, 26 of 55 pairings inside K236 to K417 crossed 309/310, against
48.5% uniform. Age or cohort inside a pool is close to irrelevant.

## 3. Rematch cooldown confirmed

284 rematches. The smallest gap is **exactly 3 KvKs** (15 cases), and there
are none at 1 or 2. A kingdom cannot meet the same opponent in the 2 KvKs
after a meeting. From the 3rd it can.

## 4. Battle record sorts pairings, weakly

Correlation of the two sides' prior battle win rate: **r = +0.34** across
8,175 pairings, against a within pool permutation null of -0.01 +/- 0.01
(p < 0.002). The value is stable from KvK 14 to 19 (0.26 to 0.36).

As a predictor it is weak. Ranking pool candidates by win rate closeness
(actual pool given, cooldown applied) gives top 5 accuracy of 6.8% against
5.3% for uniform, and top 10 of 13.4% against 10.6%. Record is either a
small input to Matchmaking Points or a noisy shadow of power. The two cannot
be separated without power data.

## 5. Corrections to the earlier handoff

- **The optimizer opponent lists were never frozen at KvK 16.** They show a
  kingdom's **first 10 KvKs**. K377's baseline list is exactly its KvK 5 to
  14 opponents. Its KvK 15 to 19 opponents (K303, K222, K658, K254, K273)
  were never in it.
- K455 is not one of K377's oldest opponents. It was KvK 14, the last cycle
  before the 2026-06-03 merge.
- The 8.8% leak for K236 to K417 in the baseline only measured KvK 14 and
  earlier.

## 6. KvK 19 pre-registered predictions, scored

| # | Prediction | Result |
|---|---|---|
| 1 | K377 opponent inside K236 to K417 | Hit, K273 |
| 2 | Not a KvK 16 to 18 opponent | Hit |
| 4 | Opponent from K236 to K309 (41.6% null) | Yes, K273 |
| 5 | At most 1 pairing leaves the group | **Miss**: 72 of 127 left, all to K116 to K234 |
| 6 | No rematches within cooldown | Hit, 0 |
| 7 | No byes | Hit, 0 of 182 |
| 8 | Crossing 309/310 near 48.5% if uniform | 47.3%, consistent with uniform |

Prediction 5 failed because the group was pooled with K116 to K235 that
cycle. The hit on prediction 1 was luck: under that pool, an inside
opponent had about a 60% chance.

## Next

1. **Pool rotation rule.** Work out from KvK 10 to 19 how units are combined
   each cycle: pool size targets, alternating neighbours, kingdom count
   balance, or random. This is now the biggest lever for the predictor.
2. **Power.** The top 100 power snapshot, if it is still coming, is the only
   way to tell power sorting apart from record sorting.
3. Republish the predictor after 1, with pool uncertainty shown.
