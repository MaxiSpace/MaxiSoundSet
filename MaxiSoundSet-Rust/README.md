# MAXI SOUNDSET v1.0.2

Initial display fix: when the window opens or is shown again from the tray, the entire render surface is marked for redraw; resizing the window is not required. Partial redraw remains in use during normal operation. The internal redraw hook is in `src/window_repaint.rs`, and the `i-slint-core` version is pinned to match Slint exactly. When upgrading Slint, review this hook and the surface-recovery test at 100% and 125% scaling.

A native Windows application built with **Rust** and **Slint 1.18.0**. The layout is inspired by FxSound and uses its own navy theme: buttons are `#00193f`, and the background gradient runs from `#090e22` to `#00193f`. Vazir Regular and Bold, version 16.1.0, are copied unchanged from the fonts on this system and embedded in the EXE; installing the fonts on the target system is not required. Panels and controls have rounded corners, and buttons, lists, page transitions, and meters use smooth animations. The in-app name is English; the FA/EN button changes the control language.

Official v1.0.2: the window appears before audio recovery and device detection. Recovery, device and conflict scans, startup registration, settings saves, and exit waits run on workers; the Run button stays disabled until recovery and detection finish. There is no loading screen. Log writing and rotation run on a separate worker; the Log model updates only when its content changes and the tab is visible. The graph path is not created while the window or Sound tab is hidden. A tray error does not close the app, and tray registration is retried every 5 seconds. Full redraw is also connected to native show, restore, DPI changes, and the first WM_PAINT after each transition, in addition to showing from the tray. When the virtual cable is missing, a clickable warning in the sidebar opens the Components tab.

Current interface: About shows Rust and Slint information, the production year 2026, and MaxiSpace.dev. The Persian interface uses localized labels for the sidebar name, Log tab, and Audio gain meter. The boost/cut bar fills from the center with two colors and a direction that follows the language; its displayed range is ±24 dB. The input/output/gain cards have equal widths independent of their text, and the line beside the selected menu item has been removed.

Official v1.0.2: input and output meters fill right-to-left in Persian and left-to-right in English; the bipolar gain meter is still calculated from the center. The Steady loudness control sits above Sound enhancement and has independent High/Balanced/Low intensity. Reaction controls attack and release time only. Manual level and boost remain active when steady loudness is off. Treble smoothing has its own toggle, its dB reduction meter appears in the same card, and turning it off does not disable the profile or equalizer. A question mark beside the virtual-cable heading opens a dismissible note briefly explaining automatic routing and manual or per-app routing. Sidebar icons stay aligned to the correct edge during open and close animations in both languages. In either mode, the header fills the available space with a fixed 4:5 ratio between the playback device and processing mode. About shows Rust and Slint logos with their versions and website.

## Run

Run `MaxiSoundSet.exe` from a writable folder. If using a portable archive, extract the entire archive first. Normal use does not require Administrator privileges. Settings and logs are stored in `Data` beside the executable. Rust, Slint, and the processing engine are bundled with the EXE; .NET, Python, and Slint do not need to be installed.

Choose a target from 0 to 100 in Windows mode or 0 to 200 in virtual-cable mode, then press Run. In the current version, both modes use the same loudness index and weighted measurement: a target of 100 corresponds to about −16 dB, and 200 to about −4 dB, on a channel-normalized weighted RMS scale; zero silences the output. This index is not actual headphone sound pressure or a standard LUFS meter. In steady-loudness mode, Windows volume changes automatically to reach the target; the target is not a fixed percentage of the Windows volume slider.

## Modes

**Windows volume**: Without a virtual cable, WASAPI loopback measures the selected output signal before the device volume. The app then controls that device's volume using the shared formula. Loud signals are reduced and weak signals are amplified up to the device's 100% limit; digital amplification beyond that limit is not possible in this path. A target above 100 prompts you to choose the virtual-cable mode. If loopback is unavailable, approximate peak measurement is used and this fallback is recorded in the log. Pause, turning off loudness control, and Stop restore the previous volume; Windows mute is preserved.

**Full boost / virtual cable**: Receives the signal from a virtual output, applies actual boost or cut, and sends it to the selected playback device. To use this mode:

1. On **Components**, click **Install required components**. The app downloads and extracts the latest VB-CABLE package from the official site over valid HTTPS, verifies the installer signature, and opens the official x64 installer with an Administrator prompt.
2. Install or repair from the official installer. Updating an existing installation may require removing the driver, restarting, and installing the new version; the installer guides you. Driver installation or updates without interaction with Windows prompts are not promised. The VB-CABLE package is not included in the app ZIP.
3. Restart Windows if prompted, reopen the app, and click ↻.
4. Select **Full boost**, **CABLE Input**, and a different playback output, such as headphones or speakers. With **Automatic** routing, Windows default devices are routed to the cable and restored afterward.

If automatic routing is off, send the desired apps to CABLE Input in Windows sound settings. Turn off the cable's “Listen to this device” option to avoid duplicate audio. Apps with a dedicated output or WASAPI Exclusive mode may not follow the default route. Bluetooth devices and some drivers add latency.

**Check for updates** checks the official package version again. If the previous installation was not registered by this app, its version is shown as unknown; detecting the device alone does not prove that an update succeeded. Without internet access, the installed state can still be detected and the official website link is available. The processing engine and UI have no separate installation dependencies; FxSound is not required to run this app.

## Independent profiles and treble smoothing

On the **Sound** tab, choose a playback type from the **Audio profile** box below the target loudness box. **Gentle** is selected by default and its fixed equalizer is flat. When the independent **Treble smoothing** option is on, only the treble band is reduced when treble energy dominates. Applying profiles requires **Full boost / virtual cable** mode and VB-CABLE; Windows volume mode displays a prompt that virtual-cable mode is required.

The **Steady loudness**, **Sound enhancement**, and **Treble smoothing** switches are independent. Turning off steady loudness changes the target control to manual level/boost, while the audio profile remains applied; a target of zero mutes the sound. Steady loudness has three levels: **High**, **Balanced**, and **Low**. **Reaction** changes only the level control's attack and release speed. Turning off smoothing removes the dynamic treble reduction with a smooth release, while retaining the profile's tonal character and equalizer; this choice is saved in settings. Selecting **System default** turns off the entire enhancer. Turning enhancement back on restores the last selected profile. The engine must be active via Run for changes to take effect; Pause fades the effects while retaining the protective peak limiter.

The app's custom profiles are designed for general listening needs; they are not copied from commercial software and are not correction curves for a specific headphone model. The values below are the gains of three broad filters and the maximum dynamic “s” reduction, not permanent volume increases:

| Profile | 120 Hz | 1800 Hz | 6500 Hz | S reduction limit |
| --- | --- | --- | --- | --- |
| System default | 0 dB | 0 dB | 0 dB | Off |
| Gentle (default) | 0 dB | 0 dB | 0 dB | 6 dB |
| Voice / dialogue | −1.5 dB | +2 dB | −1 dB | 8 dB |
| Music | +1.5 dB | 0 dB | −0.5 dB | 4 dB |
| Cinema | +2 dB | +1 dB | −1.5 dB | 6 dB |
| Warm & relaxed | 0 dB | −0.5 dB | −3 dB | 8 dB |
| Gaming focus | −2 dB | +1.5 dB | −0.5 dB | 5 dB |

When smoothing is enabled, a broad detection filter centered at 7 kHz with Q=0.75 checks its energy relative to the full signal and an absolute noise gate. Detection envelope attack/release is 4/70 ms and reduction changes over 8/120 ms; only the treble portion is reduced, preserving stereo balance. Smoothing is automatically disabled below a 16 kHz sample rate. Profile changes crossfade over 80 ms using warm parallel filters to avoid clicks. The algorithm does not recognize letters and may also affect sharp non-speech details.

The filter formulas come from the [RBJ Audio EQ Cookbook at W3C](https://www.w3.org/TR/audio-eq-cookbook/). The frequency-detection method and limiting of reduction across the full mix are consistent with the concept described in [FabFilter's official de-esser guide](https://www.fabfilter.com/help/pro-ds/using/basiccontrols); this app's implementation and profile values are independent.

## Custom equalizer

Select **Custom equalizer** from the profile box. The equalizer card appears below live audio and above the three meters. Ten bands at 31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, and 16000 Hz can be adjusted from −12 to +12 dB in 0.5 dB UI steps. Frequency increases from left to right, including in the Persian interface, as with conventional equalizers.

Each band's setting is saved in the settings file; Reset sets all bands to zero. Peaking filters use the RBJ formula with Q≈1.414 and 40 ms coefficient smoothing, preserving filter state while adjusting. Bands above 45% of the sample rate are skipped. The custom profile has a 6 dB reduction limit when smoothing is enabled. The equalizer remains independently active when steady loudness or smoothing is off; the peak limiter after it protects against strong boosts.

## Startup and Persian interface

In **Settings**, **Start Maxi with Windows** is on by default. Startup registration is written only for the current account under Windows Run and does not require Administrator privileges. The registered path is updated on first launch from a new EXE location, so run the app from a stable, writable folder. The app starts in the background when you sign in to Windows; audio processing starts when you press Run. Turning off the switch removes only Maxi's startup value. Windows may delay startup apps slightly.

Persian moves the sidebar to the right and changes row, menu, card, and slider direction to right-to-left; Latin technical text and version numbers remain readable. Regular and bold Vazir fonts are copied from the available system fonts into assets and embedded in the EXE. The UI runs with Slint 1.18.0 and software rendering, without downloading a runtime. Animations include menu scale-and-fade opening, staggered items, smooth sidebar movement, button glow and subtle resizing, and a short switch-thumb stretch. Shadows and animated geometry work with this renderer; the glass-like background uses gradients and transparency.

## Conflicts and versions

The **Audio conflicts** tab checks every 5 seconds for known active processors: FxSound, Voicemeeter, Peace, SteelSeries Sonar, Nahimic, Razer Surround, Dolby, and DTS. Warnings indicate possible simultaneous effects or routing changes; simply having a music player open is not a conflict. This is not comprehensive detection of every driver or service. Closing a processor's UI may not stop its driver or service.

After confirmation, **End app** fully terminates the selected process; no app is closed automatically. The process ID, file path, and creation time are rechecked before termination; system and Maxi processes cannot be selected. If Windows denies access or the process has changed, an error is shown and Administrator access is not requested.

The **About** tab shows official version v1.0.2, production year 2026, MaxiSpace.dev, and states that all software rights belong to this brand. Consistent Rust and Slint cards show each technology's logo, version, and official website; the duplicate text version list has been removed. License notices remain in the app folder.

## Background operation and exit

The window's close button hides it while processing continues. The tray icon offers show window, pause/resume, three preset targets, and **Exit app**. If the icon is in the hidden icons area, open the arrow beside the clock. Opening the EXE again also shows the existing instance's window.

**Pause** restores the previous volume in Windows volume mode and passes the signal through without normalization in Full boost; the peak limiter remains active for protection. **Stop / Exit app** stops processing and restores previous audio settings. Before making changes, the app records recovery state in `Data/recovery.json` so it can restore it on the next launch after an unexpected interruption. If the previous device was disconnected, reconnect it. During recovery, routes manually changed by the user after the app started are preserved.

## Log

The **Log** page shows engine events, settings changes, dependency installation, errors, and exit. The open file/folder and clear buttons affect only the log view. Log files are `Data/Logs/maxi.log` and `maxi.1.log` through `maxi.4.log`: about 2 MB each, about 10 MB total. Debug mode logs audio meters and limiter status every 5 seconds. Audio is not recorded or transmitted; internet requests occur only when checking for or installing dependencies.

## Build from source

Recommended build environment: Rust stable 1.98+, Visual Studio Build Tools with Desktop development with C++ and the Windows SDK. Then run in PowerShell:

```powershell
.\Build.ps1
.\Build.ps1 -Mode Test
```

`Cargo.lock` is included with the source. The UI is in `ui/app.slint`, profiles in `src/enhancer.rs`, loudness control in `src/dsp.rs`, WASAPI in `src/core_audio.rs`, and dependency management in `src/components.rs`. The delivered executable was built with Rust 1.98.1 and LLVM-MinGW for Windows x64.

## Processing and testing

In both paths, target 100 maps to weighted RMS around −16 dB, and target 200 in virtual-cable mode maps to around −4 dB. A 20 ms window tracks level changes; an adaptive filter, hysteresis, and rate limits combine quick response with smooth movement. At Balanced reaction, large gain changes are limited to 80 dB/s downward and 12 dB/s upward, with smaller changes made more gradually. In a 20 dB step test, time to return within ±1 dB of the target was 0.615 seconds for a sudden volume increase and 2.165 seconds for a volume drop; these are algorithm timings from a synthetic test. Details are in AUDIO-ENGINE.md.

The engine uses a 0–36 dB boost ceiling, gradual boost fade-in on weak noise, continuous packet processing, and a linked stereo peak limiter with 5 ms lookahead, short soft attack, and 250 ms release. When steady loudness is active, a zero target mutes immediately, and restoring the target recovers the previous gain. This is smooth RMS-based loudness control; it does not claim LUFS measurement or a standard true-peak limiter. How closely it reaches the target depends on audio content, boost ceiling, and peaks.

Local verification covered synthetic signals, actual Windows volume control, and capture/render startup; no extended listening assessment on real music or speech was performed. Verification artifacts and personal runtime data are excluded from this repository.

The v1.0.2 application code is licensed under GNU GPL v3. Slint is used under the Royalty-free 2.0 license, and the official AboutSlint widget is available from the About page. Rust dependencies have their own licenses; see THIRD-PARTY-NOTICES and the upstream links listed there. VB-CABLE is an independent product of VB-Audio and is subject to its own terms.

The license and notice accompanying the older Vazir font version are in `assets/Vazir-LICENSE.txt`; the Apache 2.0 license for Roboto's Latin data is in `assets/Roboto-APACHE-LICENSE.txt`.

Technical documentation: [Windows audio meter before output volume](https://learn.microsoft.com/en-us/windows/win32/api/endpointvolume/nn-endpointvolume-iaudiometerinformation), [Embedding fonts in Slint](https://docs.slint.dev/latest/docs/slint/guide/development/fonts/).

Windows startup: [Official Run / RunOnce documentation](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys).

The Windows icon and executable use the new color image c-100.jpg; the original JPEG is included with the project, with PNG and Windows-compatible ICO files generated from it. The white c.svg logo is copied unchanged and appears only when the sidebar menu is closed. The menu toggle uses an arrow that matches the panel direction. The in-app name is MAXI SOUNDSET and the tagline is “Better sound for you.”

The new method uses a 20 ms weighted window, 35 ms stable-change detection, 20/120 ms fast and 600 ms long-term filters, a 180 ms boost delay after a sudden drop, adaptive gain changes, and a transient RMS guard capped at +3 dB relative to target. Details and measured comparisons are in AUDIO-ENGINE.md.

The settings icon is from Lucide under the ISC license, with color and stroke weight matched to the app icons; its license and source are included in assets.

When steady loudness is enabled in virtual-cable mode with a nonzero target, device volume is held at 100% to prevent the output from being turned down again; adjust listening level with the app's target value. Pause or turning off steady loudness restores the previous volume. The ceiling label on the boost/cut card indicates when the device volume limit or current boost ceiling prevents the target from being fully reached.
