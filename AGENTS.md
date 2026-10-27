# Contributing instructions

Dutchman Print is a local printer companion for SolidCraft and a standalone command line.
It is Rust, Apache-2.0. It prepares and sends jobs through an open job API, using separately
installed slicers.

## Read first

- [docs/print.md](docs/print.md): configuration, commands, slicers, and backends.
- [docs/print-api.md](docs/print-api.md): job API v1, reports, and machine confirmation.

## Layout

| Path | What is there |
|---|---|
| `crates/dutchman-print/src` | Printer backends, configuration, slicing, uploads, job API, and command line. |
| `crates/dutchman-print/tests` | Synthetic inputs, simulated protocol servers, and command line tests. |
| `docs/` | Companion guide and job API specification. |
| `changelog.d/` | Unreleased changes; one fragment per change. |
| `scripts/changelog.py` | Changelog validation and assembly. |

Shared HTTP and storage guards come from `dutchman-serve`, pinned to a Git revision in
`crates/dutchman-print/Cargo.toml`.

## Checks

Run what CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/changelog.py check
```

CI also checks Rust 1.88 and tests on Linux, macOS, and Windows. Keep filesystem and path
handling portable. On the shared build machine, run Cargo through `buildq run dutchman`
with this checkout as the worktree; set Cargo's `env.TMPDIR` to a private folder below home.

## Rules

- **Never crash.** No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, `unreachable!`,
  or slice indexing outside tests; no `unsafe`. Hostile input comes back as an error.
  Cap sizes, counts, subprocess output, and time spent processing untrusted input.
- **Stable reports.** Only add fields within an API schema version. Document changes in
  `docs/print-api.md`; keep command line and API behavior consistent.
- **Messages.** Plain, specific, sentence case; say what to do next. Never expose credentials,
  confirmation values, or private backend addresses in errors or logs.
- **Machine actions.** Preserve fresh confirmation, bed-clear acknowledgement, readiness,
  and reconciliation checks. Never retry an uncertain start automatically.
- **Docs move with the code.** Update the guide, API specification, and a changelog fragment
  when changing commands, fields, configuration, or behavior.

## Clean-room rule

- Protocol facts come only from vendors' public documentation. Community protocol
  documentation may supply facts only.
- Never read or copy printer server, printer client, or slicer source code. Never use leaked,
  confidential, or decompiled material.
- Run slicers only as external programs through their public command lines; never include
  or link their implementation code.
- Tests use synthetic inputs and simulated printers on `127.0.0.1` only. Never contact real
  printers. Keep optional local slicer checks separate from the default test suite.
- Never upload design files or credentials while investigating a problem.
