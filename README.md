# varde

A minimal uptime monitor for a home server. A *varde* is the old Norwegian beacon cairn —
a signal fire kept burning so the next station notices when it goes out.

varde checks services over HTTP on a schedule, keeps the latest result per service in
memory, reports upstream via a healthchecks.io heartbeat (dead man's switch), sends push
notifications through ntfy.sh during outages, and serves current status as JSON on `GET /`.

> Work in progress: this README is a stub and grows with each implementation phase
> (see `rust.md` for the full specification and plan).

## Configuration

Read from `/config/config.json` (override with the `CONFIG_PATH` environment variable).
See `tests/fixtures/full.json` for a complete example.

## Schedules

Schedule expressions use the interval grammar:

```
every N seconds|minutes|hours|days     (N ≥ 1)
```

Parsing is case-insensitive and tolerant of singular/plural mismatches and extra
whitespace — `Every 10 minutes`, `every 1 minutes`, and `every 1 minute` all work.
Calendar expressions from the legacy system's grammar (`every weekday at 09:00`,
`at 10:00 am`) are **not** supported and fail at startup with an error naming the
expression.

Occurrences are **wall-clock aligned** (anchored to the Unix epoch): `every 10 minutes`
fires at :00, :10, :20, …, not relative to process start. This matches the legacy
later.js behavior and is stable across restarts. If a check runs long, missed
occurrences are skipped, never replayed.
