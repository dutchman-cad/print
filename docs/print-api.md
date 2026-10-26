# Print job API version 1

This is an open HTTP protocol for preparing 3MF print jobs and controlling their execution.
Any server can implement it; it does not require Dutchman or a particular printer backend.
SolidCraft's host and standalone clients can use the same protocol. All paths below are
relative to the server's base URL. JSON uses UTF-8 and `Content-Type: application/json`.

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
| `POST /api/v1/jobs/{id}/start` | Empty body; confirmation header | 200, resulting job object |
| `POST /api/v1/jobs/{id}/cancel` | Empty body; confirmation header | 200, resulting job object |
| `POST /api/v1/printers/{id}/pause` | Empty body; confirmation header | 200, resulting active job object |
| `POST /api/v1/printers/{id}/resume` | Empty body; confirmation header | 200, resulting active job object |

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

Unavailable optional fields are omitted. An empty fleet is `[]`.

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
resets service job state on restart. Clients must handle jobs disappearing.

## Confirmation for machine actions

The four machine routes are start, cancel, pause, and resume. Each requires exactly one
`Plugin-Confirmation` header whose value is exactly 64 lowercase hexadecimal characters
(`0`–`9`, `a`–`f`). SolidCraft's host attaches a fresh value after the person confirms the
action. The plugin does not wrap the 3MF body or generate this confirmation itself.
Standalone clients obtain confirmation from the person before sending a fresh value.

Values are single use across callers and all four routes. A server consumes a value before
attempting the backend action, including when that action fails. Missing, malformed,
duplicate, or reused values return 403. Clients must not retry a machine request with the
same value. Dutchman Print remembers consumed values for ten minutes; clients never reuse
values even after that interval. This header signals confirmation; authentication remains
separate. Servers must not log confirmation values or credentials.

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
| 403 | Browser origin refused or machine confirmation missing, malformed, duplicated, or reused |
| 404 | Printer, job, preview, or route not found |
| 405 | Method not allowed; `Allow` names permitted methods |
| 409 | Printer busy, incompatible job state, or backend refusal |
| 413 | Upload size limit exceeded |
| 415 | Unsupported upload content type |
| 500 | Storage, preparation, or backend failure |
| 503 | Backend unavailable/offline, insufficient storage, or upload/job/confirmation capacity exhausted |

Capacity and storage limits may vary between implementations. Retry transient failures
later; a machine action retry requires another confirmation and a fresh header value.
