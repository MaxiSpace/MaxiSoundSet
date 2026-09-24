# MAXI SOUNDSET — Installer v1.0.2

Executable: `../MAXI-SOUNDSET-v1.0.2-Setup.exe`. This is the only file needed to install on 64-bit Windows. The app, Vazir font, and interface resources are embedded in the file; installation does not require a separate download.

The installer and uninstaller are written in Rust and use a Slint 1.18 interface with a navy and turquoise theme, Vazir font, and Persian and English languages. The version is read from the application project; the current app version is 1.0.2.

The interface includes soft stage transitions, hover glows and button click responses, and gradual changes to the stage indicator and progress bar. The turquoise frame, large logo card, and line icons for the stages are inspired by the supplied reference.

## Install

Choose a language, read and accept the agreement, then type the installation path or change it with the folder picker. Desktop shortcut creation and launch at Windows startup are optional. Installation is per current Windows account. The default path is `LocalAppData\Programs\MAXI SOUNDSET`. You need write permission for the selected path.

The app is registered in the Windows list of installed apps. A Start menu shortcut and `Uninstall.exe` are created in the installation folder. The startup option stays synchronized with the app's own settings. Components and the virtual-cable driver are installed separately from the app's Components tab.

## Uninstall and upgrade

The v1.0.2 uninstaller checks whether this installed version is open and displays a warning and a button to close the app before removal. Closing uses the app's normal exit path and restores audio; removal is not allowed until processing ends. Older versions that do not support the safe-exit message must be exited from within the app. The uninstaller never force-kills the app.

Before uninstalling or upgrading, use Exit app so the background process is closed. Run `Uninstall.exe` from the installation folder or uninstall the app from Windows Settings. Settings and logs are preserved by default; the uninstaller also offers an option to remove them. Other files in the installation folder are not deleted. To remove itself, the uninstaller runs a temporary copy.

Installing again to the same path upgrades the app files and retains audio settings. If an upgrade fails while registering the installation, the previous files, settings, shortcuts, and registry values are restored.

## Build from source

This project is next to `../MaxiSoundSet-Rust`. First build the final `../MaxiSoundSet-Rust/MaxiSoundSet.exe`; the installer embeds that file using `include_bytes!`. Vazir resources, logo, and icon are also embedded from the app project during compilation. Then run `cargo build --release` with Rust on Windows. In this workspace's development environment, use `../../work/rust-tools/Build-Installer.ps1 -Action release`.

Installation tests run in separate paths and use temporary private registry keys; they do not install the app on the real account. Local verification reports are excluded from this repository.
