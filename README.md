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
large text files open at their head, with the loaded byte range labeled clearly.
Crossing an edge loads the adjacent window in the background without discarding
the visible window first. Preview also detects source size or modified-time
changes and offers an explicit reload instead of silently replacing the view.

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

Letter shortcuts are displayed as capital letters for readability; pressing
Shift is not required unless a binding explicitly says `Shift`.

| Key | Action |
| --- | --- |
| `Tab` | Switch pane |
| Arrow keys or `J`/`K` | Move focus |
| `Right Arrow` | Open the focused directory, or toggle selection for a focused file |
| `Left Arrow` or `Backspace` | Go to the parent and restore focus to the directory just exited |
| `Space` | Toggle selection |
| `Enter` | Open a directory, preview a focused file, or retry a failed scan |
| `Enter` on `..` | Go to the parent and restore focus; from a Windows drive root, show all drives |
| `/` | Filter the active pane |
| `H` | Toggle dotfiles |
| `S` | Cycle sort field |
| `P` | Preview the focused human-readable file |
| `F` | Add or remove the focused folder from Favorites |
| `Ctrl+F` | Open the dedicated Favorites panel |
| `C` | Review a copy from the active pane to the other pane |
| `M` | Review a move from the active pane to the other pane |
| `D` or `Delete` | Review moving the focused/selected items to Recycle Bin / Trash |
| `R` or `F2` | Enter a new name, then review the rename |
| `N` | Enter a name, then review creating a folder in the active pane |
| `Enter` in review | Approve and execute the exact displayed plan |
| `Esc` in review | Cancel without changing files |
| `X`, `C`, or `Esc` while running | Request cancellation at the next safe point |
| `F1` or `?` | Show help |
| `Q` or `Ctrl+C` | Quit, or request cancellation when a job is active |

Inside Preview, use arrows or `J`/`K` to scroll, `/` or `Ctrl+F` to find,
`Ctrl+R` in Find to toggle literal/regex matching, `N`/`Shift+N` for the
next/previous match, and `Esc` or `Q` to return to Browse. Markdown uses `1` Raw, `2` Split,
and `3` rendered; JSON uses `1` Raw and `3` Pretty. `F1` shows the complete
preview-specific key guide. For large files, `[` and `]` load the previous or
next byte window, while `G` and `Shift+G` load the first or last window.

The all-drives view groups the detected user folder, saved Favorites, and
available drives into separate sections. Favorites persist in FileAdmin's user
configuration directory and can be opened or removed from the `Ctrl+F` panel.
Both browser panes start in this all-drives view instead of inheriting the
folder from which FileAdmin was launched.

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
- Preview is read-only. Files over 8 MiB use bounded 1 MiB head, middle, or tail
  windows. Asynchronous search, syntax coloring, and log follow remain future
  work.
