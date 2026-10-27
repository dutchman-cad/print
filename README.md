# Dutchman Print

Dutchman Print is a local companion that prepares and sends print jobs from
[SolidCraft](https://github.com/tangency-labs/solidcraft) through an open job API.
It slices with your own PrusaSlicer or OrcaSlicer, installed separately.

Not released yet. Tested against simulated printers.

## First run

1. Build `dutchman-print` (see [Build from source](#build-from-source)) and put it on `PATH`.
2. Write a config for your printer. `init` asks for the printer type, its address, and any
   key, password, or access code, then writes a private config file. It contacts nothing.

   ```sh
   dutchman-print init
   ```

3. Check that Print can read the printer:

   ```sh
   dutchman-print printers
   ```

4. Start the job API for the [SolidCraft plugin](https://github.com/dutchman-cad/plugins):

   ```sh
   dutchman-print serve
   ```

Choose `simulated` in step 2 to try Print without a printer.

## Printers

| Printer or software | `kind` | Status | Control |
| --- | --- | --- | --- |
| Simulated printers | `simulated` | Built in, for trying the API | Start, pause, resume, cancel, Stop |
| Prusa printers with PrusaLink | `prusalink` | Simulated only | Start, pause, resume, cancel, Stop |
| OctoPrint | `octoprint` | Simulated only | Start, pause, resume, cancel, Stop |
| Klipper with Moonraker | `moonraker` | Simulated only | Start, pause, resume, cancel, Stop |
| Duet (RepRapFirmware, standalone) | `duet` | Simulated only | Start, pause, resume, cancel, Stop |
| Snapmaker U1 | `snapmaker-u1` | Simulated only | Start, pause, resume, cancel, Stop (single-tool jobs) |
| Elegoo resin printers (SDCP) | `elegoo-sdcp`, `profile = "resin"` | Simulated only | Monitoring and Stop; Print can't start resin jobs, which come from a resin slicer |
| Elegoo Centauri Carbon | `elegoo-sdcp`, `profile = "centauri"` | Simulated only | Monitoring only |
| Snapmaker 2.0 (A150, A250, A350) | `snapmaker-2` | Simulated only | Monitoring only; the connection is approved on the touchscreen |
| Snapmaker J1 and Artisan (SACP) | `snapmaker-j1` | Simulated only | Monitoring only: installed modules and firmware, never print state |
| Repetier-Server | `repetier` | Simulated only | Start, cancel of jobs Print started, and clear; pause, resume, and printer Stop are refused |
| Bambu Lab in LAN mode | `bambu` | Simulated only | Monitoring; control is experimental and off unless `lan_control = true` |

The API reports each printer's supported actions in `controls`; bed clear is available
wherever Start is. Moonraker and U1 include resume only after `resume_commissioned = true`.

Every machine action needs a confirmation the server issues for that printer and job, and
a person who has checked the bed. [docs/print.md](docs/print.md) describes each kind's
setup and what still needs checking on real hardware.

## Build from source

Use Rust 1.88 or newer:

```sh
git clone https://github.com/dutchman-cad/print.git
cd print
cargo build --release --locked
cargo test --workspace --locked
```

The executable is `target/release/dutchman-print` (`.exe` on Windows). Put it on `PATH`
or beside an installed `dutchman` command. `dutchman printers` and `dutchman print`
dispatch to `dutchman-print` when `dutchman` is installed.

## Quick start

Save this as `config.toml`:

```toml
[backend]
kind = "simulated"

[[printers]]
id = "workbench"
name = "Workbench demo"
model = "Example printer"
```

On Unix, make it private with `chmod 600 config.toml`. List the simulated printer or
start the local job API:

```sh
dutchman-print --config config.toml printers
dutchman-print --config config.toml serve --listen 127.0.0.1:8730 --data-dir private-jobs
```

The simulated backend contacts no printers and needs no slicer by default.
See the [companion guide](docs/print.md) for preparing jobs, slicing, and backend setup,
and the [job API v1 specification](docs/print-api.md) for clients.
The SolidCraft plugin lives in [dutchman-cad/plugins](https://github.com/dutchman-cad/plugins).

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE).
