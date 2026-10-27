# Dutchman Print

Dutchman Print is a local companion that prepares and sends print jobs from
[SolidCraft](https://github.com/tangency-labs/solidcraft) through an open job API.
It slices with your own PrusaSlicer or OrcaSlicer, installed separately. Backends include
simulated printers, PrusaLink, OctoPrint, Moonraker (Klipper), and Bambu Lab LAN control.

Not released yet. Tested against simulated printers.

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
