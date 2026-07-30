# Handoff: polyarb soak monitor dashboard

## Overview

A read-only operator dashboard for **polyarb**, the Rust Polymarket arbitrage scanner (Phase A, dry-run). Its job is to answer the one question the soak exists to answer: *is there enough real, fillable edge to justify building an execution engine?*

The dashboard is **strictly read-only monitoring**. There are no controls, no pause, no kill switch — the daemon cannot trade (no signing code, no wallet, `mode` locked to `dry-run`), so there is nothing to control. Do not add action buttons.

Four options are presented on one canvas so the operator can pick a direction:

| id | Name | Use |
| --- | --- | --- |
| `2a` | Evidence console | The default watch screen — airy, KPI + funnel + opportunity table + health rail |
| `2b` | Soak terminal | Maximum density, one screen, hairlines only — for a permanently open monitor |
| `2c` | Verdict-first | The weekly go/no-go read — the answer as a sentence, funnel as hero |
| `2d` | Break state | Full-page takeover for `ApiError::EmptyDiscovery` |

Only one of `2a`/`2b`/`2c` should ship as the dashboard. `2d` ships alongside whichever is chosen, as a state of the same page.

## About the design files

`Arb Bot Dashboard.dc.html` is a **design reference created in HTML** — a prototype showing intended look, structure and copy. It is not production code to copy. It is written as a streaming "Design Component" (a custom runtime with `{{ }}` template holes and a `renderVals()` logic class); that mechanism is an artifact of the design tool and should not be reproduced.

The task is to **recreate these designs in the target environment**, using its established patterns. There is no frontend in the polyarb repo today — it is a Rust binary crate that writes SQLite, JSONL logs and daily reports. Recommended approach, in order of preference:

1. **Server-rendered HTML from the existing Rust binary** (e.g. `axum` + `askama`/`maud`, one `/` route reading the same SQLite store the daemon writes). This keeps the deployment a single container, adds no JS build step, and matches the repo's "no credentials, no extra moving parts" posture. Live updates via a 2–5s poll or SSE from the existing dirty-event path.
2. A small SPA (React/Svelte) served by the same binary, if richer interaction is wanted later.

Whatever is chosen: money values must arrive as strings and be rendered as strings. The repo's enforced convention is **no `f64` in any money path** — do not parse Decimal strings into JS floats for display or sorting; sort server-side.

## Fidelity

**High fidelity.** Colors, typography, spacing and copy are final and should be reproduced exactly. Values below are the literal ones in the file.

The exception is data: all numbers are plausible placeholders. Wire every one to the real store.

## Design system

The design is built on **Nocturne** (bundled: `nocturne-styles.css`, `nocturne-readme.md`). The prototype writes Nocturne's token *values* inline rather than linking the stylesheet — that is a constraint of the design tool. **In the real implementation, link `styles.css` and use the CSS variables** (`var(--color-bg)`, `var(--space-4)`, `var(--radius-md)`, `var(--shadow-sm)`), never the raw hex/px below. The raw values are given so you can verify a match.

Nocturne rules that this design follows and you must keep:

- Dark, near-neutral blue-grey ground. Contrast from tonal ramps, not saturation.
- Accent (`#9184d9`) used as a **line, mark or glow — never a flood.** The single exception is `2c`'s stat band, which uses the deck/section ground `--color-section` (`#262a60`) deliberately, as presence at page scale.
- Buttons are **outlined** (1px accent border on transparent), never solid-filled.
- Left-aligned, flush-left, asymmetric. Dense on purpose (0.70× spacing scale).
- Headings never bolder than weight 500. Hierarchy is size and space.
- `:focus-visible { outline: 2px solid var(--color-accent); outline-offset: 2px; }` on every interactive element. Never the browser default.
- No pure black, no pure white.

### Tokens used

**Color**

| Role | Hex | Nocturne var | Where |
| --- | --- | --- | --- |
| Page ground | `#161826` | `--color-bg` | card background |
| Canvas behind cards | `#0e0f18` | — | design-canvas only, not part of the app |
| Surface | `#232532` | `--color-surface` | KPI cards, panels |
| Header bar | `#1a1c2b` | (neutral-900 area) | top bar, status strip, log block |
| Nav rail (2b) | `#131522` | — | left icon rail |
| Row hover | `#1c1e2c` | — | table row `:hover` |
| Text | `#e9e9ed` | `--color-text` | primary numbers/labels |
| Text secondary | `#cfd3e5` | `--color-neutral-300` | table cell text |
| Text muted | `#9397ab` | `--color-neutral-500` | labels, units |
| Text dim | `#75798c` | `--color-neutral-600` | notes, footnotes |
| Text dimmest | `#595d6c` / `#4a4d5c` | `--color-neutral-700` | log timestamps |
| Hairline | `rgba(233,233,237,.055)` | — | table row rules |
| Divider | `rgba(233,233,237,.10)` | `--color-divider` | header/column rules |
| Card edge | `#3f424d` | `--shadow-sm` | `0 0 0 1px #3f424d` |
| Accent | `#9184d9` | `--color-accent` | bars, marks, brand square |
| Accent light | `#b5abfc` | `--color-accent-400` | links, `negrisk-yes`, chart stroke |
| Accent mid | `#9690c9` | `--color-accent-2-500` | `negrisk-no` |
| Accent deep | `#5d5294` | `--color-accent-700` | dry-run badge border |
| Section ground | `#262a60` | `--color-section` | 2c stat band only |
| Section band text | `#a8adde` | — | labels on `#262a60` |
| Positive / survived | `#63c99a` | — | see note |
| Negative / vanished | `#e0706b` | — | see note |
| Break ground | `#2a1519` | — | 2d header + radial gradient |
| Break text muted | `#c08e8b` | — | 2d header meta |
| Break card edge | `#4d3a3d` | — | 2d primary stat card |

**Note on the two semantic colors.** Nocturne is a mono palette and ships no success/danger role. `#63c99a` and `#e0706b` were derived in OKLCH at the accent's own lightness and chroma, hue-rotated — so they sit at the same visual weight as `#9184d9`. Use exactly these two, only for verdicts and cost survival, and never as a large fill (they appear as text, 6px dots, 14–26px bars, and 14%-opacity tag backgrounds). If your codebase already has semantic tokens at matching weight, prefer those.

**Typography** — Inter throughout (`--font-heading` / `--font-body` are both Inter). Weights 400 and 500 only.

| Use | Spec |
| --- | --- |
| Break-state headline | 500 40px / 1.15 |
| 2c hero headline | 500 38px / 1.2 |
| Hero metric (2a) | 500 42px / 1 |
| Section-band metric (2c) | 500 28px / 1 |
| KPI metric | 500 26px / 1 |
| Body copy | 400 14px / 1.6 |
| Panel title | 500 13px / 1 |
| Brand wordmark | 500 13.5px / 1 |
| Table cell | 400 12.5px / 1.25 |
| Table cell (dense, 2b) | 400 11.5px / 1 |
| Meta / notes | 400 11.5px / 1 |
| Footnote | 400 11px / 1.4–1.5 |
| Log line | 400 10.5px / 1.4 |
| Eyebrow label (`.kpil`) | 500 10.5px, `letter-spacing:.09em`, uppercase, `#9397ab` |
| Section label (`.sec`) | 500 11px, `letter-spacing:.09em`, uppercase |
| Table header | 500 10.5px, `letter-spacing:.07em`, uppercase, `#75798c` |
| Table header (dense) | 500 9.5px, `letter-spacing:.08em`, uppercase, `#595d6c` |
| Tag / badge | 500 10–10.5px, `letter-spacing:.03em` |

**Every numeric cell gets `font-variant-numeric: tabular-nums`** (class `.num` in the prototype). This is not optional — columns must not jitter as values tick.

**Spacing** (Nocturne 0.70× scale): `2.8 / 5.6 / 8.4 / 11.2 / 16.8 / 22.4px`. Layout padding in the design: card gutter `22px` (2a), `12–14px` (2b), `26–40px` (2c/2d). Grid gaps: `12px` KPI row, `17–18px` column stack, `9–12px` funnel rows.

**Radius**: `4px` (`--radius-sm`) tags and bars · `8px` (`--radius-md`) cards and buttons · `99px` pills and dots.

**Elevation**: `--shadow-sm` = `0 0 0 1px #3f424d` — a hairline edge, no drop shadow. Panels use only this. Do not stack shadows.

## Canvas width

All four options are designed at **1440px** content width, matching the answer "desktop web app, wide (1440+)". Treat 1440 as the design width; below ~1280 the 2b three-column grid and the 2a KPI row will need to reflow (see Responsive).

---

## Screen: 2a — Evidence console (recommended default)

**Purpose.** The everyday watch screen. Answers, top to bottom: is the pipeline healthy, is edge surviving costs, and which specific opportunities did the detectors find.

**Layout.** Vertical stack: 52px header bar, then a two-column body `grid-template-columns: 1fr 336px`. The main column has `box-shadow: inset -1px 0 0 rgba(233,233,237,.08)` as its right divider (a hairline drawn by the column, not a border on the rail). Main column padding 22px, `display:flex; flex-direction:column; gap:17px`. Rail padding `22px 20px`, `gap:22px`.

### Header bar (52px)

`background:#1a1c2b`, `box-shadow: inset 0 -1px 0 rgba(233,233,237,.10)`, `padding: 0 22px`, `display:flex; align-items:center; gap:14px`.

Left to right:
1. 9×9px `#9184d9` square, `border-radius:2px` — brand mark.
2. `polyarb` — 500 13.5px.
3. `scanner · phase A` — 400 11px `#75798c`.
4. **Mode pill**: `DRY-RUN · LOCKED`, 500 10.5px `letter-spacing:.06em`, color `#b5abfc`, `padding:5px 10px`, `border-radius:99px`, `box-shadow: inset 0 0 0 1px #5d5294`. This pill is not decorative — it is the standing assurance that no order path is live. It must be present on every screen.
5. Spacer.
6. **Stream pill**: `border-radius:99px`, `padding:5px 10px`, `box-shadow: inset 0 0 0 1px rgba(99,201,154,.35)`; 6px `#63c99a` dot with `animation: dot 2s infinite` (`@keyframes dot{0%,100%{opacity:1}50%{opacity:.25}}`); label `STREAMING · 4 shards` 500 11px `#63c99a`. Label reflects real state: `STREAMING · n shards` / `REST FALLBACK` / `BLIND`.
7. Meta group, `display:flex; gap:20px`, 400 11.5px `#9397ab`, values in `#e9e9ed`: `universe 312 events` · `refresh 600s` · `uptime 4d 17h` · clock `HH:MM:SS`.

### Hero panel

`background:#232532; border-radius:8px; padding:18px 20px; box-shadow:0 0 0 1px #3f424d; display:flex; align-items:center; gap:28px`.

- **Left block** (`flex:1`): eyebrow `Soak day 5 of 7 — is there enough fillable edge to build execution?`; then a baseline-aligned row (`gap:12px`) of the metric `74` at 500 42px `#63c99a` and the qualifier `opportunities net-positive after spread, fee and depth — and still fillable on re-poll` at 400 12.5px/1.4 `#9397ab`, `max-width:330px`, `padding-bottom:4px`.
- 1px vertical divider, `align-self:stretch`, `rgba(233,233,237,.10)`.
- **Three stat blocks**, each: `.kpil` eyebrow, value 500 26px (`margin-top:9px`), sub-note 400 11px `#75798c` (`margin-top:7px`).
  - `Simulated net edge` / `$1,847` / `at walked size · maker basis`
  - `Median net maker` / `84 bps` (unit at 14px `#9397ab`) / `taker basis: 11 bps`
  - `Detect latency` / `384 ms` / `p50 · p95 610ms`

Copy discipline: "simulated" appears in the label, never dropped. This is dry-run evidence, not earnings.

### Funnel panel — "Where the gaps die"

`background:#232532; border-radius:8px; padding:17px 20px 19px; box-shadow:0 0 0 1px #3f424d`.

Header row, baseline aligned, `gap:12px`: title `Where the gaps die` (500 13px) + `last 24h · executable book sides only, never mid or last` (400 11.5px `#75798c`).

Rows (`margin-top:16px`, `gap:9px`), each `display:grid; grid-template-columns:210px 1fr 88px 62px; align-items:center; gap:14px`:

| Label | Count | % of detected | Note (2c only) |
| --- | --- | --- | --- |
| Gaps detected | 4,182 | 100% | Sum of executable asks below $1.00 across binary and NegRisk events. |
| Survive bid–ask spread | 1,024 | 24.5% | Gross gap computed on the sides you would lift, not mid — three quarters die here. |
| Survive taker fee curve | 386 | 9.2% | `fee = shares × rate × p × (1−p)`; worst exactly at 50/50, where gaps cluster. |
| Fillable at size ≥ $250 | 118 | 2.8% | Depth-walked VWAP still clears the net floor at a size worth having. |
| Still there on re-poll | 74 | 1.8% | `filled_simulated`. The 44 that had gone are recorded `vanished` and never revised. |

Bar track: `height:18px; border-radius:3px; background:rgba(233,233,237,.055); overflow:hidden`. Fill: `width: <pct>%` (floor of 1.2% so the last row is still visible), `border-radius:3px`, `background:#9184d9`; the final row uses `#63c99a`; the first row is `opacity:.42` (it's the 100% baseline, not an achievement). Count right-aligned 500 12.5px; percent right-aligned 400 11.5px `#75798c`.

Footnote (`margin-top:15px`, 400 11.5px/1.5 `#75798c`, `max-width:720px`, `text-wrap:pretty`): "The spread step is the big one, as the tick study predicted. Of what survives, **37.3%** was gone by the first re-poll and is recorded **vanished** — never revised in its favour." (`37.3%` in `#cfd3e5`, `vanished` in `#e0706b`.)

### Opportunity table

Title row: `Opportunities` (500 13px) + note `312 events · 11 live above the 1.5% net floor · dirty-event driven` (400 11.5px `#75798c`).

Grid (header and rows share it exactly): `1fr 104px 74px 74px 78px 78px 84px 96px`.
Header: `padding: 0 12px 7px`, `box-shadow: inset 0 -1px 0 rgba(233,233,237,.10)`, uppercase 500 10.5px `#75798c`, columns `Event · Detector · Gross · Spread · Net mk · Net tk · Size · Verdict` (all but the first two right-aligned).
Rows: `padding:9px 12px`, `box-shadow: inset 0 -1px 0 rgba(233,233,237,.055)`, `:hover { background:#1c1e2c }`.

Cells:
- **Event** — title, ellipsised, `padding-right:14px`; then ` · <category> <rate>` in `#595d6c`, where rate renders as `fee-free` when 0, else the decimal (`0.04`).
- **Detector** — `negrisk-yes` `#b5abfc` · `negrisk-no` `#9690c9` · `binary` `#9397ab`, 500 11px, lowercase, exactly as the Rust detector names.
- **Gross**, **Spread** — 400 12.5px `#9397ab`, percent to 2dp.
- **Net mk** — always `#63c99a`, 500, signed bps.
- **Net tk** — `#cfd3e5` when > 0, `#e0706b` when ≤ 0, 500, signed bps. Never merge with net maker; they are different economics.
- **Size** — `#cfd3e5`, the depth-walked size.
- **Verdict** tag — `padding:3px 7px; border-radius:4px; font:500 10px/1; letter-spacing:.03em`. `filled_simulated` renders as `filled·sim`, `#63c99a` on `rgba(99,201,154,.14)`; `vanished` `#e0706b` on `rgba(224,112,107,.14)`; `open` `#b5abfc` on `rgba(145,132,217,.16)`.

Placeholder rows (all nine, to copy verbatim while wiring):

```
NYC mayoral race — winner      nyc-mayor-2026    negrisk-yes  politics     0.04  2.41%  0.62%  +118  +31  $1,840  filled_simulated  true-arb
Fed decision — September       fed-sep-2026      negrisk-no   finance      0.04  1.88%  0.71%   +94  +12  $1,120  filled_simulated  true-arb
Israel–Lebanon ceasefire by Q4 isr-lbn-q4        negrisk-yes  geopolitics  0     1.44%  0.55%   +88  +88    $620  open              true-arb
US CPI print — August band     cpi-aug-band      negrisk-yes  economics    0.05  1.62%  0.94%   +68   −4    $740  vanished          true-arb
Premier League — title winner  epl-title-26      negrisk-no   sports       0.05  1.31%  0.88%   +43  −18    $410  vanished          rel-value
Next UK general election year  uk-ge-year        negrisk-yes  politics     0.04  1.19%  0.61%   +58   +6    $980  filled_simulated  true-arb
Best Picture 2027              oscars-bp-27      negrisk-yes  culture      0.05  1.08%  0.72%   +36  −21    $280  vanished          rel-value
Bitcoin above $180k by Dec 31  btc-180k-dec      binary       crypto       0.07  0.96%  0.51%   +45  −38    $310  vanished          rel-value
Government shutdown before Oct gov-shutdown-oct  binary       politics     0.04  0.88%  0.44%   +44   +2    $520  open              true-arb
```

Note the `true-arb` / `rel-value` label is carried in the data but **not yet shown** in 2a's table (2c's category panel and the log surface it). Adding it as a column or as a marker on the event name is a good next step — the repo is explicit that two prices disagreeing is not a locked profit, and the UI should not let the operator forget which is which.

### Right rail

Three stacked blocks, each led by a `.kpil` eyebrow.

**Pipeline** — rows `display:flex; align-items:center; gap:9px; padding:8px 0`, hairline `inset 0 -1px 0 rgba(233,233,237,.05)`, 400 11.5px/1.35. 6px round dot (`#63c99a` ok, `#b5abfc` warn), name `#cfd3e5` (`flex:1`), value `#9397ab` right-aligned.

```
ok    Stream shards        4/4 connected
ok    Books maintained     1,240 · 0 stale
ok    REST role            seed + slow sweep
ok    Universe refresh     312 events · 4m ago
warn  Fee model fallback   7 markets → table
ok    Store                sqlite · 18,402 rows
ok    Telegram             closed · 2 cooldown
```

Each row maps to a real invariant, and the wording matters:
- *Stream shards* — connected count from the sharded WS pool. Staleness comes from explicit invalidation or a shard disconnect, **never from silence**; a quiet socket is not a stale one, so never surface "no updates for Ns" as a fault here.
- *REST role* — `seed + slow sweep` normally; `FALLBACK ACTIVE` only when WS fails **while REST demonstrably works**. If both are down that's a network outage, not a fallback — show it as such.
- *Fee model fallback* — count of markets where the API stated nothing (or a formula this build doesn't implement) and the category table was used. This is a `warn` dot, not an error.
- *Telegram* — circuit breaker state (`closed`/`open`) plus the count of alerts held by the per-`(event, kind, side)` cooldown. Held messages are still logged with `delivery = "cooldown"`.

**By category · taker rate** — rows `grid-template-columns: 1fr 46px 40px 60px`, `gap:6px`, `padding:7px 0`, 400 11.5px/1.3. Name `#cfd3e5`, rate `#75798c`, count default, median net maker `#9397ab`.

```
politics     0.04  31  +96bps
geopolitics  0     14  +88bps
finance      0.04  12  +81bps
economics    0.05   9  +64bps
sports       0.05   6  +41bps
culture      0.05   2  +33bps
crypto       0.07   0  —
```

Rates are the verified ones (geopolitics 0; politics/finance/tech/mentions 0.04; sports/economics/culture/weather/other 0.05; crypto 0.07). Prefer the market's API-stated rate when present and fall back to this table — the UI should show which was used (that's the *Fee model fallback* row).

Footnote, 400 10.5px/1.5 `#75798c`: "crypto 5m/15m series open and resolve inside the 600s refresh — **0 opportunities** here means not looked at properly, not no edge." **This caveat is mandatory wherever a crypto zero appears.** A blank cell that reads as "no edge" is a wrong conclusion the dashboard would be causing.

**Log** — rows `display:flex; gap:8px; padding:6px 0`, hairline `.045`, 400 10.5px/1.4. Timestamp `#595d6c`, then `<kind>` 500 in its tone, `<subject>` `#cfd3e5`, `<detail>` `#75798c`.

Kind tones: `detect` `#e9e9ed` · `repoll` `#b5abfc` · `stream` `#9397ab` · `fees` `#9397ab` · `alert` `#9690c9` · `rest` `#75798c` · `store` `#75798c`.

```
detect  negrisk-yes        nyc-mayor-2026 net_maker +118bps @ $1,840
repoll  filled_simulated   fed-sep-2026 walked fills held 6.8s
stream  shard 2            resync 41 books after disconnect
fees    api-stated         politics 0.04 from feeSchedule
repoll  vanished           cpi-aug-band asks gone on first re-poll
fees    fallback           feeSchedule exponent 2 → category table
alert   cooldown           (epl-title-26, negrisk-no) held 300s
detect  binary             gov-shutdown-oct net_taker +2bps · logged not alerted
rest    sweep              312 events reconciled in 1.9s
stream  dirty              18 events re-evaluated in 41ms
detect  negrisk-no         epl-title-26 labelled rel-value, below floor
store   commit             74 opportunities · money as TEXT decimal
```

---

## Screen: 2b — Soak terminal

**Purpose.** A permanently-open monitor. Same information as 2a at roughly 2× density: no cards, no panel padding, hairlines only.

**Layout.** `display:flex`. Left: 46px icon rail. Right (`flex:1; min-width:0`): 34px status strip → 30px inline funnel strip → three-column body → category footer.

**Icon rail** — `width:46px; background:#131522; box-shadow: inset -1px 0 0 rgba(233,233,237,.08)`, `flex-direction:column; align-items:center; gap:15px; padding:14px 0`. 10px accent square, then four 26px icon buttons (Phosphor regular): `funnel` (active), `list-dashes`, `broadcast`, `file-text`. Active = `border-radius:5px; background:rgba(145,132,217,.16); color:#b5abfc`. Inactive `#75798c`, `:hover` `#e9e9ed`.

**Status strip** — `height:34px; padding:0 14px; background:#1a1c2b`, hairline below, 400 11.5px `#9397ab`, values `#e9e9ed`. First cell is the animated dot + `DRY-RUN` (500, `#e9e9ed`). Every subsequent cell is `padding:0 12px` with `box-shadow: inset 1px 0 0 rgba(233,233,237,.10)` as its left rule. Cells: `soak d5/7 · gaps 4,182 · surv 74 (#63c99a, 500) · netmk 84bps · nettk 11bps · p50 384 · p95 610 · ws 4/4 · rest seed · tg ok (#63c99a)`, spacer, then `HH:MM:SS UTC` in `#595d6c`.

**Inline funnel strip** — `height:30px; padding:0 14px`, hairline below, 400 11px. Leading uppercase label `funnel` (500 9.5px `#595d6c`, `padding-right:12px`), then one cell per stage with the same left-rule treatment: short name `#595d6c`, count `#e9e9ed` 500, percent `#75798c`. Short names: `detected · spread · fee · depth · held`.

**Body** — `grid-template-columns: 1fr 400px 340px`, each column `padding:12px 14px`, first two with the inset right hairline.

1. **Opportunities** — grid `1fr 74px 54px 54px 58px`, rows `height:23px`, 400 11.5px, hairline `.045`, `:hover #1c1e2c`. Columns: slug (`#cfd3e5`, ellipsised), kind (`#75798c` 10.5px), net mk, net tk, size (`#9397ab`). Uses the event **slug**, not the title — the operator knows the slugs.
2. **Re-poll verdicts** — grid `1fr 96px 84px`; columns slug, verdict tag (smaller: `padding:2px 6px; border-radius:3px; font:500 10px/1`), `held` duration `#75798c`. Ten rows; this column is the honest-fill-accounting view: how long the walked fills actually persisted.
3. **Log** — rows `height:22px`, 400 10.5px, no hairline, single-line ellipsis.

**Category footer** — `grid-template-columns: repeat(7,1fr)`, `box-shadow: inset 0 1px 0 rgba(233,233,237,.08)`; each cell `padding:11px 14px` with a left inset rule at `.06`. Uppercase 9.5px name + rate in `#595d6c`, then count 500 17px + median bps 400 10.5px `#9397ab`.

---

## Screen: 2c — Verdict-first

**Purpose.** The weekly go/no-go read — for the moment the operator decides whether to build Phase B. Not a live monitor.

**Layout.** Three bands: statement (`padding:34px 40px 30px`), saturated stat band, then `grid-template-columns: 1fr 460px` for funnel + categories.

**Statement band.** Row of mode pill + `Soak day 5 of 7 · 312 events · 4,182 gaps examined` (`.kpil`) + right-aligned clock. Then the headline at 500 38px/1.2, `max-width:1040px`, `text-wrap:pretty`:

> 74 of 4,182 detected gaps were still fillable after spread, fee and depth — **enough to justify an execution engine**, and almost all of it maker-side.

(the clause in `#63c99a`). Then 400 14px/1.6 `#9397ab`, `max-width:760px`:

> NegRisk rebalancing carries it, as the strategy report predicted. As a taker the same set nets 11 bps median and most of it is not worth the capital lockup; the case rests on maker capture, which means legging risk becomes the Phase B problem to solve.

The verdict clause and this paragraph are **generated from the data, not hardcoded** — if the evidence says the opposite, the sentence must say the opposite. Write both branches.

**Stat band.** `background:#262a60; padding:22px 40px; grid-template-columns: repeat(5,1fr); gap:26px`. Eyebrows `#a8adde`, values 500 28px `#e9e9ed`, notes 400 11px `#a8adde`.

```
Median net maker      84 bps    zero fee, no spread crossed
Median net taker      11 bps    fee curve peaks at 50/50
Still there on re-poll 62.7%    rest recorded vanished
Median walked size    $740      depth-constrained, retail
Detect p50 · p95      384 · 610 ms from book move
```

This is the one saturated field in the design. Do not add a second.

**Funnel column** (`padding:26px 40px 30px`, right hairline). Same five stages, taller treatment: per stage a baseline row of count (500 15px) + label (400 12px `#cfd3e5`) + right-aligned percent, then a 26px bar track, then the stage note at 400 11px/1.4 `#75798c`. Notes are in the funnel table above.

**Category column** (`padding:26px 32px 30px`). Grid `1fr 62px 52px 72px`, header `category · taker · opps · net mk`, rows `padding:9px 0` at 400 12px. Footnote: "Geopolitics is fee-free and therefore the only tier where taker-side survives at any scale. Crypto's zero is an instrumentation gap, not a finding."

---

## Screen: 2d — Break state (`ApiError::EmptyDiscovery`)

**Purpose.** The one failure the scanner must never hide: a discovery pass that parses fine and returns zero events looks exactly like a quiet market. Every clean number below it is meaningless, so the whole page is taken over.

Trigger this takeover for any condition where the scanner is **blind rather than idle**: `EmptyDiscovery` (as shown), WS *and* REST both unreachable (network outage — retry both forever, don't claim fallback), or all books past their staleness window. Do **not** trigger it for a quiet socket, a fee-model fallback, or an alert cooldown.

**Header.** Same 52px geometry, `background:#2a1519`, `box-shadow: inset 0 -1px 0 rgba(224,112,107,.45)`. Brand square `#e0706b`; `scanner · phase A` in `#c08e8b`. Pill is **filled** here — the one inversion in the design: `background:#e0706b`, text `#2a1519` 600 10.5px, label `BLIND`, with a `#2a1519` dot. Meta: `universe 0 events` (0 in `#e0706b`) · `last good 11m ago` · clock.

**Body.** `padding:40px 44px 44px; background: radial-gradient(120% 100% at 50% 0%, #2a1519 0%, #161826 70%)`.

1. Eyebrow row: Phosphor `warning-octagon` at 16px + `ApiError::EmptyDiscovery`, 500 11px `letter-spacing:.09em` uppercase `#e0706b`. Use the actual Rust error variant name.
2. Headline 500 40px/1.15, `max-width:960px`: "Discovery returned **0** events. The scanner is not quiet — it is blind, and every clean line below it is meaningless."
3. Body 400 14px/1.6 `#9397ab`, `max-width:740px`: "Three consecutive `/events/keyset` passes parsed successfully and yielded an empty `events[]`. Books are held from the last good pass and are past their staleness window; detection is suspended rather than run against stale depth. Soak evidence for this window will be excluded from the daily summary." (code spans in `#cfd3e5`)
4. Four stat cards, `gap:12px`, each `background:#232532; border-radius:8px; padding:14px 16px; min-width:190px`. The first carries the tinted edge `0 0 0 1px #4d3a3d`; the rest `#3f424d`.
   - `Events discovered` / `0` in `#e0706b` + ` of 312` at 13px `#9397ab`
   - `Endpoint` / `gamma /events/keyset` (500 15px/1.35) / `HTTP 200 · schema valid`
   - `Books held` / `0 fresh` / `1,240 past staleness`
   - `Alert` / `sent` / `telegram · 11m ago`
5. Log block: `background:#1a1c2b; border-radius:8px; padding:14px 16px; box-shadow:0 0 0 1px #3f424d`, lines at 400 11.5px/1.9. Timestamps `#595d6c`, source `#e9e9ed` (or `#e0706b` when the line is the error, `#9397ab` when incidental), detail `#9397ab`. Five lines showing the retry escalation to `detection suspended`.
6. Action row: outlined primary `Open discovery log` (`padding:9px 16px; border-radius:8px; font:500 12.5px; box-shadow: inset 0 0 0 1px #9184d9`, `:hover background:rgba(145,132,217,.14)`); ghost `Show last good scan` (`#9397ab`, `:hover #e9e9ed`); right-aligned note `read-only · nothing to pause, the daemon cannot trade`.

Both actions are navigation, not control. Keep the closing note — it's the reason there's no kill switch.

---

## Interactions & behavior

Deliberately minimal; this is a monitoring surface.

- **Row hover** — `background:#1c1e2c`, no transition needed (instant is correct for dense tables).
- **Icon rail hover** (2b) — `color:#75798c → #e9e9ed`.
- **Buttons** (2d) — outlined primary gains `background: rgba(145,132,217,.14)`; ghost lightens to `#e9e9ed`. Pressed state should step to `--color-accent-400` per Nocturne.
- **Focus** — `:focus-visible { outline: 2px solid var(--color-accent); outline-offset: 2px }` on every focusable element.
- **Status dot pulse** — `@keyframes dot { 0%,100% { opacity:1 } 50% { opacity:.25 } }`, `2s infinite` on the healthy stream dot, `1.2s` on the break-state dot. This is the only animation in the design. Respect `prefers-reduced-motion: reduce` and drop it.
- **Live updates** — the prototype re-derives five values on a 2.5s timer: clock, detect p50, survivor count, and the "n live" note. In production, push or poll and update **only** changed cells. Requirements: keep `tabular-nums` so nothing shifts; do not re-key or remount rows on update; no layout-affecting animation on tick. A one-shot background tint on a changed row (~400ms, `rgba(145,132,217,.14)` → transparent) is the right amount of "flash on update" — do not animate opacity or transform on the row itself, and cap the number of simultaneously animating elements.
- **No sorting/filtering** is designed. If added, sort server-side over Decimal strings.
- **Feed ordering** — opportunities newest-first; the log is newest-first.

## State

Read-only. Everything is derived from the store the daemon already writes.

| State | Source | Refresh |
| --- | --- | --- |
| `mode` | config (`dry-run`, validated) | once at load |
| soak day / uptime | daemon start time | 1/min |
| universe count, last refresh age | discovery pass | on pass (600s) |
| stream shards, book count, stale count, REST role | `ws` layer | 2–5s |
| funnel counts (5 stages) | aggregate over the window | 30s–1min |
| opportunity rows | `store` opportunities, net floor applied | on dirty-event / 2–5s |
| verdicts + held duration | re-poll lifecycle | on re-poll |
| category aggregates + rate source | `costs` + Gamma `MarketFees` | 1/min |
| fee-model fallback count | `FeeModel::resolve` | 1/min |
| Telegram breaker + cooldown count | `alert` | 2–5s |
| break condition | discovery / transport errors | immediate |

Two invariants to enforce in the UI layer:
1. **A verdict is never revised in the opportunity's favour.** `vanished` is terminal. Don't let a later poll flip it back to `filled_simulated`.
2. **Never show a total that mixes maker and taker basis.** They are separate columns, separate medians, separate stat blocks — everywhere.

Empty/edge states to design for: zero opportunities above the floor (say "none above the 1.5% net floor", not "no edge"); crypto zero (the caveat); first minutes after start (funnel window not yet full — label it); REST fallback active; alert breaker open.

## Assets

- **Fonts** — Inter 400/500/600 (600 is used only for the `BLIND` pill). Prototype loads it from Google Fonts; self-host in production.
- **Icons** — [Phosphor](https://phosphoricons.com), regular weight, per Nocturne. Used: `funnel`, `list-dashes`, `broadcast`, `file-text`, `chart-line`, `sliders-horizontal`, `warning-octagon`, `pulse`. Prototype loads the web font from unpkg; use inline SVG on `currentColor` in production.
- **No images.** No SVG illustration anywhere. All charting is bars and a polyline.

## Files

- `Arb Bot Dashboard.dc.html` — the design. One `<section id="t2">` holding four `.dv-opt` blocks: `#2a`, `#2b`, `#2c`, `#2d`. The `.dv-*` classes are canvas scaffolding for reviewing options side by side — **not part of the app**; ignore them and build from the `.dv-card` contents. Everything inside is inline-styled; the logic class at the bottom is placeholder data only.
- `nocturne-styles.css` — the Nocturne design system. Link this and use its variables.
- `nocturne-readme.md` — Nocturne's own usage guidance (component classes, do/don't).

## Open items

- Not yet designed: the opportunity drill-in (both legs' walked book), the daily report / summary view, and a maker-vs-taker distribution chart. Ask before inventing them.
- `true-arb` vs `rel-value` is in the data but not surfaced in 2a's table — worth adding.
- The designs were built from `CLAUDE.md`, `README.md` and the repo's Docker/Cargo files. `config/default.toml` and `src/` were not available, so knob names (`stream.reprobe_interval_secs` is the one exception, quoted from the docs) and the exact daily-summary fields should be checked against source before wiring.
