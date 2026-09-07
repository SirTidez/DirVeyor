# Text file preview design

Status: slices 1–3 implemented; preview polish and measured optimization pending

## 1. Goal

FileAdmin should preview human-readable files without leaving the terminal or
executing their contents. Preview is a read-only application view, not an
external opener and not part of the filesystem operation engine.

The first complete preview milestone must support:

- Markdown as raw text, raw and rendered side by side, or rendered only;
- strict JSON pretty-printing with access to the original raw document;
- configuration files, logs, ordinary text, and human-readable source code;
- responsive scrolling and search without loading arbitrarily large files;
- clear encoding, truncation, parse-error, changed-file, and unsupported-file
  states.

## 2. Interaction model

From Browse, `p` previews the focused file. `Enter` keeps opening directories
and opens preview for a supported focused file. Preview takes over the complete
TUI display: neither browser pane nor the Browse inspector remains visible. The
full-screen Preview view supplies its own path/type header, content viewport,
search status, and context-specific footer. Closing Preview restores both
browser panes exactly as they were.

### Shared controls

| Key | Preview action |
| --- | --- |
| `Esc` or `q` | Return to Browse |
| Arrows or `j`/`k` | Scroll vertically |
| `PageUp` / `PageDown` | Scroll by viewport |
| `Home` / `End` or `g` / `G` | Start/end of loaded document |
| Left/right arrows | Horizontal scroll when wrapping is off |
| `w` | Toggle soft wrapping |
| `/` or `Ctrl+f` | Open Find for the active representation |
| `n` / `N` or `F3` / `Shift+F3` | Next/previous search result |
| `r` | Reload the file after it changes |
| `v` | Cycle the formats available for this file type |
| `?` or `F1` | Preview-specific help |

Search and scrolling operate on the active representation. Search never reads
past the bounded file window silently; the UI labels a partial search.

### Find and regular expressions

Find opens a one-line prompt without obscuring the current match. Literal search
is the default and matching is initially case-insensitive. While the prompt is
open, `Ctrl+r` toggles Literal/Regex and
`Alt+c` toggles case-sensitive/case-insensitive matching. The prompt labels the
current mode, reports invalid expressions inline, and leaves the last valid
results highlighted while an invalid pattern is being edited.

While Find owns keyboard input, printable keys—including `q`—edit the query.
`Esc` closes Find but remains in Preview; a subsequent `Esc` exits Preview. This
prevents an ordinary search term from unexpectedly closing the document.

Matches use a visible highlight plus a `current / known total` status. Enter
accepts the query and moves to the next match; `n`, `N`, and the F3 bindings
navigate afterward. Search wraps only after showing a short `Wrapped to start`
or `Wrapped to end` notice. An empty query clears highlights.

Regex is line-oriented in the initial milestone; multiline expressions and
replacement are out of scope. Pattern length, compiled expression size, and
retained match locations are bounded. The implementation must use a regex
engine without uncontrolled backtracking and reject patterns that exceed its
configured limits. For a windowed large file, the status explicitly says
`matches in loaded window`; loading another window reruns the current query.

### Markdown modes

Markdown exposes three persistent modes for the open preview:

| Key | Mode | Behavior |
| --- | --- | --- |
| `1` | Raw | Original Markdown with line numbers |
| `2` | Split | Raw document on the left and rendered document on the right |
| `3` | Rendered | Rendered document using the full preview width |

In Split mode, `Tab` changes which half owns scrolling and search. The two
representations initially open at the same document heading, but scrolling is
independent because rendered blocks do not have a reliable one-to-one mapping
to raw lines. The active half has the stronger border.

```text
┌ Preview · README.md · Markdown · UTF-8 ─────────────── 18 KB ┐
│ Raw · line 42/318              │ Rendered · block 16/94       │
│ 40 ## Installation            │ INSTALLATION                 │
│ 41                            │                              │
│ 42 Run:                       │ Run:                         │
│ 43 ```console                 │   cargo install fileadmin    │
│ 44 cargo install fileadmin    │                              │
│ 45 ```                        │ Requirements                 │
├───────────────────────────────┴──────────────────────────────┤
│ Esc Back  1 Raw  2 Split  3 Rendered  Tab Focus  / Find     │
└──────────────────────────────────────────────────────────────┘
```

The renderer supports headings, paragraphs, emphasis, lists, task items,
quotes, thematic breaks, inline and fenced code, links, tables, and code-block
language labels. Images are represented by alt text and their target. Embedded
HTML is shown as inert text. Preview never fetches remote content, follows a
link, expands an include, or executes a code block.

### JSON behavior

Valid `.json` opens Pretty mode by default with indentation, deterministic
structural coloring, and collapsible containers left for a later enhancement.
`v` toggles Pretty and Raw. Invalid JSON remains viewable in Raw mode and shows
the parser's line and column without rewriting the file. JSON comments and
trailing commas are not silently accepted; `.jsonc` is treated as configuration
text until an explicit JSON-with-comments policy exists.

### Logs, configuration, text, and source

These categories share a raw line viewer with line numbers, wrapping,
horizontal scrolling, and search:

- configuration: `conf`, `config`, `cfg`, `ini`, `toml`, `yaml`, `yml`,
  `properties`, `env`, `editorconfig`, and similar recognized names;
- logs: `log`, `out`, `err`, and files recognized as text by content sniffing;
- source: Rust, C/C++, C#, Java, Kotlin, Go, Swift, Python, Ruby, PHP,
  JavaScript/TypeScript, shell, PowerShell, batch, SQL, HTML/XML, CSS, Lua,
  Protocol Buffers, and other noncompiled text sources;
- ordinary text: `txt`, `csv`, `tsv`, `diff`, `patch`, licenses, readmes, and
  extensionless files that pass text detection.

Source code display does not require a language parser in the first milestone.
Basic language-aware coloring can be added after raw correctness, bounded memory,
and terminal compatibility are proven. Log follow/tail mode is also a later
slice; initial preview is a stable snapshot with an explicit reload action.

## 3. Classification and encoding

Classification uses both the filename and a small content sample. An extension
may select a formatter, but it never overrides clear binary evidence.

1. Reject directories and known compiled/binary formats.
2. Read a bounded sample and detect UTF-8 BOM, UTF-16 BOM, NUL density, and
   invalid text sequences.
3. Prefer strict UTF-8, then BOM-identified UTF-16 LE/BE.
4. Allow lossy decoding only through an explicit preview state that identifies
   replacement characters; never silently reinterpret bytes using the system
   code page.
5. Sanitize terminal controls and bidirectional controls before rendering while
   retaining visible placeholders for removed controls.

The header reports the chosen category, encoding, total file size, and whether
the displayed document is complete or windowed. Unsupported binary content gets
a concise explanation and no best-effort terminal dump.

## 4. Large-file policy

Preview must not make the render loop or memory usage proportional to an
unbounded file.

- Files up to 8 MiB may be loaded as one immutable byte snapshot for Markdown
  rendering or JSON parsing.
- Larger Markdown and JSON files open in Raw mode with a notice that rich
  transformation is unavailable at this size.
- Large text, source, configuration, and log files use a 1 MiB window plus a
  bounded line index. Reaching either edge schedules the neighboring window.
- Logs initially open near the end of a large file; other categories open at
  the beginning. The header says `Tail window` or `Partial window` explicitly.
- A single preview reader keeps only the newest request and cooperatively
  cancels stale reads, following the focused-folder analysis pattern.
- No preview content is added to application logs, operation reports, or error
  telemetry.

The exact thresholds should be constants with tests and may later become user
preferences. Benchmarks must cover a huge single-line log, many short lines,
multibyte Unicode crossing a window boundary, and a file that changes while it
is being read.

## 5. State and architecture

The proposed domain model is independent of Ratatui and filesystem APIs:

```text
PreviewState
  Closed
  Loading { request_id, path }
  Ready(PreviewDocument)
  Failed { path, safe_message }

PreviewDocument
  identity: path + length + modified time
  kind: Markdown | Json | Config | Log | Source | Text
  encoding: Utf8 | Utf16Le | Utf16Be | LossyUtf8
  completeness: Complete | HeadWindow | TailWindow | MiddleWindow
  raw window + line index
  optional rendered Markdown or pretty JSON representation
  mode, active region, scroll offsets, wrap, and search state
```

`fileadmin-fs` owns a dedicated read-only preview service with a replaceable
request slot and bounded result channel. It reads bytes, classifies content,
decodes text, and reports source identity. Markdown and JSON transformation also
run on this reader thread. It performs no terminal rendering and never writes
the file.

A focused transformation module converts decoded snapshots into presentation
blocks. Markdown parsing and JSON parsing happen off the UI thread. Candidate
libraries are `pulldown-cmark` for CommonMark/GFM events and `serde_json` for
strict JSON; versions and feature sets must be reviewed when implementation
begins. Ratatui conversion remains in `fileadmin-app`, while reusable preview
state belongs in `fileadmin-domain`.

Preview search state stores the query, Literal/Regex mode, case mode, bounded
match spans, active match, and whether results cover the complete file. The
implemented first pass searches only the bounded in-memory representation and
caps both pattern and result sizes. Moving search compilation and matching to a
replaceable worker remains follow-up work before increasing snapshot limits.

If the source length or modified time changes after loading, Preview shows
`File changed on disk · r Reload`; it does not silently replace the content and
move the user's scroll position.

## 6. Safety boundaries

- Preview is read-only and never routes through copy/move/recycle execution.
- No Markdown network requests, HTML interpretation, scripts, file includes, or
  URI launching occur inside preview.
- ANSI escapes, C0/C1 controls, OSC sequences, and bidirectional controls cannot
  reach the terminal unchanged.
- Parser nesting, output block count, decoded text size, maximum line length,
  and search result counts are bounded.
- Errors expose a safe category and local path context without dumping file
  contents.
- The original bytes remain unchanged; JSON formatting and Markdown rendering
  only produce in-memory representations.
- Previewing `.env`, keys, configuration, or logs is local and explicit. Their
  contents are never persisted in FileAdmin diagnostics.

## 7. Responsive layout

- Preview always owns the full terminal canvas until explicitly closed.
- At 110 columns and wider, Markdown Split uses two side-by-side regions.
- From 80 through 109 columns, Split remains available with compact headers and
  horizontal scrolling; Raw or Rendered is the more readable default.
- Below 80x24, the existing resize-required screen takes precedence.
- Long paths are middle-truncated in the header, while the inspector/browser
  state is restored unchanged when Preview closes.
- Color reinforces structure but never carries category, focus, parse error, or
  truncation status by itself.

## 8. Delivery slices

### Slice 1 — bounded raw preview (implemented)

- `p` and file-aware `Enter` routing;
- text/binary classification and UTF-8/BOM decoding;
- line-numbered raw view, scrolling, wrapping, and close/restore behavior;
- literal and regex Find with highlighting and next/previous navigation;
- loading, unsupported, windowed, and safe-error states;
- source/config/log/text extension coverage.

### Slice 2 — structured representations (implemented)

- Markdown Raw, Split, and Rendered modes;
- inert CommonMark/GFM block rendering;
- JSON Pretty and Raw modes with line/column parse errors;
- mode-aware footer and help.

### Slice 3 — large-file navigation (implemented)

- bounded head, middle, and tail byte windows with UTF-8/UTF-16-safe boundaries;
- explicit and edge-triggered adjacent-window loading;
- search-result caps and partial-search labeling;
- background source-identity monitoring and explicit reload after changes.

### Slice 4 — polish and measured optimization

- optional source syntax coloring with a bounded grammar set;
- Markdown anchor-aware split positioning;
- log follow mode with explicit paused/following states;
- real-device benchmarks and threshold tuning.

## 9. Acceptance criteria

- Opening and closing preview does not change either browser pane's path,
  cursor, selection, filter, or sort.
- Markdown can switch among Raw, Split, and Rendered without rereading the file.
- JSON formatting never modifies the source and malformed JSON remains readable.
- A large log can be opened, scrolled, searched within the loaded window, and
  closed while input remains responsive and memory stays bounded.
- Source and configuration files display control characters safely and preserve
  ordinary Unicode.
- Literal and regex searches highlight matches, navigate in both directions,
  report invalid expressions safely, and identify window-limited results.
- A stale load cannot replace a newer preview request.
- Changing a file during preview produces a reload notice rather than a silent
  content swap.
- Automated tests cover classifiers, encoding boundaries, hostile terminal
  sequences, Markdown blocks, JSON failures, window boundaries, stale results,
  and the 80/110/160-column layouts.
- Live acceptance covers representative Markdown, JSON, configuration, log,
  plain-text, C#, C++, Rust, and very large log files.
