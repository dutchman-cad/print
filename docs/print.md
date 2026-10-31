# Dutchman Print

Running several printers? Start with the [print farm guide](farm.md).

Dutchman Print is a local print companion for SolidCraft and a standalone command line.
A backend prepares 3MF files, estimates jobs, and controls printers. The included
`simulated` backend is a demo: it contacts no printers and can optionally run a local slicer.
The `printers` backend supports individually configured local PrusaLink, OctoPrint, Moonraker (Klipper),
Snapmaker U1, Duet (standalone or with Duet Software Framework), Repetier-Server, and Bambu printers, and monitors Elegoo printers
that speak SDCP and Snapmaker 2.0, J1, and Artisan printers.

Build `dutchman-print` from this checkout with Rust 1.88 or newer:

```sh
cargo build --locked -p dutchman-print
```

It is not released yet; testing uses simulated printers. Put the executable beside
`dutchman` or on `PATH` to use `dutchman printers` and `dutchman print`.
Before relying on a printer, work through the [hardware checklist](hardware-checklist.md).

## Config

Choose a file with `--config FILE` or `DUTCHMAN_PRINT_CONFIG`; the option takes precedence.
Print reads only that file. Create it with private permissions (`chmod 600` on Unix).
Group and other permissions are refused before reading, even for the simulator. Backend
credentials belong in private config, never in logs or responses. Parser errors do not echo
config contents.

```toml
[backend]
kind = "simulated"

[[printers]]
id = "workbench"
name = "Workbench demo"
model = "Example printer"
```

To write this file for one printer, run `dutchman-print init` (see [Commands](#commands)).
Repeat `[[printers]]` to add printers. Leave it out for an empty fleet. Unknown backend kinds
produce an error naming the kind. With `kind = "printers"`, each printer's `kind` must be one
of `prusalink`, `octoprint`, `moonraker`, `snapmaker-u1`, `snapmaker-2`, `snapmaker-j1`,
`duet`, `duet-sbc`, `repetier`, `bambu` or `elegoo-sdcp`; any other kind is refused with that
list. Every printer that can start a job also asks for the bed check.
Config files are limited to 1 MiB and 64 printers.
IDs must be unique, with 1–64 ASCII letters, digits, hyphens, or underscores. Names are
nonempty plain text, at most 256 bytes; model descriptions are free text up to 256 bytes.

Without `--config`, the config path is:

| Platform | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/dutchman-print/config.toml`, or `~/.config/dutchman-print/config.toml` |
| macOS | `~/Library/Application Support/dutchman-print/config.toml` |
| Windows | `%APPDATA%\dutchman-print\config.toml` |

## Slicer

Install PrusaSlicer or OrcaSlicer separately and set it up in its own application first.
Dutchman runs its public command line as a separate program; it does not include or link
slicer code. The slicer's AGPL licence stays with that program. Cura is not supported yet.
PrusaLink requires PrusaSlicer and text G-code.

```toml
[backend]
kind = "simulated"
slice = true

[slicer]
kind = "prusaslicer" # or "orcaslicer"
# path = "/path/to/prusa-slicer"
# datadir = "/path/to/PrusaSlicer"
timeout_seconds = 600

[[printers]]
id = "workbench"
name = "Workbench demo"
model = "Example printer"
printer_profile = "Original Prusa MINI & MINI+"
print_profile = "0.20mm QUALITY @MINI"
filament_profile = "Generic PLA @MINI"
```

Profile values are names already installed in the slicer, including its system presets
and presets you saved there. Dutchman keeps no profile set. Job options (CLI flags or API
fields) override each printer's corresponding name. Omitted names leave the input project's
settings and slicer defaults in effect. Profile names must contain 1–256 bytes of nonempty
plain text without control characters and cannot start with `-`. This applies to config,
CLI flags, and both API upload forms; refusal says "Profile names can't start with '-'."
Job names are metadata only; uploaded files and slicer outputs use companion-built paths.
Unknown slicer kinds produce an error naming the kind.
Configure slicers with `[slicers.<name>]` tables, each containing `kind`, optional `path`,
optional `datadir`, and `timeout_seconds`. Set each printer's `slicer = "<name>"` to choose
one. Without a name, Dutchman selects the only slicer of the required kind: PrusaLink needs
`prusaslicer`, Bambu needs `orcaslicer`. Missing or ambiguous choices are refused.
OctoPrint, Moonraker, Snapmaker U1, Duet, and Repetier-Server accept either kind; select a named slicer when more than one is configured.
The simulator with `slice = true` uses the same per-printer selection; a simulated printer
without a kind can use the only configured slicer, or select one explicitly.
`[slicer]` remains a single-slicer shorthand; it cannot be combined with named slicers,
and printers using it omit `slicer`. Without `slice = true`, the simulator keeps its
size-based estimates.

An explicit `path` takes precedence and disables executable discovery. Otherwise Dutchman
searches each `PATH` directory in order: `prusa-slicer-console`, `prusa-slicer`, then
`PrusaSlicer` for Prusa; `orca-slicer`, then `OrcaSlicer` for Orca (`.exe` on Windows).
It next checks `/usr/bin`, `/usr/local/bin`, and `/opt` on Linux (directly and under the
application name); application bundles under `/Applications` and `~/Applications` on
macOS, including `Original Prusa Drivers`; and the application's folders under Program
Files, Program Files (x86), and LocalAppData on Windows.

Linux discovery then checks `/var/lib/flatpak/exports/bin`, followed by
`~/.local/share/flatpak/exports/bin`. The launcher names are `com.prusa3d.PrusaSlicer` for
Prusa and `io.github.softfever.OrcaSlicer`, then `com.orcaslicer.OrcaSlicer`, for Orca.
Both Orca IDs are public; the newer ID is listed on
[Flathub](https://github.com/flathub/com.orcaslicer.OrcaSlicer).
These exports are executable launcher scripts and receive the same slicer arguments.
For other locations, including AppImages, set `slicer.path` to the executable file.
A missing executable error tells you to set that field. Relative paths use the companion's
working folder. `datadir` selects the slicer's data folder; otherwise the slicer uses its
normal platform folder. Timeout defaults to 600 seconds; set 1–86400 seconds.

Orca profile lookup reads JSON preset files' `name` fields below `datadir/user` and
`datadir/system`, in `machine`, `process`, or `filament` subfolders. Without `datadir`, the
root is `$XDG_CONFIG_HOME/OrcaSlicer` (or `~/.config/OrcaSlicer`) on Linux,
`~/Library/Application Support/OrcaSlicer` on macOS, or `%APPDATA%\OrcaSlicer` on Windows.
When the selected Linux executable is named for either Orca Flatpak ID, lookup instead
uses `~/.var/app/<app-id>/config/OrcaSlicer`. An explicit `datadir` still takes precedence.
Missing or ambiguous names report the folders searched. Keep names unique across system
and user presets. Lookup skips symbolic links, searches at most eight folder levels and
20,000 entries, and limits each JSON file to 1 MiB and total JSON reads to 64 MiB per
profile lookup. Preset file paths must not contain semicolons, which Orca uses to separate
settings files. Orca CLI settings files need inherited values resolved. Dutchman follows
`inherits` names, preferring the same vendor or user account before searching the full
catalog, and writes merged settings into the private job folder. Child values override
parent values. Chains are limited to 32 presets; cycles, missing parents, ambiguous
parents and merged settings above 1 MiB produce an error. The 64 MiB read limit includes
inheritance reads.

Backends choose plain text G-code or a 3MF containing `Metadata/plate_1.gcode`.
PrusaSlicer runs with binary G-code off. Its G-code can be packaged with the input 3MF;
Orca receives an export filename relative to `--outputdir` and exports a sliced 3MF, from which plain G-code can also be retained. The simulator
chooses plain G-code. Preparation never starts a print.

Estimates come from the slicer's own G-code comments: time, filament length in mm, weight
in g, and layer count when present. Unavailable values remain absent in JSON and display
as `unavailable` on the command line. A plate PNG or G-code PNG thumbnail is kept for the
preview route. Only the first and last 256 KiB of G-code are inspected for comments and
thumbnails. G-code and 3MF files are limited to 512 MiB, previews to 4 MiB, and 3MFs to
5,000 entries. Large thumbnails outside those comment regions are omitted.

The child runs in the private job folder, with arguments passed directly rather than
through a shell. Both output streams are drained, retaining only their last 8 KiB; failure
messages show at most 20 lines from each. Timeout and request cancellation kill the child.
Slicer diagnostics may contain local file paths and profile names.

The command lines follow the public [PrusaSlicer profile selection documentation](https://github.com/prusa3d/PrusaSlicer/releases/tag/version_2.8.0-alpha5)
and [OrcaSlicer CLI documentation](https://github.com/OrcaSlicer/OrcaSlicer/wiki/cli_mode).
Use a PrusaSlicer release with those profile-selection flags (2.8 or newer).

## Commands

```sh
dutchman-print init
dutchman-print --config config.toml printers
dutchman-print --config config.toml clear workbench --yes
dutchman printers --config config.toml --json
dutchman print bracket.3mf --config config.toml --printer workbench
dutchman-print print bracket.3mf --printer workbench --print-profile Fine --json
dutchman-print print bracket.3mf --printer workbench --bed-clear --yes --json
dutchman-print status JOB_ID
dutchman-print cancel JOB_ID --yes
dutchman-print pause workbench
dutchman-print resume workbench
```

`dutchman printers …` runs `dutchman-print printers …`; `dutchman print …` runs
`dutchman-print print …`. Other companion commands use `dutchman-print` directly.

`init` writes a commented config for one printer. On a terminal it asks for the printer
type, its address, the key, password, or access code that kind needs, a name, id, and model,
and, for printers Print slices for, the slicer (PrusaSlicer or OrcaSlicer) and its printer,
print, and filament preset names. Keys, passwords, and access codes are hidden as you type
on Linux and macOS; on Windows they show. Every answer also has a flag, for scripts:

```sh
dutchman-print init --kind prusalink --url http://192.168.1.30 --api-key KEY --name "Workbench MINI"
dutchman-print init --kind bambu --host 192.168.1.40 --serial SERIAL --access-code CODE --output bambu.toml
```

| Flag | Answer |
|---|---|
| `--kind` | `simulated`, `prusalink`, `octoprint`, `moonraker`, `snapmaker-u1`, `duet`, `duet-sbc`, `repetier`, `bambu`, `elegoo-sdcp`, `snapmaker-2`, or `snapmaker-j1` |
| `--url` | Address for PrusaLink, OctoPrint, Moonraker, Snapmaker U1, Duet (either kind), and Repetier-Server; `http://` is added when left out |
| `--host`, `--port` | Address for Bambu, Elegoo, Snapmaker 2.0, and J1; the port only when it is not the usual one |
| `--api-key` | PrusaLink, OctoPrint, Repetier-Server, and Moonraker or Snapmaker U1 (optional there) |
| `--username`, `--password` | PrusaLink login instead of an API key; `--password` is also the optional Duet machine password, for either Duet kind |
| `--serial`, `--access-code` | Bambu serial number and LAN access code |
| `--cert-sha256` or `--ca-file` | Bambu trust; leave both out to read the fingerprint later |
| `--profile` | Elegoo `centauri` or `resin` |
| `--nozzle-diameter` | PrusaLink fallback, Duet (either kind), or Repetier-Server installed nozzle, in millimetres |
| `--storage` | PrusaLink storage path name, such as `local` or `usb`; omit for automatic selection |
| `--slug` | Repetier-Server printer slug, from the server's printer settings |
| `--slicer` | `prusaslicer` (default) or `orcaslicer`; PrusaLink needs PrusaSlicer and Bambu needs OrcaSlicer |
| `--printer-profile`, `--print-profile`, `--filament-profile` | Preset names; they default to the MINI or A1 examples on this page |
| `--name`, `--id`, `--model` | Defaults come from the kind; the id defaults to the name in lower case with hyphens |
| `--output FILE` | File to write; otherwise `--config`, `DUTCHMAN_PRINT_CONFIG`, or the platform path above |
| `--force` | Replace an existing file |

Without a terminal, `init` never asks: a missing required answer is an error naming its
flag, and answers with defaults use them. A flag the chosen kind does not use is refused.
Flags can end up in your shell history; on a shared computer, answer the questions instead.

`init` creates the file's folder if needed and refuses to replace an existing file unless
given `--force`. On Unix the file is created with mode 0600. Before writing, the text is
checked with the same loader every other command uses; a refused answer shows the loader's
message and writes nothing. Credentials go only into the file, never into the output.
`init` never contacts the printer, the network, or the slicer, and does not check that the
slicer or the preset names exist. Its last lines name the next commands: `printers` to read
the printer, then `serve`. Bambu printers are written with `lan_control = false`; without
`--cert-sha256` or `--ca-file`, the file loads only for `fingerprint` until you add one, and
the closing message names that command first. Elegoo and Snapmaker 2.0 and J1 printers are
written as monitoring only, without a slicer.

`printers` writes one line per printer, such as `Workbench demo: idle`. `--json` returns
the same array as `GET /api/v1/printers`. The CLI calls the backend directly; no server is needed.

`print` accepts a single `.3mf`, `--printer ID`, and optional `--printer-profile`,
`--print-profile`, and `--filament-profile` names. It checks the file and prepares a job,
shows the estimate, then asks two separate questions on a terminal: acknowledge the checked,
clear bed, then start the named job. Each challenge summary shows the current printer state.
Without a terminal it refuses to start unless `--yes` is present. `--json` never prompts:
without `--yes` it prepares only and returns the ready job and estimate; with `--yes` it
starts and returns the printing job. Estimates go to stderr in plain mode; job JSON goes
to stdout. `status`, `pause`, `resume`, and confirmed `cancel` return job JSON.
`cancel`, `pause`, and `resume` ask for confirmation on a terminal unless given `--yes`.
Scripts must pass `print ... --bed-clear --yes` after checking the bed.
`clear <printer> [--yes]` only prints what would be acknowledged and explains that clearance
is kept only by a running server; it cannot authorize a later CLI run.

Without `backend.slice = true`, the simulator estimates from the compressed 3MF size in bytes: seconds = 60 + size / 1024,
filament mm = size / 10, filament g = size / 1000, and layers = 1 + size / 4096.
Integer divisions round down. These are demonstration values, not slicing results.
Profile names affect slicing only when it is enabled. Started jobs progress with elapsed
time, pause freezes progress, resume continues, and cancel ends the job.
Simulated controls are `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.

### Simulated scenarios

A simulated printer can show things going wrong, so you can see how Print, a farm queue, and
the SolidCraft plugin respond without hardware. Add any of these keys to a `[[printers]]`
entry of a `simulated` backend. Without them the printer is healthy, as described above.

```toml
[[printers]]
id = "sim-error"
name = "Printer with an error"
model = "Example model"
fault = "error"

[[printers]]
id = "sim-flaky"
name = "Printer whose jobs fail"
model = "Example model"
fail_after_seconds = 30
```

| Key | Effect |
|---|---|
| `offline = true` | Status reports `offline` with a detail. Preparing and every action are refused as unavailable before anything happens. |
| `fault = "error"` or `"attention"` | Status reports that state with a detail. Start is refused, as real printers in those states refuse it. |
| `external_job = "name"` | Status reports `printing` a job Print did not start, under that name. Start is refused as busy and a queue skips the printer. Stop works after its normal confirmation and leaves the printer idle; pause and resume are refused. |
| `fail_after_seconds = N` | A job Print started ends as `failed` after N seconds of simulated printing (1 to 31536000) instead of `done`, and history records the outcome `failed`. A value at or above the job's estimate has no effect. |
| `bed_sensor = true` | The printer reports a clear bed whenever it is idle and healthy, so `needs_clearing` is false and no bed acknowledgement is needed, like a printer with a bed sensor. |

Use at most one of `offline`, `fault`, and `external_job` per printer. Unknown values are
refused when the config loads, and these keys are refused with any other backend. Scenarios
change only what the printer reports: confirmations, controls, the bed check, and the rule
that an uncertain start is never retried apply as usual.

CLI simulation state is saved in the private data folder, separately for each config path,
so later commands can read and control it. Jobs older than one hour are removed on the
next command. One CLI command at a time may change that state.
If a command is killed while holding a `.lock` folder, remove that folder once it has stopped.
The service persists its separate job state in its private data folder. Only one server may
use a data folder at once; after a killed process, remove `serve.lock` only once it has stopped. Tracked jobs are
reloaded and reconciled from live status before control after a restart.

### Private job paths

Service job and queue entry ids are generated as 32 lowercase hexadecimal characters.
A queue addition with a malformed job id is refused. Restored queue ids, source job ids, prepared
job ids, and saved-job keys are checked before use; malformed state must be repaired or
removed before the service can load it. Job folders stay below the configured data folder.
Preview reads use the job folder's `preview.png`, never a path supplied in stored metadata.

## Print history

Finished tracked jobs and bed-clear acknowledgements are saved in `history.jsonl` in
`--data-dir` (or `DUTCHMAN_PRINT_DATA`). Use the same private data folder for history and
`serve`. Choose a persistent folder: the default is in the operating system's temporary
folder and can be removed by system cleanup.

```sh
dutchman-print --data-dir private-jobs history --printer workbench --outcome done --since 2026-10-01T00:00:00Z --limit 20 --json
```

`history` lists newest records first, with a default limit of 100 (1–10,000 allowed).
Omit `--json` for one readable line per record. `--printer`, `--outcome`, and `--since`
combine; since is an inclusive RFC 3339 timestamp, including an explicit UTC offset.
Outcomes are `done`, `cancelled`, `failed`, `stopped` (a tracked job ended by printer Stop),
`ended` (the printer could not establish how it ended), `unresolved_after_restart`, and
`bed_clear`. External printer jobs are not tracked and do not get job-history records.
A ready job cancelled before Start has no start time or duration.

### Summary and CSV

```sh
dutchman-print --data-dir private-jobs history --summary --since 2026-10-01T00:00:00Z
dutchman-print --data-dir private-jobs history --summary --printer workbench --json
dutchman-print --data-dir private-jobs history --csv --since 2026-10-01T00:00:00Z > october.csv
```

`history --summary` reports, per printer and in total, the number of finished jobs by
outcome (`done`, `failed`, `cancelled`, `stopped`, `ended`, `unresolved_after_restart`),
the success rate, total and average print time, and the time of the last finished job.
It covers every retained record that matches `--printer` and `--since`; `--outcome` and
`--limit` are refused. Success rate is `done` divided by jobs that ran (jobs with a
start time; a job cancelled before Start never ran), and is `-` when nothing ran.
Print time sums `duration_seconds` over the jobs that have one; the average divides by
that same count. Bed-clear acknowledgements are counted in their own `bed_clears` column
and are not jobs. The default output is a table, `--json` prints the same figures as JSON:

```text
PRINTER  JOBS  DONE  FAILED  CANCELLED  STOPPED  ENDED  UNRESOLVED  SUCCESS  TOTAL TIME  AVG TIME  LAST FINISHED         BED CLEARS
bench    3     2     1       0          0        0      0           66.7%    0h06m40s    0h03m20s  2026-10-03T01:00:00Z  1
TOTAL    3     2     1       0          0        0      0           66.7%    0h06m40s    0h03m20s  2026-10-03T01:00:00Z  1
```

`history --csv` writes the records matching the usual filters (`--limit` still defaults to
100) as CSV with a header row and one column per record field: `printer`, `printer_kind`,
`job_id`, `job_name`, `file_name`, `file_size`, `source`, `outcome`, `started_at`,
`ended_at`, `duration_seconds`, `estimate`. Fields containing commas, quotes or line breaks
are quoted as in RFC 4180, rows end in CRLF, absent values are empty, timestamps are as
stored, and `estimate` holds its JSON text. `--csv` cannot be combined with `--json` or
`--summary`. JSON stays the default output.

Each job record includes the printer id and kind, job id and name, prepared filename and
size in bytes (the input file for simulation), source input label when known, estimates,
and UTC RFC 3339 times. Start
is the companion's Start-attempt time; end is when it observes the terminal result.
Duration is elapsed wall-clock seconds, including pauses, and is absent without a start.
History is polled while the server runs, also on requests and CLI commands. CLI simulation
results are archived before expired snapshots are removed. A restart preserves history;
if an attempted job cannot be reconciled during startup's bounded read, a single
`unresolved_after_restart` record states that uncertainty. This does not change tracked
machine state, authorize a control, or retry Start. Known active jobs stay tracked through
normal service expiry until a terminal outcome is observed.

Records contain metadata only: no file contents, credentials, confirmation values, backend
addresses, or local directory paths. Input labels are basenames, limited to 256 bytes;
URL-shaped labels are redacted. Bed-clear records contain printer identity, `bed_clear`,
and the acknowledgement time; job fields are null. No operator identity is collected.
History never restores bed clearance after a restart. History is best effort: a write
problem is logged and never blocks, delays, or changes a print action or status read.
Action-side writes run in a bounded background queue; an overloaded queue is logged
without changing the machine result. The history command and API
route still report storage errors, so repair storage if the journal cannot be read.

Pending job metadata uses its own mutex and a separate `history-pending.json` file in the
service's private data folder. CLI state uses a matching `*.history-pending.json` file,
so CLI and service records do not overwrite each other's pending metadata. A background
writer receives serialized snapshots and performs disk I/O after data locks are released;
it never holds the machine safety mutex. Stop reaches the backend before any bookkeeping,
even when a history write is stalled. A failed Stop still withdraws bed clearance.

Startup reloads the pending file and migrates a legacy `history` map from older safety
state. The legacy payload remains recoverable until the new file is synced, then the next
safety checkpoint omits it. An existing pending file is authoritative, including an empty
one, so old safety snapshots cannot resurrect finished jobs.

The JSON Lines journal retains the newest **10,000 records**, including acknowledgements.
Writes append a complete line and sync it to disk. A torn final line is ignored on read
and removed before the next append. Rotation writes and syncs a replacement before an
atomic rename, preserving earlier complete lines if interrupted. An operating-system lock
on the private `history.lock` file serializes readers and writers across CLI and service
processes and releases automatically if a process crashes. Leave the lock file in place.
Other corrupt complete lines cause a storage error; they are never silently discarded.
Back up the data folder if longer retention is needed.

## Plugin API

```sh
dutchman-print --config config.toml serve --listen 127.0.0.1:8730 --data-dir private-jobs
```

On Ctrl-C or SIGTERM, `serve` closes printer connections within three seconds without
sending machine actions; CLI commands also close their connections after saving state.

The standalone [job API version 1 specification](print-api.md) describes the protocol for
clients and other servers. Errors are JSON objects with an `error` string and a 4xx or 5xx status. Basic authentication protects all routes
except `GET /healthz` when `DUTCHMAN_PRINT_PASSWORD` is set.

| Route | Result |
|---|---|
| `GET /healthz` | `{"service":"dutchman-print","api":1}`; service liveness only |
| `GET /api/v1/printers` | Printer array, in config order |
| `POST /api/v1/confirmations` | Issue an observation-bound challenge; 201 |
| `POST /api/v1/jobs` | Raw 3MF or multipart prepare; 201 with `{id, printer, name, state: "ready", estimate, progress, seconds_left}` |
| `GET /api/v1/jobs/{id}` | Current job |
| `GET /api/v1/jobs/{id}/preview.png` | PNG when the backend supplied one; otherwise 404 |
| `POST /api/v1/jobs/{id}/start` | Start a ready job |
| `POST /api/v1/jobs/{id}/cancel` | Cancel a ready, printing, or paused job |
| `POST /api/v1/printers/{id}/pause` | Pause the printer's active job |
| `POST /api/v1/printers/{id}/resume` | Resume its paused job |
| `POST /api/v1/printers/{id}/clear` | Acknowledge its checked, clear bed; 204 |

`POST /api/v1/jobs` accepts a raw 3MF body with `Content-Type: model/3mf` (or
`application/vnd.ms-package.3dmanufacturing-3dmodel+xml`), with `printer`, optional `name`,
and profile names as query parameters. Without `name`, raw uploads use `input.3mf`.
SolidCraft’s host sends this form. Multipart uploads remain available for curl and scripts.

The multipart form takes exactly one `file` with a `.3mf` name and a `printer` ID. Optional fields
are `printer_profile`, `print_profile`, `filament_profile`, and `name`. Each text field is
nonempty plain UTF-8, at most 256 bytes. Duplicate or unknown fields are refused. Without
`name`, the uploaded filename is used. A 3MF must be a zip with a `3D/*.model` part. The
companion checks paths, entry counts, and actual expanded sizes before calling the backend.
The simulator supplies previews only when slicing produces one.

Printer objects have `id`, `name`, `model`, required `controls`, and `state`: `idle`, `printing`, `paused`,
`attention`, `busy`, `error`, or `offline`. While printing or paused they may include `job_id`, `job_name`, `progress`
(0–1), `seconds_left`, `nozzle_temperature`, and `bed_temperature` (degrees Celsius).
Unavailable printer fields are omitted.

`controls` lists machine actions supported in configuration, independent of live state.
It contains `start`, `pause`, `resume`, `cancel`, `stop`, and `clear` where supported; `clear`
requires start support. `stop` includes jobs started elsewhere. Use this list to choose
which buttons to show; fresh state and confirmation still decide whether an action can run.
Unsupported challenges and action routes return 409 before reading printer status.

Job states are `ready`, `printing`, `paused`, `done`, `cancelled`, `failed`, and `ended`. Job `progress`
and `seconds_left` may be null. Estimate fields `seconds`, `filament_mm`, `filament_g`, and
`layers` are optional and omitted when unavailable. Local preview paths are never returned.
Not found maps to 404, busy or refused to 409, unavailable/offline to 503, and backend failure
to 500. Backend error messages must never contain secrets.

Before each machine action, request a server-issued challenge with
`POST /api/v1/confirmations` and JSON `{"action":"start","printer":"workbench","job":"JOB_ID"}`.
Actions are `start`, `cancel`, `stop`, `pause`, `resume`, and `clear`; `job` is required for start
and cancel and omitted for printer actions. Issuance re-reads the printer and refuses
incompatible actions with 409. Success is 201 with `id`, `action`, `printer`, nullable `job`,
`summary`, `observed`, and `expires_at` (Unix seconds). `observed` contains normalized `state`,
nullable vendor `detail`, `job_id`, `job_name`, `progress` (fraction rounded down to a whole
percent), and `needs_clearing`.

Show the returned summary verbatim, for example:
`Start printing 'bracket' on Workbench MINI (idle, bed clear)?`.
The challenge expires after 60 seconds (`--confirmation-seconds`, 1–600).
For SolidCraft, the plugin's read step fetches this challenge and returns its summary for
the host's own confirmation dialog. Only after the person approves that dialog does the
host attach a fresh `Plugin-Confirmation` header to the physical action step. That step
sends `{"confirmation":"CHALLENGE_ID"}` as JSON, or `?confirmation=CHALLENGE_ID` with an empty
body. Send the id once. Clear and Start require separate challenges and separate approvals.

Every machine route still requires exactly one `Plugin-Confirmation` header containing
64 lowercase hexadecimal characters. Missing, malformed, or duplicate headers and missing
challenge ids return 403. A challenge must be unused, unexpired, and match the action,
printer, job, and reviewed input digest. Current state, vendor detail, job identity, whole
percent progress, bed clearance, and tracked job state must still match the observation.
A changed observation returns 409 with
"The printer changed since you confirmed; confirm again." Invalid, expired, or consumed
challenges and reused header values also return 409. Obtain a new challenge and confirmation;
never automatically repeat Start after an uncertain result.

The private data folder durably stores challenges and consumed ids until their expiry and
hashes of seen header values for at least ten minutes, across callers and restarts. Both
are consumed before attempting a backend action, including failed attempts. Header values
are never logged. Confirmation is separate from authentication. Standalone CLI prompts
and `--yes` create and consume the same observation-bound challenges internally.

Only one Start per printer runs at a time, rechecking fresh status after waiting; Stop never waits for it.
Printer commands are serialized separately for each printer. Uploads and command outcome
waits on one printer do not block another printer's status, confirmation review, or Stop.
Stop can also overtake its own printer's pending Start, pause, or resume outcome wait;
the earlier request reconciles to the confirmed cancellation without resending a command.
Start attempts are durably recorded before sending Start. Late status results cannot
replace a newer cancellation or restore a forgotten job.

## Server settings and hardening

| Option | Environment | Default |
|---|---|---|
| `--config FILE` | `DUTCHMAN_PRINT_CONFIG` | Platform path above |
| `--listen ADDRESS` | `DUTCHMAN_PRINT_LISTEN` | `127.0.0.1:8730` |
| `--data-dir DIR` | `DUTCHMAN_PRINT_DATA` | `dutchman-print` in the system temporary folder |
| `--user NAME` | `DUTCHMAN_PRINT_USER` | `dutchman` |
| | `DUTCHMAN_PRINT_PASSWORD` | None; environment only |
| `--clearance-interval-seconds N` | | 5; range 2–30 |
| `--clearance-expiry-minutes N` | | 15; range 1–120 |
| `--confirmation-seconds N` | | 60; range 1–600 |
| `--public-origin ORIGIN,...` | `DUTCHMAN_PRINT_PUBLIC_ORIGIN` | None |

- Set a password unless everyone who can reach the service may read job names and control
  jobs. Listening beyond loopback without a password prints a warning. Use an HTTPS reverse
  proxy when basic authentication crosses an untrusted network; keep the companion port private.
- Every POST checks `Sec-Fetch-Site` and `Origin`, in addition to password and action
  confirmation. Cross-site requests are refused. Origin must match Host including its port,
  or an exact configured public origin. Requests without either header are allowed. Preserve
  Host through a proxy or set `--public-origin`.
- Responses use the shared security headers and `Cache-Control: no-store`. Cross-origin reads are
  not enabled. Config credentials and confirmation values never appear in reports or logs.
- The private data folder uses the shared folder checks: new folders use mode 0700 on Unix; an existing
  folder must belong to the current user, have no group or other write permission, and be a
  directory rather than a symbolic link. Keep the folder private: it contains designs and names.
- The upload defaults match Web: 200 MiB per file, four in-flight uploads, two backend
  preparations, 5,000 zip entries, 2 GiB expanded bytes, 10 GiB stored data, and at least
  1 GiB free space on Unix. Storage admission is checked before new jobs. Failed uploads
  are removed. Service jobs expire after one hour; files are also cleaned periodically.
- The service bounds saved jobs to 5,000 and remembered confirmations to 10,000. When full,
  it refuses new work until space is available. Simulation state is local to each process;
  CLI snapshots are separate from service jobs.

Backend authors implement the asynchronous `backend::Backend` trait in
[`crates/dutchman-print/src/backend.rs`](../crates/dutchman-print/src/backend.rs). Slicing belongs inside `prepare`, which receives the
job ID, private 3MF path, printer ID, and profile names. Typed errors distinguish not found,
busy, refused with a message, unavailable/offline, and failed. The simulated implementation
accepts a clock so lifecycle tests can advance time without sleeping.

### Flatpak manual check

Keep job folders under your home folder, where the Flatpaks have filesystem access.
A host `/tmp` folder is separate from the Flatpak's `/tmp`. Set `TMPDIR` to a private
folder below home before starting the companion or a manual check.

Some Prusa Flatpak entrypoints start a background theme helper that keeps diagnostic
pipes open after slicing. If preparation times out after producing G-code, use a launcher
that selects the CLI directly, and set `slicer.path` to that executable script:

```sh
#!/bin/sh
exec flatpak run --unshare=network --nodevice=all --command=prusa-slicer com.prusa3d.PrusaSlicer "$@"
```

For Orca, replace the command with `orca-slicer` and the app ID with the installed Orca ID.
The script disables networking and device access. When using a custom Orca launcher,
set `slicer.datadir` explicitly because its filename no longer identifies the Flatpak.
Orca presets available only in the application bundle must be installed into the selected
data folder's `system` tree first, with their vendor manifests and inherited presets.

The ignored `real_slicer` test builds a 20 mm cube 3MF and checks G-code and sliced 3MF
output with system printer, print and filament profiles. It uses isolated temporary data
and the CLI launchers above. It requires Linux system Flatpaks at
`/var/lib/flatpak/app`, their bundled presets, and `flatpak` and `timeout` on PATH:

```sh
mkdir -p "$HOME/Documents/diag-print-lead/slicer/tmp"
DUTCHMAN_PRINT_REAL_SLICER=prusaslicer buildq run dutchman /path/to/print -- \
  cargo --config 'env.TMPDIR="/absolute/home/path/Documents/diag-print-lead/slicer/tmp"' \
  test -p dutchman-print --locked -- --ignored real_slicer --nocapture
```

Repeat with `DUTCHMAN_PRINT_REAL_SLICER=orcaslicer`. The test prints parsed estimates and
requires positive time, length and weight. Its preset names come from installed preset
data. To print the installed CLI help instead, also set
`DUTCHMAN_PRINT_REAL_SLICER_HELP=1`.

PrusaSlicer 2.9.6 passed with `Original Prusa MINI & MINI+`, `0.20mm QUALITY @MINI`,
and `Prusament PLA`: 1,439 seconds, 1,248.1 mm and 3.72 g for both output kinds.
The temporary Prusa data folder registers the MINI model and PLA filament in
`PrusaSlicer.ini`; copying a vendor bundle alone leaves default printer and filament
presets selected. Set up the required printer and filament in PrusaSlicer before using
your own data folder.

The installed legacy Orca Flatpak reports 2.0.0 in its package metadata and
`OrcaSlicer-01.10.01.50` in CLI help. Its bundled `Bambu Lab A1 0.4 nozzle`,
`0.20mm Standard @BBL A1`, and `Generic PLA @BBL A1` presets fail validation: relative
extrusion is enabled, but `before_layer_change_gcode` is empty. The CLI asks for `G92 E0`
to reset extrusion at every layer and exits with status 205 before slicing.

To use that installation, save an A1 printer preset with `G92 E0` in its before-layer-change
G-code, and select the saved preset's name. The manual test can verify this correction by
setting `DUTCHMAN_PRINT_REAL_SLICER_ORCA_LAYER_RESET=1`; it changes only a temporary copy
of the bundled machine preset. That corrected copy passed both output checks with
1,338 seconds, 1,214.72 mm, 3.62 g and 100 layers. A run without that variable still
exposes the installed preset's validation failure. Dutchman does not change installed
presets or skip validation. Missing display access prevented Orca thumbnails in this
check; G-code and sliced 3MF estimates were available.

Set `DUTCHMAN_PRINT_REAL_SLICER_KEEP=1` to retain the temporary model, preset copies,
resolved settings and output for inspection. Orca's effective settings are exported as
`effective.json` in that folder.


## PrusaLink

Embedded PrusaLink runs in MINI/MINI+, MK4, XL, and CORE One firmware. Pi-hosted
PrusaLink runs on a Raspberry Pi connected to older printers such as MK3S/MK3S+ and
MK2.5S. Both use `kind = "prusalink"`; configure the address of the printer or Pi.

Controls: `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.

Configure each printer explicitly. Dutchman does no network discovery. Use its local HTTP
address, without credentials, a path, or a query string. HTTPS proxies are not supported yet;
redirects are refused. Keep printer HTTP traffic on a trusted private network.

```toml
[backend]
kind = "printers"

[slicer]
kind = "prusaslicer"
# path = "/path/to/prusa-slicer"
timeout_seconds = 600

[[printers]]
kind = "prusalink"
id = "mini"
name = "Workbench MINI"
model = "Original Prusa MINI & MINI+"
url = "http://printer.local"
api_key = "REPLACE_WITH_YOUR_PRINTER_KEY"
printer_profile = "Original Prusa MINI & MINI+"
print_profile = "0.20mm QUALITY @MINI"
filament_profile = "Generic PLA @MINI"
```

For an MK3S attached to a Raspberry Pi, use the same backend and slicer tables above,
with this printer entry instead. Use the login created during PrusaLink setup:

```toml
[[printers]]
kind = "prusalink"
id = "mk3s"
name = "Workbench MK3S"
model = "Original Prusa i3 MK3S"
url = "http://prusalink.local"
username = "REPLACE_WITH_YOUR_PRUSALINK_USERNAME"
password = "REPLACE_WITH_YOUR_PRUSALINK_PASSWORD"
nozzle_diameter = 0.4 # physically check the installed nozzle
# storage = "local" # needed only when automatic selection is ambiguous
printer_profile = "Original Prusa i3 MK3S & MK3S+"
print_profile = "0.20mm QUALITY @MK3"
filament_profile = "Generic PLA"
```

`GET /api/v1/storage` determines the upload target. Automatic selection requires exactly
one available, writable storage. When several are writable, set `storage` to the path name
without slashes (`local`, for example), rather than the translated display name. An override
must still be available and writable. Missing or insufficient reported free space blocks
upload; free space never resolves an ambiguous choice. No storage name is assumed.

`init` accepts `--storage` and `--nozzle-diameter` and asks optional questions for both.
For a Pi, for example: `dutchman-print init --kind prusalink --url http://prusalink.local
--username workshop --password PASSWORD --nozzle-diameter 0.4 --storage local` (on one line).
Select the installed MK3S presets rather than accepting the MINI preset defaults.

Use either `api_key` (`X-Api-Key`) or both `username` and `password` (HTTP Digest), never
both forms. For MK4 and XL, Prusa documents `maker` as the default Digest username and
the API key as its password. Pi-hosted i3 installations use their PrusaLink login.
Digest supports MD5 and MD5-sess, with `auth` protection or older challenges without qop;
unsupported challenges are refused. Credentials stay in the private config.

On MINI, find the key in **Settings → Network → PrusaLink → Current API Key**. For
Pi-hosted PrusaLink 0.7.0 and later, Prusa documents the key under Prusa Connect's Settings;
older versions show it in the setup wizard. A configured local key requires no cloud account
to use Dutchman. Check the installed firmware's authentication options in Prusa's
[connection guide](https://help.prusa3d.com/guide/sending-files-from-prusaslicer-to-prusalink_222961)
and [troubleshooting guide](https://help.prusa3d.com/article/prusalink-troubleshooting_304411).

Commission the configured profiles for the actual model, build volume, nozzle, toolhead,
material, and plate before use. All three profiles are required; per-job profile changes are
refused if they differ from that printer's configuration. Multi-material slot assignments
are not supported yet: printers reporting MMU or omitting its capability are refused. PrusaLink does not provide a
universal live contract for loaded material, plate, or build volume. Check those physically
against the configured presets before confirmation; Dutchman cannot detect an unreported
physical change. This backend is intended for commissioned single-material setups.

Preparation reads status and machine information, then slices locally to text G-code.
Pi-hosted PrusaLink uses text G-code. Dutchman also keeps text output for embedded printers;
it never uploads `.bgcode`, even where newer firmware supports it. An explicit false or
invalid `upload-by-put` capability is refused; legacy upload APIs are not used.
Missing serial or firmware blocks preparation. If the printer omits its nozzle diameter,
set `nozzle_diameter` in private config to the physically installed size; without either
value preparation is refused. A reported diameter takes precedence over config, and an
invalid reported value is refused. Printer type and capability fields may differ between
families; the complete information and version responses are bound to the reviewed job.
The resolved profile's
`nozzle_diameter` is read from PrusaSlicer's output G-code comments in the first and last
256 KiB. Missing, conflicting, or multiple-nozzle settings are refused. Before upload the
profile diameter must match `/api/v1/info` `nozzle_diameter` or its configured fallback;
a mismatch names both diameters.
Fit a matching nozzle or slice with the matching profile. No mismatch override is offered.
Before upload and again before Start, Dutchman requires fresh `IDLE` or `READY` status,
with no job or transfer reported. `BUSY`, `ATTENTION`, terminal, unknown, and unavailable
states block Start. ATTENTION reports `attention` (waiting for a person); BUSY reports
`busy` (a non-print operation). Start, clear, and resume are refused in both states.
Pause requires printing and resume requires paused; cancel remains available for a
matching tracked job in attention. Status reads have a four-second deadline. Control
requests allow 15 seconds. One keep-alive HTTP connection per printer is reused for reads, with
cached Digest challenges and incrementing nonce counts. A request refused with a stale
Digest nonce (401 with `stale=true`) was not acted on, so it is resent once with the new
nonce, commands included. Reads also retry once on an idle socket the printer has closed;
any other failure of a command is final and it is never resent.
Uploads and controls always open a fresh connection, so a socket the printer closed while
idle cannot make a Stop or another command fail before it reaches the printer. Commands poll matching status for
25 seconds for start/stop and 10 seconds for pause/resume, announcing pending on stderr
before reporting an unconfirmed outcome. Missing estimates stay absent;
authentication failures appear separately in the safe availability detail.

Dutchman checks prepared input and G-code bytes, configured profiles, machine information,
and firmware against the preparation review. It selects available writable storage with enough
reported free space, uploads without overwrite or automatic printing, and checks the stored
file's name, print-file type, and size. Uploads stop after 30 seconds without transport
progress; continuing transfers have no fixed total deadline. Failed or uncertain uploads
and failed stored-file validation delete the staged file before returning an error. If cleanup
is not confirmed, inspect storage and remove it before preparing again. These failures never
send Start. Start is sent once and confirmed by fresh status matching the intended file and job.
A lost reply or conflict requires reconciliation through status; Start is never automatically
retried. Pause, resume, and cancel re-read and verify the live job ID before sending a command.
Only explicit matching terminal reports establish done, cancelled, or failed; idle alone never
proves success. When an attempted execution is no longer live on an idle printer, the
job becomes terminal `ended` (outcome unknown), unless FINISHED or STOPPED was seen,
which establish `done` or `cancelled`. BUSY never moves a tracked job back to printing.
The endpoint contract comes from Prusa's
[public API documentation](https://github.com/prusa3d/Prusa-Link-Web/blob/master/spec/openapi.yaml).

Check the bed before starting, then acknowledge and start within the same CLI run:

```sh
dutchman-print print part.3mf --printer mini --bed-clear --yes
```

Without `--yes`, terminal use asks separately about bed clearance and Start, showing the
current state each time. `clear mini` is an informational status command. Pause and resume
also show a fresh challenge summary before confirmation or accept `--yes`.
Printer JSON includes optional `needs_clearing` and `detail`; see [the API](print-api.md).

A remote bed-clear acknowledgement exists only in the running process. While it is held,
the server polls that printer every 5 seconds (`--clearance-interval-seconds`, 2–30).
It is withdrawn on any non-idle or unknown state, failed read, observation gap longer than
twice that interval, reconnect following unavailable telemetry, job identity or finished-job
signal change, or expiry after 15 minutes (`--clearance-expiry-minutes`, 1–120). A Start
attempt consumes it even if the result is uncertain or fails. Bed-clear polling stops when no remote
acknowledgement is held; separate history polling continues for tracked jobs. Activity entirely between polls with no identity or terminal signal
change cannot be detected; clearance is not proof that no physical activity occurred.
PrusaLink `READY` is the operator's printer-side bed-clear signal and substitutes for the
remote acknowledgement; `IDLE` still requires one. Restarting never restores remote clearance.

Standalone CLI commands save tracked jobs and reviews separately from service jobs. Use
the service API to control service jobs. The attempted Start is saved before sending it,
so restarting cannot silently retry it. The service persists tracked jobs and reconciles
live identity before control after a restart.

## OctoPrint

Controls: `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.

Configure the host explicitly; Dutchman performs no discovery. HTTP and HTTPS are accepted,
including a reverse proxy base path. HTTPS verifies the hostname and certificate against system
trust; there are no certificate exceptions. Redirects are refused. Keep HTTP on a trusted local
network. Only text G-code is sent, using your separately installed PrusaSlicer or OrcaSlicer.

```toml
[backend]
kind = "printers"

[slicers.local]
kind = "prusaslicer" # or "orcaslicer"
# path = "/path/to/prusa-slicer"

[[printers]]
kind = "octoprint"
id = "workbench"
name = "Workbench printer"
model = "Your commissioned printer model"
url = "https://octoprint.example/printing"
api_key = "REPLACE_WITH_YOUR_OCTOPRINT_USER_OR_APPLICATION_KEY"
slicer = "local"
printer_profile = "Your installed printer preset"
print_profile = "Your installed print preset"
filament_profile = "Your installed filament preset"
```

Create a key in your local OctoPrint installation. Prefer a user-approved application key
from [Application Keys](https://docs.octoprint.org/en/main/bundledplugins/appkeys.html), or
generate a personal key in your user settings using the API key controls in the change-password
dialog, described in [API authentication](https://docs.octoprint.org/en/main/api/general.html#authorization).
The key needs status, version, printer-profile read, file-list, upload, delete, select, print, pause,
and cancel permissions. Permission failures are reported as authentication failures; inspect
the key's permissions in OctoPrint. Dutchman sends it only as `X-Api-Key`, never in a URL.
No cloud account or application registration is required. Keep the config private.

Commission all three slicer profiles for the actual model, build volume, nozzle, toolhead,
material, and plate. Per-job profile differences are refused. Only single-extruder profiles
are accepted; material-slot assignment is unsupported. Review the active OctoPrint printer
profile and serial connection too. OctoPrint's REST status cannot verify physical printer
identity, firmware, loaded material, or plate changes. Check these physically before every
confirmation; a changed physical setup requires new preparation and review.

Status reads the documented [connection](https://docs.octoprint.org/en/main/api/connection.html),
[printer](https://docs.octoprint.org/en/main/api/printer.html), and
[job](https://docs.octoprint.org/en/main/api/job.html) endpoints within four seconds. A reachable
host with disconnected serial reports offline. Opening, missing, contradictory, or unknown
states block control. Summary states prefer the documented flags over display text.
Starting, Resuming, Printing from SD, Pausing, Cancelling, and Finishing report printing;
Finishing remains unresolved until newer history establishes the terminal result. Paused
reports paused. Error and Offline after error report error. Connection/detection phases and
SD/file transfers report busy with their phase in detail.
Starting still requires complete flags and consistent Operational readiness. Progress, remaining time,
temperatures, and selected file name are reported when available. Missing values stay absent.

Before starting, acknowledge the checked bed through a `clear` challenge and the confirmed
API clear route. Clearance belongs to the running server and expires under the shared
polling policy; a separate command line invocation cannot retain it. While clearance is held,
polling reads the recursive file list as well as status. A newer `prints.last.date` on any
file, or a job or state change seen by polling, withdraws clearance even if the printer is
idle again. Activity between polls with no retained history or identity change remains
unobservable. Clear and Start each require their own server-issued challenge and fresh
`Plugin-Confirmation` header.

The sliced `nozzle_diameter` setting in the G-code comments must match the selected
OctoPrint printer profile's `extruder.nozzleDiameter`, during preparation, challenge review,
and before upload. Missing or ambiguous diameters are refused; mismatches name both values.
There is no override. Check the physical nozzle too: the profile describes configuration,
not a sensor reading.

Before starting, Dutchman checks input and G-code bytes, configured profiles, the active
OctoPrint connection and printer profile, and server version against preparation. It checks
fresh readiness before upload and again before start. It stages a uniquely named local file
using [file operations](https://docs.octoprint.org/en/main/api/files.html), with select and print
disabled, refusing an existing name. Uploads are capped at 200 MiB with a 30-second transport
idle timeout, reset by byte progress. It requires a completed upload and validates the stored name,
origin, machine-code type, and size. Failed or uncertain uploads, including failed stored-file
validation, trigger deletion of the staged remote file before the error is returned. If
deletion cannot be confirmed, inspect OctoPrint storage and remove it before preparing again.
These failures never send Start. It selects without printing, reads the selected file
back, and only then sends the explicit job start command once. A conflict or lost reply
requires status reconciliation; an attempted start is never repeated automatically.

Pause, explicit resume, and cancel require a fresh local file-name/origin match to the job
Dutchman started. Pause never toggles. OctoPrint supplies no execution ID, so external reuse
of the exact staged file cannot be distinguished; avoid controlling these files elsewhere.
After an accepted start or cancel, Dutchman polls for up to 25 seconds; pause and resume
poll for up to 10 seconds. It announces pending on stderr while waiting, then reports an
unconfirmed pending outcome if the deadline expires. Polling only reads status and never
repeats the command. Read status before another action after an uncertain result.
An idle machine alone does not prove completion. Dutchman requires a newer print-history
record for that file after observing it active. History dates are finite floating-point Unix
timestamps; fractions are retained when comparing against the pre-start history. A successful record means done; an unsuccessful
record means failed, or cancelled when this companion requested cancellation. Without that
evidence the outcome stays unresolved. After every start attempt, finish, cancellation, or
failure, check and acknowledge the bed again. CLI state preserves attempted starts across
restarts; the service uses the same confirmation and safety policy described in
[the job API](print-api.md).

## Bambu Lab LAN control (experimental)

Controls: none by default; `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`
with `lan_control = true`.

Bambu monitoring and local preparation with OrcaSlicer are available by default.
Control is **experimental, verified against simulated printers only**. Check it on your
own printer before relying on it. Set `lan_control = true` in that printer's config
only after confirming LAN/Developer Mode allows control. Upload, Start, pause, resume,
and cancel then use the same review and bed-clear checks as PrusaLink. Preparation
produces a sliced 3MF and estimates without contacting storage. No cloud account is used.

Enable LAN mode using Bambu's [LAN setup instructions](https://wiki.bambulab.com/en/knowledge-sharing/enable-lan-mode).
Find the LAN access code in the printer's LAN/network settings. Find the serial number on
the printer's identification label or its device information screen; enter the printer
serial, not its display name. Menus depend on model and firmware. Bambu's
[Developer Mode announcement](https://blog.bambulab.com/updates-and-third-party-integration-with-bambu-connect/)
explains that newer authorization firmware may require manually enabling Developer Mode
to expose MQTT. Availability depends on model and firmware. Monitoring permission does
not establish control permission.

```toml
[backend]
kind = "printers"

[slicers.prusa]
kind = "prusaslicer"
# path = "/path/to/prusa-slicer"

[slicers.orca]
kind = "orcaslicer"
# path = "/path/to/orca-slicer"
# datadir = "/path/to/OrcaSlicer"
timeout_seconds = 600

[[printers]]
kind = "prusalink"
slicer = "prusa"
id = "mini"
name = "Workbench MINI"
model = "Original Prusa MINI+"
url = "http://mini.local"
api_key = "REPLACE_WITH_PRUSALINK_API_KEY"
printer_profile = "Original Prusa MINI & MINI+"
print_profile = "0.20mm QUALITY @MINI"
filament_profile = "Generic PLA @MINI"

[[printers]]
kind = "bambu"
slicer = "orca"
id = "bambu"
name = "Workbench Bambu"
model = "Bambu Lab A1"
host = "printer.local"
# port = 8883 # Optional MQTT TLS port; defaults to 8883.
# ftps_port = 990 # Optional implicit FTPS port.
lan_control = false # Change only after confirming control permission.
serial = "REPLACE_WITH_PRINTER_SERIAL"
access_code = "REPLACE_WITH_LAN_ACCESS_CODE"
ca_file = "/path/to/trusted-bambu-ca.pem"
# Alternatively remove ca_file and set cert_sha256 to 64 hexadecimal digits.
printer_profile = "Bambu Lab A1 0.4 nozzle"
print_profile = "0.20mm Standard @BBL A1"
filament_profile = "Generic PLA @BBL A1"
```

Use the exact preset names installed in each slicer. This single config prepares both
the MINI through PrusaSlicer and the A1 through OrcaSlicer. Keep the config private (`chmod 600`
on Unix). The access code never appears in reports or connection diagnostics.

Connections use MQTT over TLS. The optional `port` defaults to 8883 and must be between
1 and 65535. Set exactly one trust option: a PEM CA file
obtained through a trusted distribution, or `cert_sha256`, the SHA-256 fingerprint of the
printer's DER certificate, obtained and checked independently. Fingerprints contain 64
hexadecimal digits without colons. CA validation checks the configured serial as the
certificate name, even when connecting by IP. Address-based SNI is suppressed. Pinning
checks the exact certificate and its TLS handshake signature. A replacement certificate
requires an explicit new trust decision. Monitoring always verifies configured trust.
No vendor CA is bundled: this avoids assuming permission to redistribute a CA from community sources.

To read a certificate before enrolling its pin, run
`dutchman-print --config config.toml fingerprint bambu [--json]` (omit the brackets).
It connects once to that printer's configured TLS port, sends no MQTT login or access
code, and prints SHA-256, subject, issuer, and validity dates. Compare the fingerprint
with what the printer shows, if anything, and check it independently before pasting it
as `cert_sha256`. This observation alone does not establish trust. The command never
writes config. It permits missing trust for the selected printer so initial enrollment
is possible; normal monitoring still requires exactly one trust option. JSON fields are
`cert_sha256`, `subject`, `issuer`, `not_before`, `not_after`, and `instruction`.

The service keeps one connection per configured printer once status is first requested.
Standalone commands disconnect when they finish; control-enabled commands keep monitoring
during review and execution. Reports merge changed fields, preserve
missing estimates, convert remaining minutes to seconds, and retain the original state in
`detail`. IDLE, FINISH and FAILED map to idle when no active serious fault is present;
PREPARE/RUNNING map to printing and PAUSE to paused. Unknown states map to error.
FINISH/FAILED retain the completed execution's identity and require a new bed check.
Their existing `print_error` is historical; an error appearing or changing after the
terminal transition is active and blocks readiness. Idle never establishes success.

Up to 64 active HMS entries are retained. Severity comes from bits 8–11 of `attr`:
1 (fatal) and 2 (serious) block Start; 3 (common) and 4 (info) do not. Unknown severity
or malformed/oversized HMS data blocks Start. Active codes and their Bambu HMS wiki
links appear in `detail`. Any HMS alert accompanying PAUSE reports `attention`, adds “Needs attention”
and prevents Resume. Stop remains available for the matching live execution.

Freshness requires an established MQTT connection and an incoming message within
30 seconds. Changed fields merge onto the cached state, including across reconnects;
unchanged machine state and error fields need not repeat. A disconnected connection
is offline. Reconnects back off from one to 60 seconds. Complete-report requests are
spaced at least five minutes apart to protect P1 printers, while incoming deltas are
processed throughout that wait. Only a live connection with recent traffic restores
availability. P1/A1 delta and reconnect pacing behaviour still needs a hardware check.
No discovery or cloud login is implemented.

Start stages the reviewed bytes over implicit FTPS (`ftps_port`, default 990). Both
connections use the configured certificate trust. The passive data connection must
resume the control TLS session before file bytes are sent; completion requires a
matching server-reported file size. Start checks fresh idle status and no active fault
before upload and again before sending `project_file` once. FINISH/FAILED permit Start
after a separate bed acknowledgement through the running server. The attempt is saved before
sending, so a lost reply or restart cannot cause an automatic second Start. Read job
status to reconcile an uncertain outcome. A reply reporting rejection names authorization
as a possible cause; delivery or an acknowledgement alone never means execution.
Commands wait for matching live state for up to 25 seconds for start/stop and 10 seconds
for pause/resume. Pending is announced on stderr before “not confirmed”; waiting never
resends a command. Failed or uncertain FTPS uploads delete the staged file before returning
an error. If cleanup is not confirmed, inspect storage and remove it before preparing again.
Pause, resume, and job cancel require a matching tracked execution. Printer Stop also
applies to an external live execution after a fresh identity-bound confirmation.
Matching requires both the unique subtask name and a positive `gcode_start_time`; missing
execution identity blocks control. A later execution of the same file is a different job.
FINISH and FAILED consume bed clearance even between status polls. Stop requires FAILED
for the matching job before reporting cancellation; idle alone proves no outcome.

The single start profile is `lan-project-1`, a local plate-1 project profile. It uses
`Metadata/plate_1.gcode`, local identifiers of zero, automatic bed type, and no timelapse.
Bed leveling defaults on; the other calibration options default off. It requires `Metadata/project_settings.config` with
an explicit `filament_type` array. Missing metadata or tool selections outside that list
are refused. Prepare and review again after changing any start setting.

Before upload, the sliced plate G-code comments or project settings must provide one
unambiguous `nozzle_diameter` matching the installed report. A slicer-written
`nozzle_type`, when present, must match the installed nozzle material. Each mapped
`filament_type` must match its AMS slot's `tray_type`; a single filament without AMS
mapping must match external-spool `vt_tray.tray_type`. Missing live values or mismatches
refuse upload and name the required and installed values. Fit/load matching hardware
and material or prepare again; there is no mismatch override.

Optional settings belong to the Bambu printer table (place these after its profile names):

```toml
# Map every project filament, numbered from 1, to a zero-based global AMS slot.
# ams_mapping = { "1" = 3, "2" = 0 }
[printers.start_profile]
name = "lan-project-1"
archive_url = "sdcard"          # Alternative: "ftp"
md5_target = "plate"            # Alternative: "empty"
leveling_field = "bed_leveling" # Alternative: "bed_levelling"
ams_convention = "per-filament" # Alternative: "right-aligned-five"
bed_leveling = true # Set false explicitly to disable bed leveling.
flow_cali = false
vibration_cali = false
layer_inspect = false
```

`sdcard` sends a `file:///sdcard/` archive URL; `ftp` sends `ftp:///`. `plate` hashes the
plate G-code with MD5; `empty` sends an empty MD5 string. No archive-hash target is
established by the cited documentation. The spelling setting chooses exactly one bed
leveling field. `per-filament` sends one slot per filament, in project order, with global
slots 0–15. `right-aligned-five` puts the same ordered assignments at the end of a
five-element array, pads with -1, and supports at most four filaments and slots 0–3.
Every filament in a multi-filament archive needs an explicit mapping; refusal lists each
missing filament number and material. A single filament with no mapping uses the
community-documented `use_ams = false` and empty-string mapping form for external spool.
This does not load filament: check the spool and physical tooling before review.

Defaults follow the more recent [first-hand project notes](https://github.com/voska/bambu/blob/main/docs/protocol.md)
for the URL, plate checksum, spelling, and per-filament mapping. The alternatives follow
[OpenBambuAPI's project and AMS documentation](https://github.com/Doridian/OpenBambuAPI/blob/main/mqtt.md).
These sources disagree and provide no universal firmware contract. Select alternatives
only after checking your model and firmware; the profile name is a version identifier,
not a hardware certification. Unsupported values are refused before a command is sent.

## Moonraker (Klipper)

Controls: `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.
`resume` is omitted unless `resume_commissioned = true`.

Configure the Moonraker address explicitly; there is no discovery. HTTP and HTTPS are
supported, including reverse-proxy base paths. HTTPS uses system certificate trust and
checks the configured hostname; redirects and certificate exceptions are refused.

```toml
[backend]
kind = "printers"

[slicer]
kind = "prusaslicer" # or "orcaslicer"; use named slicers for multiple choices
# path = "/path/to/prusa-slicer"

[[printers]]
kind = "moonraker"
id = "klipper"
name = "Workbench Klipper"
model = "Commissioned Klipper printer"
url = "http://printer.local:7125"
api_key = "REPLACE_WITH_YOUR_MOONRAKER_KEY"
printer_profile = "Commissioned machine"
print_profile = "Reviewed print settings"
filament_profile = "Loaded material"
resume_commissioned = false
```

Obtain the API key through the installed interface's authorization controls. It is sent
as `X-Api-Key`. Omit `api_key` only when the installation explicitly trusts this client.
Other authentication forms are not implemented. No cloud account is required. See
[Moonraker authorization](https://moonraker.readthedocs.io/en/latest/external_api/authorization/).

Status opens one monitoring WebSocket lazily at `/websocket` (WSS for HTTPS), using
that same API key header and reverse-proxy base path. It subscribes only to available
objects, uses the subscription reply as a baseline, and merges changed object fields.
A live connection and a message within 30 seconds are required to reuse that baseline.
Before the first baseline, after socket loss, and while reconnecting, each status read
falls back to the existing HTTP status query. Reconnects use exponential backoff from
one to 30 seconds and never replay commands. Klippy disconnect or shutdown notifications
invalidate readiness until a new subscription baseline arrives. Graceful shutdown closes
the socket. Messages and merged object state are each limited to 1 MiB.
Uploads, machine commands, file metadata, and execution history continue to use HTTP;
execution identity, confirmation, readiness, and bed-clear checks still apply.

Commission profiles for the actual model, build volume, nozzle, tooling, material, and
plate. Only a single extruder is supported for control; unresolved material slots are
refused. [Snapmaker U1](#snapmaker-u1) printers have their own tool rule. Loaded material and physical plate identity have no universal telemetry here:
check them before confirming each start. Set `resume_commissioned = true` only after
checking that this printer's installed pause/resume macros behave safely. A changed
machine configuration, including macros, requires fresh preparation and review.

The HTTP fallback reads server connectivity, available objects, and a full object
snapshot. The same mapping applies to subscription baselines and merged updates.
Klippy startup reports busy while it connects; shutdown, error, missing objects,
and contradictory states block movement; disconnection or unavailable telemetry reports offline. `print_stats` standby,
complete, and cancelled map to idle; printing and paused retain those states; unknown
states map to error. `detail` contains the known state or a safe explanation. Raw fault
messages are omitted because they may contain private data. Progress is a fraction;
temperatures and job names remain absent when unavailable. Live `seconds_left` is omitted
because these objects provide no universal time-left measurement. Preparation estimates
still come from the slicer. See [printer objects](https://moonraker.readthedocs.io/en/latest/printer_objects/).

Preparation slices locally to text G-code and reads machine configuration without uploading.
The slicer-written `nozzle_diameter` must match Klipper's
`configfile.settings.extruder.nozzle_diameter` during preparation, challenge review, and
before upload. Missing or ambiguous values block the action; mismatches name both diameters.
There is no override. This verifies configuration; check the physical nozzle separately.
See [Klipper configuration status](https://www.klipper3d.org/Status_Reference.html#configfile).

Acknowledge the bed through a clear challenge and the API clear route in the running server,
then request a separate Start challenge. Clearance expires and is polled under the shared policy.
Polling also reads the latest Moonraker history entry: changes to execution identity or its
fractional start timestamp, or to observed `print_stats` state or filename, withdraw clearance.
History timestamps remain finite floating-point Unix seconds, without truncation.
Activity with no retained history or observed state change cannot be detected.

Start requires fresh ready status, an acknowledged clear bed, unchanged input and G-code,
and unchanged machine identity, firmware, configuration, and profiles. A unique file is
staged under `gcodes` with `print=false` and a SHA256 checksum; existing staging names are
refused. The upload must report neither printing nor queueing. Stored metadata must match
the reviewed filename and size before an explicit print start. Prepared files are limited
to 200 MiB, response bodies to 1 MiB, status snapshots to four seconds, and upload inactivity to
30 seconds, reset by transport byte progress. Metadata that is not ready within these checks
blocks start; it does not cause a start retry. Failed or uncertain uploads and failed
stored-file validation trigger deletion of the staged file before returning an error.
If cleanup cannot be confirmed, inspect Moonraker storage and remove the file before preparing again.
These failures never send Start. See [file management](https://moonraker.readthedocs.io/en/latest/external_api/file_manager/).

Every start, pause, resume, cancel, and clear uses the shared CLI/API confirmation and bed
policy. Tracked service and CLI jobs are checkpointed after changes; a start attempt is saved
durably before sending the command.
A lost reply is reconciled through fresh status, file metadata, and job history; start is
never repeated automatically. Accepted commands poll for matching status for up to 25 seconds
for start/cancel and 10 seconds for pause/resume, announcing pending on stderr.
Expiry leaves an unconfirmed outcome: read status before another action. Polling never resends
the command. Pause, resume, and cancel require the same live filename and
execution ID started here. Moonraker's history component must be available for execution
tracking and control. Completion, cancellation, and failure remain separate outcomes;
idle alone cannot finish a tracked job. Any start attempt consumes bed clearance, and
terminal reports require a new check. A stop requested during upload or pending start
cancels that same job once fresh status identifies it. See [print control](https://moonraker.readthedocs.io/en/latest/external_api/printer/)
and [job history](https://moonraker.readthedocs.io/en/latest/external_api/history/).

Testing uses synthetic responses on test-owned loopback servers. Physical printer and
macro commissioning remains an installation step.

## Snapmaker U1

Controls: `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.
`resume` is omitted unless `resume_commissioned = true`.

The Snapmaker U1 runs Snapmaker's fork of Moonraker. Its published
[API documentation](https://github.com/Snapmaker/u1-moonraker/blob/main/docs/web_api.md) is
upstream Moonraker's, so `kind = "snapmaker-u1"` uses the Moonraker backend above: the same
address and `api_key` settings, status mapping, staging, start, pause, resume, cancel, Stop,
history reconciliation, confirmations, bed clearance, and cleanup. Only the toolhead checks
differ.

```toml
[[printers]]
kind = "snapmaker-u1"
id = "u1"
name = "Workbench U1"
model = "Snapmaker U1"
url = "http://u1.local:7125"
api_key = "REPLACE_WITH_YOUR_MOONRAKER_KEY"
printer_profile = "Commissioned U1 profile"
print_profile = "Reviewed print settings"
filament_profile = "Material loaded in the print tool"
resume_commissioned = false
```

The printer's tools are Klipper's `extruder`, `extruder1`, and so on in
`configfile.settings`, numbered from 0 without gaps, each with a `nozzle_diameter`. Another
section named `extruder...`, a missing nozzle size, or an active `toolhead.extruder` that
is not one of these tools is refused. Status `nozzle_temperature` is the active tool's.

The documented API reports no loaded material per tool, so a job may use only one tool.
The sliced G-code must select it with a `T<n>` command, where `T0` is `extruder` and `T1` is
`extruder1`. Preparation, challenge review, and the check before upload refuse G-code that
selects no tool, selects more than one (naming them), selects a tool the printer does not
have, or changes tools with `ACTIVATE_EXTRUDER`. The slicer's `nozzle_diameter` entry for
that tool must match that tool's configured `nozzle_diameter`; a mismatch names the tool and
both diameters, and missing or disagreeing values are refused. There is no override. A
changed tool count or nozzle size after preparation requires preparing again.

Check the material loaded in the print tool, the physical nozzle, and the plate before
confirming each start. Testing uses a simulated U1 on a test-owned loopback server.

## Duet (RepRapFirmware standalone)

Controls: `start`, `pause`, `resume`, `cancel`, `stop`, and `clear`.

This backend uses the standalone RepRapFirmware HTTP interface (`rr_` requests) of a Duet
board running without a single-board computer. For an attached SBC running Duet Software
Framework, use the [SBC variant](#duet-with-sbc-duet-software-framework). Configure the
address explicitly; there is no discovery. HTTP is the board's own interface; HTTPS is accepted only through a gateway you configure, which must
present a certificate trusted by the system, for the configured hostname. Redirects and
certificate exceptions are refused. Keep HTTP on a trusted local network.

```toml
[backend]
kind = "printers"

[slicer]
kind = "prusaslicer" # or "orcaslicer"; use named slicers for multiple choices

[[printers]]
kind = "duet"
id = "duet"
name = "Workbench Duet"
model = "Commissioned Duet printer"
url = "http://duet.local"
password = "REPLACE_WITH_THE_MACHINE_PASSWORD" # omit when none is set
nozzle_diameter = 0.4 # millimetres; the installed nozzle
printer_profile = "Commissioned machine"
print_profile = "Reviewed print settings"
filament_profile = "Loaded material"
```

`password` is the machine password set with `M551`. Omitting it sends none; the firmware
then assumes its default. It is sent only to the login request and never appears in
reports, logs, or errors. Each login requests a session key (RepRapFirmware 3.5 and later)
and sends it as `X-Session-Key`. Older firmware associates the session with this computer's
address instead, so other clients on the same computer share it. The reported session
timeout is honoured: an idle session logs in again first. A request refused with 401 logs
in again; reads and uploads are then sent once more, but G-code is never resent: the
command reports that the session expired and needs fresh status before another action.
A refused password reports an authentication failure; a board with no free sessions
reports that in `detail` as an offline state. Close other clients or wait for their
sessions to expire. See [HTTP requests](https://github.com/Duet3D/RepRapFirmware/wiki/HTTP-requests).

The object model does not report a nozzle diameter. Set `nozzle_diameter` to the installed
nozzle after checking it, in millimetres, greater than 0 and at most 10. Only Duet and
Repetier-Server printers accept it. Preparation, challenge review, and upload refuse a missing value, and the sliced
`nozzle_diameter` must match it; mismatches name both values, with no override. This checks
configuration, not the physical nozzle. Commission the profiles for the actual model, build
volume, tooling, material, and plate. Only one tool with one extruder and one heater is
supported for preparation and control.

Status reads the [object model](https://github.com/Duet3D/RepRapFirmware/wiki/Object-Model-Documentation)
with `rr_model` keys `state`, `job`, `heat`, and `tools`, within four seconds. The legacy
`rr_status` response is not used. `state.status` maps as follows; `detail` keeps the known
value:

| `state.status` | State |
|---|---|
| `idle` | idle |
| `processing`, `pausing`, `resuming`, `cancelling`, `changingTool` | printing |
| `paused` | paused |
| `busy`, `starting`, `updating`, `simulating` | busy |
| `halted` | error |
| `off`, `disconnected` | offline |
| anything else | error, with an unknown-state `detail` |

Only idle permits Start. `job_id` is the live file path, such as
`0:/gcodes/part.gcode`, and `job_name` its file name. Progress is
`job.filePosition / job.file.size`: the fraction of file bytes read, not of elapsed print
time. `seconds_left` comes from `job.timesLeft.file` when the firmware reports it. The
nozzle temperature is the first heater of the tool numbered `state.currentTool`, found by
tool number rather than list position; it is omitted when no tool is selected. The bed
temperature uses the first configured entry of `heat.bedHeaters`.

Preparation slices locally to text G-code and reads board name, firmware version, machine
name, axis limits, and tool configuration without uploading. Changes to any of them, or to
the input, G-code, or profiles, require new preparation and review. Acknowledge the bed
through a clear challenge and the API clear route in the running server, then request a
separate Start challenge. Clearance follows the shared policy; polling also withdraws it
when the status, the live file, or `job.lastFileName` changes. Activity between polls that
leaves all three unchanged cannot be detected.

Start checks fresh idle status and the review, then lists `0:/gcodes` with `rr_filelist`
and refuses an existing staging name. It uploads the unique file with `POST /rr_upload`,
raw bytes, `Content-Length`, and a CRC-32, under the shared 30-second transport idle
timeout. The reply must report `err` 0 and the listing must show the reviewed size. Failed
or uncertain uploads and failed size checks delete the staged file with `rr_delete` and
confirm through the listing that it is gone before returning an error; Start is never sent.
If cleanup cannot be confirmed, inspect Duet storage and remove the file before preparing
again. After a final idle and review check, the attempt is saved durably and
`M32 "0:/gcodes/<file>"` is sent once through `rr_gcode`. A lost reply is reconciled from
status and never causes a second Start. See the [G-code reference](https://docs.duet3d.com/User_manual/Reference/Gcodes).

Pause sends `M25` and resume `M24`, only for the live file this companion started. Cancel
and printer Stop use `M0`, which cancels only a paused job: a running job is first paused
with `M25`, so the machine's `pause.g` runs before `cancel.g` (or `stop.g`). Each command is
sent once. Start and cancel wait up to 25 seconds for matching status, pause and resume up
to 10 seconds, and the pause before a cancel up to 10 seconds; if that pause is not
confirmed, `M0` is not sent. Pending is announced on stderr; expiry leaves an unconfirmed
outcome to reconcile from fresh status.

Standalone firmware does not report whether a file completed or was cancelled. A tracked
job becomes `cancelled` when this companion cancelled it and the printer is idle again,
`failed` when the printer halts, and otherwise `ended` (outcome unknown) once the printer
is idle after the job was seen running, or reports the job's unique file as the last one
processed. Its staged file is then deleted once the printer is idle; external files are
never removed.

Testing uses synthetic responses on test-owned loopback servers. Session handling, path
spelling in `job.file.fileName`, delete replies for missing files, and macro behaviour still
need checking on a real board before relying on control.

### Duet with SBC (Duet Software Framework)

Use the configuration above with `kind = "duet-sbc"` and `url` pointing to DuetWebServer
on the SBC. Base paths are preserved. The same slicers, profiles, single-tool restriction,
`nozzle_diameter`, confirmations, bed clearance, status mapping, waits, Stop, and cleanup
rules apply. This variant has simulated coverage only; see the
[SBC hardware checks](hardware-checklist.md#duet-with-sbc-duet-software-framework).

The [DSF REST documentation](https://github.com/Duet3D/DuetSoftwareFramework/wiki/REST-API)
defines these transport differences:

- Login uses `GET /machine/connect`, with optional `password` in the query. The returned
  `sessionKey` is sent as `X-Session-Key`. After eight idle seconds, login is renewed.
  A 403 renews authentication for a read or refused upload once; G-code is never resent.
  A wrong password is an authentication failure. Disconnect uses
  `GET /machine/disconnect` on release when the runtime remains available; otherwise
  authentication expires on the server.
- `GET /machine/model` returns the full shared object model. Each status or machine
  review uses one bounded full-model read, with the same fields as standalone Duet.
- `GET /machine/directory/0:/gcodes` checks for an existing staging name. Upload sends raw
  bytes with `PUT /machine/file/0:/gcodes/<file>` and requires HTTP 201. DSF documents no
  CRC-32 upload parameter. Both the directory size and
  `GET /machine/fileinfo/0:/gcodes/<file>` size must match the reviewed bytes.
- Cleanup uses `DELETE /machine/file/0:/gcodes/<file>` (204, or 404 if already absent),
  followed by a directory read confirming absence. Failed or uncertain uploads follow
  the same cleanup rule and never send Start.
- G-code uses `POST /machine/code?async=true` with a `text/plain` body. A 200 with an empty reply
  proceeds to the shared telemetry wait. Nonempty replies are treated conservatively as
  uncertain and are never shown or retried. Start is `M32`, pause `M25`, resume `M24`,
  and cancel or Stop `M0` after a confirmed pause.

The shared tracking policy reports `ended` for an ending without a confirmed companion
cancellation or halt; idle alone never establishes successful completion. Physical nozzle,
material, plate, and macro behaviour still require checks on the actual machine.

## Repetier-Server

Controls: `start`, `cancel`, and `clear`. Printer-level Stop, pause, and resume are refused.

`kind = "repetier"` talks to a [Repetier-Server](https://www.repetier-server.com/) installation,
a host program that runs one or more printers. Repetier-Server 2.0 is the final release: the
vendor has [ended development](https://www.repetier-server.com/the-end-of-repetier-server-development/),
made 2.0 free, and publishes no further fixes or compatibility updates. Configure the server
address explicitly; there is no discovery. HTTP and HTTPS are accepted, including a reverse
proxy base path. HTTPS uses system certificate trust and checks the configured hostname;
redirects and certificate exceptions are refused. Keep HTTP on a trusted local network.

```toml
[backend]
kind = "printers"

[slicer]
kind = "prusaslicer" # or "orcaslicer"; use named slicers for multiple choices

[[printers]]
kind = "repetier"
id = "bench"
name = "Workbench printer"
model = "Commissioned printer"
url = "http://repetier.local:3344"
api_key = "REPLACE_WITH_THE_REPETIER_SERVER_API_KEY"
slug = "Workbench" # the printer's slug in Repetier-Server
nozzle_diameter = 0.4 # millimetres; the installed nozzle
printer_profile = "Commissioned machine"
print_profile = "Reviewed print settings"
filament_profile = "Loaded material"
```

The server listens on port 3344 unless its configuration changes that. One server can host
several printers, so `slug` names the one this entry controls: 1–64 letters, digits, hyphens,
or underscores, as shown in Repetier-Server's printer settings. Only Repetier-Server printers
accept `slug`. Use a user's API key from **User Profiles** to give Print only that user's
rights, or the global key in **Global Settings → Connectivity**. The key is sent only as the
`x-api-key` header, never in a URL, because the manual warns that proxies may store keys
given as the `apikey` parameter. It never appears in reports, logs, or errors. Print needs
the printing, adding files, and deleting files permissions.

Every request uses the manual's [API](https://www.repetier-server.com/manuals/programming/API/index.html):
websocket commands are sent through its REST form, `/printer/api/<slug>?a=<command>&data=<json>`,
and uploads go to `/printer/model/<slug>`. No websocket is opened, so no session or ping is
needed and server events are not received.

### Status

Status reads `listPrinter`, `stateList`, and `listJobs` within four seconds. The entry whose
`slug` matches maps as follows; `detail` keeps the value shown:

| `listPrinter` entry | State | `detail` |
|---|---|---|
| `online` 0, `active` false | offline | `deactivated` |
| `online` 0 | offline | `offline` |
| `online` 1, `active` true, `job` `none` | idle | `online, no job` |
| `online` 1, `active` true, `job` naming a job | printing | `online, job active` |
| anything else | error | the raw `online`, `active`, and `job` values |
| no entry with the slug | error | asks you to check `slug` |

The manual shows only `none` for `job` and lists no state values, so nothing else is
inferred. Repetier-Server reports no paused state over this interface: a paused job still
shows as printing. Only idle permits Start. `job_name` is the live `job` value, and `job_id`
is the id of the queued job with that name in `listJobs`, when exactly one has it. The manual
does not give units for `printTime` or say what `printedTimeComp` measures, so `progress` and
`seconds_left` are omitted. The nozzle temperature is `tempRead` of the extruder numbered
`activeExtruder` in `stateList`, and the bed temperature `heatedBed.tempRead`. Target
temperatures (`tempSet`) are not reported, since the job API has no field for them. An
authentication error reply reports offline with an authentication `detail`.

### Preparation and Start

Repetier-Server does not report a nozzle diameter. Set `nozzle_diameter` to the installed
nozzle after checking it, in millimetres, greater than 0 and at most 10. Preparation,
challenge review, and upload refuse a missing value, and the sliced `nozzle_diameter` must
match it; mismatches name both values, with no override. Preparation slices locally to text
G-code and reads the server `version` and the printer's `getPrinterConfig`: firmware name,
printer name and variant, heated bed, axis limits, and extruders. Only a printer configured
with one extruder is supported. Changes to these, or to the input, G-code, or profiles,
require new preparation and review. Acknowledge the bed through a clear challenge and the API
clear route in the running server, then request a separate Start challenge. Clearance follows
the shared policy; polling also withdraws it when the state, the live job name, or its id
changes. Activity between polls that leaves all three unchanged cannot be detected.

Start checks fresh idle status and the review, refuses a staging name already present in
`listModels` or `listJobs`, and asks `canUploadGCodeWithSize` for free space. It uploads the
reviewed G-code as multipart field `filename` to `/printer/model/<slug>?a=upload&name=<name>`,
where the name is unique to the job, under the shared 30-second transport idle timeout. The
manual does not describe the upload reply, so success is HTTP 200 without an `error`, and
`listModels` must then list exactly one model with that name and the reviewed size as
`length`. A failed or uncertain upload or a failed check removes the staged model with
`removeModel` and confirms through `listModels` that it is gone before returning an error;
nothing is queued or started. If cleanup cannot be confirmed, inspect the model list and
remove the model before preparing again.

`copyModel` takes a flag to start the print, so the attempt is saved durably before it is
sent, once, with `autostart` false. The queued job is found in `listJobs` by its unique
name; if it cannot be found, the outcome is reported as unconfirmed and nothing more is
sent. If the printer is still idle, `startJob` is sent once with the queued job's id. A lost
reply is reconciled from status and never causes a second Start. Start waits up to 25
seconds for the printer to report the job; pending is announced on stderr, and expiry leaves
an unconfirmed outcome to reconcile from fresh status. A server that starts the queued job
by itself is waited for and never sent `startJob`.

### Cancel, Stop, pause, and resume

Cancel sends `stopJob` once, only while the live `job` is the unique name of
the job this companion started, and waits up to 25 seconds for the printer to stop reporting
it. `stopJob` takes no job id and stops whatever runs on the printer, so printer-level Stop
is refused; use Cancel for tracked jobs and stop external work in Repetier-Server. The live
name is checked just before the command, but a job the server starts in the moment between that check and `stopJob` would be
stopped instead; keep the server's automatic start of the next queued job off for printers
used with Print. A job that was queued but never ran, for example after a refused `startJob`,
is cancelled with `removeJob`, which the manual allows only for a job that is not printing,
and confirmed through `listJobs`.

The manual documents no pause command, and Repetier-Server reports no paused state, so
pause is refused and resume is never offered: a paused job could not be told apart from a
running one. Pause and resume jobs in Repetier-Server; `continueJob` is not sent.

Repetier-Server reports how a job ended only through websocket events, which Print does not
receive. A tracked job becomes `cancelled` when this companion stopped it and `ended`
(outcome unknown) when it no longer runs after being seen running. Its staged model, and any
queued copy, are then removed with `removeModel` and `removeJob` once the printer is idle and
confirmed gone through the lists; external files are never removed.

### What needs a real server

Testing uses a simulated server on a test-owned loopback socket. Before relying on control,
check on a real Repetier-Server 2.0:

- the REST form's reply for each command, especially whether it is the websocket `data`
  payload unwrapped, and the reply to an upload;
- what `listPrinter` `job` holds while a job runs or is paused, and whether it is the name
  given at upload, with or without the `.gcode` extension (both are matched);
- whether `listModels` `length` is the stored size in bytes;
- whether `copyModel` with `autostart` false ever starts a job, on 2.0 and on servers older
  than 0.93, which do not document the flag;
- whether a queued job stays in `listJobs` while printing and after it ends;
- whether `stopJob` also stops a paused job, and how `removeJob` replies for a printing job.

## Elegoo (SDCP)

Controls: `stop` only for resin; none for Centauri. Bed clearance is unavailable for both.

`kind = "elegoo-sdcp"` monitors Elegoo printers that speak SDCP, a JSON protocol over a
WebSocket. Set `profile` to say which protocol variant the printer uses; there is no default
and no detection:

- `profile = "resin"` follows CBD's
  [SDCP V3.0.0 specification](https://github.com/cbd-tech/SDCP-Smart-Device-Control-Protocol-V3.0.0)
  for resin printers. It adds printer-level Stop.
- `profile = "centauri"` is the Elegoo Centauri Carbon (FDM), from
  [community protocol notes](https://github.com/WalkerFrederick/sdcp-centauri-carbon/blob/main/README.md)
  written against firmware 1.1.29. It is monitoring only.

The two profiles share the connection and message envelope but never their status codes:
the same number means different things on each.

```toml
[backend]
kind = "printers"

[[printers]]
kind = "elegoo-sdcp"
profile = "resin"
id = "resin"
name = "Resin bench"
model = "Elegoo resin printer"
host = "192.168.1.40"
# port = 3030 # Optional; resin defaults to 3030.
# mainboard_id = "000000000001d354" # Optional; refuse reports from any other mainboard.

[[printers]]
kind = "elegoo-sdcp"
profile = "centauri"
id = "centauri"
name = "Centauri Carbon"
model = "Elegoo Centauri Carbon"
host = "192.168.1.41"
```

`host` is the printer's hostname or IP address, without a scheme or path. Print connects to
`ws://<host>:3030/websocket` for resin and `ws://<host>/websocket` for Centauri; `port`
replaces either port. There is no UDP discovery or network scan: configure the address
yourself. Neither source describes a login, credentials, or TLS, so the connection is plain
and unauthenticated; keep these printers on a trusted local network. `mainboard_id`, at most
64 letters and digits, pins the printer's `MainboardID`; reports from another mainboard are
then refused and the printer shows offline. Print never slices for these printers, so they
take no `slicer` and no slicer profiles, and every other printer field is refused.

### Connection and freshness

Each printer keeps one WebSocket connection, opened at the first status read. Print requests
status with command 0 every 10 seconds and sends a text `ping` every 20 seconds; the Centauri
notes report that the printer closes a connection after about 60 seconds without client
traffic. A status report is fresh for 30 seconds. A connection that delivers nothing for 45
seconds is closed and reopened; reconnects back off from 1 second, doubling up to 30 seconds.
After any reconnect the printer reads offline, and refuses every action, until a new status
report arrives on the new connection; the reconnect also withdraws bed clearance. Messages
are limited to 1 MiB.

Requests use the specification's envelope (`Id`, `Data` with `Cmd`, `Data`, `RequestID`,
`MainboardID`, `TimeStamp`, and `From`). Resin requests add `Topic: sdcp/request/<MainboardID>`
and `From: 0`; Centauri requests match the notes' examples, with no `Topic` and `From: 1`.
`MainboardID` and `Id` are learned from the printer's reports and replies (or taken from
`mainboard_id`); until the first report, requests carry an empty `MainboardID`. Replies are
matched by `RequestID` and `MainboardID`; others are ignored.

### Resin status

`Status.CurrentStatus` lists machine activities and `Status.PrintInfo.Status` is the print
substate. The substate keeps its last value after a print ends, so it is read only in light of
the activity:

| Activity and substate | State | `detail` starts with |
|---|---|---|
| Printing (1), substate 1–5, 7 or 10 | printing | the substate: `HOMING`, `DESCENDING`, `EXPOSING`, `LIFTING`, `PAUSING`, `STOPPING`, `FILE_CHECKING` |
| Printing (1), substate 6 | paused | `PAUSED` |
| Printing (1), substate 0, 8 or 9 | printing | `PRINTING`: a retained STOPPED or COMPLETED never overrides an active print |
| File transfer (2), exposure test (3), self-test (4) | busy | `FILE_TRANSFERRING`, `EXPOSURE_TESTING`, `DEVICES_TESTING` |
| Idle (0), substate 0, 8 or 9 | idle | `IDLE`, `STOPPED`, `COMPLETED` |
| Idle (0) with an active substate | error | the substate, noting the contradiction |
| Nonzero `PrintInfo.ErrorNumber` | error | the substate, then `Error <n>:` and its meaning |
| Missing, empty, or unknown activity or substate | error | `UNKNOWN ACTIVITY` or `UNKNOWN PRINT STATUS <n>` |

`job_id` is `PrintInfo.TaskId` and `job_name` `PrintInfo.Filename`; both can remain from the
last job while idle. While printing or paused, `progress` is `CurrentLayer / TotalLayer` and
`seconds_left` is `(TotalTicks − CurrentTicks) / 1000`, since the specification gives ticks in
milliseconds. The UV LED (`TempOfUVLED`) and enclosure (`TempOfBox`) temperatures appear in
`detail`, rounded to whole degrees; they are never reported as nozzle or bed temperatures.

### Centauri status

Only `Status.PrintInfo.Status` is read, with the meanings the notes observed:

| `PrintInfo.Status` | State | `detail` starts with |
|---|---|---|
| 0 | idle | `IDLE` |
| 5, 8, 9, 13, 20 | printing | `PAUSING`, `PREPARING`, `STARTING`, `PRINTING`, `RESUMING` |
| 10 | paused | `PAUSED` |
| anything else, or missing | error | `UNKNOWN STATUS <n>` |

Resin meanings never apply: Centauri 8 and 9 are preparation and start, not stopped and
completed. An unknown code is reported as error rather than attention, with the raw code in
`detail`, because nothing is known about it and it must block every action. `job_id` and
`job_name` are `TaskId` and `Filename`; `progress` is the layer fraction while printing or
paused. `nozzle_temperature` is `TempOfNozzle`, `bed_temperature` `TempOfHotbed`, and the
enclosure temperature appears in `detail`. The notes do not give tick units, so
`seconds_left` is always omitted.

### What Print can do

Centauri printers are monitoring only. The notes show commands for start, pause, resume, and
cancel, but there is no vendor contract for their fields, effects, or replies on FDM firmware,
so Print never sends upload, start, pause, resume, or Stop. Preparation, every confirmation
challenge, and every action route are refused with an explanation, and
nothing but status requests and keepalives reaches the printer.

Resin printers accept printer-level Stop of the live job, using command 130 from the
specification. Print slices FDM G-code only, while resin printers need CTB or GOO files made
by a resin slicer, so preparation and Start are refused: send resin jobs from that slicer or
from the printer, then monitor and Stop them here. Pause and resume are refused too. Their
routes act on, and return, a job this companion started and is tracking, and Print never
starts resin jobs; Stop is the one control the job API defines for work started elsewhere,
and it is bound to the live job you confirmed. Pause or resume on the printer itself.

Stop requires fresh status, a printing or paused state, and the same `TaskId` and `Filename`
you confirmed. Print sends command 130 once and waits up to 10 seconds for the reply with the
same `RequestID` and `MainboardID`. A nonzero `Ack` is reported with its meaning from the
specification (busy, file not found, and so on). A lost connection or missing reply reports
an uncertain outcome; the command is never resent, and a reconnect never replays it. After an
accepted reply, Stop waits up to 25 seconds for a report of the same task in substate STOPPED
(8) with no active print activity. A report naming another task, or COMPLETED (9) for the
same task, is never taken as the Stop outcome.

Testing uses a simulated SDCP printer on a test-owned loopback server. Before relying on
these printers, check on real hardware: whether resin firmware answers status requests sent
before its `MainboardID` is known (set `mainboard_id` if it does not), whether it accepts the
learned `Id`, the default ports, how often it pushes status unasked, how `CurrentStatus` and
`TaskId` look after a Stop, and, for Centauri, whether status 8 appears while idle, as one
example in the notes suggests, and which further status codes the firmware uses.

## Snapmaker 2.0 and J1/Artisan

Controls: none, including bed clearance, for both kinds.

Print monitors two older Snapmaker families. Both are monitoring only: Print reads status
and never uploads, starts, pauses, resumes, cancels, or stops a job. Each uses the transport
Snapmaker documents for its networked models:

- `kind = "snapmaker-2"` is the Snapmaker 2.0 series (A150, A250, A350), over the HTTP API
  Snapmaker described on its
  [forum](https://forum.snapmaker.com/t/do-snapmaker-2-0-devices-have-a-webpage/5455/4/).
- `kind = "snapmaker-j1"` is the Snapmaker J1 and Artisan, over SACP on TCP, from Snapmaker's
  [SACP documentation](https://github.com/Snapmaker/Snapmaker-SACP/tree/main/docs).

The Snapmaker U1 uses neither; it has its own kind, `snapmaker-u1`.

```toml
[backend]
kind = "printers"

[[printers]]
kind = "snapmaker-2"
id = "a350"
name = "Workshop A350"
model = "Snapmaker 2.0 A350"
host = "192.168.1.50"
# port = 8080 # Optional; defaults to 8080.

[[printers]]
kind = "snapmaker-j1"
id = "j1"
name = "Snapmaker J1"
model = "Snapmaker J1"
host = "192.168.1.51"
# port = 8888 # Optional; defaults to 8888.
```

`host` is the printer's hostname or IP address, without a scheme or path; `port` replaces
the default. There is no discovery or network scan, although Snapmaker documents a broadcast
one: configure the address yourself. Neither source describes TLS, so the connection is plain;
keep these printers on a trusted local network. Print never slices for these printers, so
they take no `slicer` and no slicer profiles, and every other printer field is refused.

### Snapmaker 2.0

The first status read sends `POST /api/v1/connect`, which the user must confirm on the
touchscreen. While the request waits, the printer shows `offline` with `detail` asking you to
tap Confirm. The wait is bounded at 60 seconds: if the printer has not confirmed by then, Print
keeps no connection, releases any token it received with `POST /api/v1/disconnect`, and shows
`offline` with `detail` saying to approve the request on the touchscreen and retry. The next
status read at least 30 seconds later sends a new request; reading status again is the retry.

The connect reply carries a token. Print keeps it in memory only, never logs it, and never
puts it in a response, an error, or `detail`; a printer value that repeats it is shown as
`(withheld)`. Status is `GET /api/v1/status` with the token as the `token` query parameter,
every 10 seconds. A reply is fresh for 30 seconds; after that, or with no live reply, the
printer is `offline`. If the printer refuses the token once it has been accepted, the
connection ends and the next status read requests a new one, which needs confirming again.

| `status` | State | `detail` starts with |
|---|---|---|
| `IDLE` | idle | `IDLE` |
| anything else, or missing | error | the value, sanitised and at most 32 characters, or `UNKNOWN STATUS` |

Snapmaker's description shows only `IDLE`, so no running, paused, or progress state is
inferred from any other value; it stays an error until the value is documented. `detail`
then lists the connect reply's `series`, `headType` (shown as reported), and an enclosure
when `hasEnclosure` is true. The published example reports `headType` as a number, and
Snapmaker has not published what the numbers mean, so a numeric head type is shown but not
interpreted: check the installed head yourself. A `headType` given as text that names a
laser or CNC head means the machine is not a printer right now, and an idle printer then
shows `attention`. The head is never guessed from `series` or the product name. Positions
and offsets in the status reply are not shown.

Print does not send `POST /api/v1/disconnect` when it shuts down; the connection is simply
dropped.

### Snapmaker J1 and Artisan

Print keeps one TCP connection per printer, opened at the first status read, and sends only
two documented requests every 10 seconds: module info (command set `0x01`, command `0x20`)
and machine info (`0x01`, `0x21`), addressed from the software peer (0) to the controller
(1). Packets follow Packet.md and Types.md: `AA 55`, a little-endian length of the bytes from
byte 7 to the end, version 1, a CRC-8 of the first six bytes, sender, attribute, a sequence
number that increases with each request, command set and ID, the payload, and the TCP/IP
checksum of bytes 7 onward. A packet is used only once it is complete and both checks pass;
anything else is skipped. A reply is applied only if it is an acknowledgement addressed to
the software peer with the sequence, command set, and command ID of a request from the
latest round, so a late reply to an earlier request is ignored.

The documentation describes no pairing or handshake for network clients. If the printer
does not answer within 3 seconds of connecting, it shows `offline` with `detail` saying that
Snapmaker has not published how these printers pair with network clients; check the
touchscreen for a connection request. Print keeps the connection open and keeps asking, and
a later reply is used normally. A module list is fresh for 30 seconds; after that, or after a
reconnect until the new connection answers, the printer is `offline`. Reconnects back off from
1 second, doubling up to 30 seconds.

Snapmaker has not published the J1 or Artisan print state; its documentation leaves the
3D-printing commands unfinished. So Print never reports `idle` or `printing` for these
printers:

| Installed modules | State | `detail` starts with |
|---|---|---|
| A printing module (ID 0 or 13), with any others | busy | `PRINTING MODULE` |
| CNC, laser, other, or unknown modules only, or none | attention | `NO PRINTING MODULE` |
| A nonzero result, or a list that cannot be read | error | `UNREADABLE MODULES` |

`detail` names each module with its ID and firmware version, then the machine model and
firmware version from machine info, and ends with why the state is what it is. The
documentation does not say how the module list is counted, so Print reads it with a one-byte
count, a two-byte count, and no count, and uses it only when every reading that fits the
reply exactly gives the same list.

### Why there is no control

Snapmaker's 2.0 description documents only connect, status, and disconnect. The SACP
documentation leaves the 3D-printing commands unfinished and describes no network pairing.
Without a published contract for uploads, starts, pauses, resumes, and stops, Print sends
none of them. Preparation, every confirmation challenge, and every
action route are refused with a message saying so, and nothing but the requests above
reaches the printer. Use the touchscreen or Snapmaker Luban to run jobs.

### What needs a real printer

Testing uses simulated printers on test-owned loopback servers. Before relying on these
printers, check on real hardware:

- Snapmaker 2.0: whether `POST /api/v1/connect` waits for the touchscreen or answers at once
  and refuses status until confirmed (both are handled), what a refused or pending request
  returns, whether the token expires without other traffic, whether `POST /api/v1/disconnect`
  takes the token in its form body, the `status` values besides `IDLE`, and what `headType`
  numbers each head reports.
- J1 and Artisan: whether network clients need a pairing step, the CRC-8 variant (Print uses
  polynomial 0x07 with initial value 0), whether the length field counts bytes 7 to the end
  as Print does, whether requests should go to the controller (1) or the screen (2), how the
  module list is counted, and which module IDs a J1's print heads report.

## Stop a printer and clean staged files

Run `dutchman-print --config config.toml stop <printer>` to review the current live job's
name, identity, state and progress, then confirm Stop. `--yes` uses the same fresh challenge
without an interactive prompt. No bed-clear acknowledgement is needed. Stop is available
while printing, paused or awaiting attention, including work started outside Dutchman.
A changed live identity or name requires another confirmation; progress may advance.
Stop sends one command and waits up to 25 seconds. An unconfirmed result requires a fresh
status read. It never resends the command automatically. Check and acknowledge the bed
again afterwards. Printer Stop can overtake that printer's pending command outcome wait;
other printers' uploads and waits do not block it.

The running service already saves tracked jobs in its private data folder and reloads
them on restart. It reconciles attempted executions with fresh status before control.
After a terminal outcome, only the recorded unique file staged by Dutchman is deleted,
and only after fresh status establishes it is no longer live. Uploaded ready jobs are
also cleaned when cancelled before Start or forgotten. A cleanup failure is reported in
job `detail` while preserving its outcome; inspect printer storage before removing a file.

Elegoo resin Stop addresses the live `TaskId` and `Filename` and sends command 130 once;
Centauri printers refuse Stop. See [Elegoo (SDCP)](#elegoo-sdcp).

Snapmaker 2.0, J1, and Artisan printers refuse Stop. See
[Snapmaker 2.0 and J1/Artisan](#snapmaker-20-and-j1artisan).

Duet Stop addresses the live file path: a running job is paused with `M25`, the pause is
confirmed, and then `M0` is sent. Stop is confirmed when the printer is idle with that file
reported as the last one processed.

Repetier-Server refuses printer-level Stop because `stopJob` names no job. Use Cancel for
tracked work and stop external jobs in Repetier-Server. See
[Repetier-Server](#repetier-server).

Bambu commands use the existing monitoring MQTT connection and match replies by sequence
number. Reconnect never replays a command. Upload is refused when telemetry reports no SD
card. Preparation and printing status show the current stage in `detail`; unknown stage
codes remain visible as numbers. Dual-extruder reports without `nozzle_temper` use extruder
0 (or 1 when 0 is absent) for the scalar temperature and list both available readings in
`detail`. Bed leveling stays enabled unless `bed_leveling = false` is explicitly set in
that printer's `start_profile` table. These details use
[Bambu's temperature field documentation](https://portal.bblmw.com/fleet-hub/wb76oog4i8k/files/Bambu%20Fleet%20Hub%20HTTP%20API%20EN%20v1.0.0.pdf),
[community MQTT field notes](https://github.com/Doridian/OpenBambuAPI/blob/main/mqtt.md)
and [community activity code notes](https://github.com/Doridian/OpenBambuAPI/blob/main/gcode.md#m1002).

## Releasing

Dutchman Print is not released. No release may be published before testing on a real
printer using the [hardware checklist](hardware-checklist.md). Simulated tests alone
do not satisfy this project rule.

Every push to `main` and every pull request also builds `dutchman-print` in release mode on
Linux, macOS and Windows. The binaries are kept for 14 days as the CI run's artifacts
(`dutchman-print-Linux-X64`, `dutchman-print-macOS-ARM64`, `dutchman-print-Windows-X64`), for
trying a change without building it; they are test builds, not releases. For example:
`gh run download <run-id> -R dutchman-cad/print -n dutchman-print-Linux-X64`.

The `Release drafts` workflow builds on pushed `v*` tags and through `workflow_dispatch`.
The tag must match the workspace version in `Cargo.toml` (for example, `v0.1.0`). A manual
run on a branch only keeps build artifacts; a run on an existing version tag also creates
a draft. The workflow never creates a tag, publishes a release, or marks one as latest.
An existing draft or published release for that tag is refused, rather than replaced.

Archives contain `dutchman-print`, `README.md`, `LICENSE`, `NOTICE`, and the version's
changelog section as `CHANGELOG.md`, with an adjacent SHA-256 checksum. Linux x86_64 and
ARM64, macOS ARM64 and x86_64, and Windows x86_64 are built with `--locked`, using the
same runners and targets as Dutchman. The same changelog section supplies the draft notes.

Print notes with `python3 scripts/changelog.py release X.Y.Z`. This leaves fragments and
`CHANGELOG.md` untouched; `--write` explicitly records the section and deletes its fragments.
`--date YYYY-MM-DD` overrides the date; draft builds use the tagged commit's date when
assembling fragments. If a version section already exists, it supplies the notes instead.
After hardware testing, check the archives, checksums, and notes, then publish manually
on GitHub. Publishing is never a workflow step.

CI checks the changelog, script tests, and release YAML without building release binaries:

```sh
python3 scripts/changelog.py check
python3 -m unittest discover -s scripts/tests -v
python3 scripts/changelog.py release 0.1.0
python3 -m pip install PyYAML==6.0.3
python3 scripts/check-release-workflow.py
```

## Print farm queue

A running server can keep a FIFO queue of prepared jobs in its private data folder.
Queue commands connect to `--server http://127.0.0.1:8730` by default; they use
`DUTCHMAN_PRINT_PASSWORD` and `--user dutchman` when the server requires login.
Start the server with the farm's config, then use another terminal:

```sh
dutchman-print queue add bracket.3mf --printer workbench --target any
dutchman-print queue list
dutchman-print queue proposals
dutchman-print queue move ENTRY_ID 0
dutchman-print queue dispatch ENTRY_ID --printer workbench --bed-clear --yes
dutchman-print queue remove ENTRY_ID
```

`add` uploads through the existing preparation route using `--printer`'s presets;
`--target` is a printer id or `any` (the default). Optional `--profile` and `--kind`
filters narrow matching. Printers must share the preparation printer's kind and
resolved printer, print, and filament presets; absent presets must also match.
A different printer gets a separate preparation and tracked job before Start review,
so its own tooling and readiness checks still apply. Review physical compatibility
of printers sharing presets before confirming.

Proposals walk entries in FIFO order and configured printers in order. They skip
busy, offline, uncleared, incompatible, and monitoring-only printers, and propose each
printer only once. An unmatched entry does not block later entries with other targets.
Proposals themselves reserve nothing and never start anything. An attempted dispatch
reserves its bound printer while the entry remains queued, even before telemetry reports
printing and after an uncertain refusal. A failed or uncertain entry keeps that printer
reserved until you remove it from the queue, so the printer gets no proposals until then:
read the tracked job, check the printer, and then run `queue remove`. Matching fails closed if an attempted entry's bound printer cannot be resolved.
Positions for `move` are zero-based.
`remove` removes queue membership, retaining prepared and tracked job records.

Dispatch requests a fresh server-issued Start challenge. The person must still check
the bed; `--bed-clear` requests its own separate Clear challenge and approval first.
Without `--yes`, each summary is shown for terminal approval. `--yes` approves those
same challenges. Fresh status, per-printer controls, reviewed bytes, and bed checks
remain mandatory. After challenge preparation, an entry stays bound to that printer;
to change it before any attempt, remove the entry and queue its original job again.

Successful dispatch removes the entry and returns the tracked job. A refusal leaves
it queued with a reason. Once a backend Start attempt begins, dispatch is durably
blocked from retrying that entry, even after failure or restart. Read its `prepared`
job's status and reconcile any uncertain result before deciding what to do next.
The service retains queued job files beyond normal expiry; removing or successfully
dispatching an entry releases that retention. Queue order, bindings, reasons, and
attempt markers survive restart, but bed acknowledgements do not. The queue is limited
to 5,000 entries; matching has a 30-second deadline.
