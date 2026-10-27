# Hardware checklist

Dutchman Print has been tested only against simulated printers. Use this checklist the first
time you connect it to your printer. Each section lists, in order, the command to run and the
result that counts as a pass. Work through one printer at a time.

Sections:

1. [Before you start](#before-you-start)
2. [PrusaLink (Prusa MINI+)](#prusalink-prusa-mini)
3. [Bambu Lab A1 in LAN mode](#bambu-lab-a1-in-lan-mode)
4. [OctoPrint](#octoprint)
5. [Moonraker (Klipper)](#moonraker-klipper)
6. [Snapmaker U1](#snapmaker-u1)
7. [Duet (RepRapFirmware standalone)](#duet-reprapfirmware-standalone)
8. [Elegoo SDCP: resin and Centauri Carbon](#elegoo-sdcp-resin-and-centauri-carbon)
9. [Snapmaker 2.0 and J1/Artisan](#snapmaker-20-and-j1artisan)

Sections 8 and 9 are only for you if you have that hardware.

## Before you start

### Safety rules

- [ ] Run the read-only checks for a printer first. Do not start, pause, resume, cancel or
  stop anything on that printer until every read-only check in its section passes.
- [ ] Check the bed yourself before every start: empty, clean, the right plate fitted. Answer
  the bed question only after looking at it.
- [ ] Stay at the printer for every machine action and for the whole of each test print.
- [ ] Before the first action, find how to stop a print from the printer's own screen, and keep
  that screen within reach.
- [ ] Make the first print a small test part: a 20 mm cube or similar, a few minutes long, in
  the material that is already loaded.
- [ ] If anything moves, heats or reports something you did not expect, stop from the printer's
  screen first, then record what happened.

### What to have ready

- [ ] The `dutchman-print` executable, built from this checkout (see the
  [guide](print.md)).
- [ ] A private config file with your printers (`chmod 600` on Unix). The
  [guide](print.md) has a config example for each kind; the
  [Bambu section](print.md#bambu-lab-lan-control-experimental) has one file that holds both a
  MINI and an A1.
- [ ] Your slicer, set up in its own application: PrusaSlicer 2.8 or newer for PrusaLink,
  OrcaSlicer for Bambu. Note the exact names of the printer, print and filament presets you
  normally slice with.
- [ ] A small test model saved as `.3mf`, for example `cube.3mf`. Dutchman slices it for you.
- [ ] A folder for results, so a mismatch can be fixed from the saved output.

Set these once per terminal so the commands below stay short:

```sh
mkdir -p ~/print-check/cli-data ~/print-check/serve-data
export DUTCHMAN_PRINT_CONFIG=~/print-check/config.toml
export DUTCHMAN_PRINT_DATA=~/print-check/cli-data
cd ~/print-check
```

The examples use the printer IDs `mini`, `a1`, `octo`, `klipper`, `u1`, `duet`, `resin`,
`centauri`, `a350` and `j1`. Use your own IDs from the config.

### Recording results

- [ ] Save the output of every status read to a file, named for the printer and step, for
  example `dutchman-print printers --json > mini-01-idle.json`.
- [ ] For each failed or unexpected check, write one line in `~/print-check/results.md`: the
  section, the check, what you expected, what you saw, and the saved file name.
- [ ] Note your printer's model, firmware version and Dutchman Print version
  (`dutchman-print --version`) at the top of `results.md`.
- [ ] Before sharing any file, remove API keys, access codes and passwords if they appear.

### Using the job API

SolidCraft uses the job API, so the release check for each printer runs its actions through
the API as well as the command line. Start the server in its own terminal and leave it
running:

```sh
dutchman-print serve --data-dir ~/print-check/serve-data
```

In another terminal, every machine action is two steps: request a challenge, read its
summary, then send the action with that challenge id and a fresh confirmation header:

```sh
API=http://127.0.0.1:8730/api/v1
# 1. Request a challenge and read the summary it returns.
curl -s -X POST "$API/confirmations" -H 'Content-Type: application/json' \
  -d '{"action":"clear","printer":"mini"}' | tee challenge.json
# 2. Only if the summary is what you expect, send the action within 60 seconds.
curl -s -X POST "$API/printers/mini/clear" \
  -H "Plugin-Confirmation: $(openssl rand -hex 32)" \
  -H 'Content-Type: application/json' -d '{"confirmation":"CHALLENGE_ID"}'
```

Replace `CHALLENGE_ID` with the `id` from `challenge.json`. Actions on a job (`start`,
`cancel`) add `"job":"JOB_ID"` to the challenge and use `$API/jobs/JOB_ID/start` or
`$API/jobs/JOB_ID/cancel`. Printer actions (`clear`, `pause`, `resume`, `stop`) use
`$API/printers/PRINTER_ID/<action>`. Each challenge and each header value is used once. The
[job API specification](print-api.md) has the full contract.

## PrusaLink (Prusa MINI+)

The MINI+ is the first printer to check, and every check here must pass before a release.

### Setup

- [ ] Add a `kind = "prusalink"` printer as in the [PrusaLink config](print.md#prusalink),
  with `url` set to the printer's local `http://` address.
- [ ] Set `api_key` to the key from **Settings → Network → PrusaLink → Current API Key** on
  the MINI. Do not also set `username` and `password`.
- [ ] Set `printer_profile`, `print_profile` and `filament_profile` to the PrusaSlicer presets
  you use for the MINI+ (for example `Original Prusa MINI & MINI+ Input Shaper`), matching the
  nozzle and filament actually fitted.
- [ ] Configure a `prusaslicer` slicer. PrusaLink needs text G-code; Dutchman turns binary
  G-code off itself.
- [ ] Note the MINI's firmware version from its screen.

### Read-only checks

- [ ] `dutchman-print printers --json > mini-01-idle.json`: `state` is `idle`, `detail` shows
  `IDLE` or `READY`, and `nozzle_temperature` and `bed_temperature` match the printer screen.
- [ ] Change `api_key` to a wrong value and read status again: the printer is reported with an
  authentication failure in `detail`, not as an unrelated error. Put the right key back.
- [ ] Record whether the firmware needs the API key (`X-Api-Key`) or Digest login. If the key
  is refused, try `username = "maker"` with the key as `password`, and record which works.
- [ ] Prepare without starting: `dutchman-print print cube.3mf --printer mini --json >
  mini-02-prepare.json`. Pass: a job in state `ready` with an estimate. Nothing appears on the
  printer and the printer does not move.
- [ ] Preparation did not refuse for missing serial, firmware, or nozzle information, and did
  not refuse for an MMU. If it did, save the message.
- [ ] The estimate's time, filament length and weight are close to what PrusaSlicer shows for
  the same model and presets.
- [ ] If you have a PrusaSlicer preset for a different nozzle size, set it as
  `printer_profile` and prepare again. Pass: refusal naming both diameters. Put the right
  preset back.
- [ ] Record how long a status read takes. Reads must finish within four seconds.
- [ ] Whenever the printer shows another state during the checks below, save a read. Pass:
  `READY` reads `idle`, `BUSY` (a non-print operation) reads `busy`, and `ATTENTION` (waiting
  for you) reads `attention`, each with the original state in `detail`.

### Machine actions (command line)

Run these on a terminal, without `--yes`, so you see each question. Check the bed before each
start.

- [ ] Start: `dutchman-print print cube.3mf --printer mini`. Pass: it shows the estimate, then
  asks about the bed with the current printer state, then asks to start the named job.
  Answer yes to both only after checking the bed. Note the job `id` from the job JSON.
- [ ] While it uploads, the file appears on the USB drive with a unique name and printing does
  not begin until after the start question. Record roughly how long the upload took.
- [ ] Printing starts once, and `dutchman-print printers --json > mini-03-printing.json`
  shows `printing` with `job_name`, `progress`, `seconds_left` and both temperatures, close to
  the printer screen.
- [ ] `dutchman-print status JOB_ID` shows the job as `printing`.
- [ ] Pause: `dutchman-print pause mini`. Pass: the summary names the job; after you confirm,
  the printer pauses and the job reads `paused` within 10 seconds.
- [ ] Resume: `dutchman-print resume mini`. Pass: the printer resumes and the job reads
  `printing` within 10 seconds.
- [ ] Cancel: `dutchman-print cancel JOB_ID`. Pass: the printer stops and the job reads
  `cancelled` within 25 seconds (the MINI reports `STOPPED`).
- [ ] `dutchman-print printers --json > mini-04-after-cancel.json`: the printer needs the bed
  checked again (`needs_clearing` is `true`, or the next start asks about the bed).
- [ ] Staged-file cleanup: after the cancel, check the USB drive's file list on the printer.
  Pass: the uniquely named staged file is gone. If it is still there, save
  `dutchman-print status JOB_ID` and record its `detail`.
- [ ] Complete print: clear the bed, start `cube.3mf` again and let it finish. Pass: the job
  reads `done`, never `ended`, and the bed must be checked again before the next start.
- [ ] Ready signal: if your printer's screen can mark it ready, do that with a clear bed,
  then start a print. Record whether the bed question is still asked; `READY` counts as the
  printer-side bed check, `IDLE` does not.
- [ ] Stop a job started elsewhere: start a short print from the MINI's own screen, then run
  `dutchman-print stop mini`. Pass: the summary shows that job's name, state and progress;
  after you confirm, the print stops within 25 seconds. The file on the USB drive stays.

### Machine actions (job API)

With the server running (see [Using the job API](#using-the-job-api)):

- [ ] Prepare: `curl -s -F file=@cube.3mf -F printer=mini "$API/jobs" | tee
  mini-api-prepare.json`. Pass: 201 with `state` `ready` and an estimate.
- [ ] Clear: request a `clear` challenge for `mini`, read the summary, check the bed, then
  send the clear action. Pass: 204.
- [ ] Start: request a `start` challenge with the job id, read the summary, then send
  `jobs/JOB_ID/start`. Pass: the printer starts once and the job reads `printing`.
- [ ] Reuse the same challenge id or header value for a second action. Pass: 409, and nothing
  happens on the printer.
- [ ] Pause, resume and cancel through `printers/mini/pause`, `printers/mini/resume` and
  `jobs/JOB_ID/cancel`, each with its own challenge. Pass: same results as on the command
  line.
- [ ] Stop the server (Ctrl-C) while a job is printing, start it again, then read
  `GET $API/jobs/JOB_ID`. Pass: the job is still tracked and reads `printing`, and a cancel
  through the API still works.

### Recording

- [ ] Save each `mini-*.json` file and every refusal message. For each state that read
  differently from what is listed here, record the printer screen's wording and the saved file.

## Bambu Lab A1 in LAN mode

Check one A1 fully before adding the second. Control is experimental and stays off until the
read-only checks pass.

### Setup

- [ ] Turn on LAN mode on the printer, and Developer Mode if your firmware has it (see the
  [Bambu section](print.md#bambu-lab-lan-control-experimental)). Note the firmware version.
- [ ] Add a `kind = "bambu"` printer with `host`, the printer's `serial` (not its display
  name), and `access_code` from the printer's network settings. Leave `lan_control = false`.
- [ ] Configure an `orcaslicer` slicer and set `printer_profile`, `print_profile` and
  `filament_profile` to your OrcaSlicer presets (for example `Bambu Lab A1 0.4 nozzle`).
- [ ] Read the certificate: `dutchman-print fingerprint a1 --json > a1-01-fingerprint.json`.
  It sends no login. Record the `subject` and `issuer`, and whether the serial appears in the
  subject's CN.
- [ ] Set `cert_sha256` to the fingerprint after checking it independently. If you have a
  trusted Bambu CA file, also try `ca_file` instead (one at a time) and record whether status
  connects with it.

### Read-only checks

- [ ] `dutchman-print printers --json > a1-02-idle.json`: `state` is `idle` and
  temperatures match the printer screen. Each command line run opens a new connection and asks
  for a full report, so do not run it in a loop; use the server for repeated reads.
- [ ] Change `access_code` to a wrong value and read once. Pass: an authentication failure in
  `detail`. Put the right code back.
- [ ] Delta reports: start the server and read `curl -s "$API/printers"` every 30 seconds for
  10 minutes while the printer is idle. Pass: it never reads `offline`. Save the reads.
- [ ] Prepare without starting: `dutchman-print print cube.3mf --printer a1 --json >
  a1-03-prepare.json`. Pass: a `ready` job with an estimate. If OrcaSlicer exits with status
  205 about `G92 E0`, follow the fix in the [guide](print.md#slicer) and record it.

### Monitoring a print started elsewhere

Continue only when the checks above pass. Dutchman sends no control here, but the printer
moves and heats: check the bed and stay at the printer.

- [ ] Start a short print from Bambu Studio, OrcaSlicer or the printer screen and watch it
  through the server. Pass: `printing` during preparation and printing, with the current
  stage in `detail`; `job_name` is the job; `progress` matches the screen; `seconds_left` is
  in seconds and close to the screen's remaining time.
- [ ] During that print, keep reading every 30 seconds. Pass: it never reads `offline` while
  the printer is connected.
- [ ] Reconnect: during the print, disconnect the computer running Dutchman from the network
  for about 15 seconds, then reconnect. Pass: status returns to `printing` within a minute or
  two, not after five minutes. Record how long it took.
- [ ] When that print finishes, read status. Pass: `idle` with `FINISH` in `detail` and the bed
  must be checked again. Read again a few minutes later and record whether it ever reads
  `error` after `FINISH` while the printer screen shows no fault.
- [ ] If an HMS alert appears on the printer at any time, save a status read and record the
  code shown in `detail`, the severity the printer screen gives it, and whether Dutchman
  reported `attention`, `error`, or neither. Do not cause an alert on purpose.

### Machine actions (command line)

Enable control only now: set `lan_control = true` for this printer. Load the material the
preset names in the AMS slot you map, or set no `ams_mapping` and use the external spool.

- [ ] Start: `dutchman-print print cube.3mf --printer a1`. Pass: the estimate, the bed
  question and the start question appear; after you confirm, the file uploads, the printer
  levels the bed and starts once, using the mapped AMS slot. Note the job `id`.
- [ ] If the printer rejects the start, save the full output and record it. A rejection that
  names authorization means Developer Mode or LAN control is not allowed on this firmware.
  Do not change `start_profile` settings during this check.
- [ ] `dutchman-print printers --json > a1-04-printing.json` shows `printing` with the job.
- [ ] Pause: `dutchman-print pause a1`. Pass: paused within 10 seconds.
- [ ] Resume: `dutchman-print resume a1`. Pass: printing within 10 seconds.
- [ ] Cancel: `dutchman-print cancel JOB_ID`. Pass: the printer stops and the job reads
  `cancelled` within 25 seconds (the A1 reports `FAILED`).
- [ ] Staged-file cleanup: check the SD card's file list. Pass: the uniquely named staged
  `.3mf` is gone. If not, save `dutchman-print status JOB_ID` and record `detail`.
- [ ] Next job after a stop: with the printer still in `FAILED`, check the bed and start
  `cube.3mf` again. Pass: Dutchman asks about the bed and then allows the start; the stopped
  job's old error does not block it. If the command line refuses, save the message and try
  the clear and start through the job API. Let this print finish; the job reads `done`.
- [ ] Next job after a finish: with the printer in `FINISH`, start once more and cancel it
  early. Pass: the bed question is asked and the start is allowed. Record whether the finished
  job's error code blocked anything.
- [ ] Material check: map the job to an AMS slot holding a different material type and start.
  Pass: it refuses before upload, naming the required and installed material. Be ready to stop
  from the printer screen; if the print starts, stop it and record a failure. Fix the mapping.
- [ ] Stop a job started elsewhere: start a short print from Bambu Studio or the printer
  screen, then `dutchman-print stop a1`. Pass: the summary names that job; the print stops and
  the printer reports `FAILED` within 25 seconds. The file stays on the SD card.

### Machine actions (job API)

- [ ] Prepare, clear and start through the API as for the MINI, with printer `a1`. Pass: the
  printer starts once.
- [ ] Stop after a reconnect: while that job prints, disconnect the computer running the
  server for about 15 seconds and reconnect. Within five minutes, request a `stop` challenge
  for `a1` and send `printers/a1/stop`. Pass: the summary names the job, and Stop is accepted
  and confirmed instead of being refused as offline. Save the output.

### Second A1

- [ ] Add the second A1 with its own `id`, serial, access code and trust setting, and repeat
  the read-only checks. Pass: both printers list together and each shows its own state.
- [ ] With the bed checked, start a print on the first A1 and read both. Pass: the second
  stays `idle`.

### Recording

- [ ] Save every `a1-*.json` file, the fingerprint output, and the output of any refused or
  unconfirmed action. Record the firmware version next to each result.

## OctoPrint

### Setup

- [ ] Add a `kind = "octoprint"` printer as in the [OctoPrint config](print.md#octoprint),
  with an application key or user key that has status, file, upload, delete, select, print,
  pause and cancel permissions.
- [ ] Set the three slicer presets for the printer, single extruder only.

### Read-only checks

- [ ] With the serial connection up, `dutchman-print printers --json > octo-01-idle.json`
  reads `idle`. Disconnect the serial connection in OctoPrint: it reads `offline`. Reconnect.
- [ ] While OctoPrint connects to the printer, save a read: it reads `busy` with the phase in
  `detail`.
- [ ] Prepare without starting. Pass: `ready`. If you set a different nozzle diameter in the
  active OctoPrint printer profile, preparation refuses naming both values. Put it back.
- [ ] Use a wrong key and read status. Pass: an authentication failure in `detail`. Put it
  back.

### Machine actions

- [ ] Start through the command line and through the API, as for the MINI. Pass: the file is
  uploaded with a unique name, is not printed on upload, and starts once after confirmation.
- [ ] Pause and resume. Pass: each within 10 seconds; pause never toggles to resume.
- [ ] Cancel. Pass: `cancelled` within 25 seconds.
- [ ] Let one print finish. Record how long the job stays `printing` during `Finishing`, and
  whether it then reads `done`.
- [ ] Stop a job started from OctoPrint's own interface with `dutchman-print stop octo`.
  Pass: it stops; the file stays.
- [ ] Staged-file cleanup: after each Dutchman job ends, the staged file is gone from
  OctoPrint's file list.
- [ ] Do not print Dutchman's staged files from OctoPrint itself; Dutchman cannot tell that
  apart from its own job.

### Recording

- [ ] Save the reads and record any OctoPrint state text that read differently from the
  above.

## Moonraker (Klipper)

### Setup

- [ ] Add a `kind = "moonraker"` printer as in the [Moonraker config](print.md#moonraker-klipper),
  with `api_key` unless the installation trusts this computer.
- [ ] Keep `resume_commissioned = false` for now.
- [ ] Make sure Moonraker's history component is enabled; control needs it.

### Read-only checks

- [ ] `dutchman-print printers --json > klipper-01-idle.json` reads `idle`.
- [ ] Restart Klipper from Mainsail or Fluidd and read during startup: `busy`, then `idle`.
- [ ] Prepare without starting. Pass: `ready`. The sliced nozzle diameter matches
  `nozzle_diameter` in the `[extruder]` section; a mismatched preset is refused naming both.

### Machine actions

- [ ] Before enabling resume, run a short print from Mainsail or Fluidd and use its own pause
  and resume. Watch that your `PAUSE` and `RESUME` macros park and return safely. Only then set
  `resume_commissioned = true`.
- [ ] Start through the command line and the API. Pass: a unique file appears under `gcodes`,
  is not printed or queued on upload, and starts once.
- [ ] While it prints, `seconds_left` is absent; this is expected.
- [ ] Pause, resume, cancel. Pass: 10, 10 and 25 seconds; the job reads `cancelled`.
- [ ] Let one print finish. Pass: `done`, and the bed must be checked again.
- [ ] Stop a job started from Mainsail or Fluidd with `dutchman-print stop klipper`.
- [ ] Staged-file cleanup: the staged file is gone after each Dutchman job ends.

### Recording

- [ ] Save the reads, the Klipper and Moonraker versions, and any refusal.

## Snapmaker U1

The U1 uses the Moonraker checks above, with `kind = "snapmaker-u1"`. Run the Moonraker
setup and read-only checks, then the read-only checks here, and only then the Moonraker machine
actions and the ones here.

### Setup

- [ ] Add the printer as in the [U1 config](print.md#snapmaker-u1). Record which `url` and
  port work; the vendor documentation does not state one.

### Read-only checks

- [ ] `dutchman-print printers --json > u1-01-idle.json` reads `idle`. Record which tool's
  temperature `nozzle_temperature` shows and compare with the screen.
- [ ] Prepare a job sliced for one tool. Pass: `ready`.
- [ ] Prepare a two-tool job. Pass: refused, naming both tools.

### Machine actions

- [ ] Start a single-tool job sliced with `T1`. Pass: the printer uses its second tool
  (`extruder1`), not the first. Record which tool it used.
- [ ] Pause, resume, cancel and Stop as for Moonraker.

### Recording

- [ ] Save the reads and record the tool each `T<n>` selected.

## Duet (RepRapFirmware standalone)

Only for a Duet without a single-board computer.

### Setup

- [ ] Add a `kind = "duet"` printer as in the [Duet config](print.md#duet-reprapfirmware-standalone),
  with `password` if one is set with `M551`, and `nozzle_diameter` set to the installed nozzle.
- [ ] Record the RepRapFirmware version.

### Read-only checks

- [ ] `dutchman-print printers --json > duet-01-idle.json` reads `idle`, with nozzle and
  bed temperatures matching Duet Web Control.
- [ ] Use a wrong password. Pass: an authentication failure. Put it back.
- [ ] If your password contains spaces, quotes, `&` or `%`, record whether login works.
- [ ] Open Duet Web Control in several browsers until the board runs out of logins, then read.
  Record what `detail` says.
- [ ] Prepare without starting. Pass: `ready`.

### Machine actions

- [ ] Start a job. Pass: a unique file appears in `0:/gcodes` and prints once.
- [ ] Observe and record the exact `job_id` while printing. Dutchman expects the full path,
  such as `0:/gcodes/<file>`. If it shows another form, record it: start will read
  unconfirmed and Stop will not match.
- [ ] Record whether `job_id` stays set after a job ends, and whether `job.lastFileName`
  changes before the board reports `processing` after a start.
- [ ] Record the sequence of `detail` values after start, pause, resume and cancel, and how
  long `pausing` and `cancelling` last.
- [ ] Pause and resume. Pass: each within 10 seconds; your `pause.g` and `resume.g` run.
- [ ] Cancel. Pass: the board pauses first, then cancels; `cancel.g` (or `stop.g`) runs; the
  job reads `cancelled`. On RepRapFirmware 3.4 or earlier, record what `M0` did.
- [ ] Stop a job started from Duet Web Control with `dutchman-print stop duet`.
- [ ] Staged-file cleanup: the staged file is gone after each Dutchman job ends. If cleanup
  fails, record the job's `detail`.
- [ ] On RepRapFirmware 3.6 or later, record whether the bed temperature is reported.

### Recording

- [ ] Save every read and record the firmware version beside each result.

## Elegoo SDCP: resin and Centauri Carbon

Only for someone with an Elegoo SDCP printer. Dutchman never prepares or starts jobs on
these printers.

### Setup

- [ ] Add the printer as in the [Elegoo config](print.md#elegoo-sdcp), with `profile =
  "resin"` or `profile = "centauri"` and `host`. Leave `mainboard_id` unset at first.

### Read-only checks (both)

- [ ] `dutchman-print printers --json > sdcp-01-idle.json` reads `idle`. Record which port
  answered (resin 3030, Centauri 80 by default).
- [ ] If the printer stays `offline`, set `mainboard_id` and read again. Record whether it was
  needed.
- [ ] Leave the server reading for a few minutes. Record how often the printer sends status
  without being asked, and whether the connection stays open.
- [ ] Record whether the printer replies to the `ping` keepalive.

### Resin

Continue only when the read-only checks pass. Check the vat and plate and stay at the printer.

- [ ] Start a resin print from the printer or your resin slicer, and watch status. Pass:
  `printing`, `progress` by layer, and `seconds_left` close to the screen.
- [ ] Record whether `detail` ever shows an error number while printing.
- [ ] Stop: `dutchman-print stop resin`. Pass: the summary names the task; after you
  confirm, the printer stops within 25 seconds.
- [ ] After the stop, save a read and record what `detail` shows and whether `job_id` (the
  `TaskId`) is kept or cleared.
- [ ] Pause and resume are refused; that is expected.

### Centauri Carbon

No control is sent; check only that status reads correctly.

- [ ] While idle, record whether `detail` ever shows `PREPARING` (status 8) with no job.
- [ ] Only after the checks above pass: during a print started from the printer, record each
  `detail` value you see, especially any `UNKNOWN STATUS <n>`.
- [ ] Nozzle and bed temperatures match the screen.

### Recording

- [ ] Save every read and record the firmware version.

## Snapmaker 2.0 and J1/Artisan

Only for someone with this hardware. No control is sent; check only that status reads
correctly.

### Setup

- [ ] Add the printer as in the [Snapmaker config](print.md#snapmaker-20-and-j1artisan), with
  `host` only. Record whether the default port (8080 for 2.0, 8888 for J1) works.

### Snapmaker 2.0

- [ ] Read status with the server running. Pass: `offline` with `detail` asking you to tap
  Confirm; after you tap it on the touchscreen, the printer reads `idle`.
- [ ] Record whether the touchscreen request appeared at once, and what `detail` showed if
  you did not confirm within 60 seconds.
- [ ] Leave the server reading for 30 minutes. Record whether the connection is lost and needs
  confirming again.
- [ ] Record the `detail` for each head you have (single extruder, dual extruder, laser, CNC),
  including the `headType` number.
- [ ] Only after the checks above pass: during a print started from the touchscreen or Luban,
  record the status value in `detail`. It reads `error` because only `IDLE` is documented.

### J1 and Artisan

- [ ] Read status. Record whether the touchscreen asks to approve a connection, and whether
  `detail` shows `PRINTING MODULE` with module names and firmware versions.
- [ ] If it stays `offline` saying Snapmaker has not published how to pair, record that: the
  CRC-8 variant (polynomial 0x07, initial value 0), the length field, or the receiver ID may
  not match the printer.
- [ ] Record the module IDs reported for each print head.

### Recording

- [ ] Save every read and record the firmware version.
