# FileAdmin

FileAdmin is a Rust terminal application for fast, understandable file
management. It combines a keyboard-first two-pane browser with an explicit
operation queue, device-aware transfer planning, and safety-focused previews.

The current milestone combines a functional two-pane browser with reviewed
filesystem operations. It can navigate and select in either pane, filter and
sort entries, toggle dotfiles, inspect metadata, preview human-readable files,
and plan copy, move, recycle, rename, or folder-creation jobs while directory
work remains responsive. On Windows, the all-drives view reports volume labels,
types, filesystems, capacity, used space, and available space.

Focusing a directory starts a replaceable background traversal. The inspector
shows its total contained file size, file/folder counts, and that total as a
percentage of the containing drive's capacity. Moving focus cancels stale work;
links and inaccessible items are skipped and identified when the result is
partial. While traversal is running, live discovered bytes and file/folder
counts provide an indeterminate progress indicator. Recently completed results
are reused within the same pane generation when focus returns to a folder.

Modified times are displayed in the machine's local time as readable calendar
dates. UTC is labeled explicitly only if the operating system's local offset is
unavailable.

Text preview replaces the complete browser display until it is closed. It
supports line-numbered logs, configuration, plain text, and source files;
Markdown Raw, Split, and rendered modes; JSON Raw and Pretty modes; and bounded
literal or regular-expression Find. Large logs open at their tail, while other
large text files open at their head, with the loaded window labeled clearly.

Every change is planned first and shown in a confirmation dialog. Existing
destinations are never overwritten: copy and move conflicts receive numbered
"Keep both" names, while rename and new-folder conflicts stop safely. Recycle
uses the operating-system Recycle Bin or Trash and never falls back to permanent
deletion.

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
| `Enter` | Open a directory, preview a focused file, or retry a failed scan |
| `Enter` on `..` or `Backspace` | Go to the parent directory; from a Windows drive root, show all drives |
| `/` | Filter the active pane |
| `h` | Toggle dotfiles |
| `s` | Cycle sort field |
| `p` | Preview the focused human-readable file |
| `c` | Review a copy from the active pane to the other pane |
| `m` | Review a move from the active pane to the other pane |
| `d` or `Delete` | Review moving the focused/selected items to Recycle Bin / Trash |
| `r` or `F2` | Enter a new name, then review the rename |
| `n` | Enter a name, then review creating a folder in the active pane |
| `Enter` in review | Approve and execute the exact displayed plan |
| `Esc` in review | Cancel without changing files |
| `x`, `c`, or `Esc` while running | Request cancellation at the next safe point |
| `F1` or `?` | Show help |
| `q` or `Ctrl+C` | Quit, or request cancellation when a job is active |

Inside Preview, use arrows or `j`/`k` to scroll, `/` or `Ctrl+f` to find,
`Ctrl+r` in Find to toggle literal/regex matching, `n`/`N` for the next/previous
match, and `Esc` or `q` to return to Browse. Markdown uses `1` Raw, `2` Split,
and `3` rendered; JSON uses `1` Raw and `3` Pretty. `F1` shows the complete
preview-specific key guide.

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
- [Text preview design](docs/PREVIEW_DESIGN.md)

## Current phase and safety boundary

- One operation runs at a time through a bounded background coordinator.
- Copies use at most two workers and publish through temporary files without
  replacing an existing destination.
- Windows same-volume moves use a native no-replace rename. Other moves copy,
  SHA-256 verify, and only then remove the frozen source tree.
- Filesystem roots, overlapping selections, links, junctions, reparse points,
  special files, and self-descendant transfers are rejected.
- A plan is capped at 1,000 top-level selections and 100,000 discovered items.
- Capacity preflight, pause/resume, persistent queues, undo, permanent delete,
  and link-aware operations are not implemented yet.
- Automated checks cover planning, state, rendering, and read-only scanning.
  Live file-changing acceptance is intentionally left to an attended run.
- Folder totals are logical file sizes. Sparse files, compression, and hard links
  can make actual allocated disk usage differ from the displayed total.
- Preview is read-only. Files over 8 MiB currently expose one bounded 1 MiB
  head or tail window; adjacent-window navigation, automatic changed-file
  notices, syntax coloring, and log follow remain future work.
