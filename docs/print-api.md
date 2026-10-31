# Print job API version 1

This is an open HTTP protocol for preparing 3MF print jobs and controlling their execution.
Any server can implement it; it does not require Dutchman or a particular printer backend.
SolidCraft's host and standalone clients can use the same protocol. All paths below are
relative to the server's base URL. JSON uses UTF-8 and `Content-Type: application/json`.

This unreleased v1 contract requires server-issued confirmations.
Version 1 routes begin with `/api/v1`. Within v1, fields may only be added; new request fields must be optional.
Existing fields, types, meanings, and routes remain compatible. Clients ignore unknown
response fields. Breaking changes require a new path version such as `/api/v2`.

## Routes

| Method and path | Request | Success |
|---|---|---|
| `GET /healthz` | No body | 200, `{"service":"dutchman-print","api":1}` for Dutchman Print |
| `GET /api/v1/history` | Optional `printer`, `outcome`, `since`, and `limit` query fields | 200, array of history records, newest first |
| `GET /api/v1/printers` | No body | 200, array of printer objects |
| `POST /api/v1/jobs` | Raw 3MF or multipart, described below | 201, prepared job object |
| `GET /api/v1/jobs/{id}` | No body | 200, current job object |
| `GET /api/v1/jobs/{id}/preview.png` | No body | 200, `image/png`, or 404 when unavailable |
| `POST /api/v1/confirmations` | Action, printer, optional job JSON | 201, confirmation challenge |
| `POST /api/v1/jobs/{id}/start` | Confirmation id in JSON or query; confirmation header | 200, resulting job object |
| `POST /api/v1/jobs/{id}/cancel` | Confirmation id in JSON or query; confirmation header | 200, resulting job object |
| `POST /api/v1/printers/{id}/stop` | Confirmation id in JSON or query; confirmation header | 200, fresh printer object |
| `POST /api/v1/printers/{id}/pause` | Confirmation id in JSON or query; confirmation header | 200, resulting active job object |
| `POST /api/v1/printers/{id}/resume` | Confirmation id in JSON or query; confirmation header | 200, resulting active job object |
| `POST /api/v1/printers/{id}/clear` | Confirmation id in JSON or query; confirmation header | 204, bed clearance acknowledged |

IDs are opaque strings returned by the server; clients encode them as URL path segments.
Health responses contain required `service` (string identifying the implementation) and
`api` (integer, 1). Health is public and does not require authentication.

Servers may require HTTP Basic authentication on all other routes. A 401 response includes
`WWW-Authenticate: Basic`. Use HTTPS when authentication crosses an untrusted network.
Responses use `Cache-Control: no-store`. Browser cross-origin reads are not enabled.
For POST requests, cross-site `Sec-Fetch-Site` is refused; an `Origin` must match the
request Host, including port, or an explicitly configured public origin. Requests without
these browser headers are allowed.

## Prepare a job

SolidCraft's host sends the 3MF itself as the body, with `Content-Type: model/3mf` or
`application/vnd.ms-package.3dmanufacturing-3dmodel+xml`. Fields are URL-encoded query
parameters, for example:

```http
POST /api/v1/jobs?printer=workbench&name=Bracket&print_profile=Fine HTTP/1.1
Content-Type: model/3mf

<3MF bytes>
```

Scripts may instead send `multipart/form-data` with exactly one `file` part whose filename
ends in `.3mf`, and the following text parts. Multipart fields come from the body, not the
query string.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `printer` | string | Yes | Printer ID from the printer list |
| `name` | string | No | Display name; defaults to the uploaded filename for multipart, `input.3mf` for raw bodies |
| `printer_profile` | string | No | Backend printer profile name |
| `print_profile` | string | No | Backend print profile name |
| `filament_profile` | string | No | Backend filament profile name |

Each supplied field is nonempty plain UTF-8 text, at most 256 bytes, with no control
characters. Duplicate and unknown fields are refused with 400. Preparing a job never starts
printing and requires no confirmation header. The response arrives after preparation and
has state `ready`. Slicing and profile interpretation belong to the backend.

Both upload forms have the same size and 3MF validation limits. A 3MF must be a readable
zip with a regular `3D/*.model` part. Servers bound entry counts and actual expanded bytes
and refuse paths outside the extraction folder. Dutchman Print defaults to 200 MiB of
uploaded file bytes, 5,000 zip entries, and 2 GiB expanded bytes. Unsupported media types
return 415 with a message naming the accepted forms. Uploads are streamed to private job
storage and removed on failure.

## Printer JSON

```json
[{"id":"workbench","name":"Workbench","model":"Example printer","state":"idle","controls":["start","pause","resume","cancel","stop","clear"]}]
```

| Field | Type | Presence |
|---|---|---|
| `id` | string | Required |
| `name` | string | Required |
| `model` | string | Required |
| `controls` | array of strings | Required; supported machine actions in this configuration |
| `state` | string | Required: `idle`, `printing`, `paused`, `attention`, `busy`, `error`, or `offline` |
| `job_id` | string | Optional; active job ID, the live file path for Duet, the queued job id for Repetier-Server, or the `TaskId` for Elegoo SDCP |
| `job_name` | string | Optional; active job name |
| `job_path` | string | Optional; backend relative live file path, currently OctoPrint |
| `progress` | number | Optional; fraction from 0 to 1 |
| `seconds_left` | nonnegative integer | Optional |
| `nozzle_temperature` | number | Optional; degrees Celsius |
| `bed_temperature` | number | Optional; degrees Celsius |
| `detail` | string | Optional; original printer state or a safe availability reason |
| `needs_clearing` | boolean | Optional; true when an operator must check and acknowledge the bed |

Unavailable optional fields are omitted. An empty fleet is `[]`.

### Controls

`controls` lists supported actions independently of the printer's current state: `start`,
`pause`, `resume`, `cancel`, `stop`, and `clear`. `stop` means printer-level Stop of any live
job, including external work; `cancel` acts on a tracked job. `clear` is listed wherever
`start` is. Clients use this list to choose which controls to show, then use fresh state and
confirmation challenges to determine what is possible now. Unsupported actions return 409
from both confirmations and action routes, before reading printer status.

Simulated, PrusaLink, OctoPrint, and Duet printers list all six. Moonraker and Snapmaker U1
list all six with `resume_commissioned = true`, otherwise they omit `resume`. Bambu lists all
six only with `lan_control = true`, otherwise `[]`. Repetier-Server lists
`["start", "cancel", "clear"]`. Elegoo resin lists `["stop"]`; Centauri, Snapmaker 2.0, J1,
and Artisan list `[]`. Readiness and reviewed tooling still apply.

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

`attention` means the printer is waiting for a person; `detail` explains why when known.
`busy` means a non-print operation is running. PrusaLink ATTENTION and BUSY map to these
states, and Bambu PAUSE with an HMS alert maps to attention. Pause requires printing;
resume requires paused. Start, clear, and resume are refused in attention and busy.
Cancel remains available for a matching tracked execution in attention.

Start and resume require fresh telemetry. Missing, stale, contradictory, or unknown
readiness blocks movement. Prepared file changes require a new preparation and review.
Commands with uncertain results require status reconciliation; clients must never repeat
Start automatically. A cancellation requested while Start is pending remains pending until
the same live job is cancelled and fresh telemetry confirms the outcome.

OctoPrint uses the same routes, confirmation header, reviewed-byte checks, and bed-clear
policy. Clearance is held only by the running server; clear and start use separate challenges.
Polling reads the recursive OctoPrint file list: a newer `prints.last.date` on any file,
or an observed job/state change, withdraws clearance even after a return to idle.
Its `detail` retains a known connection state or a safe availability reason;
authentication failure is separate from machine failure. A reachable host with a disconnected
serial printer is `offline`; `Offline after error` is `error`. Summary states prefer
printer flags over display text. Starting, Resuming, SD printing, Pausing, Cancelling, and
Finishing map to `printing`; Finishing alone cannot establish completion. Connection/detection
and file-transfer phases map to `busy`, with the phase in `detail`.
Unknown or contradictory readiness blocks start/resume.
OctoPrint checks the sliced G-code's `nozzle_diameter` against the selected printer profile's
`extruder.nozzleDiameter` during preparation, challenge review, and before upload. Missing or
ambiguous diameters block the action; a mismatch names both values, with no override. This
checks the configured profile and cannot establish which physical nozzle is installed.
A failed or uncertain upload or failed stored-file validation triggers `DELETE` on the staged
remote file before returning an error; Start is never sent. Unconfirmed cleanup requires
inspection and removal in OctoPrint before preparing again.
For OctoPrint, optional `job_id` identifies the selected file as `origin/name`, not an
execution identifier. Pause/resume/cancel require that file still be live and match the
companion's attempted start. External reprints of the exact staged file are indistinguishable;
avoid external control of those files. Idle status alone never completes a tracked job.
A newer file print-history result after observed activity establishes `done` or `failed`;
an unsuccessful result after companion cancellation establishes `cancelled`. Missing history
leaves the outcome unresolved. History timestamps are read as finite `f64` Unix seconds,
including fractions, and must be strictly newer than the pre-start value.
After an accepted command, the request waits for matching telemetry for up to 25 seconds
for start/cancel or 10 seconds for pause/resume. Pending is announced on stderr during this
wait. Expiry returns the existing 409 error with an unconfirmed pending outcome; clients
must read status before another action. The command is sent once, with no automatic retry.
Physical firmware, material, tooling, and plate changes not
reported by OctoPrint require operator review before confirmation. See the
[OctoPrint configuration and checks](print.md#octoprint).

Moonraker (Klipper) uses the same routes, server-issued challenges, reviewed-byte checks,
and bed-clear policy. The running server holds clearance and polls the latest history entry.
Status objects come from a lazy authenticated WebSocket subscription, with a fresh baseline
on every connection and merged field updates. Cached status requires a live socket and
an incoming message within 30 seconds. Until a baseline arrives or after socket loss,
status reads use the existing HTTP query; Klippy disconnect/shutdown notifications block
readiness until a new baseline. History and file identity checks and all commands remain
HTTP operations. Reconnect never resends a command. No v1 fields change.
A changed execution ID or fractional start timestamp, or an observed `print_stats` state or
filename change, withdraws clearance. Timestamps are finite floating-point Unix seconds and
retain fractions. Jobs are checkpointed after changes, including reconciled outcomes.
The sliced G-code's `nozzle_diameter` must match Klipper's
`configfile.settings.extruder.nozzle_diameter` during preparation, challenge review, and
before upload. Unknown or ambiguous values are refused; mismatches name both values, with
no override. This checks configuration rather than physical nozzle identity.
Failed or uncertain uploads and failed stored-file validation delete the staged remote
file before returning an error and never send Start. Unconfirmed cleanup requires inspection
and removal in Moonraker before preparing again.
Accepted commands poll once sent for up to 25 seconds for start/cancel and 10 seconds for
pause/resume. Pending is announced on stderr; expiry returns 409 with an unconfirmed outcome
and requires fresh status. Commands are never resent by this wait.
Its `detail` contains a known `print_stats` state or a safe Klippy readiness explanation;
Klippy startup reports busy and disconnected Klippy reports offline; shutdown, unknown, and contradictory states
block actions. Raw backend messages are not returned. `job_id`, when available, is the
Moonraker history execution ID linked by file metadata; it may identify the last terminal
job while the printer is idle. Progress is a fraction and live `seconds_left` is omitted.
Start stages text G-code with automatic printing disabled, verifies metadata, then starts
once. A lost reply requires reconciliation and never causes an automatic retry. Controls
require the same tracked filename and live execution ID. Resume additionally requires
commissioned behaviour and unchanged machine configuration. Completed, cancelled, and
failed executions remain distinct; idle alone cannot establish completion. See
[Moonraker configuration and checks](print.md#moonraker-klipper).

Snapmaker U1 (`kind = "snapmaker-u1"`) is a Moonraker printer and behaves exactly as above,
except for its toolheads. The printer's tools are `configfile.settings` `extruder`,
`extruder1`, and so on, each with a `nozzle_diameter`, and the active `toolhead.extruder`
must be one of them. Loaded material per tool is not reported, so a job must select exactly
one installed tool with `T<n>`; jobs selecting no tool, several tools (named in the error),
an absent tool, or `ACTIVATE_EXTRUDER` are refused during preparation, challenge review, and
before upload. The sliced `nozzle_diameter` for that tool must match the tool's configured
value; a mismatch names the tool and both values, with no override. `nozzle_temperature`
is the active tool's. See [Snapmaker U1 configuration and checks](print.md#snapmaker-u1).

Duet (RepRapFirmware standalone) uses the same routes, server-issued challenges,
reviewed-byte checks, and bed-clear policy. Status comes from the `rr_model` object model:
`idle` is idle; `processing`, `pausing`, `resuming`, `cancelling`, and `changingTool` are
printing; `paused` is paused; `busy`, `starting`, `updating`, and `simulating` are busy;
`halted` is error; `off` and `disconnected` are offline. Other values report error and block
Start. `detail` holds the known `state.status`. `job_id` is the live file path (for example
`0:/gcodes/part.gcode`) and `job_name` its file name; there is no separate execution ID.
`progress` is file position divided by file size, a byte fraction rather than elapsed time,
and `seconds_left` is `job.timesLeft.file` when reported. Temperatures use the current tool's
heater and the configured bed heater. Polling withdraws clearance when the status, the live
file, or the last processed file changes.
The object model has no nozzle diameter, so the sliced `nozzle_diameter` must match the
printer's configured `nozzle_diameter` during preparation, challenge review, and before
upload; a missing value is refused. Start uploads a uniquely named file with a CRC-32, checks
`err` and the listed size, deletes failed or uncertain uploads before returning an error, saves
the attempt, and sends `M32` once. Pause (`M25`), resume (`M24`), and cancel require the live
file this companion started. Cancel and Stop send `M0`, which cancels only a paused job, so a
running job is first paused with `M25` and that pause confirmed for up to 10 seconds; an
unconfirmed pause does not send `M0`. Waits otherwise match Moonraker. Login uses `rr_connect`
with a session key; an expired session is renewed, but G-code refused with 401 is not resent.
Standalone firmware does not report how a file ended: a companion cancellation yields
`cancelled`, a halt `failed`, and other endings `ended`. See
[Duet configuration and checks](print.md#duet-reprapfirmware-standalone).

Duet with SBC (`kind = "duet-sbc"`, Duet Software Framework) uses the same API fields,
confirmation challenges, readiness checks, configured nozzle check, file identity,
`ended` outcome, waits, pause-before-`M0` rule, and cleanup as standalone Duet.
Its transport reads the full shared object model from `GET /machine/model`, stages with
`PUT /machine/file/{path}` (201), verifies directory and parsed file-info sizes, and
sends raw text G-code once with `POST /machine/code?async=true`. Nonempty G-code replies are treated
as uncertain; status must reconcile the outcome. Cleanup uses `DELETE /machine/file/{path}`
and confirms absence in the directory. Authentication uses `GET /machine/connect` with
an optional password and the returned `X-Session-Key`; 403 renews reads and refused uploads
once but never resends G-code. Release attempts `GET /machine/disconnect` while a runtime
is available. No new v1 fields are required. See the
[SBC configuration](print.md#duet-with-sbc-duet-software-framework) and
[vendor REST documentation](https://github.com/Duet3D/DuetSoftwareFramework/wiki/REST-API).


Repetier-Server (`kind = "repetier"`) uses the same routes, server-issued challenges,
reviewed-byte checks, and bed-clear policy. Each entry names one printer on the server by
`slug`, and the API key is sent only as the `x-api-key` header. Status comes from the
`listPrinter` entry with that slug: `online` 0 is `offline` (`detail` `deactivated` when
`active` is false, otherwise `offline`); `online` 1 with `active` true is `idle` while `job`
is `none` (`detail` `online, no job`) and `printing` while `job` names a job (`detail`
`online, job active`); any other combination, or no entry with the slug, is `error` with
the raw values in `detail` and blocks Start. No `paused` state is ever reported, because the
server reports none. `job_name` is the live `job` value and `job_id` the id of the one queued
`listJobs` entry with that name. `progress` and `seconds_left` are omitted: the manual gives
no units for its job times. Temperatures are the active extruder's and the heated bed's
`tempRead` from `stateList`. Polling withdraws clearance when the state, live job name, or
its id changes. The server reports no nozzle diameter, so the sliced `nozzle_diameter` must
match the printer's configured `nozzle_diameter` during preparation, challenge review, and
before upload; a missing value is refused. Only a one-extruder printer configuration is
accepted. Start checks free space with `canUploadGCodeWithSize`, uploads a uniquely named
model, requires exactly one listed model of that name with the reviewed size, removes failed
or uncertain uploads with `removeModel` before returning an error, saves the attempt, sends
`copyModel` with `autostart` false once, and sends `startJob` once for the queued job; a lost
reply is reconciled from status and never resent. Pause and resume challenges and routes are
refused: the manual documents no pause and the server reports no paused state. Cancel sends
`stopJob` once and only while the live job is the one this companion started;
`stopJob` names no job, so printer-level Stop is refused. Use Cancel for tracked work.
A queued job that never ran is cancelled with `removeJob`. Waits match Moonraker. Job endings are not reported over this
interface: a companion cancellation yields `cancelled` and other endings `ended`. See
[Repetier-Server configuration and checks](print.md#repetier-server).

Elegoo SDCP (`kind = "elegoo-sdcp"`) printers report status only through the printer routes;
preparation and Start are refused for both profiles, so they never have tracked jobs. Status
needs a fresh report on the current WebSocket connection: until one arrives, and for 30
seconds after the last, the printer is `offline`, and every action is refused. `profile =
"resin"` maps `CurrentStatus` and `PrintInfo.Status` as in the vendor specification: an active
print activity is `printing` (substate 6 is `paused`) even when the substate still holds
STOPPED or COMPLETED; transfers, exposure tests, and self-tests are `busy`; idle with
substate 0, 8, or 9 is `idle`; a nonzero `ErrorNumber`, a contradictory, or an unknown value is
`error`. `profile = "centauri"` maps `PrintInfo.Status` 0 to `idle`; 5, 8, 9, 13, and 20 to
`printing`; 10 to `paused`; and anything else to `error` with the raw code in `detail`.
`detail` starts with the vendor state name. `job_id` is `TaskId` and `job_name` `Filename`.
`progress` is the layer fraction while printing or paused. Resin `seconds_left` comes from
millisecond ticks; Centauri omits it. Resin UV LED and enclosure temperatures appear only in
`detail`; Centauri reports nozzle and bed temperatures. Centauri is monitoring only: every
challenge and every action route are refused, and no command is sent. Resin
supports printer Stop only; `pause` and `resume` challenges and routes are refused, because
they act on and return a tracked job, and Print never starts resin jobs. See
[Elegoo (SDCP)](print.md#elegoo-sdcp).

Snapmaker 2.0 (`kind = "snapmaker-2"`) and Snapmaker J1 and Artisan (`kind = "snapmaker-j1"`)
printers are monitoring only and report status only through the printer routes. Preparation,
every confirmation challenge, and every action route are refused with a
message that Snapmaker has not published the control contract, and nothing beyond the
documented status requests is sent; they never have tracked jobs and never report
`printing`, `paused`, `job_id`, `job_name`, `progress`, `seconds_left`, or temperatures. A
Snapmaker 2.0 is `offline` until its connection is approved on the touchscreen, and again if
approval does not arrive within 60 seconds or the printer stops accepting the connection;
`detail` then says to approve on the touchscreen and read status again. Once connected, its
`status` value `IDLE` is `idle`; any other value is `error`, with the value (sanitised, at
most 32 characters) first in `detail`; a `headType` that names a laser or CNC head turns
`idle` into `attention`. The connection token is never part of any response. A J1 or Artisan
is `busy` while a printing module (ID 0 or 13) is installed, because the vendor has not
published its print state, and `attention` otherwise, since the machine is not a printer
right now; `detail` names the modules and firmware versions. A module-info reply with a
nonzero result, or one that cannot be read, is `error`. Both kinds are `offline` without a
reply in the last 30 seconds. See
[Snapmaker 2.0 and J1/Artisan](print.md#snapmaker-20-and-j1artisan).

Printer commands are serialized separately for each printer. Uploads and command outcome
waits on one printer do not block another printer's status, confirmation review, or Stop.
Stop can also overtake its own printer's pending Start, pause, or resume outcome wait;
the earlier request reconciles to the confirmed cancellation without resending a command.
Start attempts are durably recorded before sending Start. Late status results cannot
replace a newer cancellation or restore a forgotten job.

## Job JSON and states

```json
{"id":"example-job","printer":"workbench","name":"Bracket","state":"ready","estimate":{"seconds":120,"filament_mm":1500,"filament_g":4.5,"layers":60},"progress":0.0,"seconds_left":120}
```

| Field | Type | Presence |
|---|---|---|
| `id` | string | Required |
| `printer` | string | Required; printer ID |
| `name` | string | Required |
| `state` | string | Required: `ready`, `printing`, `paused`, `done`, `cancelled`, `failed`, or `ended` |
| `estimate` | object | Required; may be empty |
| `detail` | string | Optional; safe staged-file cleanup failure explanation |
| `progress` | number or null | Required; fraction from 0 to 1, null when unavailable |
| `seconds_left` | nonnegative integer or null | Required; null when unavailable |

Estimate fields are optional and omitted when unavailable: `seconds` (nonnegative integer),
`filament_mm` (nonnegative number), `filament_g` (nonnegative number), and `layers`
(nonnegative integer). Preview paths are never included in JSON; use the preview route.

A prepared job is `ready`; start moves it to `printing`. Pause moves a printing job to
`paused`, and resume moves it back to `printing`. Completion yields `done`. Cancel accepts
ready, printing, or paused jobs and yields `cancelled`. Backend failures may yield `failed`.
`ended` is terminal: the execution ended, but its outcome is unknown. PrusaLink uses it when a tracked execution disappears from idle
telemetry without an observed terminal result; observed FINISHED and STOPPED remain
`done` and `cancelled`. Duet uses it for every ending not caused by this companion's
cancellation or a halt, since standalone firmware does not report the outcome.
Repetier-Server uses it for every ending not caused by this companion's cancellation. BUSY cannot revive a terminal job.
Incompatible actions return 409. Printer pause and resume target that printer's active job.
Jobs may expire; subsequent reads return 404. Dutchman Print retains finished jobs for one hour and
persists service jobs in its private data folder. Startup reloads tracked jobs and reconciles
attempted jobs with live status before control; unavailable printers stay tracked and controls
require live reconciliation. Clients must still handle expired jobs disappearing.
When the service stops, it releases printer connections within three seconds without sending
machine actions; durable job and safety checkpoints are retained.

## Print history

`GET /api/v1/history?printer=workbench&outcome=done&since=2026-10-01T00:00:00Z&limit=20`
returns finished tracked jobs and bed-clear acknowledgements, newest appended first.
Filters combine: `printer` matches the printer id, `outcome` matches a value below,
`since` includes records whose `ended_at` is at or after the RFC 3339 timestamp (offset
required), and `limit` defaults to 100, with a range of 1–10,000. Invalid filters return
400. Encode `+` in timestamps as `%2B`. Authentication and no-store policy match other
read routes. An empty result is `[]`.

| Record field | Type | Meaning |
|---|---|---|
| `printer` | string | Configured printer id |
| `printer_kind` | string | Configured backend kind, or `simulated` |
| `job_id` | string or null | Tracked job id; null for bed clearance |
| `job_name` | string or null | Safe job label; null for bed clearance |
| `file_name` | string or null | Prepared basename (input for simulation); null for bed clearance |
| `file_size` | nonnegative integer or null | Prepared bytes (input for simulation); null for bed clearance |
| `source` | string or null | Prepared-from input basename when known; no local path |
| `outcome` | string | `done`, `cancelled`, `failed`, `stopped`, `ended`, `unresolved_after_restart`, or `bed_clear` |
| `started_at` | string or null | UTC RFC 3339 Start-attempt time; null if never attempted |
| `ended_at` | string | UTC RFC 3339 terminal observation or bed acknowledgement time |
| `duration_seconds` | nonnegative integer or null | Wall-clock duration, including pauses; null without a start |
| `estimate` | object or null | Preparation estimate fields from Job JSON; null for bed clearance |

`stopped` distinguishes printer Stop of tracked work from job Cancel; Job JSON remains
`cancelled`. `ended` means the printer could not establish the outcome.
`unresolved_after_restart` means an attempted job could not be reconciled during a bounded
startup read (including a missing expired snapshot). It does not assert success, alter
machine state, or permit retries. Each execution, keyed by printer id, job id, and start time, contributes at most one history record;
repeated reads and restarts do not duplicate it. History begins with jobs prepared after
this feature is installed; older tracked jobs without history metadata are not backfilled.

Bed-clear acknowledgements use `outcome: "bed_clear"`, printer identity, and `ended_at`;
other fields are null. No operator identity is known. This audit entry never restores
clearance. History writes are best effort and run outside machine-action responses.
Failures are logged without changing actions or status reads; this history route still
reports storage errors. No external printer jobs are added to history.

The private `history.jsonl` retains the newest 10,000 records including acknowledgements,
independently of the one-hour job-file retention. It appends complete lines and syncs them;
rotation syncs an atomic replacement. A torn final line is skipped on read and repaired
before append. Complete corrupt lines produce a storage error. Labels contain at most
256 bytes, omit local directories, and redact URLs; credentials, backend addresses,
confirmation values, previews, and file contents are never serialized. See
[Print history](print.md#print-history) for storage and automatic lock release after a crash.

## Confirmation for machine actions

Before each machine action, request a server-issued challenge with
`POST /api/v1/confirmations` and JSON `{"action":"start","printer":"workbench","job":"JOB_ID"}`.
Actions are `start`, `cancel`, `stop`, `pause`, `resume`, and `clear`; `job` is required for start
and cancel and omitted for printer actions. Issuance re-reads the printer and refuses
incompatible actions with 409. Success is 201 with `id`, `action`, `printer`, nullable `job`,
`summary`, `observed`, and `expires_at` (Unix seconds). `observed` contains normalized `state`,
nullable vendor `detail`, `job_id`, `job_name`, optional `job_path`, `progress` (fraction rounded down to a whole
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
For cancel, Stop, pause, and resume, progress and detail may advance; state, live identity,
live name, and optional live file path must still match.
A changed observation returns 409 with
"The printer changed since you confirmed; confirm again." Invalid, expired, or consumed
challenges and reused header values also return 409. Obtain a new challenge and confirmation;
never automatically repeat Start after an uncertain result.

The private data folder durably stores challenges and consumed ids until their expiry and
hashes of seen header values for at least ten minutes, across callers and restarts. Both
are consumed before attempting a backend action, including failed attempts. Header values
are never logged. Confirmation is separate from authentication. Standalone CLI prompts
and `--yes` create and consume the same observation-bound challenges internally.

## Errors

All unsuccessful responses have a JSON object with a required `error` string, for example:

```json
{"error":"Choose a printer."}
```

Messages are suitable for people and contain no credentials, confirmation values, or private
backend addresses. Clients branch on status codes rather than matching message text.

| Status | Meaning |
|---|---|
| 400 | Missing or malformed fields, unreadable multipart body, or failed 3MF validation |
| 401 | Authentication required or credentials refused |
| 403 | Browser origin refused or machine confirmation header missing, malformed, or duplicated; missing challenge id |
| 404 | Printer, job, preview, or route not found |
| 405 | Method not allowed; `Allow` names permitted methods |
| 409 | Printer busy, incompatible job state, or backend refusal |
| 413 | Upload size limit exceeded |
| 415 | Unsupported upload content type |
| 500 | Storage, preparation, or backend failure |
| 503 | Backend unavailable/offline, insufficient storage, or upload/job/confirmation capacity exhausted |

Capacity and storage limits may vary between implementations. Retry transient failures
later; a machine action retry requires another confirmation and a fresh header value.

Slicer selection belongs to the companion's private configuration: named
`[slicers.<name>]` tables and each printer's optional `slicer` name determine preparation.
The API does not accept a slicer override. Certificate enrollment uses the standalone
`dutchman-print fingerprint <printer> [--json]` command described in
[the companion guide](print.md#bambu-lab-lan-monitoring); it adds no HTTP route.

For PrusaLink, preparation reads the resolved `nozzle_diameter` setting from PrusaSlicer's
output text G-code comments (first and last 256 KiB). Missing, ambiguous, or multiple-nozzle
settings are refused. When `/api/v1/info` omits `nozzle_diameter` (including null),
PrusaLink uses the installed `nozzle_diameter` from private printer config. Missing both or
invalid reported values block preparation. A reported diameter takes precedence. Before
upload the sliced value must match the reported diameter or configured fallback; a
mismatch names both diameters and requires a matching nozzle or a newly sliced profile.
There is no per-printer mismatch override. Serial, firmware, MMU capability, and all returned
identity fields retain the same checks for embedded and Pi-hosted PrusaLink. Both use text
G-code. Upload storage comes from `/api/v1/storage`: exactly one available writable entry,
or the configured `storage` path name, with sufficient reported free space. Ambiguous
selection is refused before upload. The selected path is retained for Start and cleanup.

For Bambu, `detail` includes active HMS hexadecimal codes and Bambu wiki links.
Fatal/serious HMS alerts block Start; any HMS alert during PAUSE requires attention
and blocks Resume. FINISH/FAILED without an active fault are idle terminal states:
clearance can be acknowledged with a `clear` challenge, followed by a separate Start
challenge. A terminal job's existing `print_error` is historical; one appearing or
changing afterward is active. Historical job identity can remain in the report.

Bambu readiness uses a live MQTT connection and any incoming message within 30 seconds.
Deltas merge with cached fields, including while reconnect full-report requests are
paced. Stop requires live telemetry and the matching last-known execution identity.
Before upload, sliced `nozzle_diameter` and any specified `nozzle_type` must match the
installed nozzle; each `filament_type` must match its mapped AMS `tray_type` or the
single-filament external `vt_tray.tray_type`. Unknown values and mismatches are refused
with no override. These checks also apply to command line actions.

PrusaLink and Bambu commands wait for matching telemetry for up to 25 seconds for
start/stop and 10 seconds for pause/resume. Pending is announced on stderr before an
unconfirmed outcome is returned. Waiting never resends a command. Read status after
an uncertain outcome before confirming another action.

Uploads to PrusaLink, Bambu FTPS, OctoPrint, Moonraker, Duet, and Repetier-Server use a 30-second transport
idle timeout, reset by byte progress, rather than a fixed total deadline. Once the whole body has
been written, the printer's reply gets 60 seconds, since storing the file or draining socket
buffers moves no bytes. Failed or
uncertain PrusaLink and Bambu uploads attempt deletion of the staged remote file before
returning an error and never send Start. Unconfirmed cleanup requires inspection and
removal in the printer's storage before preparing again.

## Printer Stop and staged file cleanup

`POST /api/v1/printers/{id}/stop` stops the freshly challenged live execution, including
work started from a printer screen or another client. Request a `stop` challenge without
`job`; show its summary including the live name, identity, state and progress. Stop is
available in printing, paused and attention, requires no bed acknowledgement, and withdraws
clearance even if its result is uncertain. Identity or name replacement refuses the
command; progress changes are allowed. The backend rechecks the identity before sending.
Success returns fresh Printer JSON and updates a matching tracked job to its confirmed
outcome. External jobs are not added to the prepared-job list.

PrusaLink addresses the live job ID; OctoPrint cancels only the same live origin/file path;
Moonraker requires the same history execution and filename; Bambu requires the same
subtask name and positive start time; Duet requires the same live file path and pauses a
running job before sending `M0`; Repetier-Server refuses printer-level Stop because
`stopJob` names no job (use Cancel for tracked work);
Elegoo resin requires the same `TaskId` and `Filename`, sends
command 130 once, waits up to 10 seconds for its `Ack`, and is confirmed by STOPPED for that
task. OctoPrint cannot distinguish another execution of
the same file. Stop waits up to 25 seconds, sends once, and returns 409 when unconfirmed.
The same per-printer command locking and pending-wait preemption apply as for cancel.

After done, cancelled, failed or ended, Dutchman deletes its recorded unique staged file
once fresh status establishes that the file is no longer live. It also cleans an uploaded
ready job when cancelled before Start or forgotten. External files are never removed.
Unavailable status, a live file, or a deletion failure leaves a safe explanation in the
job's optional `detail`; the terminal job outcome is retained. Later status reads may
retry cleanup. The stored terminal result remains readable after deletion.

Bambu commands share the monitoring MQTT connection. Replies are matched by `sequence_id`;
a matching acknowledgement still requires fresh execution telemetry. Reconnect does not
replay commands. Bambu upload refuses an explicit `sdcard: false` report. During PREPARE
and RUNNING, `detail` adds the activity from `stg_cur`, with unknown codes shown numerically.
If `nozzle_temper` is unavailable, the scalar nozzle temperature uses extruder 0, falling
back to extruder 1; `detail` lists the available per-extruder temperatures. Packed temperatures
use the low sixteen bits for current temperature. Bed leveling defaults on and can be
disabled explicitly with `start_profile.bed_leveling = false`.

## Print farm queue

These additive v1 routes operate on the running service's private, durable FIFO queue.
They use the same authentication and origin checks as job routes. Nothing starts
without an explicit confirmed dispatch.

| Method and path | Request | Success |
|---|---|---|
| `GET /api/v1/queue` | No body | 200, entry array in FIFO order |
| `POST /api/v1/queue` | `{"job":"JOB_ID","target":"any","profile":null,"kind":null}` | 201, entry |
| `DELETE /api/v1/queue/{id}` | No body | 204, membership removed; job records retained |
| `POST /api/v1/queue/{id}/move` | `{"position":0}` (zero-based, within queue) | 200, ordered entry array |
| `GET /api/v1/queue/proposals` | No body | 200, proposal array in FIFO order |
| `POST /api/v1/queue/{id}/confirmations` | `{"printer":"workbench"}` | 201, ordinary Start challenge |
| `POST /api/v1/queue/{id}/dispatch` | Ordinary confirmation JSON or query, plus `Plugin-Confirmation` | 200, tracked job |

Entries contain `id`, original prepared `job`, `target` (printer id or `any`), nullable
`profile`, nullable `kind`, nullable refusal `reason`, nullable bound `prepared` job id,
and boolean `attempted`. Stored refusal reasons are limited to 1 KiB. Add accepts a ready job from `POST /api/v1/jobs` once; profile
and kind are optional filters. Filters and fields are plain text of at most 256 bytes.
Add, move, and queue challenge bodies are limited to 4 KiB. Capacity is 5,000 entries.
Queue mutations share the service's state-write lock and use durable private state
replacement. Entries and their job files survive restart and normal job expiry while
queued. Remote bed acknowledgements are still withdrawn at restart.

Each proposal has `id`, nullable `printer`, and nullable `reason`. Matching reads fresh
printer status, requires `idle`, `needs_clearing: false`, and `start` in `controls`,
and compares backend kind and resolved printer, print, and filament preset names
against the original preparation. Absent presets must match absent presets. Optional
profile/kind filters must match too. Candidates use configured printer order; each
printer is proposed at most once, assigned to its earliest compatible entry. Entries
without a match do not prevent later entries matching different printers. Matching
has a 30-second deadline (503 on expiry). Proposals themselves are advisory. Before
matching, every attempted entry with a bound job reserves that job's printer while the
entry remains queued, regardless of FIFO position or still-idle telemetry. Reservations
also remain after refusal and restart; reconcile the execution before removing its entry.
If a bound printer lookup fails, matching returns an error without proposals. An attempted
entry without a bound job reserves nothing.

To dispatch, first request the queue entry's challenge for its current proposed printer.
The server reuses the original job when it was prepared for that printer, or prepares
a new job from the retained input for a different printer, applying all existing
preparation checks. `prepared` records the binding. A reviewed entry remains bound
to that printer; before any attempt, remove and re-add the original job to change it.
The returned challenge is the existing observation- and digest-bound `start` challenge
for that printer and the bound job. Show its summary verbatim and obtain approval.
Clearance requires its own ordinary printer Clear challenge and action first.

Send the challenge once to dispatch, with the ordinary unique 64-character lowercase
hexadecimal `Plugin-Confirmation` header. Dispatch rechecks the proposal, invokes the
existing confirmation consumer, checks controls and bed readiness, and calls the same
Start implementation. Direct job Start refuses queued source or bound jobs; use queue
dispatch. Invalid/missing confirmations, changed observations, missing clearance, and
backend refusals retain the entry and record a safe `reason`. Dispatch uses the normal
403/409 errors, or backend/storage error codes. Success removes membership and returns
the bound tracked job. The original prepared record remains available.

Before calling backend Start, the entry is durably marked `attempted: true`. This
entry can never dispatch again, including after an uncertain result, storage failure,
or restart; a crash after success may leave this blocked entry in the queue. Read
`GET /api/v1/jobs/{prepared}` to reconcile the outcome. No queue request or background
work automatically retries Start. Normal tracked-job controls remain available with
their own fresh challenges. Removing an entry never cancels or retries its execution.
