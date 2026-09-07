# FileAdmin architecture

Status: implemented foundation, milestone 1  
Scope: read-only browser shell

## Current boundary

The first milestone intentionally performs no filesystem mutations. It proves
terminal ownership, background directory reads, navigation, focus, selection,
filtering, sorting, responsive rendering, and error presentation before copy,
move, delete, or rename logic is introduced.

```text
keyboard events                          filesystem workers (2)
      │                                          │
      ▼                                          ▼
fileadmin-app ── commands ──────────────► fileadmin-fs
      │                                          │
      │             bounded scan events ◄────────┘
      ▼
fileadmin-domain snapshot ──────────────► Ratatui renderer
```

The foreground loop is the sole owner of terminal and application state.
Workers never render and never mutate application state. The UI never performs
filesystem work directly.

## Workspace crates

### `fileadmin-domain`

Pure application data with no terminal or filesystem dependencies:

- left/right pane identifiers;
- file entry metadata and kinds;
- a synthetic, non-selectable `..` parent entry, including a Windows drive-list
  transition at drive roots;
- focus and stable path-based selection;
- filtering, hidden-dotfile policy, and deterministic sorting;
- loading, ready, and failed states;
- scan generation numbers that reject stale results.

This crate is the seam for state-machine and property tests.

### `fileadmin-fs`

Read-only filesystem adapter:

- two named background workers;
- bounded request and result channels;
- directory and metadata reads;
- stable error classifications and user-safe messages;
- filename control/bidirectional-character sanitization;
- a 50,000-entry in-memory cap with explicit truncation state.
- Windows drive discovery plus native volume labels, filesystem types, and
  free/total capacity metrics.

The entry cap is a prototype safety bound, not the final large-directory
strategy. Paging or a disk-backed index must replace it before million-entry
support can be claimed.

### `fileadmin-app`

Executable and presentation layer:

- raw-mode and alternate-screen lifecycle guards;
- panic-path terminal restoration;
- Crossterm input polling;
- command routing and navigation requests;
- responsive Ratatui layouts;
- Unicode display-width truncation and padding;
- inspector, help overlay, notices, and read-only affordances.

## Scan lifecycle

Each pane owns a monotonically wrapping generation number. Navigation updates
the visible target immediately, clears selection, enters `Loading`, and submits
a bounded `ScanRequest` containing the pane, generation, and path. A worker
returns the same identity with either a listing or classified error. The app
applies it only if the generation is still current, preventing a slow result
from an older path replacing the latest view.

The current scanner returns one bounded listing rather than incremental
batches. The event model is deliberately small for this milestone. A later
version should add batches, cancellation tokens, and coalescing without changing
the pane generation contract.

## Sorting and selection semantics

Directories sort before non-directories. The active sort field then determines
the primary order, with display name and path used as deterministic tie-breakers.
Selection is stored by path rather than row index, so sorting and cursor movement
cannot silently select a different item. Location changes clear selection.

Filtering is case-insensitive and applies to the loaded subset. Dot-prefixed
files are concealed by default on every platform in this prototype. Native
Windows hidden attributes are not yet considered.

## Safety properties already enforced

- No mutation code paths exist in the filesystem crate.
- Future mutation keys return a visible read-only notice.
- Directory work runs outside the rendering/input loop.
- Queues and directory snapshots are bounded.
- Stale results cannot replace current pane state.
- Filesystem roots have harmless parent navigation.
- Filenames and paths cannot inject terminal control or bidirectional controls.
- Terminal state is restored after normal exit and unwinding panics.
- Focus and selection have independent visual and state representations.

## Known limitations

- Directory results arrive as one batch, not incrementally.
- The selected set and filter string are not yet explicitly capped.
- File timestamps are displayed as raw Unix time pending a deliberate local-time
  formatting policy.
- Symlinks are identified but cannot be followed from the UI.
- Only dot-prefixed hidden files are recognized.
- Inspector layout is hidden below 110 columns; a compact overlay is planned.
- There is no persistent configuration, bookmarks, history, logging, queue, or
  operation engine.
- Automated checks exercise platform-neutral behavior on the local Windows
  toolchain; Linux, SSH, and real terminal compatibility remain separate gates.

## Next architectural slice

The next milestone should add a non-mutating operation planner rather than copy
workers immediately. It should produce an inspectable manifest for a proposed
copy or move, including source/destination relationships, estimated scope,
conflicts, capacity uncertainty, link policy, verification policy, and recovery
implications. Execution should remain disabled until plan validation and review
states are tested.
