# Dutchman Print

Dutchman Print is a local print companion for SolidCraft and a standalone command line.
A backend prepares 3MF files, estimates jobs, and controls printers. The included
`simulated` backend is a demo: it does no slicing, contacts no printers, and uses no network.
Real printer backends come later.

Build `dutchman-print` from this checkout with Rust 1.88 or newer:

```sh
cargo build -p dutchman-print
```

It is not included in release archives or the Docker image yet. Put the executable beside
`dutchman` or on `PATH` to use `dutchman printers` and `dutchman print`.

## Config

Choose a file with `--config FILE` or `DUTCHMAN_PRINT_CONFIG`; the option takes precedence.
Print reads only that file. Create it with private permissions (`chmod 600` on Unix).
Group and other permissions are refused before reading, even for the simulator. Future
backend credentials belong in private config, never in logs or responses. Parser errors
do not echo config contents.

```toml
[backend]
kind = "simulated"

[[printers]]
id = "workbench"
name = "Workbench demo"
model = "Example printer"
```

Repeat `[[printers]]` to add printers. Leave it out for an empty fleet. Unknown backend kinds
produce an error naming the kind. Config files are limited to 1 MiB and 64 printers.
IDs must be unique, with 1–64 ASCII letters, digits, hyphens, or underscores. Names are
nonempty plain text, at most 256 bytes; model descriptions are free text up to 256 bytes.

Without `--config`, the config path is:

| Platform | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/dutchman-print/config.toml`, or `~/.config/dutchman-print/config.toml` |
| macOS | `~/Library/Application Support/dutchman-print/config.toml` |
| Windows | `%APPDATA%\dutchman-print\config.toml` |

## Commands

```sh
dutchman-print --config config.toml printers
dutchman printers --config config.toml --json
dutchman print bracket.3mf --config config.toml --printer workbench
dutchman-print print bracket.3mf --printer workbench --print-profile Fine --json
dutchman-print print bracket.3mf --printer workbench --yes --json
dutchman-print status JOB_ID
dutchman-print cancel JOB_ID --yes
dutchman-print pause workbench
dutchman-print resume workbench
```

`dutchman printers …` runs `dutchman-print printers …`; `dutchman print …` runs
`dutchman-print print …`. Other companion commands use `dutchman-print` directly.

`printers` writes one line per printer, such as `Workbench demo: idle`. `--json` returns
the same array as `GET /api/v1/printers`. The CLI calls the backend directly; no server is needed.

`print` accepts a single `.3mf`, `--printer ID`, and optional `--printer-profile`,
`--print-profile`, and `--filament-profile` names. It checks the file and prepares a job,
shows the estimate, then asks `Start printing <name> on <printer>? [y/N]` on a terminal.
Without a terminal it refuses to start unless `--yes` is present. `--json` never prompts:
without `--yes` it prepares only and returns the ready job and estimate; with `--yes` it
starts and returns the printing job. Estimates go to stderr in plain mode; job JSON goes
to stdout. `status`, `pause`, `resume`, and confirmed `cancel` return job JSON.
`cancel` asks for confirmation on a terminal unless given `--yes`.

The simulator estimates from the compressed 3MF size in bytes: seconds = 60 + size / 1024,
filament mm = size / 10, filament g = size / 1000, and layers = 1 + size / 4096.
Integer divisions round down. These are demonstration values, not slicing results.
Profile names are accepted but do not affect the demo. Started jobs progress with elapsed
time, pause freezes progress, resume continues, and cancel ends the job.

CLI simulation state is saved in the private data folder, separately for each config path,
so later commands can read and control it. Jobs older than one hour are removed on the
next command. One CLI command at a time may change that state.
If a command is killed while holding a `.lock` folder, remove that folder once it has stopped.
The service has its own in-memory simulation; restarting it resets its job state.

## Plugin API

```sh
dutchman-print --config config.toml serve --listen 127.0.0.1:8730 --data-dir private-jobs
```

The standalone [job API version 1 specification](print-api.md) describes the protocol for
clients and other servers. Errors are JSON objects with an `error` string and a 4xx or 5xx status. Basic authentication protects all routes
except `GET /healthz` when `DUTCHMAN_PRINT_PASSWORD` is set.

| Route | Result |
|---|---|
| `GET /healthz` | `{"service":"dutchman-print","api":1}`; service liveness only |
| `GET /api/v1/printers` | Printer array, in config order |
| `POST /api/v1/jobs` | Raw 3MF or multipart prepare; 201 with `{id, printer, name, state: "ready", estimate, progress, seconds_left}` |
| `GET /api/v1/jobs/{id}` | Current job |
| `GET /api/v1/jobs/{id}/preview.png` | PNG when the backend supplied one; otherwise 404 |
| `POST /api/v1/jobs/{id}/start` | Start a ready job |
| `POST /api/v1/jobs/{id}/cancel` | Cancel a ready, printing, or paused job |
| `POST /api/v1/printers/{id}/pause` | Pause the printer's active job |
| `POST /api/v1/printers/{id}/resume` | Resume its paused job |

`POST /api/v1/jobs` accepts a raw 3MF body with `Content-Type: model/3mf` (or
`application/vnd.ms-package.3dmanufacturing-3dmodel+xml`), with `printer`, optional `name`,
and profile names as query parameters. Without `name`, raw uploads use `input.3mf`.
SolidCraft’s host sends this form. Multipart uploads remain available for curl and scripts.

The multipart form takes exactly one `file` with a `.3mf` name and a `printer` ID. Optional fields
are `printer_profile`, `print_profile`, `filament_profile`, and `name`. Each text field is
nonempty plain UTF-8, at most 256 bytes. Duplicate or unknown fields are refused. Without
`name`, the uploaded filename is used. A 3MF must be a zip with a `3D/*.model` part. The
companion checks paths, entry counts, and actual expanded sizes before calling the backend.
The simulator does not produce previews.

Printer objects have `id`, `name`, `model`, and `state`: `idle`, `printing`, `paused`, `error`,
or `offline`. While printing or paused they may include `job_id`, `job_name`, `progress`
(0–1), `seconds_left`, `nozzle_temperature`, and `bed_temperature` (degrees Celsius).
Unavailable printer fields are omitted.

Job states are `ready`, `printing`, `paused`, `done`, `cancelled`, and `failed`. Job `progress`
and `seconds_left` may be null. Estimate fields `seconds`, `filament_mm`, `filament_g`, and
`layers` are optional and omitted when unavailable. Local preview paths are never returned.
Not found maps to 404, busy or refused to 409, unavailable/offline to 503, and backend failure
to 500. Backend error messages must never contain secrets.

Every start, cancel, pause, and resume requires a `Plugin-Confirmation` header containing
exactly 64 lowercase hex characters. SolidCraft supplies a fresh value after its confirmation
step. Missing, malformed, duplicate, or reused values return 403 with a message requiring
confirmation in SolidCraft (or `--yes` on the command line). Accepted values are remembered
for ten minutes, across all actions and callers; a failed backend action also consumes its
value. This header is a confirmation signal, not authentication. Every successful action
writes a timestamp, printer ID, job ID, and action to stderr; header values are never logged.

## Server settings and hardening

| Option | Environment | Default |
|---|---|---|
| `--config FILE` | `DUTCHMAN_PRINT_CONFIG` | Platform path above |
| `--listen ADDRESS` | `DUTCHMAN_PRINT_LISTEN` | `127.0.0.1:8730` |
| `--data-dir DIR` | `DUTCHMAN_PRINT_DATA` | `dutchman-print` in the system temporary folder |
| `--user NAME` | `DUTCHMAN_PRINT_USER` | `dutchman` |
| | `DUTCHMAN_PRINT_PASSWORD` | None; environment only |
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
`crates/dutchman-print/src/backend.rs`. Slicing belongs inside `prepare`, which receives the
job ID, private 3MF path, printer ID, and profile names. Typed errors distinguish not found,
busy, refused with a message, unavailable/offline, and failed. The simulated implementation
accepts a clock so lifecycle tests can advance time without sleeping.
