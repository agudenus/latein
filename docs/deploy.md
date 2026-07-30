# Deploying polyarb for the dry-run soak

This guide takes you from nothing to a `polyarb` scanner running unattended on a cheap
server, 24/7, for at least a week. It assumes no prior Docker or Linux experience — every
command is meant to be copied and pasted in order.

**What you are deploying.** A read-only scanner. It reads Polymarket's public market and
order-book APIs, works out where an arbitrage would exist after spread, fees and depth,
writes each one down, and simulates whether it could actually have been filled. It has no
wallet, holds no key, and the code path that would place an order does not exist yet. The
`mode` setting is locked to `dry-run` and the daemon **refuses to start** if it is
anything else. Nothing in this guide can lose you money.

**What you get out of it.** A `reports/summary-YYYY-MM-DD.md` file every day, and enough
evidence after a week to make one decision: is there enough real, fillable edge here to
justify building the execution engine (Phase B)? The last section of this document tells
you exactly what to look at.

---

## Contents

1. [Provision a server](#1-provision-a-server)
2. [Add swap](#2-add-swap-do-not-skip-this-on-a-1-gb-box)
3. [Install Docker](#3-install-docker)
4. [Get the code](#4-get-the-code)
5. [Create your `.env`](#5-create-your-env)
6. [Prepare the data directories](#6-prepare-the-data-directories)
7. [Start it](#7-start-it)
8. [Verify it is actually working](#8-verify-it-is-actually-working)
9. [Day-to-day operation](#9-day-to-day-operation)
10. [Updating](#10-updating)
11. [Stopping](#11-stopping)
12. [Troubleshooting](#12-troubleshooting)
13. [The soak plan and the go/no-go review](#13-the-soak-plan-and-the-gono-go-review)

---

## 1. Provision a server

Any small Linux VPS works. The scanner is one process doing a few HTTP requests every
five seconds; it is not demanding.

| Provider | Plan | Cost |
|---|---|---|
| Hetzner Cloud | CX22 (2 vCPU, 4 GB) | ~€4/month |
| DigitalOcean | Basic droplet (1 vCPU, 2 GB) | ~$12/month |
| Any | 1 vCPU, 1 GB | fine to *run*, see step 2 |

- **Image**: Ubuntu 24.04 LTS. Everything below assumes it.
- **Region**: anywhere. Latency does not matter in Phase A — the scanner polls every five
  seconds and is only measuring, not racing. (It *will* matter in Phase B; that is a
  later decision.)
- **Sizing**: 1 GB of RAM is enough to run the scanner but is marginal for *compiling*
  it, because the release build uses link-time optimisation and briefly needs more than a
  gigabyte. If you picked a 1 GB box, step 2 is mandatory. If you picked 2 GB or more,
  step 2 is still cheap insurance.

Log in as root with the SSH key you gave the provider:

```bash
ssh root@YOUR_SERVER_IP
```

Bring the system up to date:

```bash
apt-get update && apt-get upgrade -y
```

---

## 2. Add swap (do not skip this on a 1 GB box)

Compiling Rust with LTO can exhaust a small server's memory, and the symptom is
confusing: the build dies with `signal: 9, SIGKILL` or the SSH session freezes. Two
gigabytes of swap prevents it.

```bash
fallocate -l 2G /swapfile
chmod 600 /swapfile
mkswap /swapfile
swapon /swapfile
echo '/swapfile none swap sw 0 0' >> /etc/fstab
```

Check it took:

```bash
free -h
```

You should see a non-zero `Swap` row.

---

## 3. Install Docker

The official installer script sets up both Docker and the `compose` plugin:

```bash
curl -fsSL https://get.docker.com | sh
```

Confirm both are present:

```bash
docker --version
docker compose version
```

Both must print a version. If `docker compose version` fails, you have the old standalone
`docker-compose` instead — install the plugin with
`apt-get install -y docker-compose-plugin` and try again.

> Note: this guide uses `docker compose` (a space). The older `docker-compose` (a hyphen)
> is a different, deprecated program and some of the options here behave differently
> under it.

---

## 4. Get the code

```bash
cd /opt
git clone https://github.com/YOUR_GITHUB_USER/latein.git polyarb
cd polyarb
```

Replace `YOUR_GITHUB_USER` with your own account. If the repository is private, GitHub
will ask for credentials — use a personal access token as the password, or set up a
deploy key.

**Which branch.** The Phase A work lives on `claude/codebase-analysis-eenv7n` until it is
merged. Check what you got and switch if needed:

```bash
git branch --show-current
git checkout claude/codebase-analysis-eenv7n   # only if you are not already on it
```

Once that branch is merged to `main`, use `main` and ignore the checkout line above.

Sanity-check that you are in the right place — all four of these must exist:

```bash
ls Dockerfile docker-compose.yml config/default.toml .env.example
```

---

## 5. Create your `.env`

```bash
cp .env.example .env
```

`.env` is listed in `.gitignore`, so it can never be committed by accident. It is also
excluded from the Docker build context by `.dockerignore`, so it never enters an image
layer either. It reaches the container only as environment variables at run time.

**You can leave it empty.** With no Telegram credentials the scanner still runs, still
detects, still stores and still writes daily reports — alerts simply go to
`logs/events.jsonl` instead of your phone. If you would rather set Telegram up now:

**a) Create a bot and get the token.** In Telegram, message
[@BotFather](https://t.me/BotFather), send `/newbot`, and follow the prompts. It replies
with a token that looks like `123456789:AAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`.

**b) Get your chat id.** Send any message (e.g. "hello") to your new bot *from your own
Telegram account* — the bot cannot message you first. Then, on your laptop or the server,
run this with your token substituted in:

```bash
curl -s "https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates"
```

In the JSON reply, find `"chat":{"id":123456789,` — that number is your chat id. It is
negative for groups (e.g. `-1001234567890`); include the minus sign.

If `getUpdates` returns `{"ok":true,"result":[]}`, you have not actually sent the bot a
message yet. Send one and retry.

**c) Fill them in.**

```bash
nano .env
```

```
TELEGRAM_BOT_TOKEN=123456789:AAxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
TELEGRAM_CHAT_ID=123456789
```

Save with `Ctrl+O`, `Enter`, then exit with `Ctrl+X`.

Lock the file down, since it now holds a credential:

```bash
chmod 600 .env
```

> The bot token is the only secret in Phase A, and it can do nothing worse than send you
> messages. **Never** put a wallet private key in this file, or in any file. When Phase B
> needs one it will come from deployment secrets, and it will belong to a dedicated hot
> wallet holding only money you can afford to lose.

---

## 6. Prepare the data directories

The container runs as an unprivileged user with **uid 10001**, not root. The three
directories it writes to are shared with the host so you can read reports directly — and
if Docker creates them for you they will be owned by root and the scanner will die on
startup with `Permission denied`.

Create them yourself, with the right owner, once:

```bash
mkdir -p data logs reports
chown -R 10001:10001 data logs reports
```

You never have to do this again unless you delete the directories.

---

## 7. Start it

```bash
docker compose up -d --build
```

The first run compiles the scanner from source inside the container. **Expect 3–8 minutes
on a small VPS** and a lot of `Compiling ...` output. Subsequent rebuilds are much faster
because the dependency layer is cached.

`-d` means "detached" — it keeps running after you close your SSH session.

---

## 8. Verify it is actually working

Run these four checks in order. Do not walk away until all four pass.

**a) The container is up.**

```bash
docker compose ps
```

`STATUS` should read `Up ... (health: starting)` and, within about five minutes,
`Up ... (healthy)`.

**b) The startup log looks right.**

```bash
docker compose logs --tail 50
```

You are looking for, in order:

```
starting polyarb dry-run daemon (no orders are ever placed)   mode=dry-run ...
market discovery drop breakdown   events_seen=... markets_seen=... markets_dropped=... drop_reasons="closed_or_inactive=... no_token_ids=..."
market universe refreshed   events=... markets=... negrisk_events=... partial_negrisk_events=... markets_seen=... dropped_markets=... drop_reasons=... truncated=false
scan cycle complete   books=... markets=... opportunities=... new=... duration_ms=...
```

Five things matter in those lines:

- `mode=dry-run` — confirms the safety lock.
- `negrisk_events` is **greater than zero**. NegRisk multi-outcome events are the primary
  strategy; if this is `0`, discovery is broken (see troubleshooting).
- `truncated=false`. `true` (and an accompanying `WARN universe truncated at max_events`)
  means pagination stopped at the `scan.max_events` backstop instead of at the end of the
  event list, so the universe is incomplete — raise `scan.max_events`.
- `drop_reasons` accounts for every dropped market by bucket. `markets_kept + dropped =
  markets_seen` always balances, so a surprising bucket is the thing to chase.
- `books` on the scan line is close to `markets`. A large shortfall means order books are
  not being fetched.

`partial_negrisk_events` counts NegRisk events whose tracked outcomes are a strict subset
of what Gamma listed. Their sweeps are **suppressed** (they are not risk-free — a dropped
outcome can win and pay every leg nothing), so a large number here means real opportunity
is being skipped and the drop reasons are worth fixing. See
`scan.report_partial_negrisk` in `config/default.toml`.

`opportunities=0` on most cycles is **normal and expected** — genuine arbitrage is rare.
That is precisely what the soak is measuring.

**c) The database exists and is growing.**

```bash
ls -la data/
```

You should see `polyarb.sqlite`, plus `polyarb.sqlite-wal` and `polyarb.sqlite-shm`. Run
it again a minute later; the `-wal` file's timestamp must advance. That heartbeat is
exactly what the healthcheck watches.

**d) The report pipeline works** — do not wait until tomorrow to find out.

```bash
docker compose exec polyarb polyarb report
```

This prints today's summary (mostly empty if you just started), writes it to
`reports/summary-<today>.md`, **and pushes it through the alert path** — so if you
configured Telegram, this doubles as a live test of your token and chat id. If the
message arrives on your phone, alerting is correctly plumbed.

> The command really does say `polyarb` twice. The first names the compose *service*, the
> second is the *program*. `docker compose exec` runs a program directly and does not
> apply the image's entrypoint, so the program name has to be repeated.

---

## 9. Day-to-day operation

**Watch it live** (`Ctrl+C` stops watching, not the scanner):

```bash
docker compose logs -f
```

**The dashboard.** `docker compose up -d` starts a second container, `polyarb-dashboard`,
running the same image with `command: ["dashboard"]`. It opens the same database
**read-only** and serves the soak monitor — the survivor count, the "where the gaps die"
funnel, the opportunity feed and the pipeline health rail — on the VPS's own loopback,
port 8080. It has no controls, and there is nothing to control: the scanner has no order
path.

It is **not** exposed to the internet, and should not be: the page has no authentication
and shows the whole evidence trail. Reach it by tunnelling from your own machine:

```bash
ssh -L 8080:127.0.0.1:8080 you@your-vps    # leave this running
```

Then open <http://127.0.0.1:8080> in your browser. If the page takes itself over with a
red header, read what it says — it means the scanner is blind (discovery returning zero
events, both transports down, or the daemon no longer publishing its status), and every
clean number underneath would have been meaningless.

```bash
docker compose logs -f dashboard      # just the dashboard's own log
docker compose restart dashboard      # safe: it cannot affect a running soak
```

**Reports.** One file per UTC day appears in `reports/`, written automatically at **23:55
UTC** (`daemon.daily_summary_utc` in `config/default.toml`):

```bash
ls -la reports/
cat reports/summary-2026-07-27.md
```

If Telegram is configured, the same summary is sent to you when it is generated.

**A report on demand**, at any moment, without waiting for 23:55:

```bash
docker compose exec polyarb polyarb report              # today, also alerts it
docker compose exec polyarb polyarb report --no-send    # today, writes only
docker compose exec polyarb polyarb report --date 2026-07-27
```

**The raw event log** — every detected opportunity as one JSON object per line, whether or
not it cleared the alert threshold:

```bash
tail -f logs/events.jsonl
wc -l logs/events.jsonl

# Why each one did or did not reach the phone (M6.1 adds "cooldown"):
grep '"event":"opportunity"' logs/events.jsonl \
  | python3 -c 'import json,sys,collections;print(collections.Counter(json.loads(l)["delivery"] for l in sys.stdin))'
```

`delivery = "cooldown"` means the same `(event, kind, side)` construction was already
alerted inside `alerts.per_event_cooldown_secs` and had not improved by
`alerts.realert_improvement` per share. The row and this log line exist either way — only
the message was held back. A large cooldown count is the intended state for a slow-moving
market, not a fault.

**A one-off scan**, printed to the terminal with the full cost breakdown, without
disturbing the running daemon:

```bash
docker compose exec polyarb polyarb scan
```

**Health at a glance:**

```bash
docker compose ps
docker stats --no-stream polyarb
```

---

## 10. Updating

```bash
cd /opt/polyarb
git pull
docker compose up -d --build
```

Your `data/`, `logs/`, `reports/` and `.env` are untouched — they live on the host, not in
the image. The soak's history is preserved across updates.

Then re-run the checks from [step 8](#8-verify-it-is-actually-working). If the update
brought a new config option, `git pull` updates `config/default.toml` and the rebuild
bakes it in automatically.

---

## 11. Stopping

```bash
docker compose stop     # pause; `docker compose start` resumes, data intact
docker compose down     # stop and remove the container; data on the host survives
docker compose restart  # bounce it
```

`docker compose down` is safe: it removes the container, not your `data/`, `logs/` or
`reports/` directories.

To reclaim disk from old images after several rebuilds:

```bash
docker image prune -f
```

---

## 12. Troubleshooting

**`Permission denied` / `could not open data/polyarb.sqlite` on startup.**
The host directories are not owned by uid 10001. Fix and restart:

```bash
chown -R 10001:10001 data logs reports
docker compose restart
```

**The build is killed, or the server freezes while building.**
Out of memory. Go back and do [step 2](#2-add-swap-do-not-skip-this-on-a-1-gb-box).

**`docker compose ps` shows `(unhealthy)`.**
The scan loop has stopped writing. Look at the logs:

```bash
docker compose logs --tail 100
```

Almost always this is a network problem reaching Polymarket, or a crash. The container
restarts itself; if it restart-loops, the logs will say why.

**`env file .env not found`.**
You skipped [step 5](#5-create-your-env). `cp .env.example .env`.

**Telegram alerts never arrive.**
Run `docker compose exec polyarb polyarb report` and watch the logs for an alert failure.
Common causes: you never sent the bot a first message, the chat id is missing its minus
sign (groups), or the token was pasted with a trailing space. After several consecutive
failures the alerter opens a circuit breaker and stops trying for 15 minutes — so fix the
credentials, then `docker compose restart`.

**`negrisk_events=0` in the logs.**
Market discovery is not recognising multi-outcome events. This is the single most
important assumption in the codebase to verify against the live API — see the next
section.

**No opportunities at all after several days.**
That is a *result*, not necessarily a fault — but confirm the scanner is healthy first
(`books` close to `markets`, `errors` at zero in the daily report). A healthy scanner
finding nothing is a real and decision-relevant finding.

---

## 13. The soak plan and the go/no-go review

### The plan

Run the scanner **untouched for at least seven full UTC days**. Not six: prediction market
activity is strongly weekday/weekend and event-cycle dependent, and a partial week will
mislead you. Resist the urge to tune thresholds mid-soak — changing `scan.floors` or the
alert thresholds halfway through makes the week's statistics incomparable with themselves.
Write down anything you want to change and do it after the review.

The only thing worth doing during the week is a daily glance at
`docker compose ps` to confirm it still says `healthy`.

At the end, read all seven `reports/summary-*.md` files together.

### Part A — did the scanner actually work?

Answer this **before** looking at any profit number. A broken scanner produces a
confident-looking report full of zeros.

From the **`## Scan loop`** section of each report:

- [ ] **`cycles`** ≈ 17,280/day (86,400 seconds ÷ the 5-second interval). Materially fewer
      means downtime or a loop that fell behind. **With `stream.enabled = true` (the
      default) this number is no longer a clock**: it counts every detection pass, and a
      stream-triggered pass happens whenever books move, so expect *more* than 17,280 on a
      busy day and far fewer on a quiet one. Under streaming, read `cycles` together with
      the `## Detection latency (stream)` section, not on its own.
- [ ] **`failed`** is at or near zero. A meaningful count means the Polymarket APIs were
      rejecting or timing out, and everything downstream is under-counted.
- [ ] **`cycle duration: mean`** is comfortably under 5,000 ms. If the mean approaches the
      scan interval, the loop is saturated and REST polling has hit its ceiling — that on
      its own is an argument that Phase B needs the WebSocket feed.
- [ ] **`markets scanned (peak)`** and **`books fetched (peak)`** are close to each other
      and stable day to day. A gap means books are being requested but not returned.

### Part B — did the live-API assumptions hold?

Eight modules carry `TODO(verify-live)` markers: shapes inferred from Polymarket's public
docs that had never met the real API, because the development container could not reach
it. The soak is their first contact with reality. Each of these checks maps to one
assumption; run them on the server.

```bash
# 1. JSON shape drift — the single most important check. Any hit means a response no
#    longer parses, and whole categories of market may be silently invisible.
docker compose logs --no-color | grep -c "could not parse the response from"

# 2. HTTP-level rejections. A cluster of 429s means the rate limiter is too aggressive;
#    404/400 means an endpoint moved.
docker compose logs --no-color | grep -o "returned HTTP [0-9]*" | sort | uniq -c | sort -rn

# 3. Discovery health over time — negrisk_events must be > 0, and dropped_markets should
#    be a small fraction of markets, not most of them.
docker compose logs --no-color | grep "market universe refreshed" | tail -20

# 4. Book coverage — `books` should track `markets` closely on every cycle.
docker compose logs --no-color | grep "scan cycle complete" | tail -20

# 5. Anything that failed and retried.
docker compose logs --no-color | grep -E "request failed, retrying|book fetch failed|universe refresh failed" | wc -l
```

Interpreting them:

- [ ] **Check 1 returns 0.** Non-zero → `src/http.rs` deserialisation is failing;
      the Gamma or CLOB response shape has drifted from what `src/gamma.rs` /
      `src/clob.rs` expect. This must be fixed before any number in the report is
      trustworthy.
- [ ] **Check 3 shows `negrisk_events` > 0.** Zero → the `negRisk` field name or casing on
      `/events` is wrong (`RawEvent::neg_risk` in `src/gamma.rs`). Since NegRisk rebalancing is ~72% of realised
      arbitrage platform-wide, a zero here invalidates the *entire* soak, not part of it.
- [ ] **Check 3 shows `dropped_markets` small relative to `markets_seen`, and the
      `drop_reasons` breakdown is plausible.** The buckets say exactly where markets went:
      `no_token_ids` points at the `clobTokenIds` double-encoding assumption,
      `no_order_book` at the `enableOrderBook` filter, `not_binary` at a multi-token
      market shape we do not model, `event_dropped` at whole events failing the
      active/closed re-check. A large `no_token_ids` or `no_order_book` share is a wire-shape
      bug, not a fact about the market.
- [ ] **Check 3 shows `truncated=false`.** `true` means `scan.max_events` — not the API —
      ended discovery, so the universe is a prefix of reality and NegRisk events may be
      split across the boundary. Raise `scan.max_events` and re-soak.
- [ ] **Check 4 shows `books` ≈ `markets`.** A big shortfall points at the batch book
      endpoint's `asset_id` key assumption (`src/clob.rs:30`).
- [ ] **Categories in the report's opportunity table are not overwhelmingly `other`.** The
      category→fee-tier alias table (`Category::from_text` in `src/types.rs`) is inferred from Polymarket's
      public taxonomy. If everything lands in `other`, opportunities are being costed at
      the 0.05 fallback rate — which would understate fee-free geopolitics and understate
      crypto's 0.07, distorting every net figure.

#### The WebSocket stream (M6) — the least verified thing in the build

Every wire detail of the CLOB market channel was **guessed** from the public docs: the
container could not reach it at all, so unlike the REST shapes it has never even been
seen. It ships on by default because it is what takes detection latency from seconds to
milliseconds, and it fails safe — an unreachable or unusable channel becomes REST polling,
which is exactly the M3 daemon. Check it explicitly:

```bash
# 6. Did it ever connect? One line per shard per connection.
docker compose logs --no-color | grep -c "market stream connected"

# 7. Did it hand detection back to REST? (Only happens when REST works and WS does not.)
docker compose logs --no-color | grep "handing \\|answered a re-probe"

# 8. Frame health at shutdown (snapshots/deltas/unknown/malformed/out_of_order/divergences).
docker compose logs --no-color | grep "market stream stopping"

# 9. Did our locally maintained books disagree with REST?
docker compose logs --no-color | grep "disagreed with REST at the top of book"

# 10. Is traffic actually arriving? One line per full REST sweep (M6.1).
docker compose logs --no-color | grep "stream data-plane health"

# 11. How much REST load did the targeted resync path cause? (M6.1: should be small.)
docker compose logs --no-color | grep "resynced stale books over REST" | tail -20
```

- [ ] **Check 6 is non-zero.** Zero → `stream.url` is wrong, or the handshake is rejected.
      Nothing is lost (the daemon polls), but the whole latency benefit is.
- [ ] **Check 7 is empty.** A `handing detection back` line means the endpoint failed
      `stream.fallback_after_failures` times in a row *while REST kept working* — i.e. the
      socket specifically is broken. Latency numbers are absent, not zero, from that point
      until an `answered a re-probe` line shows streaming restored (M6.1: the hand-over
      lasts only until the endpoint proves itself again, re-probed every
      `stream.reprobe_interval_secs`). Repeated pairs of both lines mean a flapping
      endpoint. A *total* network outage produces neither line — both transports retry.
- [ ] **Check 8 shows `snapshots` > 0 and `deltas` > 0.** `snapshots: 0` with a successful
      connection means the subscribe frame shape (`assets_ids`, `type: "market"` in
      `subscribe_message`, `src/ws.rs`) is wrong — we connected and were ignored.
      `deltas: 0` with snapshots flowing means the `price_change` event name or its
      `changes[]`/`side` shape is wrong.
- [ ] **Check 8 shows `unknown_frames` / `malformed_frames` low.** A large
      `malformed_frames` is the direct signal that a field we *do* model has a different
      shape (asset id, side vocabulary, level arrays). `unknown_frames` is benign — event
      types we do not model.
- [ ] **Check 8 shows `orphan_deltas` and `out_of_order` near zero.** Sustained
      `out_of_order` means `timestamp` is not what we assume (milliseconds, monotone per
      asset) and the guard is throwing away good updates.
- [ ] **Check 9 is empty, and `divergences` in check 8 is 0.** This is the one that
      matters most: the frames may carry no checksum we can verify, so the slow REST sweep
      is the *only* thing that can tell us our streamed books are wrong. A non-zero
      divergence count means detection has been running on a book that does not match the
      venue — treat every stream-detected opportunity as unproven until it is explained,
      and re-soak with `stream.enabled = false` to get a clean REST baseline.
- [ ] **Check 10 shows a non-zero `events_per_sec` on every sweep.** This is the M6.1
      addition that makes "subscribed but silent" distinguishable from "the market is
      quiet" — from the book state alone they look identical. `frames = 0` across sweeps
      while `connections` is non-zero means we connected and are being ignored (see the
      subscribe-frame note under check 8).
- [ ] **Check 11 shows `requested` in the hundreds at most, and usually nothing at all.**
      Before M6.1 this line read `requested=16500+` every ~90 s — every *quiet* book in the
      universe, on a loop, because silence was being treated as staleness. It should now
      only fire after a reconnect or a universe re-plan. A sustained large `requested` means
      shards are flapping; correlate with `reconnects` in check 8.
- [ ] **`## Detection latency (stream)` p50 is in the tens of milliseconds.** Hundreds of
      ms or more points at `stream.debounce_ms`, a saturated shard, or a `timestamp` unit
      we guessed wrong (the fallback measurement — time since we read the frame — cannot
      exceed the debounce by much, so an implausibly *small* number with a large p95 is
      also worth a look).

> Docker keeps roughly the last two weeks of logs (10 MB × 5 files) before rotating, so
> these greps still reach back over a one-week soak. The daily reports are permanent; the
> logs are not. If you want the raw logs kept, copy them off before they rotate:
> `docker compose logs --no-color > ~/polyarb-soak-logs.txt`.

### Part C — the actual economics

Only once Parts A and B pass. Aggregate across all seven reports.

**Opportunities per day, by strategy and category** — the `## Opportunities` table.

- [ ] Total distinct opportunities per day, and the trend across the week.
- [ ] The **split by strategy**. The research predicts NegRisk multi-outcome rebalancing
      dominates and single-market binary is secondary. Does your own data agree? A result
      that contradicts the research is a genuine finding, not an error.
- [ ] The **split by category**, especially whether fee-free **geopolitics** is
      over-represented — it is the only category with a zero taker fee, so thinner gaps
      survive there, and it should punch above its weight.
- [ ] The **maker-only count**. If nearly everything is maker-only, there is no takeable
      arbitrage at all, and Phase B would have to be a market-making bot with legging
      risk — a substantially harder and different project.

**Net edge distribution** — the `## Net edge per share` section.

- [ ] **p50, p90 and max** net per share. Compare against the floors that produced them
      (0.005 default, 0.003 geopolitics). If p50 sits right on the floor, you are seeing
      threshold noise rather than real edge. A p90 well clear of the floor is the
      encouraging shape.

**Fill vs. vanished — the most important number in the entire soak.**
From `## Persistence and fills`. This is what separates arbitrage that exists on a screen
from arbitrage you could have taken.

- [ ] **fill rate %** — the share of detected opportunities whose walked fills were still
      on the book at the first re-poll. This is the honest "could we have got it?" number,
      decided once and never revised.
- [ ] **filled vs. vanished** counts.
- [ ] **persistence p50/p90/max**. The research (arXiv:2605.00864) found single-market
      arbitrage episodes with a **~3.6 second median lifetime**. If your p50 persistence is
      of that order, a 5-second REST polling loop is structurally too slow and Phase B is
      a WebSocket-and-latency project before it is a trading project.
- [ ] **untracked** must be ~0. A non-zero count means `lifecycle.max_concurrent` (32) was
      exceeded and some opportunities were never followed — the fill rate is then measured
      on a biased subsample and understates or overstates accordingly.
- [ ] **unresolved** should be small. It counts rows orphaned by a restart.

**Simulated P&L — both views.** From `## Simulated P&L`.

- [ ] **Taker P&L** — credited only where the fills genuinely survived. Treat this as the
      realistic ceiling of what Phase A would have earned, minus execution slippage and
      failures that a dry run cannot simulate.
- [ ] **Maker P&L (hypothetical)** — assumes every resting quote gets crossed, which is
      optimistic by construction. It is an upper bound on a *different* strategy, not a
      better estimate of the same one. Never compare it directly against the taker figure.

**Capital.** From `## Capital`.

- [ ] **rows sized at the cap (≥99%)**. If most opportunities were clipped by the $50
      per-trade cap, depth was not the binding constraint — capital was, and more capital
      would scale the returns roughly linearly. If almost none hit the cap, the book depth
      is the ceiling and more capital would buy you nothing.
- [ ] **mean and max capital per opportunity** — tells you what a working float actually
      needs to be.

### The decision

Weigh it against what Phase B actually costs: order signing, execution with
abort-and-unwind on partial fills, a full risk manager, and real money in a hot wallet.

**Lean GO** when, over the week:

- Parts A and B are clean.
- Opportunities appear on most days rather than in one anomalous burst.
- The fill rate is high enough that the opportunities are real rather than ghosts —
  around a third or better is a reasonable anchor, but read it together with persistence.
- Simulated **taker** P&L is positive and large enough to matter to you after you mentally
  discount it for slippage, partial fills and the days the bot will be broken.
- Sizes are meaningful — opportunities sized at a handful of shares are a curiosity, not a
  business.

**Lean NO-GO, or iterate before committing** when:

- Persistence p50 is in the low seconds — fix the data path (WebSocket) before building
  execution, or you will build a bot that is always late.
- Nearly everything is maker-only — that is a different, harder project than the one
  scoped.
- Opportunities are near zero on a demonstrably healthy scanner — the honest answer is
  that this edge has been competed away, and the week cost you a few euros to find out.
- Part B turned up a broken assumption — re-run the soak after fixing it. Do not make a
  go/no-go call on data you have reason to distrust.

Whatever you decide, **Phase B does not begin without an explicit decision**. Live
trading stays behind an opt-in flag, with a dedicated hot wallet funded only with money
you can afford to lose.
