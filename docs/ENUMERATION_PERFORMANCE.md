# Enumeration performance

## Implemented optimizations

- Transfer counting and permanent-delete planning read each child's `DirEntry`
  metadata and pass its validated fingerprint into recursion. Root inspection,
  link/reparse-point rejection, and execution-time revalidation remain in place.
- Filename sanitization maps characters directly into the output string.
- Pane state normalizes names once per listing, reuses them for sorting and
  filtering, and caches visible indices. Accessors return borrowed slices;
  callers must use state methods to update entries, filtering, and visibility.
- Ordinary directory rendering constructs only the visible viewport's rows.
- Each pane has one scanner worker and one replaceable pending request. New
  requests cancel previous work for that pane; traversal checks cancellation
  before opening the directory and between entries. Dropping the scanner wakes
  idle workers and cancels active directory scans.
- Uncached folder-size measurement waits 200 ms for focus to settle. New
  requests and cancellation wake that wait early. Cached sizes stay immediate.

Windows `DirEntry::metadata()` uses enumeration metadata without an additional
system call. The normal listing and folder-size paths already used it; the
recursive planners previously queried each child's path again. On Unix this
specific substitution does not remove the metadata system call. See the
[Rust platform notes](https://doc.rust-lang.org/std/fs/struct.DirEntry.html#method.metadata).

## Reproduce measurements

```powershell
cargo run --release -p dirveyor-fs --example enumeration_bench
cargo run --release -p dirveyor-fs --example enumeration_bench -- 'D:\a-stable-test-tree'
```

The synthetic workload contains 50,000 deterministically shuffled names. It
measures sanitization, listing preparation, and 100 filtered focus/count/view
reads. The optional path adds three direct listings and three pairs of recursive
metadata walks. It only reads the supplied tree. The recursive comparison
asserts identical file, directory, and byte totals and skips links, reparse
points, and special objects. Use a stable tree; errors terminate the benchmark.

## Local results, 2026-09-07

Windows, Rust 1.96.0, release builds. The synthetic baseline used source from
commit `a588c339fc6035c3a5164081dce159dce1b3ef4d` in a separate temporary build,
with the same shuffled benchmark harness. Representative standalone runs:

| Work | Before | After |
| --- | ---: | ---: |
| Sanitize 50,000 names | 65.90 ms | 6.19 ms |
| Prepare 50,000-entry listing | 66.55 ms | 16.73 ms |
| 100 filtered focus/count/view reads | 692.46 ms | Below 0.001 ms |

The cached-read timing is near timer resolution; it demonstrates elimination
of repeated whole-list scans, not a reliable nanosecond latency estimate. The
new cache adds one normalized string per entry and one index per visible entry.
Reverse-sorted input was also checked: listing preparation was roughly 4.5–7.5 ms
before and 6.1 ms after. The sorting gain depends on input order, and cache
construction has an up-front cost.

A freshly generated local temporary tree contained 4,096 nine-byte files in
32 child directories (33 directories including its root). A later standalone
run measured the recursive path-query approach at 87.20–114.88 ms and entry
metadata reuse at 2.15–2.25 ms, with matching totals. Both implementations run
in the benchmark executable; this is an isolated comparison of metadata walks,
not a measurement of the complete operation planner or transfer engine.

The second walk benefits from filesystem caches populated by the first.
Repeated passes were not cache-flushed. These measurements do not establish
cold-cache, network-share, HDD, or end-to-end transfer performance. Direct
listing remains capped at 50,000 entries; recursive benchmark walks are not.

## Remaining experiments

- Extend the folder-size worker measurements below to controlled cold-cache
  runs and network shares. File-copy worker counts are separate from
  directory-enumeration concurrency.
- An optional transfer mode could skip recursive pre-counting, using growing
  totals during execution. Same-volume renames could then avoid walking every
  descendant. Exact pre-execution recursive scope is still the current behavior;
  this pass does not change review totals or execution semantics.
- Profile any remaining enumeration bottleneck before adding a native Windows
  listing backend or incremental listing delivery. Cancellation cannot interrupt an
  individual blocked filesystem call; drive-info queries also remain blocking
  within their pane's worker.

## Validation

`cargo test --workspace`: 92 tests passed. Coverage includes cache updates across
filter/hidden/sort/load transitions, Unicode sorting/filtering, stale requests,
cancellation between entries, focus settling, viewport scrolling, and nested
delete-manifest order and execution-time change detection.

`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`,
and `cargo build --release --workspace` passed. Interactive terminal and slow
network-share behavior have not been manually exercised.

## Folder-size follow-up on a mechanical data drive

The first pass's planner optimization did not speed up recursive folder-size
inspection: that path already reused entry metadata and still walked one
directory at a time. The data drive was verified locally as a WDC
WD4004FZWX-00GBGB0 SATA HDD.

The folder-size scanner now uses:

- Four scoped directory workers, with a configurable API limit of 1–8 for
  benchmarking. Four is the default after testing 1, 2, 4, and 8 locally; eight
  gave little additional benefit. Results on cold HDDs may differ.
- A shared backlog of at most 1,024 paths, with local iterative descent when
  it fills. Overflow memory and open handles scale with traversal depth, not
  with all remaining descendants. Workers never wait for queue space.
- Windows `FindFirstFileExW` with `FindExInfoBasic` and
  `FIND_FIRST_EX_LARGE_FETCH`, with an ordinary-fetch fallback for unsupported
  flags. Files contribute size directly from the enumeration record without
  constructing paths or metadata objects. The portable backend uses `read_dir`.
- One root resolution for verbatim Windows paths, 64-bit size accumulation,
  and rejection of file and directory reparse points, including junction loops.
- Batched count aggregation and coordinator-driven progress, removing the
  per-entry clock check. The capacity query no longer requests volume labels
  and filesystem information. Scanner drop cancels work and wakes its coordinator.

Microsoft documents the larger directory-query buffer in
[FindFirstFileExW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfileexw).
The locally installed Rust 1.96.0 source uses basic information with flags set
to zero for its general-purpose directory reader.

### End-to-end measurements

```powershell
cargo run --release -p dirveyor-fs --example folder_size_bench -- 'D:\a-stable-tree' 4
```

The benchmark calls the real scanner, drains its events, and includes the
200 ms focus-settling delay and capacity lookup. Its optional last argument is
the worker count. A separate baseline executable was built from the working
tree immediately before this follow-up, including the previous optimization
pass. Each invocation uses a new scanner, so its application result cache is
empty; Windows filesystem caches persist between runs.

For benchmark tree A (a local Unity project), every variant returned exactly
74,584 files, 2,789 directories, 17,210,269,343 bytes, and zero skipped items:

| Warm run | End-to-end time |
| --- | ---: |
| Previous folder-size scanner | 0.3363 s |
| New scanner, 1 worker | 0.3788 s |
| New scanner, 2 workers | 0.3144 s |
| New scanner, 4 workers (default) | 0.2848 s |
| New scanner, 8 workers | 0.2728 s |

This representative warm comparison is about 15% faster end-to-end at four
workers. Removing the fixed 200 ms delay from both timings gives roughly 38%
less remaining time; this is not a cold-scan claim. A second tree,
benchmark tree B (another local Unity project), produced matching totals of 31,259 files, 1,944
directories, 1,872,970,171 bytes, and zero skipped items. Warm baseline runs
took 0.291–0.304 s; four-worker traversal took 0.262 s.

The first baseline scan of tree A took **25.14 s**. The first new
scan of tree B took **14.59 s**. These are different trees with uncontrolled
cache conditions and must not be compared as a before/after speedup. They
demonstrate that the first scan on this mechanical disk can still take many
seconds. A maintained filesystem index would be a separate architectural
change to support near-instant repeated queries; this scanner still enumerates
the tree for uncached requests.

Follow-up validation: **98 workspace tests passed**, strict Clippy and formatting
checks passed, and the release application built. New coverage includes equal
totals at 1/2/4/8 workers, queue overflow, deep Unicode paths beyond MAX_PATH,
junction cycles, cancellation of waiting workers, aggregate overflow, and
64-bit file sizes without allocating giant test files. Windows was tested;
the portable backend was source-reviewed but not built on a Unix target.
