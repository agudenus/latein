# KvK Matchmaking — What Changed Since August

**Update pass, 16 September 2026.** Previous analysis: 15 August 2026.

---

## 1. Two more cycles completed

| | 15 Aug 2026 | 16 Sep 2026 |
|---|---|---|
| Latest KvK | 16 (18 Jul) | **18 (12 Sep)** |
| Matchups in dataset | 8,312 | **10,503** |
| Kingdoms tracked | 2,023 | **2,263** |
| Oldest server | 531 days | 567 days |

KvK 17 ran 15 Aug, KvK 18 ran 12 Sep. The 28-day cycle is holding exactly.
**KvK 19 matchmaking: 10 October 2026.**

Leaderboard also moved: K924 now 22-0 across 11 KvKs and still unbeaten. K240, second in
August, has dropped out of the top ten; K1085 and K1210 are new arrivals at 2nd and 3rd.

---

## 2. The mechanism has an official name

The single most useful thing learned this pass. What I had been calling a "matchmaking pool"
is a real, named game structure:

> **"After the optimization, the Neighboring Kingdom Leaderboard group becomes 1427-1558."**

These groups are created by Century Games' **Kingdom Progress Optimization** announcements,
which publish the exact kingdom-number ranges. Stage 1 of the model is therefore no longer
inferred from scraped matchups — it can be read directly off official announcements.

### The complete announced map

| Effective | Merged | Resulting group | Size |
|---|---|---|---|
| 2026-06-03 | 1–25 + 26–115 | **K1–115** | 115 |
| 2026-06-03 | 236–309 + 310–417 | **K236–417** | 182 |
| 2026-06-03 | 588–674 + 675–758 | **K588–758** | 171 |
| 2026-06-03 | 1087–1159 + 1160–1221 | **K1087–1221** | 135 |
| 2026-07-29 | 759–846 + 847–927 | **K759–927** | 169 |
| 2026-07-29 | 1222–1277 + 1278–1331 | **K1222–1331** | 110 |
| 2026-08-26 | 1332–1381 + 1382–1426 | **K1332–1426** | 95 |
| 2026-09-23 | 1427–1502 + 1503–1558 | **K1427–1558** | 132 |

**K377 sits in K236–417** — 182 kingdoms, merged 3 June 2026.

---

## 3. Groups merge on the KvK cycle, and the frontier marches upward

Effective dates: 3 Jun → 29 Jul → 26 Aug → 23 Sep. After the first gap, that is **28 days
exactly** — the same period as the KvK cycle. Each merge lands in the gap between two cycles.

The ranges are consecutive and non-overlapping: 1222–1331, then 1332–1426, then 1427–1558.
A merge frontier is sweeping upward through the kingdom numbers as each new cohort ages in,
while older ranges get merged separately.

**Forward prediction:** next merge effective **21 October 2026**, covering roughly
**K1559–K1670** (group sizes have run 95–132). Announced ~2 weeks prior, so watch early
October. This is falsifiable and worth checking.

---

## 4. This corrects my August conclusion about old kingdoms

In August I concluded that old cohorts "have no walls" and that pairing there follows a
distance-decay kernel. That was the wrong interpretation of a real observation.

The walls exist. They **move**. A kingdom that has played ten cycles accumulated its opponents
under several different boundary layouts, so pooling its whole history smears the boundaries
into an apparent smooth decay.

### The test

Leak rate = share of a kingdom's recorded opponents falling outside its *current* group.

| Group | Kingdoms | Mean cycles played | Leak rate |
|---|---|---|---|
| K1332–1426 | 14 | 4.0 | **0.0%** |
| K1222–1331 | 42 | 4.6 | **5.2%** |
| K236–417 | 118 | 9.9 | **8.8%** |

Correlation between cycles played and personal leak rate: **r = +0.300** (n=174).

Young cohorts whose entire history fits inside one boundary era leak **zero**. The more history
a kingdom has, the more of it predates the current walls. The boundary is hard in any given
cycle — the leak is archaeology, not permeability.

This also explains the odd pattern from August where nearly every kingdom had exactly one
opponent far outside its neighbourhood. That is its earliest KvK, played before its group was
carved out. Not noise, not an artifact — a fossil of the previous boundary layout.

**K377 specifically:** 8 of 10 recorded opponents sit inside K236–417. The two that don't
(K423, K455) are the oldest entries on the record.

---

## 5. A data problem that blocks verification

I could not test the KvK 17/18 pairings, because **the source data is frozen**.

`kingshotoptimizer.com/kingdom-timeline/<N>/` opponent lists are byte-identical to what I
collected on 15 August — for every kingdom checked (K377, K1250, K1300, K1310, K1332, K1500),
including a cache-busted request.

This is not a stale site. It rebuilt **2026-09-14 01:21 UTC**, its ranking table has moved, and
its kingdom count and server ages have advanced. The per-kingdom opponent lists are simply not
part of that refresh. They cover **KvK ≤ 16 only**.

Consequence: my August prediction that K1222–1277 kingdoms would start drawing opponents from
K1278–1331 in KvK 17 **remains untested**. The merge is confirmed as an announcement; its
effect on pairings is not yet visible in any data I can reach.

---

## 6. Where the model stands

**Stage 1 (which group)** — solved and authoritative for the eight announced ranges. This is
the bulk of the constraint and it now comes from the publisher rather than from inference.

**Stage 2 (who inside the group)** — unchanged and still the gap. Kingdom number carries almost
no signal inside a group (11th percentile vs random). Matchmaking Points — top-100 power shaded
by battle record — is what sorts it, and no public source exposes it.

For K377 that means 181 candidates at roughly 0.57% each. Honest, and not much use without
power figures.

**Unannounced ranges** — K116–235, K418–587, K928–1086, K1559+ have no published grouping, so
predictions there fall back to an estimated window.
