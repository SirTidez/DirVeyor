# DirVeyor for Windows x64

Extract all files, open a terminal at least 80 columns by 24 rows, and run
`dirveyor.exe`. Rust is not required. This is an early-development file manager;
test file-changing operations on disposable data before using important files.

- Tab switches panes; arrows navigate; Enter opens folders or previews files.
- Space selects files; F1 shows the current view's controls.
- Review the displayed source, destination, and operation before confirming.
- Folder counts and logical sizes appear progressively. The first scan of a
  large tree on a hard disk can still take time.
- Copy and move default to full SHA-256 verification. Recycle and permanent
  deletion are separate actions. Undo and recovery are not implemented.

## Install, update, or repair

The same PowerShell command installs or reinstalls the latest stable release:

```powershell
irm https://raw.githubusercontent.com/SirTidez/DirVeyor/main/install.ps1 | iex
```

Close DirVeyor before rerunning the installer. It verifies the release checksum,
remembers custom installation paths, and replaces missing or damaged program
files even when the installed version is already current. Application favorites
and settings are preserved. Repair installs the latest release; it does not
recover deleted files or roll back file operations.

The default location is `%LOCALAPPDATA%\Programs\DirVeyor`. The installer adds
it to the current user's PATH unless `-NoPath` is supplied. Existing terminals
may need to be reopened. No administrator access is required for the default
location. Manual ZIP extraction does not modify PATH.

To uninstall, close DirVeyor, remove its executable and the two
`DirVeyor-README.md` / `DirVeyor-LICENSE.txt` files from the installation directory,
remove that directory's entry from your user PATH, and delete
`%LOCALAPPDATA%\DirVeyor\installer\installation.json`. Preserve any other files
in a custom installation directory.

Documentation and source: https://github.com/SirTidez/DirVeyor
