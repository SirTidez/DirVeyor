# FileAdmin architecture

Status: implemented foundation and reviewed-operation milestone
Scope: two-pane browser plus guarded filesystem changes

## Current boundary

The foreground loop owns terminal and application state. Directory discovery
and filesystem operations run behind separate bounded channels so planning,
copying, verification, and pane refreshes do not block rendering or input.
Mutation is unreachable until the user approves an immutable plan summary.

```text
keyboard events                 scan workers (2)
      │                                │
      ▼                                ▼
fileadmin-app ── scan requests ─► fileadmin-fs
      │               scan events ◄────┘
      │
      ├── operation intent ─────► fileadmin-engine coordinator
      │                           plan → review → approved execution
      │               bounded progress/results ◄────┘
      ▼
fileadmin-domain snapshot ─────► Ratatui renderer
```

The foreground loop is the sole owner of terminal and application state.
Workers never render and never mutate application state. The UI never performs
filesystem work directly, and only one operation may be active at a time.

The focused-folder size inspector has its own single background worker. It keeps
only the newest request, checks cancellation throughout traversal, and keys each
result by pane, pane generation, path, and request id. Rapid scrolling therefore
replaces old work instead of building a queue, and stale results cannot attach to
a different focused item. Folder analysis also yields whenever a reviewed file
operation is active so it does not compete with planning or transfer I/O. The
worker emits throttled accumulated-byte and discovered-item updates, retains a
small generation-scoped result cache, and reuses directory-entry metadata to
avoid redundant Windows path lookups.

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
- cancellable recursive folder-size analysis that does not follow links,
  junctions, or reparse points.
- a bounded JSON Favorites store published through a temporary file, plus
  platform-specific user-home discovery for the all-drives quick-access area.

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
- inspector, help overlay, notices, and operation affordances;
- name-entry, plan-review, progress/cancellation, and result overlays;
- active-pane source and other-pane destination command routing;
- automatic refresh of panes whose visible directory was affected.

### `fileadmin-engine`

Reviewed operation planner and executor:

- a single bounded coordinator and at most two copy workers;
- immutable plan summaries with exact copy/move/permanent-delete counts or
  explicitly labeled top-level recycle scope, strategy, warnings, and conflict
  resolutions;
- root, overlap, self-descendant, link/reparse, special-file, and plan-size guards;
- streaming transfer discovery with temporary-file publication and interactive
  conflict resolution;
- Windows same-volume native no-replace moves;
- selectable Full SHA-256 or Fast verification, followed by journaled source
  cleanup for cross-volume moves;
- platform Recycle Bin / Trash integration with no permanent-delete fallback;
- opt-in permanent deletion with a typed `DELETE` confirmation;
- source revalidation after review and cancellation at safe boundaries.

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

## Filesystem operation lifecycle

1. A pane with selections remains the marked transfer source when focus moves to
   the destination. If both panes have selections, the active pane is the source.
2. Copy and move validate roots and destination, then stream traversal during
   execution without an in-memory item manifest. Permanent delete freezes an
   exact manifest during planning. Recycle keeps
   selected-root scope so an inaccessible descendant cannot block the operating
   system's Recycle Bin / Trash facility.
3. The user either presses `Esc` to abandon the plan or `Enter` to approve it.
   Permanent deletion additionally requires typing `DELETE`.
4. The executor revalidates source identity, performs the planned strategy, and
   reports bounded progress. Transfer discovery feeds a bounded queue while one
   same-volume or two cross-volume workers copy independent files. A conflict
   pauses the discovery producer, but workers can finish already queued files.
   Permanent delete removes manifest files one at a time and directories
   deepest-first, exposing completed bytes and separate file/folder counts.
   Cancellation stops before the next safe step.
5. The UI displays a durable outcome and refreshes affected visible panes.

## Safety properties already enforced

- The directory-scanning crate remains read-only; mutations are isolated in the
  operation engine.
- No operation executes before explicit plan approval.
- Existing destinations are not overwritten without an explicit conflict
  choice (or a matching Apply-to-all policy), and the chosen destination is
  revalidated before replacement.
- Failed Recycle Bin / Trash operations never fall back to permanent deletion.
- Permanent deletion is a separate, visibly red mode selected with `Ctrl+D`;
  directory failures may be partial and are reported as such.
- Cross-volume move sources remain until the streamed copy pass completes.
  Verified source identities are stored in a temporary binary on-disk journal;
  cleanup revalidates every journal entry before removal.
- Sources are revalidated after review before mutation.
- Directory work runs outside the rendering/input loop.
- Transfer queues and directory snapshots are bounded; a large tree does not
  become an in-memory file manifest.
- Stale results cannot replace current pane state.
- Filesystem roots have harmless parent navigation.
- Filenames and paths cannot inject terminal control or bidirectional controls.
- Terminal state is restored after normal exit and unwinding panics.
- Focus and selection have independent visual and state representations.
- User-folder and Favorite rows are non-selectable virtual locations, so they
  can navigate a pane but can never become filesystem-operation sources.

On Windows, a permission-denied delete result offers elevation only when the
current process is not already elevated. `Ctrl+E` invokes the operating-system
`runas` flow and starts a new FileAdmin process at the current pane locations;
it never executes the failed operation automatically. The user must plan and
approve the operation again in the elevated process. If an elevated process is
still denied, the UI points to ownership, access-control, or filesystem-health
investigation rather than requesting redundant elevation.

## Quick access and Favorites

The all-drives listing is decorated in the foreground after native drive
discovery. It presents separate User folder, Favorites, and Drives groups while
preserving the drive metadata returned by the scanner. The dedicated Favorites
panel reads the same ordered list and opens its chosen folder in the active
browser pane.

Favorites persist as a bounded list of paths in FileAdmin's per-user
configuration directory. Saving creates the parent directory when needed and
publishes a complete replacement file, preventing a partially written JSON
document from becoming the next startup state. Duplicate paths are removed
using platform-appropriate path comparison, and the user's home directory is
not repeated in the Favorites subsection of the all-drives view.

## Known limitations

- Directory results arrive as one batch, not incrementally.
- The selected set and filter string are not yet explicitly capped.
- File timestamps are rendered as local calendar dates and times, with a labeled
  UTC fallback when the local offset cannot be determined.
- Symlinks are identified but cannot be followed from the UI.
- Only dot-prefixed hidden files are recognized.
- Inspector layout is hidden below 110 columns; a compact overlay is planned.
- Beyond Favorites, there is no persistent configuration, browsing history,
  logging, durable queue, pause/resume, undo, or recovery journal.
- Capacity preflight is not yet connected to operation plans.
- Links, junctions, reparse points, and special files are intentionally blocked.
- Folder totals use logical file lengths rather than allocated clusters, so
  sparse, compressed, and hard-linked data may differ from physical disk usage.
- Drive-share percentage is available when the platform can report the volume's
  total capacity; the current native implementation targets Windows.
- Non-Windows moves use verified copy/remove; safe no-replace atomic directory
  rename is currently Windows-specific.
- Automated checks exercise platform-neutral behavior on the local Windows
  toolchain without approving mutations; Linux, SSH, live filesystem changes,
  and real-terminal interaction remain separate acceptance gates.

## Next architectural slice

The next milestone should add destination-capacity preflight, richer per-item
results, and an operation queue with pause/resume and persisted recovery state.
Media-aware concurrency should distinguish rotational disks, solid-state media,
network shares, and removable storage rather than using the current conservative
two-worker ceiling.

## Text preview boundary

Text Preview is implemented as a full-screen, read-only application mode with
a dedicated replaceable reader request. The background reader owns bounded file
I/O, classification, decoding, Markdown transformation, and JSON formatting.
Reusable document, presentation, and search state remains in
`fileadmin-domain`; Ratatui rendering and input routing remain in
`fileadmin-app`. Neither preview contents nor search terms enter diagnostic or
operation logs.

The current search pass runs against the bounded in-memory representation when
the query changes. Files through 8 MiB may be complete snapshots; larger files
use replaceable 1 MiB head, middle, or tail windows with UTF-8/UTF-16-aligned
byte boundaries. A second background watcher compares source length and
modified time and emits a one-shot changed event; it never reads preview
contents. Asynchronous search is the next performance slice. The full
interaction, resource-limit, and delivery plan is in
[Text file preview design](PREVIEW_DESIGN.md).
