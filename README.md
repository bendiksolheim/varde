# varde

A minimal uptime monitor for a home server, written in Rust. A *varde* is the old
Norwegian beacon cairn — a signal fire kept burning so the next station notices when it
goes out. That is the design in one word: varde does not alert by itself; it keeps a
signal burning that external services watch.

- **Checks services over HTTP** on per-service schedules; a service is up when the
  response status matches the configured code exactly.
- **Keeps only the latest result per service in memory** — no database, no history.
- **Reports upstream via a heartbeat** (dead man's switch): if *everything* is up, it
  pings [healthchecks.io](https://healthchecks.io); if anything is down — or the monitor
  itself has died — the ping goes missing and healthchecks.io raises the alarm.
- **Sends push notifications via [ntfy.sh](https://ntfy.sh)**: a rate-limited reminder
  while an outage lasts, one "all back up" message on recovery.
- **Serves exactly one endpoint**, `GET /`, returning status as JSON. No web UI.

It replaces a Next.js/Node.js implementation; the config file format is inherited and
stays compatible. Runtime footprint is a single-digit-MB RSS and a `FROM scratch` image.

## Configuration

Read from `/config/config.json`, overridable with the `CONFIG_PATH` environment
variable. The process refuses to start on a config it cannot fully honor, naming the
offending field. Unknown keys are accepted with a warning (typos surface in the logs);
the legacy `nodes` key is accepted and ignored.

```json
{
  "services": [
    {
      "service": "Home Assistant",
      "schedule": "Every 10 minutes",
      "url": "http://192.168.1.89:4357",
      "okStatusCode": 200
    },
    {
      "service": "Nginx redirect",
      "schedule": "every 1 minute",
      "url": "http://server:80",
      "okStatusCode": 301
    }
  ],
  "heartbeat": {
    "type": "healthchecks.io",
    "uuid": "12345678-1234-1234-1234-123456789012",
    "schedule": "Every 10 minutes"
  },
  "notify": [
    {
      "topic": "my-ntfy-topic",
      "schedule": "Every 10 minutes",
      "minutesBetween": 120
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `services[].service` | display name; must be unique and non-empty |
| `services[].schedule` | check schedule (see [Schedules](#schedules)) |
| `services[].url` | absolute `http`/`https` URL; redirects are **not** followed — a 301 is compared as-is against `okStatusCode` |
| `services[].okStatusCode` | the one status code (1–599) that counts as up |
| `heartbeat` | optional; `type` is `"healthchecks.io"` (requires `uuid`) or `"httpbin"` (a legacy dev stub that alerts nobody) |
| `notify[].topic` | ntfy.sh topic; one rate-limit window per entry |
| `notify[].minutesBetween` | minimum minutes between down-messages (fractional OK, `0` = every tick) |

Checks GET the URL with a 10-second timeout and never read the body. Latency (whole
milliseconds, headers-only) is recorded for every completed response — also wrong-status
ones; only transport errors (timeout, refused, DNS, TLS) yield no latency.

## Schedules

Schedule expressions use the interval grammar:

```
every N seconds|minutes|hours|days     (N ≥ 1)
```

Parsing is case-insensitive and tolerant of singular/plural mismatches and extra
whitespace — `Every 10 minutes`, `every 1 minutes`, and `every 1 minute` all work.
Calendar expressions from the legacy grammar (`every weekday at 09:00`) are **not**
supported and fail at startup with an error naming the expression.

Occurrences are **wall-clock aligned** (anchored to the Unix epoch): `every 10 minutes`
fires at :00, :10, :20, …, not relative to process start — matching the legacy later.js
behavior, stable across restarts. Every service is also checked once immediately at
startup. If a check runs long, missed occurrences are skipped, never replayed. The
heartbeat and notify loops first fire at their first scheduled occurrence, giving the
initial round of checks time to land.

## Status endpoint

`GET /` returns compact JSON; every other path or method is 404.

```json
{
  "operational": false,
  "services": [
    { "service": "Home Assistant", "ok": true,  "lastChecked": "2026-07-17T12:34:56Z", "latencyMs": 42 },
    { "service": "Wrong status",   "ok": false, "lastChecked": "2026-07-17T12:35:02Z", "latencyMs": 87 },
    { "service": "Unreachable",    "ok": false, "lastChecked": "2026-07-17T12:35:10Z", "latencyMs": null },
    { "service": "Not yet checked", "ok": null, "lastChecked": null,                   "latencyMs": null }
  ]
}
```

- One entry per configured service, in config order; never-checked services have all
  three fields `null` and do not count against `operational`.
- HTTP status is **200 when operational, 500 otherwise**, so an external checker can
  watch the monitor itself without parsing JSON.
- `lastChecked` is UTC, RFC 3339, second precision.

## Notifications

While services are failing, each ntfy topic receives at most one message per
`minutesBetween` window: `Title: Service down`, `Tags: warning`, body like
`2 services down: a and b`. When everything recovers, one message —
`Title: Services recovered`, `Tags: white_check_mark`, body `All services back up` —
and the rate-limit window resets. Sends only count on a 2xx response; failed sends are
retried at the next tick. Rate-limit state is in memory: a restart mid-outage may
re-notify once (accepted trade-off).

## Deployment

Multi-arch images (amd64/arm64) are published as
[`bendiksolheim/varde`](https://hub.docker.com/r/bendiksolheim/varde) by tagging
`vX.Y.Z`; the image is `FROM scratch` (CA roots are compiled in via rustls).

```yaml
services:
  varde:
    image: bendiksolheim/varde:latest
    restart: unless-stopped
    volumes:
      - ./config:/config
    ports:
      - 3000:3000
```

### Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `CONFIG_PATH` | `/config/config.json` | config file location |
| `PORT` | `3000` | listen port (1–65535); the bind address is always `0.0.0.0` |
| `RUST_LOG` | `info` | log filter (`debug` shows every check result) |

Graceful shutdown on SIGTERM/SIGINT: stop serving, drop in-flight work, exit 0.

## Development

```sh
cargo test                                   # full suite incl. end-to-end binary tests
cargo llvm-cov --all-targets \
  --fail-under-lines 100 \
  --ignore-filename-regex 'src/main\.rs'     # the CI coverage gate (100% is the contract)
```

CI enforces `cargo fmt`, `clippy -D warnings`, the 100% line-coverage gate, and a Docker
smoke test. `src/main.rs` is process wiring, excluded from unit coverage and exercised by
`tests/e2e.rs` instead (spawns the real binary against mock upstreams).

For tests, `VARDE_HC_BASE_URL` and `VARDE_NTFY_BASE_URL` override the heartbeat and ntfy
base URLs (defaults: `https://hc-ping.com` / `https://httpbin.org` per heartbeat type,
and `https://ntfy.sh`). The config file format stays legacy-compatible.

The full specification and implementation plan lives in [`rust.md`](rust.md).
