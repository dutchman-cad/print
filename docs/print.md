# Dutchman Print

Dutchman Print is a local print companion for SolidCraft and a standalone command line.
A backend prepares 3MF files, estimates jobs, and controls printers. The included
`simulated` backend is a demo: it contacts no printers and can optionally run a local slicer.
The `printers` backend supports individually configured local PrusaLink, OctoPrint, Moonraker (Klipper), and Bambu printers.

Build `dutchman-print` from this checkout with Rust 1.88 or newer:

```sh
cargo build --locked -p dutchman-print
```

It is not released yet; testing uses simulated printers. Put the executable beside
`dutchman` or on `PATH` to use `dutchman printers` and `dutchman print`.

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
OctoPrint and Moonraker accept either kind; select a named slicer when more than one is configured.
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

CLI simulation state is saved in the private data folder, separately for each config path,
so later commands can read and control it. Jobs older than one hour are removed on the
next command. One CLI command at a time may change that state.
If a command is killed while holding a `.lock` folder, remove that folder once it has stopped.
The service persists its separate job state in its private data folder. Only one server may
use a data folder at once; after a killed process, remove `serve.lock` only once it has stopped. Tracked jobs are
reloaded and reconciled from live status before control after a restart.

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

Printer objects have `id`, `name`, `model`, and `state`: `idle`, `printing`, `paused`,
`attention`, `busy`, `error`, or `offline`. While printing or paused they may include `job_id`, `job_name`, `progress`
(0–1), `seconds_left`, `nozzle_temperature`, and `bed_temperature` (degrees Celsius).
Unavailable printer fields are omitted.

Job states are `ready`, `printing`, `paused`, `done`, `cancelled`, `failed`, and `ended`. Job `progress`
and `seconds_left` may be null. Estimate fields `seconds`, `filament_mm`, `filament_g`, and
`layers` are optional and omitted when unavailable. Local preview paths are never returned.
Not found maps to 404, busy or refused to 409, unavailable/offline to 503, and backend failure
to 500. Backend error messages must never contain secrets.

Before each machine action, request a server-issued challenge with
`POST /api/v1/confirmations` and JSON `{"action":"start","printer":"workbench","job":"JOB_ID"}`.
Actions are `start`, `cancel`, `pause`, `resume`, and `clear`; `job` is required for start
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
Missing serial, firmware, or nozzle information blocks preparation. The resolved profile's
`nozzle_diameter` is read from PrusaSlicer's output G-code comments in the first and last
256 KiB. Missing, conflicting, or multiple-nozzle settings are refused. Before upload the
profile diameter must match `/api/v1/info` `nozzle_diameter`; a mismatch names both diameters.
Fit a matching nozzle or slice with the matching profile. No mismatch override is offered.
Before upload and again before Start, Dutchman requires fresh `IDLE` or `READY` status,
with no job or transfer reported. `BUSY`, `ATTENTION`, terminal, unknown, and unavailable
states block Start. ATTENTION reports `attention` (waiting for a person); BUSY reports
`busy` (a non-print operation). Start, clear, and resume are refused in both states.
Pause requires printing and resume requires paused; cancel remains available for a
matching tracked job in attention. Status reads have a four-second deadline. Control
requests allow 15 seconds. One keep-alive HTTP connection per printer is reused, with
cached Digest challenges and incrementing nonce counts. Only reads may retry once on
a stale Digest challenge; controls are never resent. Commands poll matching status for
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
attempt consumes it even if the result is uncertain or fails. Polling stops when no remote
acknowledgement is held. Activity entirely between polls with no identity or terminal signal
change cannot be detected; clearance is not proof that no physical activity occurred.
PrusaLink `READY` is the operator's printer-side bed-clear signal and substitutes for the
remote acknowledgement; `IDLE` still requires one. Restarting never restores remote clearance.

Standalone CLI commands save tracked jobs and reviews separately from service jobs. Use
the service API to control service jobs. The attempted Start is saved before sending it,
so restarting cannot silently retry it. The service persists tracked jobs and reconciles
live identity before control after a restart.

## OctoPrint

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
Pause, resume, and stop apply only to the matching live job started by this companion.
Matching requires both the unique subtask name and a positive `gcode_start_time`; missing
execution identity blocks control. A later execution of the same file is a different job.
FINISH and FAILED consume bed clearance even between status polls. Stop requires FAILED
for the matching job before reporting cancellation; idle alone proves no outcome.

The single start profile is `lan-project-1`, a local plate-1 project profile. It uses
`Metadata/plate_1.gcode`, local identifiers of zero, automatic bed type, and no timelapse.
Calibration options default off. It requires `Metadata/project_settings.config` with
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
bed_leveling = false
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

Commission profiles for the actual model, build volume, nozzle, tooling, material, and
plate. Only a single extruder is supported for control; unresolved material slots are
refused. Loaded material and physical plate identity have no universal telemetry here:
check them before confirming each start. Set `resume_commissioned = true` only after
checking that this printer's installed pause/resume macros behave safely. A changed
machine configuration, including macros, requires fresh preparation and review.

Status polls server connectivity and available printer objects, then reads a full object
snapshot. Klippy startup reports busy while it connects; shutdown, error, missing objects,
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
