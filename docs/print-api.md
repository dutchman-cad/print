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
| `GET /api/v1/printers` | No body | 200, array of printer objects |
| `POST /api/v1/jobs` | Raw 3MF or multipart, described below | 201, prepared job object |
| `GET /api/v1/jobs/{id}` | No body | 200, current job object |
| `GET /api/v1/jobs/{id}/preview.png` | No body | 200, `image/png`, or 404 when unavailable |
| `POST /api/v1/confirmations` | Action, printer, optional job JSON | 201, confirmation challenge |
| `POST /api/v1/jobs/{id}/start` | Confirmation id in JSON or query; confirmation header | 200, resulting job object |
| `POST /api/v1/jobs/{id}/cancel` | Confirmation id in JSON or query; confirmation header | 200, resulting job object |
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
[{"id":"workbench","name":"Workbench","model":"Example printer","state":"idle"}]
```

| Field | Type | Presence |
|---|---|---|
| `id` | string | Required |
| `name` | string | Required |
| `model` | string | Required |
| `state` | string | Required: `idle`, `printing`, `paused`, `error`, or `offline` |
| `job_id` | string | Optional; active job ID |
| `job_name` | string | Optional; active job name |
| `progress` | number | Optional; fraction from 0 to 1 |
| `seconds_left` | nonnegative integer | Optional |
| `nozzle_temperature` | number | Optional; degrees Celsius |
| `bed_temperature` | number | Optional; degrees Celsius |
| `detail` | string | Optional; original printer state or a safe availability reason |
| `needs_clearing` | boolean | Optional; true when an operator must check and acknowledge the bed |

Unavailable optional fields are omitted. An empty fleet is `[]`.

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
and file-transfer phases map to `offline`, with the phase in `detail` (v1 has no busy state).
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
startup and disconnected Klippy report offline; shutdown, unknown, and contradictory states
block actions. Raw backend messages are not returned. `job_id`, when available, is the
Moonraker history execution ID linked by file metadata; it may identify the last terminal
job while the printer is idle. Progress is a fraction and live `seconds_left` is omitted.
Start stages text G-code with automatic printing disabled, verifies metadata, then starts
once. A lost reply requires reconciliation and never causes an automatic retry. Controls
require the same tracked filename and live execution ID. Resume additionally requires
commissioned behaviour and unchanged machine configuration. Completed, cancelled, and
failed executions remain distinct; idle alone cannot establish completion. See
[Moonraker configuration and checks](print.md#moonraker-klipper).

## Job JSON and states

```json
{"id":"example-job","printer":"workbench","name":"Bracket","state":"ready","estimate":{"seconds":120,"filament_mm":1500,"filament_g":4.5,"layers":60},"progress":0.0,"seconds_left":120}
```

| Field | Type | Presence |
|---|---|---|
| `id` | string | Required |
| `printer` | string | Required; printer ID |
| `name` | string | Required |
| `state` | string | Required: `ready`, `printing`, `paused`, `done`, `cancelled`, or `failed` |
| `estimate` | object | Required; may be empty |
| `progress` | number or null | Required; fraction from 0 to 1, null when unavailable |
| `seconds_left` | nonnegative integer or null | Required; null when unavailable |

Estimate fields are optional and omitted when unavailable: `seconds` (nonnegative integer),
`filament_mm` (nonnegative number), `filament_g` (nonnegative number), and `layers`
(nonnegative integer). Preview paths are never included in JSON; use the preview route.

A prepared job is `ready`; start moves it to `printing`. Pause moves a printing job to
`paused`, and resume moves it back to `printing`. Completion yields `done`. Cancel accepts
ready, printing, or paused jobs and yields `cancelled`. Backend failures may yield `failed`.
Incompatible actions return 409. Printer pause and resume target that printer's active job.
Jobs may expire; subsequent reads return 404. Dutchman Print retains jobs for one hour and
persists service jobs in its private data folder. Startup reloads tracked jobs and reconciles
attempted jobs with live status before control; unavailable printers stay tracked and controls
require live reconciliation. Clients must still handle expired jobs disappearing.

## Confirmation for machine actions

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
settings are refused. Before upload it must match `/api/v1/info` `nozzle_diameter`; a
mismatch names both diameters and requires a matching nozzle or a newly sliced profile.
There is no per-printer mismatch override.

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
