# MAXI SOUNDSET 1.0.3

MAXI SOUNDSET is a Windows audio loudness and enhancement app built with Rust and Slint. It includes a Persian right-to-left interface and an English left-to-right interface.

## Features

- Windows volume mode controls the selected output and accepts targets from 0 to 100. Full mode routes audio through VB-CABLE and supports targets up to 200.
- Loudness leveling, sound enhancement, and treble smoothing have separate controls. Sound enhancement and treble smoothing are unavailable in Windows volume mode.
- The compact sidebar can collapse, with animated opening and closing. Input, output, and gain meters are available in both languages.
- VB-CABLE detection uses its standard interface identity rather than its display name or installation path, including when the displayed channel-count label changes.
- Components reuses a valid cached VB-CABLE package by default. Download Again saves a fresh package and does not launch the vendor installer.
- The optional VB-CABLE removal flow moves Windows audio roles to the selected physical output and includes checks to protect volume and mute state.
- The About page offers a user-controlled app update check. Download and installation require an explicit user action. Updates install to the existing app path and support rollback for handled failures.
- The optional Release Notes button opens notes for the selected version.
- Settings and logs are stored in the app's `Data` folder. Uninstall keeps them by default and offers a separate choice to remove app data.
- The app and installer use static C runtime linking and do not require a separate Microsoft Visual C++ runtime installation.

## Install and run

Run `MAXI-SOUNDSET-v1.0.3-Setup.exe`, choose an installation folder, and launch the app from its shortcut. In Windows volume mode, choose a target from 0 to 100. For Full mode, install VB-CABLE from the Components page, then select the virtual cable as input and a physical playback device as output. Use a physical output distinct from the virtual cable.

Choose a sound profile from the Sound page: Gentle, Voice / dialogue, Music, Cinema, Warm & relaxed, or Gaming focus. The EQ page also offers manual adjustment. Sound profiles and treble smoothing require Full mode with VB-CABLE. Loudness leveling, sound enhancement, and treble smoothing remain separate controls; the Windows volume mode does not apply sound enhancement or treble smoothing.

Full mode can route Windows' default playback and communications devices automatically. Apps using an exclusive output may need to be pointed to CABLE Input in their own settings. Stop or pause the app to restore the audio routing and output level captured for the selected devices.

The app does not install or update itself in the background. Update checks, downloads, and installation are started by the user.

The app targets Windows 10 and Windows 11 on x64 systems. Normal app use does not require Administrator privileges; installing or repairing VB-CABLE uses the official vendor installer and may request elevation.

## Build from source

Install Rust stable 1.98 or newer, Visual Studio Build Tools with the Desktop development with C++ workload, and the Windows SDK. From this project directory, build the canonical `MaxiSoundSet.exe`:

```powershell
.\Build.ps1 -Mode Release
```

The installer embeds the app executable, so build the app first. Then, from the adjacent `MaxiSoundSet-Installer` directory, build the x64 MSVC installer:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc
```

The `.cargo/config.toml` files set static CRT linking and an 8 MiB stack for the MSVC target. Build paths are resolved from the project; no host installation path is embedded as a production default.

## Audio engine and licensing

See [AUDIO-ENGINE.md](AUDIO-ENGINE.md) for the processing model and limitations. The app source is licensed under GNU GPL v3; third-party components and their licenses are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and the `licenses` directory. Slint is used under its Royalty-free 2.0 license. VB-CABLE is a separate product from VB-Audio and is subject to its own terms.
