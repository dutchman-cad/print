# Running a print farm with Dutchman Print

This guide takes you from several printers to a working farm. It is a walk-through:
each step links to [print.md](print.md) (the companion guide) or [print-api.md](print-api.md)
(the job API) for the full detail instead of repeating it.

1. [What Print does for a farm](#what-print-does-for-a-farm)
2. [Configure several printers](#configure-several-printers)
3. [Run the service](#run-the-service)
4. [The daily loop](#the-daily-loop)
5. [Bed clearance](#bed-clearance)
6. [Stopping](#stopping)
7. [History](#history)
8. [Using it from SolidCraft](#using-it-from-solidcraft)
9. [Troubleshooting](#troubleshooting)

## What Print does for a farm

Print keeps one list of your printers, prepares 3MF files with your own slicer, holds a
first-in, first-out queue of prepared jobs, and suggests which idle printer can take
each queued job. It records finished jobs and bed checks in a history.

Print never starts a print by itself. Every start, pause, resume, cancel, Stop, and bed
acknowledgement needs a person to confirm it. Each confirmation is tied to what the printer
reported when the person reviewed it; if the printer changed, the action is refused and you
confirm again. Proposals reserve nothing and start nothing. Nothing retries a start
automatically, including after a restart.

Print is not released yet and has been tested against simulated printers. Before you rely
on a printer, work through the [hardware checklist](hardware-checklist.md) for its kind,
one printer at a time.

## Configure several printers

A farm config uses `kind = "printers"` for the backend and one `[[printers]]` block for each
printer. Each block has its own `kind`, from this list: `prusalink`, `octoprint`,
`moonraker`, `snapmaker-u1`, `snapmaker-2`, `snapmaker-j1`, `duet`, `duet-sbc`, `repetier`,
`bambu`, or `elegoo-sdcp`. Ids must be unique (1 to 64 ASCII letters, digits, hyphens, or
underscores); a config holds at most 64 printers. Some kinds are monitoring only; see the
table in the [README](../README.md#printers) and the section for each kind in
[print.md](print.md).

This example mixes PrusaLink and Moonraker printers and uses one PrusaSlicer for both. The
keys are placeholders:

```toml
[backend]
kind = "printers"

[slicer]
kind = "prusaslicer"
timeout_seconds = 600

[[printers]]
kind = "prusalink"
id = "mini"
name = "Shelf MINI"
model = "Original Prusa MINI & MINI+"
url = "http://mini.local"
api_key = "REPLACE_WITH_YOUR_PRINTER_KEY"
printer_profile = "Original Prusa MINI & MINI+"
print_profile = "0.20mm QUALITY @MINI"
filament_profile = "Generic PLA @MINI"

[[printers]]
kind = "moonraker"
id = "klipper"
name = "Bench Klipper"
model = "Commissioned Klipper printer"
url = "http://klipper.local:7125"
api_key = "REPLACE_WITH_YOUR_MOONRAKER_KEY"
printer_profile = "Commissioned machine"
print_profile = "Reviewed print settings"
filament_profile = "Loaded material"
resume_commissioned = false
```

Make the file private (`chmod 600 config.toml` on Unix); Print refuses a config that group or
other users can read. To write a first printer, run `dutchman-print init`, which asks
questions on a terminal and contacts nothing. Add further printers by hand, or run `init`
again with `--output` and copy the block. The key names for each kind are in its section of
[print.md](print.md); the slicer rules (named `[slicers.<name>]` tables, which kind each
printer needs) are in [Slicer](print.md#slicer), and the file location rules in
[Config](print.md#config).

Check that Print can read every printer:

```sh
dutchman-print --config config.toml printers
```

It prints one line per printer, such as `Shelf MINI: idle`.

## Run the service

The queue needs the running service:

```sh
export DUTCHMAN_PRINT_PASSWORD='choose-a-password'
dutchman-print --config config.toml serve --listen 127.0.0.1:8730 --data-dir /srv/print-data
```

- **Data folder.** `--data-dir` (or `DUTCHMAN_PRINT_DATA`) holds the queue, tracked jobs,
  challenges, and `history.jsonl`. The default is in the system temporary folder, which
  system cleanup can remove, so choose a persistent private folder and use the same one
  every time. Only one server may use a folder at once. After a killed process, remove
  `serve.lock` in it only once the process has stopped.
- **Access.** `serve` listens on `127.0.0.1:8730` by default. Set
  `DUTCHMAN_PRINT_PASSWORD` unless everyone who can reach the port may read job names and
  control jobs; the user name defaults to `dutchman` (`--user`). See
  [Server settings and hardening](print.md#server-settings-and-hardening).
- **Restarts.** The queue, its order, bindings, refusal reasons, and attempt markers survive a
  restart. Bed acknowledgements do not. Tracked jobs are reconciled with live printer status
  before control.
- **Shutdown.** On Ctrl-C or SIGTERM, `serve` closes printer connections within three
  seconds. It sends no machine action, so running prints continue on the printers.

## The daily loop

Queue commands talk to the running server. They use `--server` (default
`http://127.0.0.1:8730`) and `--user`, and read `DUTCHMAN_PRINT_PASSWORD` for the password.
The full description is [Print farm queue](print.md#print-farm-queue); the routes are in the
[API](print-api.md#print-farm-queue).

### 1. Prepare and queue a job

`queue add` uploads the 3MF, slices it with the presets of the printer you name in
`--printer`, and appends it to the queue:

```sh
dutchman-print queue add bracket.3mf --printer mini
dutchman-print queue add lid.3mf --printer mini --target mini
dutchman-print queue add gear.3mf --printer mini --target any --kind prusalink
```

- `--target` is a printer id or `any` (the default). A named target limits the job to that
  printer.
- `--kind` and `--profile` are optional filters that narrow which printers may take the job:
  `--kind` matches a printer's `kind`, and `--profile` matches its `printer_profile`.
- Printers must share the preparation printer's kind and its resolved printer, print, and
  filament preset names. A job prepared on `mini` will not be proposed to `klipper`
  because the kinds differ. To run a part on `klipper`, queue it with `--printer klipper`.
- A different printer of the same kind and presets gets its own preparation before its Start
  review, so its tooling and readiness checks still apply. Check yourself that printers
  sharing presets are physically compatible (nozzle, plate, loaded material).

### 2. Read the queue and the proposals

```sh
dutchman-print queue list
dutchman-print queue proposals
dutchman-print queue move ENTRY_ID 0
```

Proposals walk the entries in order and the printers in config order. Each printer is
proposed once, to its earliest compatible entry. Busy, offline, uncleared, incompatible,
and monitoring-only printers are skipped, and an entry with no match does not block later
ones. An entry without a match shows a reason. `move` takes a zero-based position.

### 3. Check the bed and dispatch

Look at the printer's bed yourself: empty, clean, the right plate. Then:

```sh
dutchman-print queue dispatch ENTRY_ID --printer mini --bed-clear
```

`--printer` must be the printer the entry is currently proposed for. `--bed-clear` asks for
a separate Clear confirmation first; Print then shows a Start summary, for example
`Start printing 'bracket' on Shelf MINI (idle, bed clear)?`, for you to approve. Each is
its own question. `--yes` approves both without asking; use it only in a script that
follows your own bed check. Once you review an entry for a printer it stays bound to that
printer; to change it before any attempt, `queue remove` it and add the original job
again.

A successful dispatch removes the entry from the queue and returns the tracked job.

### 4. Monitor

`dutchman-print --config config.toml printers` shows each printer's state. The same
information is in `GET /api/v1/printers`, and a tracked job is in
`GET /api/v1/jobs/{id}`; for a queue entry, use its `prepared` job id. The CLI commands
`status`, `pause`, `resume`, and `cancel` act on jobs started by the CLI, which keeps its
own state separate from the service. For service jobs use the API or SolidCraft, and for
pause and resume each action needs a fresh confirmation challenge
([Confirmation](print-api.md#confirmation-for-machine-actions)).

### 5. When something fails

- **A refusal leaves the entry queued** with a `reason`. Fix the cause and try again.
- **After a start attempt begins**, the entry is marked `attempted` and can never be
  dispatched again, even after a failure, an uncertain result, or a restart. Its printer stays
  reserved, so it gets no proposals, until you remove the entry.
- **Reconcile first.** Read the entry's `prepared` job (`GET /api/v1/jobs/{id}`) and look at
  the printer. Then run `dutchman-print queue remove ENTRY_ID`. Removing an entry never
  cancels or retries anything and keeps the job records.
- **There is no automatic retry.** To print the part again, add it to the queue again.

## Bed clearance

Print will not start a job until a person has acknowledged that the printer's bed is checked
and clear, because it cannot see the bed. The acknowledgement is its own confirmed action,
separate from Start, and it is recorded in [history](#history).

- The running server holds the acknowledgement; a separate command cannot keep it. The CLI
  `clear <printer>` only explains this; use `queue dispatch --bed-clear` or the API route
  `POST /api/v1/printers/{id}/clear`.
- Only idle printers can be acknowledged.
- While one is held, the server polls that printer every 5 seconds
  (`--clearance-interval-seconds`, 2 to 30). It is withdrawn when the printer leaves idle,
  a read fails, the job identity changes, after 15 minutes (`--clearance-expiry-minutes`,
  1 to 120), or on restart. Any start attempt consumes it, even a failed one.
- Printers whose own firmware reports the bed state: a PrusaLink printer in `READY` state
  counts as the operator's bed-clear signal and needs no remote acknowledgement; `IDLE`
  still does. Print has no bed sensor of its own, and clearance is not proof that nothing
  moved between polls.

Details: [Controls and bed clearance in the API](print-api.md#controls) and each printer's section in
[print.md](print.md).

## Stopping

- **Cancel a job.** Cancels a tracked job that is ready, printing, or paused:
  `POST /api/v1/jobs/{id}/cancel` with a confirmation (CLI jobs: `dutchman-print cancel
  JOB_ID`). A ready job cancelled before Start is cleaned up.
- **Stop a printer.** Stops whatever the printer is running, including work started at
  its screen or by another program:

  ```sh
  dutchman-print --config config.toml stop mini
  ```

  Print shows the live job's name, identity, state, and progress and asks you to confirm.
  `--yes` skips the prompt. Stop needs no bed acknowledgement and withdraws the bed
  clearance. It sends one command and waits up to 25 seconds; it never resends. If the
  result is unconfirmed, read the printer's status, and check and acknowledge the bed again
  afterwards.
- **External jobs.** Jobs not started by Print are not tracked, appear in no history, and
  can only be ended with printer Stop. Not every kind supports Stop; the `controls` list in
  `GET /api/v1/printers` says what each printer accepts. Details: [Stop a printer](print.md#stop-a-printer-and-clean-staged-files).

## History

Finished tracked jobs and bed acknowledgements are saved in the data folder.

```sh
dutchman-print --data-dir /srv/print-data history --printer mini --outcome done --since 2026-10-01T00:00:00Z --limit 20
dutchman-print --data-dir /srv/print-data history --json
```

Filters combine: `--printer` is a printer id, `--outcome` is one of `done`, `cancelled`,
`failed`, `stopped`, `ended`, `unresolved_after_restart`, or `bed_clear`, `--since` is an
inclusive RFC 3339 time, and `--limit` is 1 to 10,000 (default 100). Newest records come
first. `stopped` is a tracked job ended by printer Stop; `ended` means the printer could not
establish how the job ended; `unresolved_after_restart` means an attempted job could not be
reconciled at startup, which does not say whether it succeeded. The API route is
`GET /api/v1/history` with the same filters as query fields. Records hold metadata only,
and the journal keeps the newest 10,000. See [Print history](print.md#print-history) and
the [API](print-api.md#print-history).

## Using it from SolidCraft

SolidCraft's print plugin uses the same job API as the commands above. The documentation
here describes the Plugin API and what it implies, not the plugin's panel layout; the plugin
is in [dutchman-cad/plugins](https://github.com/dutchman-cad/plugins).

- The host sends the 3MF as a raw body with `Content-Type: model/3mf`.
- For each machine action, the plugin's read step fetches a challenge from
  `POST /api/v1/confirmations` and returns its summary for the host's own confirmation
  dialog. Only after the person approves does the host attach a fresh `Plugin-Confirmation`
  header to the action step. Clear and Start need separate challenges and approvals.
- The queue is available over `GET/POST /api/v1/queue`, `GET /api/v1/queue/proposals`,
  `POST /api/v1/queue/{id}/confirmations`, and `POST /api/v1/queue/{id}/dispatch`
  ([API](print-api.md#print-farm-queue)). Direct job Start refuses a job that is queued or
  bound to a queue entry, so dispatch it from the queue.
- Printer `controls` tell a client which buttons to show; fresh state and confirmation still
  decide whether an action can run. See [Plugin API](print.md#plugin-api).

## Troubleshooting

Messages are quoted as Print writes them. Clients should branch on the status code; the
text is for people ([Errors](print-api.md#errors)).

| Message | What to do |
|---|---|
| `No compatible idle printer with a cleared bed is available.` | The entry shows this reason in `queue proposals`. Wait for a matching printer to be idle, check its bed, and acknowledge it. Check the entry's `--target`, `--kind`, and `--profile` filters and that a printer has the same kind and presets. |
| `Choose this entry's current proposed printer and confirm again.` | Run `queue proposals` and dispatch with the printer it names. |
| `The proposal changed. Read fresh status and confirm again.` | Read `queue proposals` again, then dispatch again. |
| `This entry is already reviewed for another printer. Remove it and queue the original job again.` | An entry stays bound to the printer you reviewed. `queue remove` it and `queue add` the file again. |
| `Check that <name> has a clear bed, then acknowledge it in this run or through the server for <id>.` | Look at the bed, then dispatch with `--bed-clear`. |
| `<name> is not idle. Wait before acknowledging its bed.` | Wait until the printer is idle. |
| `The printer changed. Read fresh status before acknowledging its bed.` | Read the printer's status and try again. |
| `Printer readiness is unknown or the printer is busy. Read fresh status before starting.` | Check the printer; try again when it is idle. |
| `The printer changed since you confirmed; confirm again.` | Review the new summary and confirm again. |
| `Confirmation is invalid, used, or expired. Confirm again.` | Challenges last 60 seconds and work once. Dispatch again for a new one. |
| `This action needs confirmation in SolidCraft (or --yes on the command line).` | An API call lacked a confirmation. Request a challenge first. |
| `Dispatch was already attempted. Read the tracked job and reconcile its outcome; never retry automatically.` | See [When something fails](#5-when-something-fails). Read the `prepared` job, check the printer, then `queue remove`. |
| `This job is already queued.` | Each prepared job can be queued once. |
| `The queue is full. Remove entries first.` | The queue holds 5,000 entries. |
| `Queue entry not found. List the queue again.` | Run `queue list` for current ids. |
| `Queue matching timed out. Read printer status and try proposals again.` | A printer was slow to answer. Run `queue proposals` again. |
| `This printer does not support this action. Choose a supported control.` | Check the printer's `controls`. |
| `Another server is using this data folder. Stop it first; remove serve.lock only after it has stopped.` | Stop the other server, or use another `--data-dir`. |
