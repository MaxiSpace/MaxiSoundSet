# Audio engine 2.5 — analysis and validation

This release changes loudness processing, target interpretation, metering and native gain management. EQ profiles and filter settings are preserved.

## Why cable 200 could be quieter than Windows 100

In 2.4, Windows 100 selected native unity and compensated relative to the initial programme level. Cable 200 instead targeted −10 dBFS RMS; cable 100 targeted −26 dBFS RMS. The numbers therefore described different objectives. A loud source at native unity could exceed cable 200.

The digital path also retained the physical endpoint's prior volume and used the application's default render session. The endpoint or an attenuated mixer session could reduce the normalized signal again. The native checks found the physical endpoint initially at 30%; percentage is not an amplitude ratio, so the correction uses the device's reported dB gain.

Finally, 80% of the programme estimate used a 3-second envelope, with normal upward slew capped at 2 dB/s. Recognition and recovery were slow.

## A common target and detector

For target n:

- n = 0: mute while leveling is enabled.
- n = 1…100: T(n) = −50 + 0.34n dB.
- n = 101…200: T(n) = −16 + 0.12(n − 100) dB.
- Target amplitude A(n) = 10^(T(n)/20).

Both paths use channel-normalized K-weighted RMS over a sliding 20 ms window. The two filters follow BS.1770's shelving/pre-filter and RLB high-pass design, adjusted for sample rate. This is a short-window loudness proxy; it is not integrated/gated LUFS, a standard multichannel loudness meter, calibrated sound pressure, or a certification claim.

Desired programme gain is G = T(n) − 20log10(L), where L is the adaptive weighted programme amplitude. The target and its inverse meter mapping are shared. The digital detector observes the signal after the existing EQ; EQ processing itself is unchanged.

Windows clips target to 100 and gain to [native minimum − native maximum, 0] dB. The actuator writes native maximum + G to the endpoint. It can attenuate and return to native unity; it cannot digitally boost beyond unity. If loopback is unavailable, peak/√2 is an explicitly approximate fallback recorded in the log.

Cable processing clips digital gain to [−80 dB, configured boost ceiling], with the existing peak limiter downstream. At steady state, target 100 has the same measured objective in both paths. Cable 200 requests 12 dB more than 100, subject to the boost ceiling and peak headroom. A signal already near full scale cannot always become 12 dB louder cleanly.

## Fast response without constant gain hunting

The fast energy envelope rises with a 20 ms time constant and falls with 120 ms; the stable envelope uses 600 ms. A fast/stable divergence greater than 3 dB, sustained for 35 ms, engages fast tracking. Tracking ends inside a 1 dB hysteresis band. A 40 ms blend prevents an abrupt detector switch.

A drop greater than 6 dB holds upward gain for 180 ms. Short breaths and pauses therefore do not immediately trigger a large boost; sustained quiet content does. Raw and weighted noise measurements suppress positive gain on subthreshold noise.

Large-error gain slew, in dB/s:

| Reaction | Reduce | Increase |
|---|---:|---:|
| Fast | 120 | 24 |
| Normal | 80 | 12 |
| Relaxed | 40 | 6 |

Small corrections use 30% of these rates, with a 0.25 dB deadband and asymmetric exponential smoothing. These are application tuning choices, not standard-mandated values.

A fast RMS guard starts 3 dB above the target. The cable path feeds this ceiling into the existing stereo-linked 5 ms lookahead limiter, using a 0.35 ms attack and 250 ms release. The sample peak ceiling remains 0.944. It is not a measured true-peak ceiling. Windows applies the RMS guard through its endpoint actuator after capture and OS scheduling latency; native volume control cannot anticipate an uncaptured transient.

Full-path gain telemetry includes AGC, limiter and endpoint attenuation. The gain card indicates when the native/digital boost ceiling or protection is active. Native output metering is an estimate from captured RMS and reported endpoint gain, not an acoustic measurement.

## Avoiding double attenuation and restoring state

Active, nonzero cable leveling reserves the physical endpoint at 100% and maintains it with a 100 ms headroom check. A dedicated, non-persistent render session starts at unity and does not inherit an old app mixer attenuation. The app's loudness target controls the digital output; endpoint mute is respected.

Profile-only playback, target zero, pause and leveling-off retain or restore the original endpoint volume. Exit and failures restore volume and owned automatic routes through the persisted recovery guard. Manual mute is preserved. Other processors or streams bypassing the selected cable can still affect playback; this release does not change their settings.

The target is now a common loudness index, not a fixed Windows slider percentage. During native leveling, Windows volume changes automatically to maintain that target.

## Reproducible comparison

Fixture: 440 Hz mono, 48 kHz, profile bypass, boost ceiling 36 dB, normal reaction. Warm with a 0.04-peak tone for 16 seconds; step to 0.4 peak (+20 dB) for 10 seconds; return to 0.04 peak (−20 dB) for 20 seconds. Report first return within ±1 dB after departing that band, in 5 ms packets, excluding the initial lookahead interval.

| Version | Own target-100 RMS objective | Loud-step recovery | Quiet-step recovery |
|---|---:|---:|---:|
| 2.4 | 0.050119, unweighted | 5.470 s | 18.765 s |
| 2.5 | 0.158489, weighted | 0.615 s | 2.165 s |

Each run uses its release's own target-100 objective. The objective, weighting and protection differ. These are deterministic algorithm timings, not listening results or measured Windows end-to-end latency. LoudnessBenchmark.rs provides the standalone new probe.

Unit tests cover native/cable objective agreement, the 12 dB upper range, quiet-signal boost, short-dip hold, loud-burst protection, recovery, packet independence, filter stability, noise and stereo peak safety. Actual Windows checks verify native targets 50/100 within 1 dB using a tone confined to the virtual endpoint, pre-volume capture stability within 0.3 dB, full worker operation, headroom reservation/maintenance/release and restoration. No test tone is sent to the physical output. Final results are recorded in Verification.txt.

## Primary references

- [Microsoft: pre-volume peak metering](https://learn.microsoft.com/en-us/windows/win32/api/endpointvolume/nn-endpointvolume-iaudiometerinformation)
- [Microsoft: loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [Microsoft: default pre-volume loopback](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/windows-11-apis-for-audio-processing-objects)
- [ITU BS.1770 loudness basis](https://www.itu.int/rec/R-REC-BS.1770)
- [K-filter coefficient design reference: libebur128](https://github.com/jiixyj/libebur128/blob/master/ebur128/ebur128.c)
- [Shure: AGC attack, hold, release and pumping/breathing](https://content-files.shure.com/Pubs/asp-selection-and-operation-of-audio-signal-processors/us_pro_audiosignalprocessor_ea.pdf)
