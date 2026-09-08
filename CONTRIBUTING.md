# Contributing to DirVeyor

DirVeyor is an early-development Windows x64 terminal file manager. Read the
[README](README.md) for current behavior and the
[architecture](docs/ARCHITECTURE.md) before changing filesystem operations.
Design documents describe future ideas as well as implemented behavior.

## Development

Install Rust 1.88 or later and the Visual Studio C++ build tools needed for
`x86_64-pc-windows-msvc`. Keep `Cargo.lock` committed. From the repository root:

```powershell
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
powershell -NoProfile -File tests/installer.tests.ps1
```

The Windows CI matrix tests stable Rust and the minimum supported version.
Linux/macOS runtime support has not been validated. Installer tests mock GitHub
downloads, use temporary directories, and do not change the real user PATH.

## Changes and bug reports

- Keep pull requests focused and describe the trigger, changed behavior, and
  validation. Include a regression test when changing observable behavior.
- Preserve cancellation, bounded work queues, source revalidation, and explicit
  operation review. Do not replace skipped or failed work with reported success.
- Use temporary fixtures for tests. Never point destructive tests at a user's
  real folders. Distinguish automated checks from attended transfer acceptance.
- For performance changes, report tree shape, storage type, cache conditions,
  exact file/byte totals, and before/after timings. A warm scan is not evidence
  of cold-drive speed.
- Bug reports should include Windows version, release version, steps, and
  expected/actual results. Remove private paths, filenames, and credentials
  before sharing logs or screenshots.

For binary packaging and publication, follow [Releasing](docs/RELEASING.md).
