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
