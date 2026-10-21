# Automated Windows builds

The **Windows build** GitHub Actions workflow runs on pushes, pull requests, and
manual requests from the repository's **Actions** tab. It uses Windows Server 2022
with Rust 1.98.1 and the committed Cargo lockfiles. Each crate is built from its
own directory so its `.cargo/config.toml` settings apply.

The workflow builds the app first, then compiles the custom Rust/Slint installer
with that app executable embedded inside it. It also runs both crates' tests
serially, with opt-in ignored tests excluded. The installer itself is not launched.

After a successful run, download the artifact named
`MaxiSoundSet-windows-x64-unsigned-<commit>` from the run page. It contains:

- `MaxiSoundSet.exe`
- `MaxiSoundSet-Setup.exe`
- `SHA256SUMS.txt` with SHA-256 hashes of both executables

These builds are **unsigned**. This workflow does not publish GitHub Releases or
call SignPath. After SignPath approves the project, signing must be integrated
so the app is signed before it is embedded, and the completed installer is signed
afterwards. Do not describe these artifacts as digitally signed.

To run the same build locally, install Rust 1.98.1 and the Visual Studio C++ build
tools with the Windows SDK, then run from the repository root in PowerShell:

```powershell
./.github/scripts/Build-Windows.ps1
```

The output is written to `artifacts/windows-x64`. GitHub keeps uploaded artifacts
for 14 days. The workflow has read-only repository permissions, does not retain
checkout credentials, and does not need repository secrets.
