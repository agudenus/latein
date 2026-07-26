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

Money is `Decimal` throughout — no `f64` ever touches a price or a size.

## Status: Phase A, dry-run

Milestones M1–M4 are complete: market ingestion, the detectors and cost model, the
continuous daemon with SQLite persistence, Telegram alerts and daily reports, and
deployment packaging.

**It cannot trade.** There is no signing code, no order placement, no wallet and no
private key anywhere in the repository. `mode` is locked to `dry-run` in config and the
daemon refuses to start on any other value. The purpose of Phase A is to collect a week of
evidence and answer one question: is there enough real, fillable edge to justify building
an execution engine at all?

Phase B — authentication, execution, a full risk manager, live mode behind an explicit
opt-in flag — is not built and will not begin without a deliberate go/no-go decision made
on that evidence.

## Quickstart

### Local

Needs Rust 1.94+. No credentials of any kind.

```bash
cargo build --release
cargo test                  # 76 tests

./target/release/polyarb markets      # discover the tracked universe, fetch one book batch
./target/release/polyarb scan         # run the detectors once, with the full cost breakdown
./target/release/polyarb scan --json  # the same, as JSON
./target/release/polyarb run          # the continuous dry-run daemon
./target/release/polyarb report       # today's summary
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

## Layout

```
src/            the binary: config, types, gamma, clob, detect, costs, dryrun, alert,
                store, risk, http
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
