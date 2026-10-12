# MAXI SOUNDSET Installer 1.0.3

The MAXI SOUNDSET installer and uninstaller are written in Rust with a Slint interface in Persian and English. The installer embeds the app executable and interface resources, so a separate download is not needed during installation.

## Install

Choose a language, review and accept the agreement, then select an installation folder. Desktop shortcut creation and starting the app when you sign in are optional. Installation is for the current Windows account; the default folder is under `LocalAppData\Programs\MAXI SOUNDSET`.

The installer registers the app with Windows and creates Start menu and uninstall entries. Installing, repairing, or updating VB-CABLE is handled separately through the official vendor installer from the app's Components page.

## Uninstall and update

The uninstaller asks the app to exit safely and waits for the process to close. It does not forcibly terminate the app. Settings and logs are kept by default; a separate option removes app data. Other files in the installation folder are preserved.

During app removal, VB-CABLE removal is optional. When selected, the flow checks the cable identity and an available physical output, then moves Windows audio roles to that output before launching the official VB-Audio installer. If a safe route cannot be established, the operation is guarded. Volume and mute state are protected by the routing and recovery checks.

App updates are initiated from the app's About page. The user chooses when to download and install. The installer targets the existing installation and preserves app settings, startup choice, and shortcuts on a successful update; handled failures support rollback.

## Build from source

Install Rust stable 1.98 or newer and the Visual Studio Build Tools with the Windows SDK. Build the app first because the installer embeds its canonical executable. From the `MaxiSoundSet-Rust` directory:

```powershell
.\Build.ps1 -Mode Release
```

Then, from this installer project directory, build for 64-bit Windows with the MSVC toolchain:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc
```

The `.cargo/config.toml` sets static CRT linking and an 8 MiB stack for the MSVC target. Project-relative paths are used; no host-specific install location is a production default.

The installer source is licensed under GPL v3 as stated in `LICENSE.txt`. See the app's `THIRD-PARTY-NOTICES.md` and `licenses` directory for bundled component notices. Slint is used under its Royalty-free 2.0 license.
