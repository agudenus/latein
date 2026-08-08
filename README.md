# polyarb — Polymarket arbitrage scanner

Finds arbitrage on [Polymarket](https://polymarket.com) and, crucially, works out whether
it was ever actually takeable.

Prediction-market prices are implied probabilities, so mutually exclusive outcomes that do
not price to $1.00 imply a risk-free profit. Spotting that sum is trivial and nearly
always wrong. Almost every apparent gap dies to the bid-ask spread, to Polymarket's taker
fee, or to an order book too thin to fill at any size worth having — and the survivors
often last a few seconds. polyarb is built around those frictions rather than around the
headline number:

- **Executable sides only.** Gaps are computed from the asks you would actually lift,
  never from mid or last price.
- **Depth-walked sizing.** A binary search walks every leg's book to find the largest size
  whose VWAP still clears the net floor, capped by a hard per-trade limit. The answer is a
  size, not just a spread.
- **Per-category fee model.** `fee = shares × rate × p × (1 − p)`, taker-only, with the
  verified rate for each category — geopolitics is fee-free, crypto is 0.07, and the curve
  peaks at 50/50, exactly where gaps cluster.
- **Taker and maker, reported separately.** Makers pay no fee and cross no spread, so a
  gap that dies as a taker can live as a maker — at the price of legging risk. Both
  figures are shown; neither is presented as the other.
- **True arbitrage vs. relative value, labelled.** Two prices disagreeing is not the same
  thing as a locked profit.
- **Honest fill accounting.** Every detected opportunity is re-polled after detection. If
  the walked fills are gone by the first re-poll it is recorded as `vanished`, and the
  verdict is never revised in its favour.
- **A read-only monitor.** `polyarb dashboard` serves the soak's evidence — the
  cost-survival funnel, the opportunity feed with maker and taker kept apart, and the
  pipeline's own health — as a web page. It is a separate process that opens the same
  SQLite read-only and has no control on it, because there is nothing to control.
- **Event-driven detection.** Books are maintained live from the CLOB WebSocket market
  channel and only the events whose books actually moved are re-evaluated, so detection
  happens tens of milliseconds after the market moves rather than on a five-second timer.
  REST is not retired: it seeds the books, re-fetches anything stale, sweeps the whole
  universe on a slow cadence as an integrity check, and takes over entirely if the socket
  cannot be reached.

Money is `Decimal` throughout — no `f64` ever touches a price or a size.

## Status: Phase A, dry-run

Milestones M1–M4 and M6 are complete: market ingestion, the detectors and cost model, the
continuous daemon with SQLite persistence, Telegram alerts and daily reports, deployment
packaging, and the streaming market-data layer.

Every wire detail of the WebSocket channel is inferred from the public docs and has never
met the real API — the build container cannot reach Polymarket at all. It is marked
`TODO(verify-live)` throughout `src/ws.rs` and `config/default.toml`, and it fails safe:
if the endpoint is unreachable or unusable the daemon logs loudly and reverts to the
REST polling loop. `stream.enabled = false` turns it off entirely.

**It cannot trade.** There is no signing code, no order placement, no wallet and no
private key anywhere in the repository. `mode` is locked to `dry-run` in config and the
daemon refuses to start on any other value. The purpose of Phase A is to collect a week of
evidence and answer one question: is there enough real, fillable edge to justify building
an execution engine at all?

Crypto is a staged second focus and is scanned like any other category, at its own higher
taker floor — but the fast-cycling BTC/ETH Up-or-Down 5m/15m series open and resolve inside
the 600 s universe-refresh interval, so most of them are never seen at all; a dedicated
fast poller for those series is a later crypto-engine milestone, and until then a `crypto:
0 opportunities` line in the daily summary means "not looked at properly", not "no edge".

Phase B — authentication, execution, a full risk manager, live mode behind an explicit
opt-in flag — is not built and will not begin without a deliberate go/no-go decision made
on that evidence.

## Quickstart

### Local

Needs Rust 1.94+. No credentials of any kind.

```bash
cargo build --release
cargo test                  # 207 tests

./target/release/polyarb markets      # discover the tracked universe, fetch one book batch
./target/release/polyarb scan         # run the detectors once, with the full cost breakdown
./target/release/polyarb scan --json  # the same, as JSON
./target/release/polyarb run          # the continuous dry-run daemon
./target/release/polyarb report       # today's summary
./target/release/polyarb dashboard    # the read-only web monitor on 127.0.0.1:8080
```

Configuration is `config/default.toml`, overridable per setting by environment variable —
see the comments at the top of that file. Runtime state lands in `data/`, `logs/` and
`reports/`, all gitignored.

### Docker

```bash
cp .env.example .env                # optional Telegram credentials; empty is fine
mkdir -p data logs reports && chown -R 10001:10001 data logs reports
docker compose up -d --build
docker compose logs -f
```

For a real deployment — provisioning a VPS, installing Docker, getting a Telegram chat id,
verifying it works, and the week-long soak — follow **[docs/deploy.md](docs/deploy.md)**.
It assumes no prior Docker experience.

## The dashboard

`polyarb dashboard` serves the soak monitor at `dashboard.bind` (default
`http://127.0.0.1:8080`). Run it **alongside** `polyarb run`, against the same database:

```bash
./target/release/polyarb run &        # the daemon writes
./target/release/polyarb dashboard    # the dashboard reads
```

It answers the question the soak exists to answer — *is there enough real, fillable edge
to justify building an execution engine?* — on one screen: the survivor count, the
"where the gaps die" funnel over the last 24 h, the newest opportunities with maker and
taker in separate columns, and a health rail (stream shards, REST role, fee-model
fallbacks, the Telegram breaker).

**It is read-only by construction, not by convention.**

- A **separate process** from the daemon. No shared memory, no channel, no lock.
- It opens the database with `SQLITE_OPEN_READ_ONLY` plus `PRAGMA query_only` (WAL allows
  a concurrent reader), and it never migrates: the writer owns the schema.
- Every route is a `GET` — `/`, `/api/state`, `/assets/*` — and a test enumerates them and
  refuses anything else. The page carries no button, no form and no input.
- There is nothing to control anyway: the binary has no signing code, no wallet and no
  order path, and `mode` is locked to `dry-run`. A pause button would pause nothing.

Two things it deliberately refuses to fake:

- **The break state.** When the scanner is *blind rather than idle* — discovery returning
  zero events, both transports down, every book past its staleness window, the daemon no
  longer publishing its status, or its scan loop alive but stalled — the whole page is
  taken over and says so. A quiet socket, a fee-model fallback, an alert cooldown and a
  merely *slow* loop are none of those things and do not trigger it.
- **The unmeasured funnel stage.** "Survive bid–ask spread" renders as *not instrumented*,
  because polyarb computes every gap on the executable side from the start and so has no
  pre-spread population to filter. A plausible number there would be an invented one.

The daemon publishes the state only it can see (shard counts, REST role, discovery health,
the alert breaker) to a single `runtime_status` row every ~5 s, which is what lets a
read-only reader tell a quiet market from a blind scanner. That row and the funnel
counters are schema v3: run `polyarb run` against a database once before pointing the
dashboard at it.

The row is written by a heartbeat task with its own timer, not by the scan loop, and it
carries the loop's own progress timestamp alongside. That separation is deliberate: a stale
row means the *process* is gone, while a fresh row with an old progress timestamp means the
process is alive and its loop is stuck — two failures a single timestamp used to conflate,
at the cost of a working daemon being reported as dead. A loop that finishes nothing for
`daemon.watchdog_stall_secs` is ended by the daemon's own watchdog (exit code 75) so a
restart policy can revive it.

The page has **no authentication** and shows a whole soak's evidence, so `bind` stays on
loopback. Reach a remote one over an SSH tunnel:

```bash
ssh -L 8080:127.0.0.1:8080 you@your-vps    # then open http://127.0.0.1:8080
```

Under Docker, `docker compose up -d` starts the dashboard alongside the daemon and
publishes it on the host's loopback only (see `docker-compose.yml`).

## Safety posture

- **Dry-run only.** No order path exists. Config validation rejects any mode but
  `dry-run`, and the daemon refuses to start.
- **No credentials required.** The scanner reads only public APIs. The one optional secret
  is a Telegram bot token, whose worst-case abuse is sending you messages.
- **Secrets live in the environment, never in a tracked file.** `.env` is gitignored *and*
  excluded from the Docker build context, so it never enters an image layer.
  `config/default.toml` has no credential fields, and a test asserts it never grows any.
- **No private key, ever, in code, config, logs or commits.** When Phase B needs one it
  will come from deployment secrets, for a dedicated hot wallet funded only with capital
  that can be lost.
- **The container runs unprivileged** (uid 10001), with the per-trade capital cap applied
  inside the sizing path.
- **The dashboard cannot write.** It is a separate read-only process on a loopback port
  with `GET` routes only — see [The dashboard](#the-dashboard).

## Layout

```
src/            the binary: config, types, gamma, clob, detect, costs, dryrun, alert,
                store, risk, http, ws
src/dashboard/  the read-only web monitor (state, render, embedded css/js assets)
config/         default.toml — no secrets, ever
tests/          integration tests over recorded order-book fixtures
research/       the strategy source of truth (below)
docs/deploy.md  VPS deployment and the soak plan
Dockerfile      multi-stage build; docker-compose.yml runs the soak
```

## Research

`research/` holds the analysis the design is built on, and is the source of truth for
strategy decisions:

- **`market-opportunity-and-strategy-report.md`** — the synthesis that set the MVP focus.
  Realised-arbitrage evidence puts NegRisk multi-outcome rebalancing at ~72% of platform
  profit and combinatorial arbitrage at ~0.24%, which is why this scanner prioritises the
  former and deprioritises the latter.
- **`execution-costs-and-arb-filtering.md`** — why spreads and the fee curve kill most
  detected gaps, and the verified per-category fee rates.
- **`arbitrage-analysis-polymarket-nba.md`** — single-market arbitrage is nearly extinct,
  and what survives is measured in seconds.
- **`polymarket-paradox-manipulation-whales.md`** — why cross-platform divergence is not
  automatically arbitrage, and why liquidity must come from book depth rather than
  reported volume.
- **`awesome-prediction-market-tools.md`** — the surrounding tool and API ecosystem.
- **`short-duration-crypto-bot-architecture.md`** — execution and risk patterns for a
  later phase.
