# varde

An intentionally minimal uptime monitor used to keep track of the current status of your services and tell you
when they don’t work as expected. Works by pinging configured services on a schedule and keeping track of the current
status. Reports to healthchecks.io and optionally notifies you directly via https://ntfy.sh, Telegram, or Pushover.

Varde features no GUI, only an endpoint serving the current status as JSON.

## Features

Intentionally minimal feature set. Prioritizes a low resource footprint over features.

- **Checks services over HTTP** on a per-service schedule. A service is up when the
  response status matches the configured code exactly.
- **Reports upstream via a heartbeat** (dead man's switch): if *everything* is up, it
  pings [healthchecks.io](https://healthchecks.io): if anything is down — or the monitor
  itself has died — the ping goes missing and healthchecks.io raises the alarm.
- **Sends push notifications via [ntfy.sh](https://ntfy.sh), Telegram, or [Pushover](https://pushover.net)**:
  a rate-limited reminder while an outage lasts, one "all back up" message on recovery.
- **Reports the current status over HTTP**: `GET /`, returning status as JSON. No web UI.

## Installation

It is currently only distributed as a container image on Docker Hub: [`bendiksolheim/varde`](https://hub.docker.com/r/bendiksolheim/varde).

See [`docker-compose.example.yml`](docker-compose.example.yml) for an example.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `CONFIG_PATH` | `/config/config.json` | config file location |
| `PORT` | `3000` | listen port (1–65535); the bind address is always `0.0.0.0` |
| `RUST_LOG` | `info` | log filter (`debug` shows every check result) |

## Configuration

Reads from `/config/config.json` by default, overridable with the `CONFIG_PATH` environment
variable.

Example config file:

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
      "type": "ntfy",
      "topic": "my-ntfy-topic",
      "schedule": "Every 10 minutes",
      "minutesBetween": 120
    },
    {
      "type": "telegram",
      "botToken": "123456:my-bot-token",
      "chatId": "987654321",
      "schedule": "Every 10 minutes",
      "minutesBetween": 120
    },
    {
      "type": "pushover",
      "apiToken": "my-pushover-api-token",
      "userKey": "my-pushover-user-key",
      "priority": 1,
      "schedule": "Every 10 minutes",
      "minutesBetween": 120
    }
  ]
}
```

### Field specification

| Field | Type | Required | Description |
|---|---|---|---|
| `services` | Object | Yes | Contains a list of services to ping |
| `heartbeat` | Object | No | Optional. `type` must be `"healthchecks.io"` as this is the only supported service for now |
| `notify` | Object | No | Optional. A list of notification targets; each entry's `type` selects `ntfy`, `telegram`, or `pushover` |

#### Services

| Field | Type | Required | Description |
|---|---|---|---|
| `service` | String | Yes | Name of the service to make them distinguishable from each other |
| `schedule` | String | Yes | How often this check should run. See #schedules for format |
| `url` | Url (String) | Yes | A valid, absolute, URL you want to ping |
| `okStatusCode` | Number | Yes | The status code you expect from a healthy call |

There is a set, non configurable timeout of **10 seconds** at the moment.

#### Heartbeat

Supports sending a heartbeat to healthchecks.io.

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | Enum (String) | Yes | The type of heartbeat. Currently "healthchecks.io" is the only supported one |
| `uuid` | UUID (String) | Yes | Your heathlchecks.io UUID |
| `schedule` | String | Yes | How often the heartbeat should ping healthchecks.io. Correspond this to what you configure on healthchecks.io. See #schedules for format |

#### Notifications

Each entry in `notify` is one target. `type` picks the backend and, like `heartbeat`,
determines which other fields are required.

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | Enum (String) | Yes | The notification backend: `"ntfy"`, `"telegram"`, or `"pushover"` |
| `schedule` | String | Yes | How often the status should be checked to consider sending a notification. See #schedules for format |
| `minutesBetween` | Number | Yes | In case a service is down over a prolonged amount of time, how long should we wait before sending a repeat message? |

##### `type: "ntfy"`

| Field | Type | Required | Description |
|---|---|---|---|
| `topic` | String | Yes | Your ntfy.sh topic |

##### `type: "telegram"`

| Field | Type | Required | Description |
|---|---|---|---|
| `botToken` | String | Yes | Your Telegram bot token, from [@BotFather](https://t.me/BotFather) |
| `chatId` | String | Yes | The chat ID to send messages to |

##### `type: "pushover"`

| Field | Type | Required | Description |
|---|---|---|---|
| `apiToken` | String | Yes | Your Pushover application's API token |
| `userKey` | String | Yes | Your Pushover user (or group) key |
| `priority` | Number | No | Message priority, `-2` (lowest) to `1` (high); omitted means Pushover's own default (`0`, normal). Emergency priority (`2`) is not supported |

## Testing notifications

Run varde with the flag `test-notify [--only {service}]` to test notifications

```sh
varde test-notify              # every configured entry
varde test-notify --only pushover
```

If you run Varde in a Docker container, use `docker exec`:

```sh
docker exec <container> /varde test-notify
```

## Schedules

Schedules are expressed as repeat intervals:

```
every N seconds|minutes|hours|days     (N ≥ 1)
```

They are case-insensitive and tolerant of singular/plural mismatches.

Occurrences are **wall-clock aligned** (anchored to the Unix epoch): `every 10 minutes`
fires at :00, :10, :20, …, stable across restarts. Every service is also checked once immediately at
startup. If a check runs long, missed occurrences are skipped, never replayed. The
heartbeat and notify loops first fire at their first scheduled occurrence, giving the
initial round of checks time to land.

## Status endpoint

`GET /` returns current status as compact JSON.

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

- Never-checked services have all three fields `null` and do not count against `operational`.
- HTTP status is **200 when operational, 500 otherwise**, so an external checker can
  watch the monitor itself without parsing JSON.
- `lastChecked` is UTC, RFC 3339, second precision.

## Release new versions

Multi-arch images (amd64/arm64) are published at
[`bendiksolheim/varde`](https://hub.docker.com/r/bendiksolheim/varde) by creating a new release with
the tab `vX.Y.Z`.

## Development

Run with Cargo:
```rust
CONFIG_PATH={path-to-config-file} RUST_LOG={debug} cargo run
```

### Build a container locally

Example here uses [`container` CLI](https://github.com/apple/container). Also works with Docker if
you prefer that.

```sh
container build -t varde:local .
container run --rm -p 3000:3000 -v "$(pwd)/config:/config" varde:local
```

### Tests and coverage

```sh
cargo test                                   # full suite incl. end-to-end binary tests
cargo llvm-cov --all-targets \
  --fail-under-lines 100 \
  --ignore-filename-regex 'src/main\.rs'     # the CI coverage gate (100% is the contract)
```

CI enforces `cargo fmt`, `clippy -D warnings`, 100% line-coverage, and a Docker
smoke test. `src/main.rs` excluded from unit coverage and exercised by
`tests/e2e.rs` instead (spawns the real binary against mock upstreams).

For tests, `VARDE_HC_BASE_URL`, `VARDE_NTFY_BASE_URL`, `VARDE_TELEGRAM_BASE_URL` and
`VARDE_PUSHOVER_BASE_URL` override the heartbeat and notify base URLs (defaults:
`https://hc-ping.com` / `https://httpbin.org` per heartbeat type, and `https://ntfy.sh` /
`https://api.telegram.org` / `https://api.pushover.net` per notify type). The config file
format stays legacy-compatible.
