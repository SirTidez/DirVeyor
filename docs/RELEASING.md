# Publishing DirVeyor

## Repository publication

The intended repository is `SirTidez/DirVeyor`, with `main` as the default branch.
Create that repository without generated README/license files, then connect and
push the reviewed local history. Repository creation and pushing are separate
publication actions; preparation scripts do neither.

Before pushing, review `git status`, include source, docs, scripts, `Cargo.lock`,
and `.github`, and exclude `target`, `dist`, environment files, and credentials.
The historical interface-concept PNGs are intentional documentation assets.
Require the Windows stable and Rust 1.88 CI checks when configuring branch
protection. Hosted CI can only be verified after the repository is created.

## Build a Windows release

1. Set the version in the workspace `Cargo.toml` and refresh `Cargo.lock` when
   necessary. Finalize `CHANGELOG.md` and run the checks in `CONTRIBUTING.md`.
2. Test copy, move, conflict choices, cancellation, recycle, permanent delete,
   and elevation on disposable files in an attended Windows session. Automated
   tests alone do not establish release acceptance for file-changing operations.
3. Run the package builder, or dispatch **Package Windows** in GitHub Actions:

   ```powershell
   powershell -NoProfile -File scripts/package-windows.ps1
   ```

4. Inspect both files in `dist`:
   - `dirveyor-<version>-windows-x86_64.zip`
   - `dirveyor-<version>-windows-x86_64.zip.sha256`

The ZIP contains exactly `dirveyor.exe`, `LICENSE`, and `README.md`. The checksum
is one line: SHA-256, two spaces, and the ZIP filename. The filename version must
match the Git tag after removing an optional leading `v`. Keep these conventions
because `install.ps1` validates them.

## Publish the assets

Create a Git tag such as `v0.1.0` from the reviewed commit and push it. The
**Release** workflow runs automatically for `v*` tags. Alternatively, choose
**Actions > Release > Run workflow** and enter an existing tag. The workflow
checks out that exact tag; its stable `vX.Y.Z` version must match the application
version in Cargo metadata. Prerelease tags are not supported by this workflow.

The action checks formatting, runs Rust tests and Clippy, tests the installer
under PowerShell 5.1 and 7, builds the Windows x64 ZIP and checksum, and verifies
installation from the actual package. It saves review artifacts and creates a
**draft GitHub Release** with both assets and generated release notes.
Review the notes and assets, then publish the draft only after acceptance checks
are complete. The workflow never publishes a draft automatically.

Existing releases are never overwritten. If creation or asset upload fails,
inspect the release first. To retry a failed draft, delete that draft in GitHub
(keep its tag), then rerun the workflow for the same tag. For an already published
release, prepare a new version instead. Concurrent runs for a tag are serialized.
The workflow requires GitHub Actions to be allowed to write repository contents.

The separate **Package Windows** action still produces artifacts without creating
a release. An Actions artifact is not a GitHub Release asset and cannot be used
by the installer until its ZIP and checksum are attached to a release.

The installer uses GitHub's `/releases/latest` endpoint. The release must be
published as a stable release, not a draft or prerelease, to become available
through the default install command. Never publish the ZIP without its matching
checksum. Installer failures must leave existing installations intact.

## Installer behavior

The root `install.ps1` is the public install/update/repair entry point:

```powershell
irm https://raw.githubusercontent.com/SirTidez/DirVeyor/main/install.ps1 | iex
```

It always downloads and verifies the latest stable package, including when the
same version is installed. Rerunning repairs damaged or missing program files.
`-Repair` makes that intent explicit; it uses the same latest-version behavior.
This does not recover user files deleted or changed through the application.

Custom install directories and PATH preferences are recorded under
`%LOCALAPPDATA%\DirVeyor\installer\installation.json`. Subsequent runs reuse
that record. Without a record, the installer looks for `dirveyor.exe` on PATH,
then uses `%LOCALAPPDATA%\Programs\DirVeyor`. A manually extracted installation
outside PATH needs `-InstallDir` on its first managed update. `-NoPath` is
remembered; supply `-NoPath:$false` to enable PATH management later.

The executable is staged on the installation volume and replaced only after
download, checksum, and archive validation. A locked executable produces a
close-and-retry error. The installer preserves unrelated files, favorites,
and application settings. SHA-256 verifies asset integrity against the published
checksum; it is not a substitute for Authenticode signing or account security.

Offline tests run under PowerShell 5.1 and 7, using mocked release downloads and
temporary installations. After the first stable release is live, verify a real
fresh installation, update, and repair on a disposable Windows user profile.
