# FileAdmin

FileAdmin is a Rust terminal application for fast, understandable file
management. It combines a keyboard-first two-pane browser with an explicit
operation queue, device-aware transfer planning, and safety-focused previews.

The first implemented milestone is a functional **read-only browser**. It can
navigate two panes, select items, filter and sort entries, toggle dotfiles, and
inspect metadata while directory work runs on bounded background workers.

Copy, move, rename, delete, external file opening, and the operation queue are
deliberately disabled until the planning and review model is implemented.

## Run

Requirements: Rust 1.88 or newer and a terminal at least 80×24.

```console
cargo run
```

Common controls:

| Key | Action |
| --- | --- |
| `Tab` | Switch pane |
| Arrow keys or `j`/`k` | Move focus |
| `Space` | Toggle selection |
| `Enter` | Open a directory or retry a failed scan |
| `Backspace` | Go to the parent directory |
| `/` | Filter the active pane |
| `h` | Toggle dotfiles |
| `s` | Cycle sort field |
| `F1` or `?` | Show help |
| `q` or `Ctrl+C` | Quit |

## Validate

```console
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Design documents

- [Product requirements](docs/PRODUCT_REQUIREMENTS.md)
- [Interface concepts](docs/INTERFACE_CONCEPTS.md)
- [Architecture](docs/ARCHITECTURE.md)

## Current phase

1. Validate the read-only browser on Windows and Linux terminals.
2. Add batched directory results and compact inspector behavior.
3. Build a non-mutating copy/move planner and action-review flow.
4. Introduce transfer execution only after the planner is well tested.
