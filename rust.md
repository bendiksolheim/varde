# varde — specification and implementation plan

This document is self-contained. It specifies a small monitoring daemon written in Rust and
lays out a phased plan to build it from an empty repository. No knowledge of any prior
codebase is required; everything the implementation must do is written down here.

---

## 1. What we are building

**varde** is a minimal uptime monitor for a home server. A *varde* is the old Norwegian
beacon cairn — a signal fire lit so the next station notices when something is wrong, or
when the fire itself goes out. That is the design in one word: varde does not fix or alert
by itself; it keeps a signal burning that external services watch. It is a single
long-running process that:

1. **Checks services over HTTP.** Each configured service is fetched on its own schedule;
   a service is "up" when the response status code matches the configured expected code.
2. **Keeps the latest result per service in memory.** No database, no history.
3. **Reports upstream via a heartbeat (dead man's switch).** On a schedule, if *every*
   service is up, it pings an external monitoring service (healthchecks.io). If anything is
   down — or if the monitor itself has died — the ping is skipped/missing and the external
   service raises the alarm. Alerting is thus delegated: the monitor never sends "I am
   broken" emails itself.
4. **Sends push notifications via ntfy.sh** when services are down: a rate-limited reminder
   while the outage lasts, and a single "all back up" message on recovery.
5. **Serves exactly one HTTP endpoint**, `GET /`, returning current status as JSON.
   There is no web UI of any kind.

It replaces a Next.js/Node.js implementation that did the same job with a full web frontend,
a SQLite event log, and a job-runner spawning worker threads — too heavy for the Synology
NAS it runs on. The config file format is inherited from that system and **must remain
compatible**; everything else is a clean rewrite.

### Goals

- Minimal resource usage: target single-digit-MB RSS, a Docker image under ~15 MB.
- Existing `config.json` files keep working unchanged.
- Correctness over features: this is a monitoring system, so every behavior in §2 is
  specified precisely and must be covered by tests. **Target: 100 % line coverage at
  completion** (§5).

### Non-goals

- No web UI, HTML, charts, or static assets.
- No persistence: state is rebuilt from live checks after a restart.
- No history, uptime percentages, or latency trends.
- No config hot-reload: the process reads config once at startup; restart to apply changes.
- No built-in alerting channels beyond healthchecks.io and ntfy.sh.

### Accepted trade-offs (explicit decisions, do not "fix" these)

- **Restart may re-notify:** notification rate-limit state is in memory, so a restart during
  an outage can trigger an immediate duplicate ntfy message.
- **Startup blindness lasts seconds, not minutes:** every service is checked immediately at
  startup (then on schedule), so the in-memory picture fills fast.
- **The `httpbin` heartbeat type is kept** for config compatibility even though it pings a
  test site and alerts nobody (it existed as a dev stub). (Correction 2026-07-18: the
  actual production config already uses `healthchecks.io`, so Phase 6 step 5's
  httpbin-to-healthchecks switch is moot — but the type stays for old configs.)

---

## 2. Functional specification

This section is the behavioral contract. Every statement here must be enforced by at least
one test (see the per-phase test lists in §6).

### 2.1 Configuration

Read from `/config/config.json`, overridable with the `CONFIG_PATH` environment variable.
If the file is missing, unreadable, not valid JSON, or fails validation: print a readable
error to stderr naming the problem (and the offending field/expression) and exit with
code 1. The process must never start with a config it cannot fully honor.

Full example:

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

Schema and validation rules:

| Field | Rules |
|---|---|
| `services` | required array, may be empty |
| `services[].service` | non-empty string; the service's identity. **Duplicate names are a startup error.** |
| `services[].schedule` | schedule expression (§2.2); must parse at startup |
| `services[].url` | valid absolute URL (`http`/`https`) |
| `services[].okStatusCode` | integer in `1..=599`; fractional numbers are rejected (legacy: zod `int().positive().lte(599)`) |
| `heartbeat` | optional; tagged union on `type` |
| `heartbeat.type` | `"healthchecks.io"` or `"httpbin"`; anything else is an error |
| `heartbeat.uuid` | required iff type is `healthchecks.io`. Validation is **loose**: hyphenated `8-4-4-4-12` hex, case-insensitive; version/variant bits are *not* checked — the legacy suite's valid vector is `12345678-1234-1234-1234-123456789012`, which has invalid RFC 4122 variant bits and must stay accepted. (Beware: Rust's `uuid` crate parser is *more* lenient — also accepts unhyphenated/braced/URN forms — so validate the hyphenated shape explicitly.) A stray `uuid` on an `httpbin` heartbeat is ignored with an unknown-key warning (legacy ignored it silently) |
| `heartbeat.schedule` | schedule expression |
| `notify` | optional array |
| `notify[].topic` | non-empty string (an ntfy.sh topic). **Duplicate topics are a startup error** — with per-entry rate-limit state (§2.5) they would double-message one topic. (Legacy tolerated them only because its rate limit was global.) |
| `notify[].schedule` | schedule expression |
| `notify[].minutesBetween` | non-negative number; fractional values are legal and compared in fractional minutes (legacy accepted any `number`, negative included — varde tightens to ≥ 0) |
| `nodes` | legacy key from the old system (fed a removed UI page). Accepted and **ignored**, with an **info**-level log line: `config key "nodes" is ignored` |
| any unknown key | accepted and ignored, with a **warn**-level log naming the key and its path (catches typos without breaking old configs). Detection is **recursive**: unknown keys inside service entries, `heartbeat`, and notify entries warn too — a typo like `okStatuscode` must not vanish silently (legacy zod ignored unknown keys at every level; this is a deliberate improvement). Implementation note: parse to a generic JSON value first, walk it against the known key set per level, then deserialize into typed structs — a strict deny-unknown-fields mode is too brittle here. |

Every field listed above is required unless marked optional; there are no defaults
(matches the legacy zod schema — `notify[].minutesBetween` in particular has no default).
Note: some old example configs used an `expression` key instead of `schedule`; such a
config fails on the missing `schedule` field, which is correct (the legacy validator
rejects it too).

### 2.2 Schedule expressions

Schedules are human-readable strings. The legacy system parsed them with the JavaScript
`human-interval`/later.js grammar; real-world config files contain expressions like:

- `"Every 10 minutes"` — note the capital E
- `"Every 1 minutes"` — note the mismatched plural
- `"every 5 minutes"`, `"every 30 seconds"`, `"every 2 hours"`

Requirements:

1. Parsing is **case-insensitive** and tolerant of singular/plural mismatches
   (`1 minutes`, `2 minute`) and surrounding/repeated whitespace. Implement this as a
   normalization step (trim, lowercase, collapse whitespace, fix pluralization) in front of
   the parser.
2. The **required** grammar is the interval form `every N seconds|minutes|hours|days`
   with N ≥ 1 (`every 0 minutes` and negative N are startup errors). This covers every
   expression ever observed in real configs — production uses only `Every 10 minutes` and
   `Every 1 minutes`; README/tests add `every 1 minute`, `every 5 minutes`,
   `every 10 minutes`, `every 30 seconds`. Calendar forms (`every weekday at 09:00`,
   `at 10:00 am`) are **best-effort bonus**: if the chosen parser supports them they are
   evaluated in UTC and documented in the README; if not, they fail at startup like any
   other unparsable expression. Primary candidate: the
   [`hron`](https://docs.rs/hron) crate (human-readable scheduling, a superset of cron,
   built on `jiff`). It is young (v1.0.0), so Phase 0 spikes it; fallbacks are specified
   there — the hand-rolled interval parser is a fully acceptable outcome, since the
   interval form is the whole compatibility floor.
3. Every schedule in the config is parsed **once, at startup**; a bad expression is a
   startup error naming the expression. Ticks never re-parse.
4. Interval semantics: `every N minutes` may fire relative to process start or aligned to
   the wall clock — either is acceptable, but the choice must be documented in the README
   and stable. (Legacy, via later.js, was wall-clock aligned.)
5. The parser lives behind a single seam, `schedule::parse(expr) -> Result<Schedule, Error>`,
   where `Schedule` can yield "next occurrence after instant T". Swapping the underlying
   crate must not touch any other module.

### 2.3 Health checks

Per service, on its schedule (plus one immediate run at startup):

- HTTP **GET** to the configured URL.
- **Timeout: 10 seconds**, hardcoded.
- **Redirects are not followed.** A 301 response is compared as-is against `okStatusCode`
  (this is how a redirecting service with `okStatusCode: 301` is monitored).
- The service is **up** iff `status == okStatusCode` (exact match, single code).
- **Latency** = time from sending the request until the response headers are received,
  in whole milliseconds (truncated). The response body is never read. Latency is recorded
  for **every completed response, up or down** — a 500 against `okStatusCode: 200` is down
  *with* a latency value (legacy behavior: `latency = end - start` regardless of `ok`).
- Any transport error (timeout, connection refused, DNS failure, TLS error) → down,
  latency = none. Only transport errors yield a missing latency. Errors are recorded as
  results, never crash the check loop.
- The result overwrites the service's entry in the in-memory state:

  ```
  ServiceStatus { ok: bool, last_checked: timestamp (UTC), latency_ms: Option<u64> }
  ```

- A slow check delays that service's next tick (no overlapping checks of the same service).
  After a tick completes, the loop sleeps until the first occurrence strictly after *now* —
  missed occurrences are skipped, never replayed (legacy bree dropped them the same way).
  Different services are independent and run concurrently.

### 2.4 Heartbeat (dead man's switch)

If `heartbeat` is configured, on its schedule:

1. Collect the latest result of every service that **has been checked at least once**.
   Services with no result yet are simply not considered (vacuously OK) — this makes the
   heartbeat correct during the startup window.
2. If **all** collected results are up:
   - type `healthchecks.io` → GET `https://hc-ping.com/<uuid>`
   - type `httpbin` → GET `https://httpbin.org/get`
3. If **any** is down: do nothing (skipping the ping is the alarm).
4. A ping counts as failed on any transport error **or non-2xx response**. Failures are
   logged at warn level and retried implicitly at the next scheduled tick; they never
   crash the loop. (Legacy never inspected the ping's response status — a 404 from a bad
   UUID counted as success. Checking for 2xx is a deliberate improvement.)

The first heartbeat tick happens at the first *scheduled* occurrence (not immediately at
startup), giving the initial round of checks time to land.

### 2.5 Ntfy notifications

Each entry in `notify` runs independently on its own schedule, with its own state:

```
NotifyState { last_sent: Option<timestamp>, was_down: bool }
```

On each tick:

1. Compute the list of failing services from the in-memory state, **in config order**
   (message wording depends on order; config order makes it deterministic).
2. **Nothing failing and `was_down == false`:** do nothing.
3. **Nothing failing and `was_down == true`:** the outage just ended. Send a recovery
   message — POST to `https://ntfy.sh/<topic>`, headers `Title: Services recovered`,
   `Tags: white_check_mark`, body `All services back up`. Set `was_down = false` **and
   reset `last_sent = None`** — the rate-limit window never carries over from one outage
   to the next, so the first down-message of a fresh outage always sends immediately.
   Recovery messages are not rate-limited (at most one per outage by construction).
4. **Something failing:** set `was_down = true`. If `last_sent` is unset, or more than
   `minutesBetween` minutes have elapsed since it, send a down message and set
   `last_sent = now`; otherwise skip (rate-limited).
   Down message: POST to `https://ntfy.sh/<topic>`, headers `Title: Service down`,
   `Tags: warning`, body per §2.6.
5. A send succeeds iff the POST completes with a **2xx** status; transport errors and
   non-2xx responses are failures, logged at warn level. State is only updated on
   successful send (so a failed notification is retried at the next tick). Loops never
   crash.

Rate limiting is **per notify entry** — two topics never share state.

**Deliberate divergences from the legacy system** (§2.5 is a redesign, not a port —
don't consult legacy code as the authority here):

- Legacy had **no recovery messages** at all; the recovery mechanism is new.
- Legacy rate-limit state was one **global** persisted table shared by every notify entry
  and surviving restarts; varde's is per entry and in memory (§1 trade-offs).
- Legacy recorded the notification **before** sending, so a failed send was rate-limited
  as if it had succeeded and never retried; varde updates state only on success.
- Legacy treated any completed HTTP response (even 429/500) as a successful send; varde
  requires 2xx.

The strictly-greater rate-limit comparison ("more than `minutesBetween`") *is* inherited
from legacy (`minutesSince > minutesBetween`).

### 2.6 Notification message format

Given the failing services' names in order, the body is (exact strings — these come with
regression test vectors in Phase 4):

| Failing services | Message |
|---|---|
| none | *(no message is produced at all)* |
| `[a]` | `1 service down: a` |
| `[a, b]` | `2 services down: a and b` |
| `[a, b, c]` | `3 services down: a, b and c` |
| n ≥ 3 | `<n> services down: <all but last joined by ", "> and <last>` (no Oxford comma) |

Note the singular/plural of the word `service`/`services`.

### 2.7 Status endpoint

The only route. `GET /` returns `application/json`:

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

- One entry per **configured** service, in config order — including services not yet
  checked (`ok`, `lastChecked`, `latencyMs` all `null`).
- `lastChecked` is UTC, RFC 3339, second precision.
- `latencyMs` is `null` only for transport errors and never-checked services; a down
  service that answered with the wrong status code has a latency (§2.3).
- `operational` = every *checked* service is ok. Unchecked services do not count against it
  (an empty state right after boot is operational).
- HTTP status: **200 when operational, 500 otherwise** — so any external checker can watch
  the monitor itself without parsing JSON.
- Any other path or method → 404 (including non-GET on `/`; no 405s, keep the router
  trivial). No HTML, no favicon, no static files.
- The body is compact JSON (no pretty-printing), `Content-Type: application/json`.
- The server listens on `0.0.0.0:3000`; `PORT` env var overrides the port. `PORT` must
  parse as an integer in `1..=65535`, anything else is a startup error. (Legacy hardcoded
  3000; `PORT` support is new.)

### 2.8 Process lifecycle & logging

- Startup order: load+validate config → initialize state → spawn check loops (immediate
  first run) → spawn heartbeat/notify loops (first run at first scheduled occurrence) →
  serve HTTP.
- Graceful shutdown on SIGTERM/SIGINT (`docker stop` friendliness): stop accepting
  connections, abort loops, exit 0 — immediately, dropping in-flight checks and sends;
  there is no drain period or deadline (legacy did the same: stop scheduler, `exit(0)`).
- Logging: human-readable lines to **stdout** (Docker collects it), level filtered by
  `RUST_LOG`, default `info`. Per-check results log at `debug`; state transitions,
  notification/heartbeat sends at `info`; send/check failures at `warn`.
- Outbound requests use User-Agent `varde/<crate version>`.

---

## 3. Architecture

One process, one tokio runtime (single-threaded flavor — the workload is a handful of
timers and outbound HTTP calls; multi-threading buys nothing).

```
main
 ├─ load + validate config (exit 1 on failure)
 ├─ shared state:  Arc<AppState>
 │     statuses: RwLock<HashMap<ServiceName, ServiceStatus>>
 │     notify:   one Mutex<NotifyState> per notify entry
 ├─ one reqwest Client: rustls, embedded roots, no redirects, 10 s timeout
 ├─ spawn per service      → check loop
 ├─ spawn if heartbeat     → heartbeat loop
 ├─ spawn per notify entry → notify loop
 └─ axum server: GET /
```

Module layout (also the map for the phases):

```
src/
  main.rs        // wiring only: config → state → spawn → serve (kept thin, §5)
  config.rs      // types, deserialization, validation, unknown-key warnings
  schedule.rs    // normalization + parser seam + "sleep until next occurrence"
  state.rs       // AppState, ServiceStatus, NotifyState
  check.rs       // check_once() + check loop
  heartbeat.rs   // heartbeat_tick() + loop
  notify.rs      // notify_tick() + loop + format_notification_message()
  server.rs      // axum router + GET / handler
```

**Design rule that makes 100 % coverage feasible:** every loop body is a free async
function taking its dependencies as arguments — `check_once(client, service) -> ServiceStatus`,
`heartbeat_tick(client, statuses, heartbeat)`, `notify_tick(client, statuses, entry, state, now)`.
The `loop { sleep; tick }` wrappers contain no logic beyond scheduling. Ticks take `now` as
a parameter (never call the clock themselves) and take a base URL for external services
(hc-ping, ntfy) so tests point them at a local mock server. All decision logic is testable
without time or network trickery.

### Crates

| Crate | Purpose | Notes |
|---|---|---|
| `tokio` | runtime, timers, signals | features `rt`, `macros`, `time`, `net`, `signal` |
| `axum` | the one route | default features |
| `reqwest` | outbound HTTP | `default-features = false`, `rustls-tls-webpki-roots` (embedded CA roots → runs in `FROM scratch`, no OpenSSL) |
| `serde`, `serde_json` | config + response | |
| `jiff` | timestamps | schedule parsing is hand-rolled (Phase 0 outcome — hron rejected); use `jiff` for all timestamps (no `chrono`) |
| `tracing`, `tracing-subscriber` | logging | env-filter |
| `anyhow` | startup error reporting | tick functions use concrete error handling, not `anyhow` |
| dev: `wiremock` | mock HTTP server in tests | decided (async-native, most used); `httpmock` was the alternative |
| dev: `cargo-llvm-cov` | coverage measurement | installed in CI, not a dependency |

Deliberately absent: database crates, file-watchers, background-job frameworks, a cron
crate (the schedule seam covers it), `chrono`.

---

## 4. Packaging & deployment

### Docker

Multi-stage build to a `FROM scratch` image:

```dockerfile
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --target "$(uname -m)-unknown-linux-musl" \
 && cp target/*/release/varde /varde

FROM scratch
COPY --from=build /varde /varde
EXPOSE 3000
ENTRYPOINT ["/varde"]
```

- No CA bundle copy needed (roots are compiled in via `rustls-tls-webpki-roots`).
- Expected: image ≈ 5–15 MB, runtime RSS ≈ 3–8 MB.

Compose usage (drop-in replacement for the legacy container):

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

### CI (GitHub Actions)

- `ci.yml` on push/PR: `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo llvm-cov --fail-under-lines 100` (which runs the test suite; §5).
- `release.yml` on tag: `docker buildx` for `linux/amd64,linux/arm64`, push to Docker Hub
  as `bendiksolheim/varde` (the legacy repo's release workflow already uses secrets named
  `DOCKER_USERNAME` / `DOCKER_PASSWORD` — reuse those names).
  Use a Rust build cache (`Swatinem/rust-cache`, or `cargo-chef` inside the Dockerfile);
  arm64-under-QEMU is painfully slow uncached. If still too slow, cross-compile both
  targets natively (`cargo zigbuild` or `cross`) and assemble a multi-arch manifest from
  prebuilt binaries.

---

## 5. Testing strategy — the 100 % coverage contract

Correctness is the point of a monitoring system: a monitor that silently fails to alert is
worse than no monitor. The finished rewrite targets **100 % line coverage**, enforced in CI
with `cargo llvm-cov --fail-under-lines 100` from Phase 2 onward (each phase lands fully
covered; coverage is never "caught up later"). The gate measures non-test code in `src/`
only: the `tests/` directory and `#[cfg(test)]` modules are excluded from the denominator
(configure `cargo-llvm-cov` accordingly when adding the gate in Phase 2).

How that stays honest and achievable:

1. **Logic is separated from I/O and time** (§3): tick functions take clock values and base
   URLs as parameters. There is no branch that can only be reached by waiting for wall-clock
   time or by a real external service being down.
2. **Network branches are covered with a local mock server** (`wiremock`): success, wrong
   status, timeout (delayed response beyond the client timeout — use a shortened-timeout
   client constructor in tests), connection refused (unbound port), redirect responses.
3. **The HTTP endpoint is tested in-process** via axum's `Router` + `tower::ServiceExt::oneshot`
   — no sockets needed for handler coverage; one real-socket smoke test covers the bind path.
4. **Error paths are first-class:** every `Err` arm, every validation failure message, and
   every early return in §2 has a named test. Uncovered error arms are treated as bugs in
   the tests, not accepted noise.
5. **Exclusions are the exception and must be justified inline.** The only anticipated one
   is the few lines of `main()` wiring (process entry, signal hookup), which are instead
   exercised by an end-to-end test: spawn the release binary with a temp config against
   mock upstreams, poll `GET /`, send SIGTERM, assert exit 0. If a line needs a coverage
   exclusion, it carries a comment explaining why it cannot be reached from a test.
6. **Test taxonomy:** unit tests live beside each module; integration tests in `tests/`
   cover config-file → running-process flows; the notification wording vectors (§2.6) are
   table-driven regression tests inherited from the legacy system's test suite.

Each phase in §6 ends with its **required tests** — they are part of the phase's
deliverable, not an afterthought, and a phase is not done until they pass with the
coverage gate green.

---

## 6. Execution phases

Phases are strictly ordered; each produces something runnable/verifiable and its full test
suite. Estimated sizes assume familiarity with Rust but no prior knowledge of this project.

### Phase 0 — schedule-grammar spike (throwaway, do first)

The only real unknown in the plan. `hron` is young (v1.0.0, sparsely documented); prove it
before building on it.

**Tasks**
- Empty repo (`varde`), `cargo init --name varde`, commit a `rust-toolchain.toml` pinning
  the current stable release (updated deliberately, never implicitly). Cargo.toml license:
  `ISC` (inherited from the legacy `package.json`). The image name is `bendiksolheim/varde`
  (confirmed available on Docker Hub, 2026-07-18).
- Tiny spike binary: parse a list of expressions with `hron` and print the next 3
  occurrences of each.
- Expression corpus to try: `Every 10 minutes`, `Every 1 minutes`, `every 30 seconds`,
  `every 2 hours`, `every 1 minute`, `every weekday at 09:00`, `at 10:00 am`, plus garbage
  (`banana`, ``, `every -5 minutes`).

**Decision to make (record the outcome in this file):**
- Adoption requires only the interval floor of §2.2.2 (`every N seconds|minutes|hours|days`);
  calendar-form support is a bonus, never a requirement.
- `hron` handles the corpus (with normalization) → adopt it.
- `hron` has gaps → fallbacks, in order: (a) `hron` + a wider normalization layer;
  (b) a hand-rolled `every N seconds|minutes|hours|days` parser behind the same seam —
  this covers every expression observed in real configs, so it is a fully acceptable
  outcome, not a compromise; (c) `english-to-cron` + `cron` crates.

**Exit criteria**
- Both real-world forms (`Every 10 minutes`, `Every 1 minutes`) yield occurrences ~10 min /
  ~1 min apart with the chosen approach.
- Garbage inputs produce errors, not panics.
- Spike code is deleted; the knowledge moves into Phase 2.

**Outcome (2026-07-18): hron rejected; hand-rolled interval parser adopted (fallback b).**
Spike results against hron v1.0.0:
- Bare interval forms do not parse: `every 10 minutes` → `expected 'from'` (hron's grammar
  requires a `from HH:MM to HH:MM` window). Rewriting to
  `every 10 minutes from 00:00 to 23:59` parses and is wall-clock aligned
  (12:34:56 → 12:40:00), so minutes/hours *could* be rescued by a rewrite layer.
- **`every 30 seconds` → `unknown keyword 'seconds'`** — seconds are absent from hron's
  grammar entirely, and they are part of the required floor (§2.2.2). Unrescuable.
- Calendar forms work (`every weekday at 09:00` correctly skips the weekend); garbage
  inputs (`banana`, empty, `every -5 minutes`, `every 0 minutes`) all error, no panics.

Decision: hand-rolled `every N seconds|minutes|hours|days` parser behind the §2.2.5 seam,
epoch-anchored (`next_after(t)` = smallest multiple of N strictly after t) which equals
wall-clock alignment for all real-world intervals. Calendar forms fail at startup and the
README documents the supported grammar. `jiff` is kept for all timestamps. The hybrid
(hand-rolled + hron for calendar bonus forms) was offered and declined — no real config
uses calendar forms, and the extra dependency + rewrite layer isn't worth the bonus.

**Tests:** none kept (spike). The corpus above becomes Phase 2's test fixtures.

### Phase 1 — project scaffold + config module

**Tasks**
- Crate layout from §3, CI `ci.yml` (fmt, clippy, test; add the coverage gate at the end of
  Phase 2), `README.md` stub.
- `config.rs`: serde types for §2.1 (externally-tagged-by-`type` heartbeat union), loading
  via `CONFIG_PATH` with `/config/config.json` default, two-pass parse (generic JSON for
  unknown-key detection → typed deserialize), validation (URL, status-code range, UUID
  syntax, duplicate service names, non-empty names/topics), readable error type whose
  `Display` names the offending field.
- Schedules are validated in this phase only as "non-empty string"; real parsing arrives in
  Phase 2 and plugs into the same validation pass.

**Required tests** (unit, in `config.rs`; fixtures under `tests/fixtures/`)
- Full example config from §2.1 parses; every field lands where expected.
- Minimal config (`{"services": []}`) parses; `heartbeat`/`notify` default to none.
- Heartbeat union vectors: accepts `healthchecks.io` with valid UUID — including the
  loose-validation vector `12345678-1234-1234-1234-123456789012` (§2.1); accepts `httpbin`
  without UUID; `httpbin` *with* a stray `uuid` parses and warns; rejects `healthchecks.io`
  missing UUID; rejects malformed UUID (`not-a-valid-uuid`, an unhyphenated hex string);
  rejects unknown type (`"invalid"`).
- Rejections, each asserting the error message names the culprit: missing file, invalid
  JSON, missing `services`, empty service name, relative/garbage URL, `okStatusCode` 0,
  600, and fractional (`200.5`), negative `minutesBetween`, duplicate service names,
  duplicate notify topics.
- Unknown-key handling: config with `nodes`, a top-level typo key, **and a nested typo**
  (`okStatuscode` inside a service entry) parses successfully **and** the warning path
  runs for each (assert via the returned warnings list — have the loader return
  `(Config, Vec<Warning>)` rather than logging directly, so this is testable).
- `CONFIG_PATH` override honored; default path used when unset.

**Exit criteria:** `cargo test` green; parsing the real production config succeeds.
(Reality check 2026-07-18: the production config has 4 services, a `healthchecks.io`
heartbeat, and one ntfy topic — not the 6-service/`httpbin` shape this plan assumed.
A **sanitized** copy — placeholder UUID and ntfy topic, everything else verbatim — lives
in `tests/fixtures/production.json` as the canonical legacy fixture; the real values stay
out of the repo since the ntfy topic and hc-ping UUID are write-capable secrets.)

### Phase 2 — schedule module

**Tasks**
- `schedule.rs` implementing the seam from §2.2: `parse(expr) -> Result<Schedule, ScheduleError>`;
  `Schedule::next_after(t: Timestamp) -> Timestamp`; `sleep_until_next()` helper for loops.
- Normalization: trim, lowercase, whitespace-collapse, singular/plural repair.
- Wire into config validation: every `schedule` field in the config must parse at startup;
  error output includes the original (pre-normalization) expression.
- Add the CI coverage gate (`cargo llvm-cov --fail-under-lines 100`) now that real logic
  exists.

**Required tests**
- Normalization: each corpus entry from Phase 0 → expected normalized form (table-driven);
  idempotence (`normalize(normalize(x)) == normalize(x)`).
- Parsing: `Every 10 minutes`, `Every 1 minutes`, `every 30 seconds`, `every 2 hours` all
  parse; successive `next_after` results are spaced by the expected interval.
- Calendar forms (if the chosen parser supports them): `every weekday at 09:00` skips
  weekends.
- Errors: empty string, `banana`, `every -5 minutes`, `every 0 minutes` (decision:
  startup error, per §2.2.2) — all return `Err` with the original expression in the message;
  no panics (add a small fuzz-ish loop over random ASCII strings asserting "returns
  Ok or Err, never panics").
- Config integration: a config with one bad schedule fails startup validation and the error
  names that expression.

**Exit criteria:** tests green, coverage gate green.

### Phase 3 — state, health checks, status endpoint (usable MVP)

**Tasks**
- `state.rs`: `AppState` per §3; constructor takes the config so the endpoint can render
  configured-but-unchecked services.
- `check.rs`: `check_once(client, &service) -> ServiceStatus` implementing §2.3 exactly
  (GET, exact status match, latency measurement, error → down); the check loop
  (immediate first run, then `sleep_until_next`, writes state, never panics).
- `server.rs`: router + handler per §2.7 (JSON shape, field names, RFC 3339 UTC timestamps,
  config-order services, `operational` over checked services only, 200/500).
- Client construction: rustls, `redirect::Policy::none()`, 10 s timeout (injectable in
  tests), UA `varde/<version>`.
- `main.rs`: wire config → state → spawn check loops → serve; SIGTERM/SIGINT graceful
  shutdown.
- Milestone: point it at a real config — it is now a working uptime checker with a JSON
  status page.

**Required tests**
- `check_once` against a mock server (wiremock): 200 vs `okStatusCode` 200 → up with
  latency; 500 vs 200 → down **with latency recorded** (completed responses always carry
  latency, §2.3); **301 vs `okStatusCode` 301 → up and the redirect is not
  followed** (assert the mock's `Location` target got zero requests); 301 vs 200 → down;
  timeout (mock delays past a test-shortened client timeout) → down, latency none;
  connection refused (unbound port) → down, latency none; malformed/garbage response handled.
- Latency plausibility: mock delays ~50 ms → recorded latency ≥ 50 ms.
- Check loop: state entry is written after a tick; a panic inside one tick's future does
  not kill subsequent ticks (or prove ticks cannot panic by construction).
- Endpoint via `oneshot`: empty state → all-null entries, `operational: true`, 200;
  mixed up/down → 500 and correct per-service JSON, covering both down-with-latency
  (wrong status) and down-with-`null`-latency (transport error); all up → 200;
  unknown path → 404; non-GET → 404 (§2.7); response is valid JSON with
  exactly the §2.7 field names; services appear in config order; timestamp format matches
  RFC 3339 second precision.
- One real-socket smoke test: bind port 0, GET /, assert parseable response (covers the
  serve/bind path). `PORT` parsing: valid override honored; `0`, `65536`, and non-numeric
  values → startup error.
- End-to-end (in `tests/`): spawn the binary with a temp config pointing at a wiremock
  service; poll `GET /` until the service shows up; flip the mock to failing; observe the
  endpoint flip to 500; SIGTERM → exit 0.

**Exit criteria:** MVP runs against a real config; tests + coverage gate green.

### Phase 4 — heartbeat + notifications

**Tasks**
- `heartbeat.rs`: `heartbeat_tick(client, statuses, config, base_url)` per §2.4; loop
  starting at first scheduled occurrence. `base_url` defaults to `https://hc-ping.com` /
  `https://httpbin.org` and is overridden in tests.
- `notify.rs`: `format_notification_message(failing: &[&str]) -> Option<String>` per §2.6;
  `notify_tick(...)` implementing the full state machine of §2.5 (rate limit, recovery,
  state-update-only-on-successful-send); loop as above.
- `main.rs`: spawn these loops when configured.

**Required tests**
- Message format vectors (table-driven; inherited from the legacy suite and §2.6):
  0 failing → `None`; 1 → `1 service down: my-service-0`;
  2 → `2 services down: my-service-0 and my-service-1`;
  3 → `3 services down: my-service-0, my-service-1 and my-service-2`;
  one down + one up → only the down one is named; also 4+ services (comma/`and` placement)
  and names containing commas.
- Heartbeat tick against a mock: all checked services up → exactly one GET to
  `/{uuid}` (healthchecks.io) or `/get` (httpbin); any service down → **zero** requests;
  empty state (nothing checked yet) → ping sent (vacuously OK); unchecked services ignored
  while checked ones decide; ping network failure → tick returns normally (logged, no panic,
  no state corruption); ping answered with non-2xx → treated as failure (§2.4), logged,
  tick returns normally.
- Notify tick state machine (drive `now` explicitly):
  - all up, `was_down=false` → no request, state unchanged;
  - one down, `last_sent=None` → POST with body/`Title: Service down`/`Tags: warning`
    asserted verbatim; `last_sent=now`, `was_down=true`;
  - still down, elapsed < `minutesBetween` → no request;
  - still down, elapsed > `minutesBetween` → second POST;
  - elapsed exactly == `minutesBetween` → whichever the spec's "more than" dictates (no
    send) — test the boundary;
  - recovery: down then all up → exactly one POST with `Services recovered` /
    `white_check_mark` / `All services back up`; next tick sends nothing;
  - recovery is not rate-limited: recovery fires even when a down-message was just sent;
  - recovery resets the rate limit (§2.5.3): after a recovery, a *new* outage sends a
    down-message immediately, even less than `minutesBetween` after the previous one;
  - a down-POST answered with non-2xx counts as a failed send (state not updated);
  - send failure (mock returns 500 / connection refused): `last_sent` NOT updated → next
    tick retries; recovery-send failure keeps `was_down=true` → retried;
  - two notify entries: independent state (one topic's send does not rate-limit the other);
  - `minutesBetween: 0` → every tick during an outage sends.
- End-to-end extension: binary + mock service + mock ntfy/hc endpoints. Base URLs are
  injected via the env vars `VARDE_HC_BASE_URL` and `VARDE_NTFY_BASE_URL` (defaulting to
  the real hosts; the config schema stays untouched and legacy-compatible — mention them
  only in a test/advanced section of the README). Kill the mock service → observe ntfy
  POST; restore → observe recovery POST and heartbeat resume.

**Exit criteria:** feature-complete per §2; tests + coverage gate green.

### Phase 5 — packaging, CI release, docs

**Tasks**
- Dockerfile (§4), `.dockerignore`, compose example.
- `release.yml` multi-arch build+push; version tagging scheme (git tag `vX.Y.Z` → image
  tags `X.Y.Z` + `latest`).
- README: what it is, config reference (§2.1 duplicated in user-facing form), schedule
  grammar with examples, endpoint contract, deployment instructions, `RUST_LOG`/`PORT`/
  `CONFIG_PATH` reference.

**Required tests / verification**
- CI runs the full gate (fmt, clippy, tests, 100 % coverage) on every push — verify it
  fails on a deliberately broken branch before trusting it.
- Build the image locally for the NAS architecture; `docker run` with a sample config
  mounted; verify `GET /`, memory usage (`docker stats` — expect single-digit MiB), image
  size, and that HTTPS pings work from `FROM scratch` (proves the embedded-roots choice).
- Container smoke test in CI: build amd64 image, run with a fixture config against a
  containerized mock, curl `GET /`, assert 200. (Automates the previous point's core.)

**Exit criteria:** pushing a tag produces a pullable multi-arch image that passes the smoke
test.

### Phase 6 — cutover from the legacy system

**Tasks (operational, on the NAS)**
1. Pull the image.
2. Copy the production config; **remove `heartbeat` and `notify` from the copy** (avoid
   double pings/notifications during parallel running).
3. Run varde alongside the legacy container on port 3001 with the trimmed config.
4. Parallel-run for a few days: compare `GET :3001/` with the legacy UI; deliberately stop
   a non-critical service and verify both systems detect it within one schedule interval.
5. Cut over: restore `heartbeat`/`notify` to the new config (switching any `httpbin`
   heartbeat to a real `healthchecks.io` UUID is strongly recommended — `httpbin` alerts
   nobody), point compose at the new image on port 3000, stop and remove the legacy
   container, delete its now-unused SQLite files from the config directory.
6. Archive the legacy repository with a pointer to the new one.

**Verification (this phase's "tests")**
- During parallel run: zero missed detections vs. legacy over the observation window.
- After cutover: healthchecks.io shows pings arriving on schedule; a deliberate outage
  produces an ntfy message and a recovery message; stopping the monitor container itself
  produces a healthchecks.io "late" alert (the dead man's switch works end to end).
- `docker stats` confirms the resource win that motivated the rewrite.

---

## 7. Risks & open items

- **`hron` maturity** is the only dependency risk; Phase 0 retires it before anything is
  built on top, and the parser seam (§2.2.5) caps the blast radius of a later swap. Since
  the required grammar is only the interval form (§2.2.2), the hand-rolled fallback is a
  complete answer, not a degraded one.
- **100 % line coverage is a discipline, not a metric to game.** If a line is genuinely
  unreachable from tests, prefer restructuring the code (§5's dependency-injection rules)
  over sprinkling exclusions; every exclusion needs an inline justification.
- **Restart re-notification** is accepted (§1); if it proves annoying in practice, the
  designed fix is a tiny JSON state snapshot on shutdown — out of scope for the rewrite.

Decided in the pre-implementation interview (2026-07-18):

- **Interval anchoring (§2.2.4): wall-clock aligned**, matching legacy later.js — `every 10
  minutes` fires at :00/:10/:20. Implemented as epoch-anchored multiples of the interval
  (`next_after(t)` = smallest multiple of N strictly after t), which is deterministic and
  equals wall-clock alignment for all real-world intervals; documented in the README.
- **`was_down` is set unconditionally** when something is failing (§2.5.4 literal reading);
  "state updated only on successful send" (§2.5.5) governs `last_sent` only. Consequence: a
  failed down-send followed by recovery produces a recovery message for an unannounced
  outage — accepted as harmless and informative.
- **Rate limiting ignores changes to the failing set** (§2.5.4 literal reading): a second
  service failing mid-window does not bypass the limit; one timer per notify entry.
- **Duplicate service names / notify topics: exact byte equality**, case-sensitive, no
  normalization.
- Execution scope: Phases 0–5 built here (crate at repo root, one commit per phase on
  `main`, no remote yet — repo is pushed and Docker Hub secrets wired by the owner
  afterwards). Phase 6 is manual/operational.
- Phase 1's canonical legacy fixture: a spec-faithful placeholder until the real production
  `config.json` is pasted in, then swapped and the exit check re-run.
- Phase 0 spike: if `hron` fails the corpus, **check in before falling back** — the
  fallback ladder is not walked autonomously.

Previously-open items, now decided (2026-07-18, after auditing the legacy code):
test base-URL injection uses env vars (§ Phase 4: `VARDE_HC_BASE_URL`/`VARDE_NTFY_BASE_URL`);
`every 0 minutes` is a startup error (§2.2.2); the mock crate is `wiremock` (§3);
recovery resets the notify rate limit (§2.5.3); non-2xx responses count as send/ping
failures (§2.4, §2.5.5); unknown-key warnings are recursive (§2.1); non-GET requests get
404 (§2.7); `PORT` must be `1..=65535` (§2.7); the coverage gate measures `src/` non-test
code only (§5).
