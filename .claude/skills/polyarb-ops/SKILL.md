---
name: polyarb-ops
description: >
  Operate and troubleshoot the polyarb bot soaking on the owner's Raspberry Pi 5.
  Use this skill whenever the owner reports anything about the bot or the Pi being
  down, unreachable, erroring, or behaving oddly — SSH timeouts, "connection refused",
  DNS / "name resolution" errors in the logs, the dashboard not loading, docker
  errors, "readonly database", the container showing unhealthy — and also for routine
  operations: pausing/stopping, resuming, updating to the latest code, reading logs,
  or accounting for downtime in the soak evidence. If a message contains a pasted
  PowerShell error, Pi log line, or docker output, this skill almost certainly applies.
---

# polyarb-ops — running the bot on the owner's Raspberry Pi

## Who you are talking to

The owner has no coding background and often works from a Windows PC (German
locale — error messages may arrive in German). Give commands to copy-paste,
one step at a time, and say which machine each command runs on: **PowerShell
on the PC** vs **the SSH session on the Pi**. Explain what a command will do
in one plain sentence before or after it. Never ask them to paste secrets
(tokens, keys) into chat. When they paste output, read the timestamps —
docker `logs` scrollback routinely resurfaces old errors that look current.

## Fixed facts (verified in production)

- Pi: hostname `poolyarb` (note the spelling), user `johannes`, Ethernet to the
  home router. Usual address `192.168.67.107`, router/gateway `192.168.67.1`,
  home subnet `192.168.67.0/24`. `poolyarb.local` (mDNS) usually does NOT
  resolve from Windows — always prefer the IP.
- Project directory on the Pi: `~/polyarb` (compose commands only work there).
- Two containers: `polyarb` (the daemon) and `polyarb-dashboard` (read-only
  monitor, published on the home LAN at `http://192.168.67.107:8080`).
- Containers run as uid 10001. The host dirs `data/ logs/ reports/` must be
  owned by 10001 or the daemon dies with "readonly database" / "Permission
  denied" (`sudo chown -R 10001:10001 data logs reports` — needed again after
  any scp/copy that resets ownership).
- The bot is dry-run only: no wallet, no keys, nothing on the dashboard can
  trade. Restart/power-cut is always safe for the database (SQLite WAL).

## Routine operations (all in `~/polyarb` on the Pi)

- **Resume / start**: `docker compose up -d` — first cycle takes a couple of
  minutes (universe discovery) before logs look busy.
- **Pause / stop**: `docker compose stop` — this IS the pause. Graceful:
  SIGTERM drains in-flight measurements before exit (grace period 45 s). A
  deliberate stop stays stopped across reboots (`restart: unless-stopped`).
  Never suggest `docker compose pause` (freezes the process; its market
  connections rot).
- **Health check**: `docker compose logs --tail 20 polyarb` — healthy is shard
  connects and scan cycles; sick is a repeating retry line.
- **Update to latest code**: `git pull && docker compose up -d --build` (or
  `up -d dashboard` if only the dashboard changed). The bot need not be
  stopped first; compose recreates only what changed.
- **Dashboard**: `http://192.168.67.107:8080` from any home device. If its
  page "takes over" with a break state, the scanner is blind — investigate,
  it is not cosmetic.

## Troubleshooting

Work these in order; each is a real incident we have already debugged.

### PC cannot reach the Pi (SSH timeout, dashboard dead)

1. PC side first: `ipconfig` in PowerShell — the active adapter's IPv4 must be
   `192.168.67.x`. Anything else = wrong Wi-Fi / guest net / VPN; fix that first.
2. `ping 192.168.67.107`. Replies but SSH fails is rare — usually both fail.
3. Physical: Pi power LED, Ethernet port lights blinking. Dark = cable/port —
   reseat both ends, try another router port.
4. Power-cycle the Pi (pull plug, 10 s, replug). Safe; containers auto-start
   (unless they were deliberately stopped).
5. Router device list at `http://192.168.67.1` → find `poolyarb`, use whatever
   IP it shows now (DHCP may have moved it).
6. Recommend once per incident: a DHCP reservation for poolyarb ("diesem Gerät
   immer die gleiche IPv4-Adresse zuweisen" on German routers) so the address
   stops moving.

### Logs repeat "failed to lookup address information / Temporary failure in name resolution"

The Pi (or just the container) cannot resolve DNS. The daemon is designed to
retry forever when both transports are down (network outage ≠ give up), so
nothing is lost except measurement time.

1. On the Pi: `ping -c 3 8.8.8.8` and `ping -c 3 google.com`.
   - Both fail → no internet: router/cable problem (see previous section).
   - IP works, name fails → DNS broken: reboot the router, or `sudo reboot`
     the Pi.
   - Both work but the bot still errors → **stale container DNS**: containers
     copy the host's DNS config at start, so after any network change run
     `docker compose restart`. This is the most common resolution.
2. Simplest broad fix when in doubt: reboot the Pi; everything restarts clean.

### Container "unhealthy" or the dashboard says the daemon stopped

The compose healthcheck means only "the process writes its database": unhealthy
= process gone or cannot write, NOT "the loop is slow". A wedged loop is the
daemon's own watchdog's job — it exits code 75 and compose restarts it. So:
`docker compose ps` then `logs --tail 50 polyarb`; if the last lines are old,
`docker compose up -d` brings it back. "readonly database" → the chown fix
from Fixed facts.

### Old errors in scrollback

`docker compose logs` without `--since` shows history. Before diagnosing
anything, compare the log line's timestamp with the current time; if the
owner is worried about an old line, `docker compose logs --since 10m` settles it.

## Soak-evidence accounting

Downtime is a hole in the maker-fill evidence, never fabricated data — the
daily report shows errors/gaps honestly. If an outage or pause reaches roughly
half a day or more, offer to push out the scheduled go/no-go review by the
same amount (the reminder is a Claude Code Remote trigger; update its
`run_once_at` via `update_trigger`, don't delete/recreate). Short pauses need
no action.

## Boundaries

- Never suggest exposing the dashboard (or SSH) beyond the home LAN; the
  LAN-wide dashboard port is an explicit, documented owner decision.
- Never touch `.env`, tokens, or keys; never echo them.
- Mode stays `dry-run`; the daemon refuses anything else in Phase A, and
  enabling live trading is an owner decision outside this skill's scope.
