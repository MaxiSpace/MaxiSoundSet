# MAXI SOUNDSET

[فارسی](README.fa.md)

MAXI SOUNDSET is a Windows desktop application for adaptive audio processing and loudness control. It is built with Rust and Slint and currently released as **v1.0.1**.

## Highlights

- Select and process an audio output device.
- Use adaptive loudness processing, profiles, intensity controls, and treble smoothing.
- Optionally route processed audio to a virtual cable for recording or streaming workflows.
- Start with Windows and, when enabled, resume the last active audio session after sign-in.
- Switch the interface between English and Persian (RTL).
- Check for new application releases from GitHub within the About page.

## Installation

Download `MAXI-SOUNDSET-v1.0.1-Setup.exe` from the repository's [Releases](../../releases) page and run the installer.

The application can also be used from its installed folder. Do not remove the files that are installed alongside the executable.

## Virtual-cable routing

Virtual-cable routing is optional. Enable it only when you want another application—such as streaming or recording software—to receive MAXI SOUNDSET's processed audio through a virtual audio device. Install and configure a compatible virtual audio cable separately before using this option.

## Updates

The About page checks GitHub Releases for application updates. Component checks, such as virtual-cable availability, are separate from application-update checks.

## Build from source

This project targets Windows. From `MaxiSoundSet-Rust`, run the project's release-check script:

```powershell
.\Run-Beta2-Checks.ps1 -Action release
```

## License

MAXI SOUNDSET is licensed under the [GNU General Public License v3.0](MaxiSoundSet-Rust/LICENSE.txt).
