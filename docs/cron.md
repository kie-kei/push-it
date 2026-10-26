# Scheduled requests (cron)

An app can schedule GET requests to its own WASM component in `pushit.yaml`:

```yaml
name: reporting-bot
cron:
  - name: report
    schedule: "0 9 * * *"
    path: /_cron/report
  - name: cleanup
    schedule: "*/15 * * * *"
```

Names are unique within the app and must be DNS labels: 1–63 lowercase
letters, digits or hyphens, without a leading/trailing hyphen. `name` and
`schedule` are required. `path` defaults to `/_cron/<name>` and must start
with `/` and be a valid request URI. An omitted or empty `cron` list disables
scheduled requests.

## Schedule semantics

Schedules have exactly five fields: minute, hour, day of month, month, day
of week. All times are **UTC**, with no daylight-saving adjustment. Lists,
ranges and steps are supported (for example `0 9 * * MON-FRI`). Weekdays
use Unix numbering: `0` or `7` is Sunday, `1` is Monday, through `6` for
Saturday; `SUN`–`SAT` names are also accepted. When both day of month and
day of week are restricted, either matching field triggers the job.
Second/year fields and shorthand expressions such as `@daily` are not accepted.

The controller scans active apps every **30 seconds**. Deploying, activating,
rolling back or deactivating a revision changes scheduled jobs within one
tick. Only active revisions run. New or changed jobs start at the first
occurrence strictly after discovery: there is **no catch-up after downtime**.
A late scan runs a due job once and skips other missed occurrences.

Each app/name pair has at most one run in flight, including across revision
changes or removing and re-adding a job. Occurrences while it is running are
skipped, not queued. Changing a job's path or schedule resets its next due
time; unchanged jobs retain their schedule across deploys. In-flight runs
finish against their pinned original revision even if the app is deactivated.

## Invocation and logs

Requests go straight to the app component, not through the public router:
GET-only, empty body, configured path and `Host: <app domain>`. The controller
provides the same manifest environment, decrypted secrets and capability
grants as an ordinary function request, plus:

- Environment variable `PUSHIT_CRON=<job name>` (overrides any manifest value).
- Header `x-pushit-cron: <job name>`.
- Identity headers `x-user-id: cron`, `x-user-name: cron`,
  `x-user-email: cron@pushit.local`.

The usual runtime handler timeout (30 seconds) applies. The response body
is discarded. A failed run is logged,
not retried; later scheduled occurrences remain eligible.

Guest stdout/stderr and `wasi:logging` output go to the app's normal log.
Each run also appends a result line:

```text
cron 'report' GET /_cron/report -> 200
```

Errors include an error summary instead of a status. Read these through
`pushit logs reporting-bot` or `GET /api/apps/reporting-bot/logs`.
Scheduler scan/run failures also appear in controller warning logs.

## Controller configuration

`PUSHIT_CRON_TICK_SECS` overrides the scan interval in whole seconds. It is
read once at startup; missing, invalid or zero values fall back to 30 seconds.
For local smoke tests, use `PUSHIT_CRON_TICK_SECS=1`. This changes scan
frequency, not the one-minute precision of five-field schedules.
