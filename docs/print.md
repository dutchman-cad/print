# Dutchman Print

Dutchman Print is a local print companion for SolidCraft and a standalone command line.
A backend prepares 3MF files, estimates jobs, and controls printers. The included
`simulated` backend is a demo: it contacts no printers and can optionally run a local slicer.
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

## Slicer

Install PrusaSlicer or OrcaSlicer separately and set it up in its own application first.
Dutchman runs its public command line as a separate program; it does not include or link
slicer code. The slicer's AGPL licence stays with that program. Cura is not supported yet.
No printer backend is included here.

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
`slice = true` requires `[slicer]`; without it the simulator keeps its size-based estimates.

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

Without `backend.slice = true`, the simulator estimates from the compressed 3MF size in bytes: seconds = 60 + size / 1024,
filament mm = size / 10, filament g = size / 1000, and layers = 1 + size / 4096.
Integer divisions round down. These are demonstration values, not slicing results.
Profile names affect slicing only when it is enabled. Started jobs progress with elapsed
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
The simulator supplies previews only when slicing produces one.

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
DUTCHMAN_PRINT_REAL_SLICER=prusaslicer buildq run dutchman /path/to/dutchman -- \
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
