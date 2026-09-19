# Changelog

## 0.2.0 — Unreleased

- Open the terminal's current working folder in the left pane on startup,
  with the All drives overview in the right pane. Explicit pane locations
  used for administrator relaunch continue to take precedence.

## 0.1.0 — Unreleased

- Windows-first two-pane terminal browser with drive information, favorites,
  filtering, deterministic sorting, and keyboard navigation.
- Read-only text, Markdown, and JSON previews with bounded large-file windows.
- Reviewed copy, move, recycle, permanent-delete, rename, and folder creation.
- Recursive size inspection with cancellable bounded parallel traversal and
  Windows bulk enumeration; cached listing/filter data and viewport rendering.
- Windows ZIP packaging with SHA-256 checksums and a per-user PowerShell
  installer that also updates and repairs the latest stable release.

This is an early-development release. Undo, persistent job queues, capacity
preflight, and link-aware operations are not implemented. Automated tests do
not replace attended file-changing acceptance before binary publication.
