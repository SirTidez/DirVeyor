# FileAdmin product requirements

Status: concept draft 0.1  
Working name: FileAdmin

## 1. Product intent

FileAdmin is a keyboard-first terminal file manager that makes file operations
easy to understand before and during execution. It should feel as immediate as
a traditional two-pane manager while exposing enough context to prevent
surprises: selected items, destination capacity, conflicts, operation strategy,
progress, verification, and failures.

The application should optimize work for the source and destination media
without turning concurrency into a user-facing guessing game. Performance
decisions must remain bounded, observable, and cancellable.

## 2. Product principles

1. **Preview before mutation.** Potentially destructive actions show their
   source, destination, scope, conflicts, and recovery implications before they
   run.
2. **Truthful progress.** Separate discovery, transfer, verification, and
   finalization. Do not display a precise ETA until enough samples exist.
3. **Visible concurrency.** The queue shows which jobs and files are active,
   waiting, paused, verifying, completed, or failed.
4. **Keyboard first, mouse compatible.** Every action has a discoverable
   keyboard path; mouse support may improve accessibility but is not required
   for the first usable release.
5. **Optimize by topology, not folklore.** Scheduling uses measured behavior
   and detected source/destination relationships, with conservative defaults.
6. **Recoverable where possible.** Prefer recycle/trash semantics and atomic
   same-filesystem operations. Clearly label actions that cannot be undone.
7. **Stay responsive.** Directory scans, metadata reads, hashing, and transfers
   never block rendering or input processing.

## 3. Target users and environments

Primary users are developers, administrators, power users, and remote-shell
users who regularly browse large trees or move data between local disks,
external media, network shares, and remote mounts.

Initial target environments:

- Windows 10/11 terminals and PowerShell-hosted sessions.
- Linux terminal emulators and SSH sessions.
- Common filesystems exposed through normal OS file APIs.

Later validation targets include macOS, terminal multiplexers, high-latency
network mounts, removable media, and very large directory trees.

## 4. Core jobs to be done

- Browse two locations without losing context.
- Search, filter, sort, and inspect file metadata.
- Preview Markdown, JSON, configuration, logs, plain text, and source code
  without leaving the TUI.
- Search previewed content with literal or bounded regular-expression matching,
  highlighted results, and next/previous navigation.
- Select one item, ranges, patterns, or many discontiguous items.
- Copy, move, rename, create, delete, and restore where the platform permits.
- Preview the exact scope and likely strategy of an operation.
- Resolve name, type, permission, and content conflicts predictably.
- Queue multiple jobs and understand how they share devices.
- Pause, resume, cancel, retry, or reprioritize work.
- Verify copies when assurance matters.
- Review an audit trail after completion or failure.

## 5. First-release scope

### Browse and inspect

- Two independently navigable file panes.
- Breadcrumb/path editor with history and bookmarks.
- Name, extension, size, modified-time, and type columns.
- Sorting and incremental filtering.
- Hidden-file toggle and configurable symlink behavior.
- Inspector for file metadata, aggregate selection size, permissions, and
  destination free space.
- Clear handling for unavailable, permission-denied, and disconnected paths.

### Select and act

- Single, toggle, range, all, invert, and pattern-based selection.
- Copy, move, rename, new directory/file, and delete.
- Explicit action review for destructive operations or detected conflicts.
- Conflict policies: keep both, replace, skip, newer-wins, and decide each.
- Policy scope: this item, remaining items in this job, or remembered default.
- Dry-run plan that performs no writes.

### Preview text files

- Full-screen, read-only preview that replaces Browse until explicitly exited.
- Markdown Raw, Raw + Rendered side-by-side, and Rendered modes.
- Strict JSON pretty display with raw fallback and visible parse locations.
- Line-oriented configuration, log, plain-text, and source-code display.
- Literal and regular-expression Find, case control, match highlighting, and
  next/previous navigation.
- Bounded large-file windows, text/binary detection, safe decoding, and visible
  partial-content states.

### Queue and observe

- Multiple independent jobs with bounded concurrency.
- Per-job and aggregate transferred bytes, item counts, throughput, elapsed
  time, and confidence-qualified ETA.
- Distinct planning, scanning, queued, running, paused, needs-input, verifying,
  finalizing, completed, cancelled, and failed states.
- Expandable error details and retry from a failed subset.
- Pause/resume/cancel with visible acknowledgement and completion semantics.
- Session event log suitable for troubleshooting without exposing file contents.

### Preferences

- Remappable keys with a built-in reference overlay.
- Color themes that remain usable with 16 colors and common color-vision
  deficiencies.
- Transfer policy presets: balanced, interactive, throughput, and conservative.
- Verification: off, metadata, size, or cryptographic hash.
- Trash/recycle preference and confirmation thresholds.

## 6. Interface model

The top-level views are **Browse**, **Queue**, and **Log**. Browse is the default.
A stable top strip shows view, current job summary, and transient application
state. A stable bottom strip shows commands relevant to the current focus.

Text Preview is a full-screen subordinate view rather than a Browse region. It
replaces the complete Browse layout until exited and restores Browse state
without changing pane paths, focus, selections, filters, or sorting.

Focus must always be visible. Selection and focus are different concepts: a row
can be focused without being selected, and selected rows remain marked when
focus moves elsewhere.

Modal overlays are reserved for decisions that block safe progress: action
review, conflicts, authentication/permission escalation handoff, and irreversible
confirmation. Routine feedback belongs in the main layout or a short-lived
notification that never hides errors.

## 7. Default command language

| Key | Browse meaning | Queue meaning |
| --- | --- | --- |
| `Tab` / `Shift+Tab` | Change pane or region | Change region |
| Arrows or `j`/`k` | Move focus | Move focus |
| `Enter` | Open directory or preview supported file | Expand job |
| `Space` | Toggle selection | Select job |
| `Backspace` | Parent directory | Collapse details |
| `/` | Filter current pane | Filter jobs |
| `c` | Review copy | Cancel filter / contextual action |
| `m` | Review move | Move/reprioritize job |
| `d` | Review delete | Remove completed entry |
| `r` | Rename | Retry failed subset |
| `p` | Preview focused text file | Pause or resume |
| `Esc` | Back/cancel overlay | Back/cancel overlay |
| `?` or `F1` | Context help | Context help |

Key labels shown in the footer must update with focus and terminal width. Final
bindings should be validated in a usability pass; this table is a starting point.

## 8. Operation lifecycle

```text
Intent -> Plan -> Scan -> Review if needed -> Queue -> Transfer
       -> Verify if enabled -> Finalize -> Completed / Partial / Failed
```

- **Intent** captures source, destination, selection, and requested action.
- **Plan** resolves paths, filesystem/device relationships, link behavior,
  conflicts detectable up front, capacity estimates, and recovery options.
- **Scan** enumerates enough work to present scope; very large trees may stream
  additional discoveries while clearly showing that totals are still growing.
- **Review** is mandatory for destructive or ambiguous plans.
- **Transfer** emits monotonic byte and item counters from actual completed I/O.
- **Verify** is independently visible and never counted as copying.
- **Finalize** applies metadata, directory timestamps, cleanup, or atomic swaps.
- **Partial** preserves a manifest of completed, skipped, and failed items.

## 9. Device-aware scheduling

The scheduler should model resources, not merely spawn one task per file.

### Inputs to the planner

- Whether source and destination resolve to the same filesystem/volume.
- Rotational, solid-state, network, removable, or unknown medium when the OS can
  report it reliably.
- File count and size distribution.
- Current measured read/write latency and throughput.
- Other active jobs that share a source or destination resource.
- User-selected policy and an explicit worker cap.

### Baseline behavior

- Use an atomic rename for same-filesystem moves when semantics permit.
- Favor sequential access and low concurrency for rotational media.
- Permit modest parallelism for many independent files on SSD/NVMe while
  bounding outstanding bytes and open handles.
- Treat a copy between two paths on the same physical device differently from a
  copy across independent devices.
- Use conservative concurrency for network and unknown mounts, then adapt only
  within safe bounds from observed latency and throughput.
- Keep metadata discovery on a separate bounded pool from bulk data I/O.
- Hash verification should be pipelined only when it does not cause harmful
  rereads or starve interactive work.
- Apply backpressure from destination writes to source reads.

No device class guarantees an optimal fixed worker count. The implementation
must benchmark representative workloads and expose enough telemetry in debug
mode to tune the policy.

## 10. Safety and correctness requirements

- Normalize paths for comparison without changing the user-visible spelling.
- Detect attempts to copy or move a directory into itself or its descendants.
- Define symlink/junction traversal explicitly and prevent cycles.
- Avoid time-of-check/time-of-use assumptions where platform APIs allow safer
  handles or atomic primitives.
- Write copies to a temporary sibling when possible, flush according to the
  selected durability policy, then atomically publish the destination.
- Never delete a move source until the destination is successfully finalized.
- Preserve metadata on a documented best-effort basis and report exceptions.
- Make cancellation cooperative and document the last safe cancellation point.
- Never silently overwrite a destination under the default policy.
- Protect logs from control-character injection and redact credentials embedded
  in network paths.
- Treat archive extraction as out of scope until path traversal and resource
  exhaustion protections are designed.

## 11. Architecture boundaries

Suggested Rust workspace crates/modules:

- `fileadmin-app`: startup, configuration, terminal ownership, shutdown.
- `fileadmin-ui`: Ratatui rendering, focus model, responsive layouts, themes.
- `fileadmin-domain`: commands, selections, plans, jobs, conflicts, events.
- `fileadmin-fs`: platform-neutral filesystem operations and capability model.
- `fileadmin-platform`: Windows/Linux-specific volume, trash, metadata, and
  atomic-operation adapters.
- `fileadmin-engine`: planner, resource scheduler, workers, cancellation, retry.
- `fileadmin-store`: bookmarks, preferences, session manifests, audit history.
- `fileadmin-testkit`: deterministic fake filesystem, fault injection, and
  scheduler simulation.

Recommended implementation direction: Tokio for asynchronous coordination,
bounded blocking pools for filesystem calls that are not truly asynchronous,
Crossterm for terminal input/output, Ratatui for layout/rendering, and structured
tracing for diagnostics. These are hypotheses to validate in the architecture
phase, not locked dependencies.

The UI consumes immutable snapshots/events and sends domain commands. It must
not perform filesystem work directly. Workers never write terminal state.

## 12. Responsiveness and performance targets

- Input-to-visible-response p95 below 50 ms while transfers are active.
- No unbounded channel, task, worker, selection list, or in-memory directory
  snapshot.
- Render loop should redraw on meaningful state change, resize, or a modest
  animation tick—not continuously at maximum rate.
- First directory entries visible within 150 ms for ordinary local folders;
  large directories populate incrementally.
- Memory usage remains bounded for million-entry traversals through streaming
  manifests and paged UI models.
- Transfer tuning is evaluated separately for large sequential files, many tiny
  files, same-device, cross-device, removable, and network scenarios.

These targets require benchmark baselines on real hardware before release gates
can be finalized.

## 13. Accessibility and terminal compatibility

- Do not communicate state through color alone; pair it with words or symbols.
- Support no-color mode and a safe ASCII fallback for box-drawing/icons.
- Preserve readable hierarchy at 80x24; use the richer three-region layout at
  wider sizes.
- Provide horizontal truncation indicators and full values in the inspector.
- Avoid rapid blinking; respect reduced-motion configuration for progress
  animation.
- Restore terminal mode, cursor visibility, and alternate screen after panics
  through a guarded shutdown path.

## 14. Testing strategy

- Unit tests for selection, sorting, conflict policy, plan validation, and state
  transitions.
- Property tests for path relationships, operation manifests, and resume logic.
- Deterministic scheduler tests with modeled devices and virtual time.
- Fault injection for permission failures, disconnects, full disks, short
  writes, partial reads, locked files, rename races, and cancellation.
- Snapshot tests for major UI widths and states; avoid making snapshots the only
  accessibility assertion.
- Integration tests in isolated temporary filesystems.
- Cross-platform behavior matrix for metadata, symlinks, trash, and atomicity.
- Real-device benchmarks kept separate from correctness tests.

## 15. Delivery phases

1. **Read-only shell:** terminal lifecycle, two-pane browsing, focus, selection,
   inspector, errors, and responsive layouts.
2. **Planning engine:** copy/move dry runs, manifests, capacity checks, conflict
   detection, and review UI—still without mutation by default.
3. **Safe execution:** one bounded copy worker, cancellation, temp-and-publish,
   logs, and verification.
4. **Scheduler:** multiple jobs, device resource graph, adaptive bounded
   concurrency, pause/resume/retry.
5. **Mutation breadth:** move, rename, create, trash/delete, platform adapters,
   and recovery manifests.
6. **Text preview:** bounded raw viewing, Markdown raw/split/rendered modes,
   JSON formatting, large-file windows, and safe content handling.
7. **Hardening:** fault injection, large-tree tests, compatibility matrix,
   benchmarks, packaging, and documentation.

## 16. Decisions still to make

- Product name and command name.
- Minimum supported Rust version and OS versions.
- Configuration format and stable location on each OS.
- Whether session manifests survive restart in the first release.
- Default verification and durability levels.
- Scope and semantics of trash/recycle integration.
- Mouse support and preview plug-ins beyond the built-in text preview.
- Remote protocols beyond mounted filesystems.
- Whether elevated operations are delegated to a small platform helper or left
  to relaunch/manual workflows.

## 17. First usability questions

- Can a new user tell which pane and row own keyboard focus?
- Can they distinguish focused items from selected items?
- Before confirming, can they state exactly what will be changed?
- During a large job, can they identify whether work is scanning, copying,
  verifying, waiting for input, or stalled?
- Can they find the failure and retry only unfinished work?
- At 80x24, are the essential path, selection, action, and progress facts still
  visible?
