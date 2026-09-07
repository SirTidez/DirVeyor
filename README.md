# FileAdmin

FileAdmin is a Rust terminal application for fast, understandable file
management. It combines a keyboard-first two-pane browser with an explicit
operation queue, device-aware transfer planning, and safety-focused previews.

The current milestone combines a functional two-pane browser with reviewed
filesystem operations. It can navigate and select in either pane, filter and
sort entries, toggle dotfiles, inspect metadata, preview human-readable files,
and plan copy, move, recycle, permanent-delete, rename, or folder-creation jobs
while directory work remains responsive. On Windows, the all-drives view reports
volume labels, types, filesystems, capacity, used space, and available space.

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
deletion. Permanent deletion is a separate mode with an additional typed
confirmation.

## User guide

### Running FileAdmin

Normal use starts with the packaged `fileadmin.exe`; Rust and Cargo are not
required. Open the executable directly or run it from a terminal:

```console
.\fileadmin.exe
```

Use a terminal at least 80×24 characters. Both browser panes initially show All
drives, so behavior does not depend on the folder from which the executable was
started.

### Interface tour

These captures are rendered from FileAdmin's actual Ratatui interface at
140×38 terminal cells.

#### Drive-first browsing

The User folder and Favorites are kept separate from physical volumes. Focusing
a drive exposes its type, filesystem, capacity, usage, available space, and a
graphical usage bar.

![Two FileAdmin panes showing user-folder shortcuts, Favorites, drive usage, and the drive inspector](docs/images/all-drives.png)

#### Favorites

`Ctrl+F` opens the dedicated Favorites panel over the current browser state.
Saved folders can be opened in the active pane or removed directly from this
panel.

![FileAdmin Favorites panel listing two saved folders](docs/images/favorites.png)

#### Folder inspection

Focusing a directory starts background analysis without blocking navigation.
The inspector reports contained size, drive share, and discovered file and
folder counts.

![FileAdmin browsing two folders while the inspector displays a focused directory's contained size and drive share](docs/images/folder-inspector.png)

#### Full-screen text preview

Preview temporarily replaces the browser. This Markdown example shows raw text
and its rendered representation side by side; other modes include JSON Pretty,
logs, configuration files, source code, and plain text.

![FileAdmin full-screen Markdown preview showing raw and rendered content side by side](docs/images/markdown-preview.png)

#### Reviewed filesystem operations

Filesystem actions do not run immediately. FileAdmin first presents the plan,
destination, strategy, item totals, and warnings for explicit approval.

![FileAdmin copy review showing source, destination, totals, safety warnings, and execution controls](docs/images/operation-review.png)

### Controls

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
| `Enter` on `..` | Go to the parent; from a Windows drive root, show all drives |
| `/` | Filter the active pane |
| `H` | Toggle dotfiles |
| `S` | Cycle sort field |
| `P` | Preview the focused human-readable file |
| `F` | Add or remove the focused folder from Favorites |
| `Ctrl+F` | Open the dedicated Favorites panel |
| `C` | Review a copy from the active pane to the other pane |
| `M` | Review a move from the active pane to the other pane |
| `D` or `Delete` | Review deletion using the currently displayed mode |
| `Ctrl+D` | Switch between Recycle Bin / Trash and permanent deletion |
| `R` or `F2` | Enter a new name, then review the rename |
| `N` | Enter a name, then review creating a folder in the active pane |
| `Enter` in review | Approve the displayed plan; permanent deletion also requires typing `DELETE` |
| `Esc` in review | Cancel without changing files |
| `X`, `C`, or `Esc` while running | Request cancellation at the next safe point |
| `F1` or `?` | Show help |
| `Q` or `Ctrl+C` | Quit, or request cancellation when a job is active |

### Deletion modes and Windows elevation

Recycle mode is the default. It uses the operating-system Recycle Bin or Trash
and never silently falls back to permanent deletion. `Ctrl+D` switches to the
clearly labeled permanent mode; permanent plans require the user to type
`DELETE` before Enter can execute them. Permanent deletion cannot be undone,
and a directory error can occur after some descendants have already been
removed.

FileAdmin snapshots selected top-level items without reading every directory
entry before presenting a delete plan. This allows Windows to handle old or
partially inaccessible directory trees directly. Root paths, links, junctions,
reparse points, special objects, and overlapping selections remain blocked.

If Windows returns Access Denied during a delete attempt and FileAdmin is not
already elevated, the result offers `Ctrl+E`. This requests UAC and opens a new
elevated FileAdmin at the same pane locations; the operation must be planned and
approved again. FileAdmin cannot elevate an already-running process in place.
Elevation is not offered for unrelated failures or when the process is already
elevated. An elevated Access Denied result usually requires inspection of the
path's ownership, access-control entries, or filesystem health.

### Text preview controls

Inside Preview, use arrows or `J`/`K` to scroll, `/` or `Ctrl+F` to find,
`Ctrl+R` in Find to toggle literal/regex matching, `N`/`Shift+N` for the
next/previous match, and `Esc` or `Q` to return to Browse. Markdown uses `1` Raw,
`2` Split, and `3` rendered; JSON uses `1` Raw and `3` Pretty. For large files,
`[` and `]` load adjacent byte windows, while `G` and `Shift+G` load the first or
last window. `F1` shows the complete preview-specific key guide.

Favorites persist in FileAdmin's per-user configuration directory and can be
opened or removed from the `Ctrl+F` panel.

## Developer documentation

### Build and run from source

Development requires Rust 1.88 or newer. Build the executable and then run the
same release artifact used by normal users:

```console
cargo build --release
.\target\release\fileadmin.exe
```

`cargo run` is a developer convenience for testing source changes; it is not the
normal end-user launch path.

### Validate

```console
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
```

### Design documents

- [Product requirements](docs/PRODUCT_REQUIREMENTS.md)
- [Interface concepts](docs/INTERFACE_CONCEPTS.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Text preview design](docs/PREVIEW_DESIGN.md)

### Current phase and safety boundary

- One operation runs at a time through a bounded background coordinator.
- Copies use at most two workers and publish through temporary files without
  replacing an existing destination.
- Windows same-volume moves use a native no-replace rename. Other moves copy,
  SHA-256 verify, and only then remove the frozen source tree.
- Filesystem roots, overlapping selections, links, junctions, reparse points,
  special files, and self-descendant transfers are rejected.
- A plan is capped at 1,000 top-level selections and 100,000 discovered items.
  Delete plans count selected roots without recursively enumerating contents.
- Capacity preflight, pause/resume, persistent queues, undo, and link-aware
  operations are not implemented yet.
- Automated checks cover planning, state, rendering, and read-only scanning.
  Live file-changing acceptance is intentionally left to an attended run.
- Folder totals are logical file sizes. Sparse files, compression, and hard links
  can make actual allocated disk usage differ from the displayed total.
- Preview is read-only. Files over 8 MiB use bounded 1 MiB head, middle, or tail
  windows. Asynchronous search, syntax coloring, and log follow remain future
  work.
