# Kingshot KvK Matchmaking Prediction

Reverse engineering how the mobile game **Kingshot** pairs kingdoms in its
**Kingdom of Power** (KvK) event, and building a tool that predicts who a given
kingdom will face next.

Project owner: **Alex**, plays **Kingdom 377**, which sits in group **K236 to K417**.

Context handed over from a Claude (Cowork) session on 2026-09-19. Everything below
is established, not speculation, unless explicitly flagged.

---

## 1. The model as it currently stands

Matchmaking runs in two stages. Stage 1 is solved. Stage 2 is the open problem.

### Stage 1: which group (SOLVED, authoritative)

Every kingdom belongs to a **Neighboring Kingdom Leaderboard group**, which is a
contiguous block of kingdom numbers. Because kingdom numbers are issued sequentially
by launch date, a number block is really a launch date cohort. Opponents are drawn
from inside the group.

This is not inferred. Century Games publishes the exact ranges in its **Kingdom
Progress Optimization** announcements, which are mirrored on the kingshotoptimizer
bulletin page. The phrase used verbatim is:

> "After the optimization, the Neighboring Kingdom Leaderboard group becomes 1427-1558."

### Stage 2: who inside the group (OPEN)

Inside a group, kingdom number carries almost no signal. Measured at the **11th
percentile** against random pairing via Monte Carlo on the sealed K1278 to K1331
group. What actually sorts it is **Matchmaking Points**.

Whiteout Survival (same studio, same event engine, better documented) states the
composition outright:

> "Matchmaking Points are calculated based on the Troop Power of the top 100 chiefs
> within each state, as well as considering factors such as state battle records and
> overall activeness."

So three inputs: top 100 player power, battle record, activity. Kingshot's own text
mentions only top 100 power plus generation. Generation is Stage 1, so inside a group
the sorter is power shaded by record. **No public source exposes any of it.** This is
the entire remaining gap.

### Constraints layered on top

| Rule | Value |
|---|---|
| Kingdom age gate | 70 days (Whiteout Survival uses 84) |
| Rematch cooldown | 3 or more KvKs |
| Cycle length | 28 days, matchmaking opens Saturday |
| Unmatched kingdom | receives a bye plus 100 Kingdom Coins |

Power manipulation does **not** work. Unequipping hero gear before matchmaking is
confirmed ineffective by Kingshot's own event text and repeatedly denied by the
Whiteout Survival developers. Treat as settled.

---

## 2. The official group map

| Effective | Merged blocks | Resulting group | Size |
|---|---|---|---|
| 2026-06-03 | 1-25 + 26-115 | **K1 to K115** | 115 |
| 2026-06-03 | 236-309 + 310-417 | **K236 to K417** | 182 |
| 2026-06-03 | 588-674 + 675-758 | **K588 to K758** | 171 |
| 2026-06-03 | 1087-1159 + 1160-1221 | **K1087 to K1221** | 135 |
| 2026-07-29 | 759-846 + 847-927 | **K759 to K927** | 169 |
| 2026-07-29 | 1222-1277 + 1278-1331 | **K1222 to K1331** | 110 |
| 2026-08-26 | 1332-1381 + 1382-1426 | **K1332 to K1426** | 95 |
| 2026-09-23 | 1427-1502 + 1503-1558 | **K1427 to K1558** | 132 |

Ranges with **no published grouping**: K116-235, K418-587, K928-1086, K1559+. The
predictor falls back to an estimated window for those.

Machine readable copy lives in `data/groups.json`.

### Groups merge on the KvK cycle

Effective dates run 3 Jun, 29 Jul, 26 Aug, 23 Sep. After the first gap that is
**28 days exactly**, the same period as the KvK cycle, with each merge landing between
two cycles. Announcements arrive 8 to 16 days ahead.

The frontier sweeps **upward through consecutive, non overlapping blocks**:
1222-1331, then 1332-1426, then 1427-1558. Older ranges get merged separately on
their own schedule.

**Live prediction under test:** next merge effective on or near **2026-10-21**,
covering roughly **K1559 to K1670**. A scheduled task checks this weekly (see §5).

---

## 3. Key evidence, with numbers

### Group walls are hard within a single boundary era

Leak rate is the share of a kingdom's recorded opponents falling outside its *current*
group. Computed by `scripts/analyze_groups.py`.

| Group | Kingdoms | Mean cycles played | Leak rate |
|---|---|---|---|
| K1332 to K1426 | 14 | 4.0 | **0.0%** |
| K1222 to K1331 | 42 | 4.6 | **5.2%** |
| K236 to K417 | 118 | 9.9 | **8.8%** |

Correlation between cycles played and personal leak rate: **r = +0.300**, n = 174.

The original sealed pool test: across 65 observed matchups inside K1278 to K1331,
**zero** crossed the boundary.

### IMPORTANT: a correction already made once

An earlier pass concluded that old cohorts "have no walls" and that pairing there
follows a smooth distance decay kernel. **That was wrong.** The walls exist, they
**move**. A kingdom with ten cycles of history accumulated opponents under several
different boundary layouts, so pooling its whole record smears the walls into a fake
smooth curve. Young cohorts whose entire history fits one boundary era leak zero.

Do not re-derive the distance decay model. It is an artifact. The fitted kernel is
kept in git history only as a record of the wrong turn.

This also explains a pattern visible in the data: nearly every kingdom has exactly one
opponent far outside its neighbourhood. That is its **first KvK**, played before its
group was carved out. A fossil of the previous layout, not noise.

### K377 specifically

Recorded opponents: **K348, K356, K368, K373, K375, K395, K407, K412, K423, K455**.

8 of 10 sit inside K236 to K417. The two that do not (K423, K455) are the oldest
entries. With no power data, the predictor gives 181 candidates at about 0.57% each,
which is honestly all that Stage 1 alone supports.

---

## 4. Data sources and their traps

### What works

**`https://kingshotoptimizer.com/kvk-rankings/bulletin/`** is server rendered and rich.
This is where the alignment announcements and dataset totals live. Fetch it with
WebFetch and it returns real content.

**`https://kingshotoptimizer.com/kingdom-timeline/<N>/`** is server rendered and exposes
a section reading "Kingdom N has played against these kingdoms in KvK" followed by a
list. This is where `data/opponents_baseline.csv` came from.

### What does not work

**The opponent lists are FROZEN.** As of 2026-09-16 every kingdom checked (K377, K1250,
K1300, K1310, K1332, K1500) returned byte identical output to 2026-08-15, including on
cache busted requests. The rest of the site refreshes normally: build stamp 2026-09-14,
rankings moved, server ages advanced. The opponent lists simply are not part of the
refresh. **They cover KvK 16 and earlier only.** KvK 17 and 18 pairings are unreachable
this way. A weekly scheduled task watches for this to unfreeze.

**ks-atlas.com and kingshot.net are JavaScript applications.** WebFetch sees only the
empty shell. ks-atlas has a private `/api/` path, disallowed in robots.txt.

**Reddit is blocked** by the web proxy entirely (HTTP 403 at the proxy).

**No official API exists for this data.** Century Games' gift code API
(`ks-giftcode.centurygame.com`) returns player nickname, kingdom ID and town centre
level only. No power, no aggregates, no matchups.

### Access etiquette, IMPORTANT

Both kingshotoptimizer and ks-atlas carry Cloudflare managed robots.txt blocks
including `User-agent: ClaudeBot / Disallow: /` and `Content-Signal: ai-train=no,
use=reference`. The signals are mixed, since each site's own trailing rule is
`User-agent: * / Allow: /` and `use=reference` plausibly covers reading pages for
analysis, but the ClaudeBot line points the other way.

**Position taken and communicated to Alex: no mass scraping.** The ~325 page pull that
produced the baseline happened before this was checked. Targeted reads of a handful of
pages to test a specific hypothesis are fine and proportionate. Bulk re-scraping is not.
The maintainer route is the correct path to a full dataset, and it is already in motion.

---

## 5. Live threads

### Atlas admin, data incoming

Alex contacted a **ks-atlas admin** who replied asking to confirm scope:

> "So you just need the full KvK History and the total power of the top 100 players of
> each kingdom?"

Alex confirmed yes, and specified that **each matchup must carry its KvK number or a
date**, since without cycle tags the shifting group boundaries smear together. That
exchange is the single highest value thread in the project. Expected format:

```
kvk,kingdom_a,kingdom_b        (plus prep/battle result if available)
kingdom,power,wins,losses      (current snapshot fine, per cycle better)
```

**When this data arrives, it unblocks everything.** See §7.

### Scheduled task

`trig_01Y6wxbN4gBL7NmRfxsa3BYd`, "Kingshot KvK bulletin check". Runs weekly, Mondays
06:00 UTC (08:00 Austria). Checks four things: new alignment announcements against the
K1559-1670 prediction, latest KvK number and dataset size, whether the frozen opponent
lists have unfrozen, and anything else notable. Push notifications on.

### Published artifact

The predictor is live at **https://claude.ai/artifact/CNKrSNyXMwdBZcTVJqbi8e**
(title "Kingdom Draw"). Source is `web/predictor.html`. To update it from a new session
you must pass that URL explicitly, and read the artifact first, or you will create a
duplicate.

Build step: `web/predictor_tpl.html` contains a `/*DATA*/` marker. Inline
`data/groups.json` as `const KVK_DATA = {...};` at that marker to produce
`web/predictor.html`.

### Bug report, unsent

A note to **kingshotoptimizer** (not ks-atlas) about the frozen opponent lists was
drafted but may still be unsent. It is a genuine bug report and tends to get a warmer
reception than a data request.

---

## 6. Dataset state as of 2026-09-16

| | Value |
|---|---|
| Latest KvK | **18**, run 2026-09-12 |
| Matchups in optimizer dataset | 10,503 |
| Kingdoms tracked | 2,263 |
| Oldest server | 567 days |
| Next cycle | KvK 19, matchmaking 2026-10-10 |
| Following | KvK 20, matchmaking 2026-11-07 |
| Top ranked | K924, unbeaten at 22-0 across 11 KvKs |

Local baseline: **186 kingdom histories, 1,158 distinct matchups**, covering blocks
K200-299, K300-429, K780-868, K1250-1349. Frozen at KvK 16.

---

## 7. What to do when the Atlas data arrives

This is the priority path. In order:

1. **Ingest and normalise** into `data/matchups.csv` keyed by `kvk, kingdom_a,
   kingdom_b` and `data/power.csv` keyed by `kingdom, power, wins, losses`.

2. **Rebuild the group timeline.** With cycle tags you can score each KvK against the
   boundaries *in force at that date* rather than current ones. Prediction: leak rate
   should collapse to near zero for **every** cohort, old and young alike. This is the
   decisive test of the whole model and it has never been run.

3. **Verify the K1222 to K1331 merge.** Kingdoms in K1278-1331 should start drawing
   opponents from K1222-1277 in KvK 17 onward, since the merge took effect 2026-07-29.
   Still untested because of the frozen data.

4. **Verify the rematch cooldown** directly. Currently only "no repeats observed" across
   387 mutually confirmed pairs, which is weaker than confirming the 3 cycle rule.

5. **Fit Stage 2.** Within each group and cycle, test whether pairing sorts by power,
   by Matchmaking Points including record, or is closer to uniform. Measure top 1 and
   top 5 accuracy by backtesting on held out cycles. This is the actual prize.

6. **Test the Swiss drift hypothesis.** If battle record feeds matchmaking, a kingdom's
   opponents should trend stronger after a win streak. Inferred from the Whiteout
   Survival mechanism, never stated by either game, never tested.

7. **Rebuild and republish** the artifact with real probabilities.

---

## 8. Conventions

- **Drafted messages for Alex to send contain no dashes and no semicolons.** He asked
  for this explicitly. Use plain commas and full stops. Applies to Discord messages and
  similar, not to code or internal docs.
- Alex sometimes wants messages in **German**. Offer it, do not assume it.
- He is in the **ks-atlas Discord**, not the kingshotoptimizer one. Do not send one
  project's bug report to the other's maintainers.
- Be straightforward and concise in drafted messages. Admins respond to bounded asks.

---

## 9. Files

```
data/opponents_baseline.csv   186 kingdom opponent histories, frozen at KvK 16
data/groups.json              official group map, dataset state, cycle timing
scripts/analyze_groups.py     leak rate analysis, reproduces the §3 tables
web/predictor.html            published artifact source, data already inlined
web/predictor_tpl.html        template with /*DATA*/ marker
docs/WHAT_CHANGED_SEP2026.md  the September update write up
```

Run the analysis with `python3 scripts/analyze_groups.py` from the project root.
Requires pandas and numpy.
