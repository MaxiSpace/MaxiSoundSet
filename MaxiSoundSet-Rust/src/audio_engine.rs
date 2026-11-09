use crate::{
    audio_routing,
    core_audio::{
        self, CaptureStream, ComScope, Endpoint, RecoveryGuard, RecoveryState, RenderStream,
    },
    dsp::{self, DspConfig, MeterReading, Normalizer, SampleQueue},
    settings::Settings,
};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Controls {
    pub target: AtomicI32,
    pub boost: AtomicI32,
    pub speed: AtomicI32,
    pub intensity: AtomicI32,
    pub profile: AtomicI32,
    pub leveling: AtomicBool,
    pub treble_smoothing: AtomicBool,
    pub eq_bands: [AtomicI32; 10],
    pub paused: AtomicBool,
    pub stop: AtomicBool,
}
impl Controls {
    fn new() -> Self {
        Self {
            target: AtomicI32::new(100),
            boost: AtomicI32::new(24),
            speed: AtomicI32::new(1),
            intensity: AtomicI32::new(1),
            profile: AtomicI32::new(1),
            leveling: AtomicBool::new(true),
            treble_smoothing: AtomicBool::new(true),
            eq_bands: std::array::from_fn(|_| AtomicI32::new(0)),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }
    fn dsp_config(&self) -> DspConfig {
        DspConfig {
            target: self.target.load(Ordering::Relaxed),
            boost_db: self.boost.load(Ordering::Relaxed),
            speed: self.speed.load(Ordering::Relaxed),
            intensity: self.intensity.load(Ordering::Relaxed),
            profile: self.profile.load(Ordering::Relaxed),
            leveling: self.leveling.load(Ordering::Relaxed),
            treble_smoothing: self.treble_smoothing.load(Ordering::Relaxed),
            eq_bands: std::array::from_fn(|i| {
                self.eq_bands[i].load(Ordering::Relaxed) as f32 / 10.
            }),
            enabled: !self.paused.load(Ordering::Relaxed),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MeasurementSource {
    Sampled,
    PeakEstimate,
    #[default]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CaptureState {
    #[default]
    AwaitingPackets,
    CaptureUnavailable,
    NoRecentPackets,
    ZeroSamples,
    NonzeroSamples,
    NonfiniteSamples,
}

#[derive(Clone, Copy, Debug, Default)]
struct CaptureReading {
    binding: Option<Instant>,
    state: CaptureState,
    packets: u64,
    nonzero_packets: u64,
}

struct CaptureObservation {
    binding: Instant,
    available: bool,
    last_packet: Option<Duration>,
    packets: u64,
    nonzero_packets: u64,
    last_state: CaptureState,
}
impl Default for CaptureObservation {
    fn default() -> Self {
        Self { binding: Instant::now(), available: true, last_packet: None, packets: 0, nonzero_packets: 0, last_state: CaptureState::AwaitingPackets }
    }
}
impl CaptureObservation {
    // Only call for decoded capture data, before DSP. Synthetic queue padding
    // and an empty GetBuffer result are not evidence of received audio.
    fn record(&mut self, frames: usize, samples: &[f32]) {
        self.record_at(self.binding.elapsed(), frames, samples);
    }
    fn record_at(&mut self, now: Duration, frames: usize, samples: &[f32]) {
        if frames == 0 { return; }
        self.last_packet = Some(now);
        self.last_state = CaptureState::ZeroSamples;
        for sample in samples {
            if !sample.is_finite() { self.last_state = CaptureState::NonfiniteSamples; }
            else if *sample != 0.0 {
                self.last_state = CaptureState::NonzeroSamples;
                break;
            }
        }
        self.packets = self.packets.saturating_add(1);
        if self.last_state == CaptureState::NonzeroSamples {
            self.nonzero_packets = self.nonzero_packets.saturating_add(1);
        }
    }
    fn reading(&self) -> CaptureReading {
        self.reading_at(self.binding.elapsed())
    }
    fn reading_at(&self, now: Duration) -> CaptureReading {
        CaptureReading {
            binding: Some(self.binding),
            state: if !self.available { CaptureState::CaptureUnavailable }
                else if self.packets == 0 { CaptureState::AwaitingPackets }
                else if now.saturating_sub(self.last_packet.unwrap_or_default()) > Duration::from_millis(250) { CaptureState::NoRecentPackets }
                else { self.last_state },
            packets: self.packets,
            nonzero_packets: self.nonzero_packets,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DiagnosticState {
    capture: CaptureState,
    measurement: MeasurementSource,
    paused: bool,
    muted: bool,
    silent: bool,
}
#[derive(Default)]
pub struct DiagnosticTransitions {
    binding: Option<Instant>,
    reported: Option<DiagnosticState>,
    reported_at: Duration,
    saw_packet: bool,
    saw_nonzero: bool,
}
impl DiagnosticTransitions {
    fn should_report(&mut self, now: Duration, value: &AudioSnapshot, paused: bool) -> bool {
        if self.binding != value.capture.binding {
            *self = Self { binding: value.capture.binding, ..Default::default() };
        }
        if self.binding.is_none() { return false; }
        let state = DiagnosticState { capture: value.capture.state,
            measurement: value.source, paused, muted: value.muted, silent: value.silent };
        let milestone = (value.capture.packets > 0 && !self.saw_packet)
            || (value.capture.nonzero_packets > 0 && !self.saw_nonzero);
        let lifecycle = self.reported.is_none_or(|previous| previous.paused != paused);
        // Compare to the last report, not the last poll: a persistent recovery
        // during the cooldown is emitted later. Quiet fluctuations coalesce.
        if lifecycle || milestone || (self.reported != Some(state)
            && now.saturating_sub(self.reported_at) >= Duration::from_secs(1)) {
            self.reported = Some(state);
            self.reported_at = now;
            self.saw_packet |= value.capture.packets > 0;
            self.saw_nonzero |= value.capture.nonzero_packets > 0;
            true
        } else { false }
    }
    // Called by the existing UI snapshot timer, never by the audio packet loop.
    pub fn report(&mut self, now: Duration, value: &AudioSnapshot, paused: bool) {
        if self.should_report(now, value, paused) {
            log::info!("Audio observation: capture={:?}, packets={}, finite_nonzero_packets={}, measurement={:?}, paused={}, muted={}, quiet={}",
                value.capture.state, value.capture.packets, value.capture.nonzero_packets,
                value.source, paused, value.muted, value.silent);
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct AudioSnapshot {
    pub meters: MeterReading,
    pub muted: bool,
    pub silent: bool,
    pub reference: f32,
    pub source: MeasurementSource,
    capture: CaptureReading,
}
impl AudioSnapshot {
    pub fn captured_nonzero_packets(&self) -> u64 { self.capture.nonzero_packets }
}
#[derive(Debug)]
pub enum AudioEvent {
    PhysicalOutput(String),
    Started,
    Stopped,
    Error(String),
    DeviceInvalidated(String),
    RestoreWarning(String),
}

fn engine_error_event(error: &anyhow::Error) -> AudioEvent {
    let message = format!("{error:#}");
    if error.chain().any(|cause| cause.downcast_ref::<windows::core::Error>()
        .is_some_and(|error| error.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED)) {
        AudioEvent::DeviceInvalidated(message)
    } else {
        AudioEvent::Error(message)
    }
}
pub struct AudioEngine {
    pub controls: Arc<Controls>,
    pub snapshot: Arc<Mutex<AudioSnapshot>>,
    events: Receiver<AudioEvent>,
    sender: Sender<AudioEvent>,
    worker: Option<JoinHandle<()>>,
}
impl AudioEngine {
    pub fn new() -> Self {
        let (sender, events) = mpsc::channel();
        Self {
            controls: Arc::new(Controls::new()),
            snapshot: Arc::new(Mutex::new(AudioSnapshot::default())),
            sender,
            events,
            worker: None,
        }
    }
    pub fn update(&self, s: &Settings) {
        self.controls.target.store(
            s.target.clamp(0, if s.mode == 0 { 100 } else { 200 }),
            Ordering::Relaxed,
        );
        for (control, gain) in self.controls.eq_bands.iter().zip(s.eq_bands) {
            control.store(
                (gain.clamp(-12., 12.) * 10.).round() as i32,
                Ordering::Relaxed,
            );
        }
        self.controls.boost.store(s.boost_db, Ordering::Relaxed);
        self.controls.speed.store(s.speed, Ordering::Relaxed);
        self.controls
            .intensity
            .store(s.intensity, Ordering::Relaxed);
        self.controls.profile.store(s.profile, Ordering::Relaxed);
        self.controls.leveling.store(s.leveling, Ordering::Relaxed);
        self.controls
            .treble_smoothing
            .store(s.treble_smoothing, Ordering::Relaxed);
    }
    pub fn start(&mut self, settings: Settings, data: PathBuf) -> Result<()> {
        if self.worker.as_ref().is_some_and(|h| !h.is_finished()) {
            anyhow::bail!("Audio engine is already running");
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        core_audio::recover(&data)
            .context("Reconnect the previous output before restarting audio")?;
        self.update(&settings);
        self.controls.paused.store(false, Ordering::Relaxed);
        self.controls.stop.store(false, Ordering::Relaxed);
        let controls = self.controls.clone();
        let snapshot = self.snapshot.clone();
        let events = self.sender.clone();
        self.worker = Some(thread::Builder::new().name("Maxi Audio".into()).spawn(move || {
            let result = (|| -> Result<()> { let _com = ComScope::new()?; if settings.mode == 1 { run_full(&settings, &data, &controls, &snapshot, &events) } else { run_system(&settings, &data, &controls, &snapshot, &events) } })();
            if let Err(e) = result { log::error!("Audio engine error: {e:#}"); let _ = events.send(engine_error_event(&e)); }
            if data.join("recovery.json").exists() { let _ = events.send(AudioEvent::RestoreWarning("Audio restoration is incomplete. Reconnect the previous device or select speakers in Windows sound settings.".into())); }
            let final_snapshot = *snapshot.lock().unwrap_or_else(|p| p.into_inner());
            log::info!("Audio engine stopped: last_published_snapshot capture={:?}, packets={}, finite_nonzero_packets={}, measurement={:?}; resetting observation", final_snapshot.capture.state, final_snapshot.capture.packets, final_snapshot.capture.nonzero_packets, final_snapshot.source);
            self::snapshot(&snapshot, AudioSnapshot::default()); let _ = events.send(AudioEvent::Stopped);
        })?);
        Ok(())
    }
    pub fn stop(&self) {
        self.controls.stop.store(true, Ordering::Relaxed);
    }
    pub fn pause(&self) -> bool {
        let value = !self.controls.paused.load(Ordering::Relaxed);
        self.controls.paused.store(value, Ordering::Relaxed);
        log::info!("Audio {}", if value { "paused" } else { "resumed" });
        value
    }
    pub fn poll(&self) -> Vec<AudioEvent> {
        self.events.try_iter().collect()
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.worker.as_ref().is_some_and(|h| !h.is_finished()) {
            anyhow::ensure!(
                Instant::now() < deadline,
                "Audio device is taking too long to stop. Please retry Exit app."
            );
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(h) = self.worker.take() {
            h.join()
                .map_err(|_| anyhow::anyhow!("Audio worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            log::error!("Shutdown: {e:#}");
        }
    }
}

fn snapshot(store: &Mutex<AudioSnapshot>, value: AudioSnapshot) {
    let mut current = store.lock().unwrap_or_else(|p| p.into_inner());
    *current = value;
}

fn measurement_source(
    capture_available: bool,
    has_packet: bool,
    packet_age: Duration,
    fallback: bool,
) -> MeasurementSource {
    if fallback {
        MeasurementSource::PeakEstimate
    } else if capture_available && has_packet && packet_age <= Duration::from_millis(250) {
        MeasurementSource::Sampled
    } else {
        MeasurementSource::Unavailable
    }
}

fn uses_peak_fallback(capture_available: bool, packet_age: Duration, peak: f32) -> bool {
    !capture_available || (packet_age > Duration::from_millis(250) && peak > 0.001)
}
fn log_effective_route(mode: &str, source: &str, output: &str, auto_route: bool) {
    // Explicit start/rebind boundary only; failure is diagnostic and must not
    // change routing or prevent audio from starting.
    match core_audio::default_id(1) {
        Ok(default) => log::info!("Audio effective route: mode={mode}, capture_id={source:?}, output_id={output:?}, auto_route={auto_route}, Windows_multimedia_default={default:?}"),
        Err(error) => log::warn!("Audio effective route: mode={mode}, capture_id={source:?}, output_id={output:?}, auto_route={auto_route}, Windows multimedia default query unavailable: {error:#}"),
    }
    log::info!("Capture observations describe decoded packets, not audibility. No packets or zero-valued packets alone do not establish a device fault; measurement=PeakEstimate is not sampled audio.");
}
fn run_system(
    s: &Settings,
    data: &Path,
    controls: &Controls,
    store: &Mutex<AudioSnapshot>,
    events: &Sender<AudioEvent>,
) -> Result<()> {
    let devices = core_audio::devices()?;
    let defaults: Vec<_> = if s.auto_route {
        (0..3).map(core_audio::default_id).collect::<Result<_>>()?
    } else if s.output_id.is_empty() {
        vec![String::new(), core_audio::default_id(1)?]
    } else { Vec::new() };
    let id = audio_routing::resolve_output(s, &devices, defaults.get(1).map(String::as_str))?;
    let routes = audio_routing::windows_routes(s, &devices, &defaults, &id)?;
    let mut ep = Endpoint::new(&id)?;
    let mut original = ep.scalar()?;
    let mut recovery = RecoveryGuard::new(
        RecoveryState {
            volume_id: id,
            original_scalar: original,
            ..Default::default()
        },
        data,
    )?;
    let mut capture = CaptureStream::new(&ep.id).ok();
    if let Some(c) = capture.as_mut() {
        c.start()?;
    } else {
        log::warn!("Windows detector: loopback unavailable; using peak/RMS approximation");
    }
    let mut detector = capture
        .as_ref()
        .map(|c| crate::loudness::Detector::new(c.format.rate, c.format.channels));
    let mut samples = Vec::new();
    let mut leveler = dsp::WindowsLeveler::default();
    let (mut min, mut max, mut step) = ep.range()?;
    let mut device_check = Instant::now();
    let mut last = Instant::now();
    let mut last_audio = Instant::now() - Duration::from_secs(1);
    let mut last_packet = Instant::now();
    let mut has_packet = false;
    let mut captured = CaptureObservation { available: capture.is_some(), ..Default::default() };
    let mut last_debug = Instant::now();
    let mut engaged = false;
    let mut was_zero = false;
    for route in routes { recovery.route(route)?; }
    if s.output_id.is_empty() {
        let current = core_audio::default_id(1)?;
        anyhow::ensure!(audio_routing::playback(&devices, &current).is_some() && current == ep.id,
            "{}", audio_routing::guidance(s.english));
    }
    // Retain normalization only after all startup operations succeed. A failed
    // durable commit still rolls the changed roles back to actual originals.
    if !recovery.state.routes.is_empty() { recovery.replace_routes(Vec::new())?; }
    log::info!(
        "Windows shared-scale leveler started: device={}, target={}, loopback={}",
        ep.id,
        s.target,
        capture.is_some()
    );
    log_effective_route("Windows", &ep.id, &ep.id, s.auto_route);
    if audio_routing::playback(&devices, &ep.id).is_some_and(|d| d.verified_physical) {
        let _ = events.send(AudioEvent::PhysicalOutput(ep.id.clone()));
    }
    let _ = events.send(AudioEvent::Started);
    while !controls.stop.load(Ordering::Relaxed) {
        let dt = last.elapsed().as_secs_f64().clamp(0.001, 0.1);
        last = Instant::now();
        let intensity = controls.intensity.load(Ordering::Relaxed);
        if s.output_id.is_empty() && device_check.elapsed() > Duration::from_secs(1) {
            device_check = Instant::now();
            let id = core_audio::default_id(1)?;
            if id != ep.id {
                let devices = core_audio::devices()?;
                // A new cable default has no forwarder in Windows mode. Stop
                // before opening/actuating it; a fresh Run can normalize safely.
                anyhow::ensure!(audio_routing::playback(&devices, &id).is_some(), "{}", audio_routing::guidance(s.english));
                recovery.restore()?;
                ep = Endpoint::new(&id)?;
                original = ep.scalar()?;
                recovery = RecoveryGuard::new(
                    RecoveryState {
                        volume_id: id,
                        original_scalar: original,
                        ..Default::default()
                    },
                    data,
                )?;
                capture = CaptureStream::new(&ep.id).ok();
                if let Some(c) = capture.as_mut() {
                    c.start()?;
                }
                detector = capture
                    .as_ref()
                    .map(|c| crate::loudness::Detector::new(c.format.rate, c.format.channels));
                leveler = dsp::WindowsLeveler::default();
                (min, max, step) = ep.range()?;
                engaged = false;
                last_packet = Instant::now();
                has_packet = false;
                captured = CaptureObservation { available: capture.is_some(), ..Default::default() };
                log::info!("Following new default output: {}", ep.id);
                log_effective_route("Windows", &ep.id, &ep.id, s.auto_route);
                if audio_routing::playback(&devices, &ep.id).is_some_and(|d| d.verified_physical) {
                    let _ = events.send(AudioEvent::PhysicalOutput(ep.id.clone()));
                }
            }
        }
        let mut measured = false;
        if let Some(c) = capture.as_ref() {
            while c.next_packet()? > 0 {
                let frames = c.read(&mut samples)?;
                captured.record(frames, &samples);
                if let Some(d) = detector.as_mut() {
                    d.block(&samples, c.format.rate, c.format.channels);
                }
                measured = true;
                last_packet = Instant::now();
                has_packet = true;
            }
        }
        let peak = ep.peak()?;
        let muted = ep.muted()?;
        let packet_age = last_packet.elapsed();
        let fallback = uses_peak_fallback(capture.is_some(), packet_age, peak);
        let source = measurement_source(capture.is_some(), has_packet, packet_age, fallback);
        let (rms, level, may_boost, returning) = if fallback {
            let rms = peak as f64 / std::f64::consts::SQRT_2;
            leveler.update_with_intensity(
                rms,
                dt,
                controls.target.load(Ordering::Relaxed),
                controls.speed.load(Ordering::Relaxed),
                intensity,
            );
            (rms, leveler.level, true, false)
        } else if let Some(d) = detector.as_ref() {
            (
                d.rms,
                d.programme.level,
                d.programme.may_boost(),
                d.programme.recovering_after_pause(),
            )
        } else {
            (0., 0., false, false)
        };
        let active = (measured || fallback) && rms > 0.001;
        if active {
            last_audio = Instant::now();
        }
        let target = controls.target.load(Ordering::Relaxed).clamp(0, 100);
        let paused = controls.paused.load(Ordering::Relaxed);
        let leveling = controls.leveling.load(Ordering::Relaxed);
        let mut at_ceiling = false;
        let mut guarded = false;
        if paused {
            if (ep.scalar()? - original).abs() > 0.001 {
                ep.set_scalar(original)?;
            }
            engaged = false;
            leveler.correction = 0.;
        } else if target == 0 {
            if ep.scalar()? > 0. {
                ep.set_scalar(0.)?;
            }
            was_zero = true;
        } else if !leveling {
            let desired = target as f32 / 100.;
            if (ep.scalar()? - desired).abs() > 0.001 {
                ep.set_scalar(desired)?;
            }
            engaged = false;
            was_zero = false;
            leveler.correction = 0.;
        } else {
            if !engaged || was_zero {
                // Target is a common loudness index; native gain is its actuator/ceiling.
                // In silence use the corresponding percentage until actual samples arrive.
                ep.set_scalar(target as f32 / 100.)?;
                leveler.correction = (ep.db()? - max) as f64;
                engaged = true;
                was_zero = false;
            }
            if active && !muted {
                let gain = if fallback {
                    leveler.correction
                } else {
                    leveler.update_measured_with_intensity_after_pause(
                        level,
                        rms,
                        may_boost,
                        returning,
                        dt,
                        target,
                        controls.speed.load(Ordering::Relaxed),
                        intensity,
                    )
                };
                let guard = dsp::target_db(target) + dsp::dynamic_headroom_db(intensity) - dsp::db(rms);
                guarded = gain > guard + 0.25;
                let desired = gain.min(guard);
                at_ceiling = leveler.wanted > 0.25;
                let next = (desired + max as f64).clamp(min as f64, max as f64);
                leveler.constrain((min - max) as f64, 0.0);
                let db = ep.db()? as f64;
                if (db - next).abs() >= (step as f64 / 2.).max(0.1) {
                    ep.set_db(next as f32)?;
                }
            }
        }
        let db = ep.db()?;
        let silent = last_audio.elapsed() > Duration::from_millis(120);
        snapshot(
            store,
            AudioSnapshot {
                meters: MeterReading {
                    input: if silent { 0. } else { rms as f32 },
                    output: if muted || silent || ep.scalar()? == 0. {
                        0.
                    } else {
                        (rms * 10_f64.powf((db - max) as f64 / 20.)) as f32
                    },
                    gain_db: db - max,
                    boost_limited: at_ceiling,
                    limited: guarded,
                    ..Default::default()
                },
                muted,
                silent,
                reference: dsp::amplitude(100) as f32,
                source,
                capture: captured.reading(),
            },
        );
        if last_debug.elapsed() > Duration::from_secs(5) {
            log::debug!("Windows leveler: RMS={rms:.5}, programme={level:.5}, target={target}, native_db={db:.1}, guard={guarded}, ceiling={at_ceiling}, fallback={fallback}");
            last_debug = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }
    recovery.restore()?;
    Ok(())
}
fn run_full(
    s: &Settings,
    data: &Path,
    controls: &Controls,
    store: &Mutex<AudioSnapshot>,
    events: &Sender<AudioEvent>,
) -> Result<()> {
    let devices = core_audio::devices()?;
    let current = if s.output_id.is_empty() { Some(core_audio::default_id(1)?) } else { None };
    let output_id = audio_routing::resolve_output(s, &devices, current.as_deref())?;
    anyhow::ensure!(
        !s.source_id.is_empty(),
        "Install VB-CABLE from Audio components first"
    );
    anyhow::ensure!(
        devices.iter().any(|d| d.id == s.source_id && d.virtual_input),
        "{}", audio_routing::guidance(s.english)
    );
    anyhow::ensure!(
        s.source_id != output_id,
        "Virtual input and playback output must be different devices"
    );
    let mut capture = CaptureStream::new(&s.source_id)?;
    let rate = capture.format.rate;
    let channels = capture.format.channels;
    let mut render = RenderStream::new(&output_id, &capture.format)?;
    let mut dsp = Normalizer::new(rate, channels)?;
    let mut input = Vec::with_capacity(rate * channels);
    let mut output = vec![0.0; render.frames * channels];
    let mut queue = SampleQueue::new(rate / 2 * channels);
    let output_ep = Endpoint::new(&output_id)?;
    let output_original = output_ep.scalar()?;
    let output_max_db = output_ep.range()?.1;
    let defaults = if s.auto_route { (0..3).map(core_audio::default_id).collect::<Result<Vec<_>>>()? } else { Vec::new() };
    let routes = audio_routing::full_routes(s, &defaults);
    let mut recovery = RecoveryGuard::new(
        RecoveryState {
            volume_id: output_id.clone(),
            original_scalar: output_original,
            ..Default::default()
        },
        data,
    )?;
    let mut headroom = false;
    if controls.leveling.load(Ordering::Relaxed) && controls.target.load(Ordering::Relaxed) > 0 {
        output_ep.set_scalar(1.)?;
        headroom = true;
        log::info!("Full output headroom reserved: original_scalar={output_original:.3}, processing_scalar=1.0");
    }
    render.start()?;
    capture.start()?;
    for route in routes { recovery.route(route)?; }
    if s.auto_route {
        anyhow::ensure!(core_audio::default_id(1)? == s.source_id,
            "{}", audio_routing::guidance(s.english));
    }
    let committed = audio_routing::committed_full_routes(&recovery.state.routes, &s.source_id, &output_id);
    if s.auto_route { recovery.replace_routes(committed)?; }
    log::info!(
        "Full boost started: {} Hz, {} channels, source={}, output={}, auto_route={}",
        rate,
        channels,
        s.source_id,
        output_id,
        s.auto_route
    );
    log_effective_route("Full", &s.source_id, &output_id, s.auto_route);
    if audio_routing::playback(&devices, &output_id).is_some_and(|d| d.verified_physical) {
        let _ = events.send(AudioEvent::PhysicalOutput(output_id.clone()));
    }
    let _ = events.send(AudioEvent::Started);
    let clock = Instant::now();
    let mut last_packet = 0.0;
    let mut has_packet = false;
    let mut captured = CaptureObservation::default();
    let mut last_audio = 0.0;
    let mut last_silence = 0.0;
    let mut last_debug = 0.0;
    let mut headroom_check = Instant::now();
    while !controls.stop.load(Ordering::Relaxed) {
        let reserve = controls.leveling.load(Ordering::Relaxed)
            && controls.target.load(Ordering::Relaxed) > 0
            && !controls.paused.load(Ordering::Relaxed);
        if reserve != headroom {
            output_ep.set_scalar(if reserve { 1. } else { output_original })?;
            headroom = reserve;
            log::info!(
                "Full headroom {}, output_scalar={:.3}",
                if reserve { "enabled" } else { "released" },
                output_ep.scalar()?
            );
        }
        if reserve && headroom_check.elapsed() > Duration::from_millis(100) {
            headroom_check = Instant::now();
            if (output_ep.scalar()? - 1.).abs() > 0.001 {
                output_ep.set_scalar(1.)?;
                log::debug!("Output headroom restored after external slider change");
            }
        }
        while capture.next_packet()? > 0 && !controls.stop.load(Ordering::Relaxed) {
            let frames = capture.read(&mut input)?;
            captured.record(frames, &input);
            dsp.process(&mut input, controls.dsp_config());
            queue.add(&input, channels);
            last_packet = clock.elapsed().as_secs_f64();
            has_packet = true;
            if dsp.meters.input > 0.001 {
                last_audio = last_packet;
            }
        }
        queue.trim(rate / 10 * channels, channels);
        let available = render.available()?;
        let now = clock.elapsed().as_secs_f64();
        if now - last_packet > 0.03
            && now - last_silence > 0.01
            && queue.len() < rate / 50 * channels
        {
            input.resize(rate / 100 * channels, 0.0);
            input.fill(0.0);
            dsp.process(&mut input, controls.dsp_config());
            queue.add(&input, channels);
            last_silence = now;
        }
        let frames = available.min(queue.len() / channels);
        if frames > 0 {
            let length = frames * channels;
            queue.take(&mut output[..length]);
            render.write(&output[..length])?;
        }
        let silent = now - last_audio > 0.3;
        let mut meters = dsp.meters;
        if silent {
            meters.input = 0.0;
            meters.output = 0.0;
        }
        let muted = output_ep.muted()?;
        let master_db = output_ep.db()? - output_max_db;
        let master_gain = 10_f32.powf(master_db / 20.);
        meters.output *= master_gain;
        meters.gain_db += master_db;
        if muted {
            meters.output = 0.;
        }
        snapshot(
            store,
            AudioSnapshot {
                meters,
                muted,
                silent,
                reference: 0.0,
                source: measurement_source(
                    true,
                    has_packet,
                    Duration::from_secs_f64(now - last_packet),
                    false,
                ),
                capture: captured.reading(),
            },
        );
        if now - last_debug > 5.0 {
            log::debug!("Full boost meter: in={:.4}, out={:.4}, gain={:.1} dB, limiter={}, sibilance_cut={:.1} dB, profile={}, dropped_samples={}", meters.input, meters.output, meters.gain_db, meters.limited, meters.sibilance_db, controls.profile.load(Ordering::Relaxed), queue.dropped);
            last_debug = now;
        }
        thread::sleep(Duration::from_millis(3));
    }
    recovery.restore()?;
    Ok(())
}

#[cfg(test)]
mod source_tests {
    use super::*;

    /// Owns two recovery markers around a deliberately staged cable default.
    /// Normal shutdown precedes fixture rollback. Driver hangs remain subject
    /// to AudioEngine's bounded shutdown timeout, as in the production app.
    #[test]
    #[ignore = "opt-in native default routing roundtrip; requires MAXI_ROUTE_EVIDENCE_DIR"]
    fn native_windows_cable_normalization_roundtrip() -> Result<()> {
        native_cable_routing_roundtrip(0)
    }

    #[test]
    #[ignore = "opt-in native Full routing roundtrip; run serially with MAXI_ROUTE_EVIDENCE_DIR"]
    fn native_full_cable_routing_roundtrip() -> Result<()> {
        native_cable_routing_roundtrip(1)
    }

    // Exact-name invocations run serially: both cases temporarily change the
    // host's multimedia default and must never execute concurrently.
    fn native_cable_routing_roundtrip(mode: i32) -> Result<()> {
        let _com = ComScope::new()?;
        let mode_name = if mode == 0 { "Windows" } else { "Full" };
        let directory = PathBuf::from(std::env::var_os("MAXI_ROUTE_EVIDENCE_DIR")
            .context("Set MAXI_ROUTE_EVIDENCE_DIR for native routing evidence")?)
            .join(format!("{}-roundtrip-{}-{}", mode_name.to_lowercase(), std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis()));
        std::fs::create_dir_all(&directory)?;
        let devices = core_audio::devices()?;
        let initial: Vec<_> = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
        let output = audio_routing::playback(&devices, &initial[1])
            .filter(|device| device.verified_physical)
            .context("Native routing fixture requires a verified physical multimedia default")?;
        let cable = devices.iter().find(|device| device.vb_cable && initial.contains(&device.id))
            .or_else(|| devices.iter().find(|device| device.vb_cable))
            .context("Native routing fixture requires installed VB-CABLE")?;
        let endpoint = Endpoint::new(&output.id)?;
        let scalar = endpoint.scalar()?;
        let muted = endpoint.muted()?;
        let mut started_defaults = Vec::new();
        let mut stopped_defaults = Vec::new();
        let result = (|| -> Result<()> {
            // A guard for the deliberate physical -> cable staging operation.
            let mut staging = RecoveryGuard::new(RecoveryState::default(), &directory.join("staging"))?;
            // Successful Windows normalization and Full's orphan-cable policy
            // retain physical defaults. Restore the host's prior cable roles
            // separately, without treating them as a new user preference.
            let cleanup_routes = initial.iter().enumerate().filter(|(_, id)| **id == cable.id)
                .map(|(role, id)| core_audio::RouteRestore {
                    role: role as i32, original_id: id.clone(), expected_id: output.id.clone(),
                }).collect();
            let mut cleanup = RecoveryGuard::new(RecoveryState::default(), &directory.join("cleanup"))?;
            // No engine exists yet: a failed preflight must not claim a role
            // that another application changed to the physical output.
            cleanup.restored = true;
            let mut engine = AudioEngine::new();
            for (role, original) in initial.iter().enumerate() {
                anyhow::ensure!(core_audio::default_id(role as i32)? == *original,
                    "Host defaults changed before the routing fixture started");
            }
            anyhow::ensure!(staging.route(core_audio::RouteRestore {
                role: 1, original_id: initial[1].clone(), expected_id: cable.id.clone(),
            })?, "Host default changed while staging the cable");
            let settings = Settings {
                mode, target: 0, leveling: false, auto_route: true,
                source_id: cable.id.clone(), last_physical_output_id: output.id.clone(),
                ..Default::default()
            };
            let engine_data = directory.join("engine");
            cleanup.replace_routes(cleanup_routes)?;
            if let Err(error) = engine.start(settings.clone(), engine_data.clone()) {
                cleanup.replace_routes(Vec::new())?;
                return Err(error);
            }
            cleanup.restored = false;
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut events = Vec::new();
            loop {
                events.extend(engine.poll());
                anyhow::ensure!(!events.iter().any(|event| matches!(event,
                    AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_) | AudioEvent::RestoreWarning(_))),
                    "{mode_name} routing failed: {events:?}");
                if events.iter().any(|event| matches!(event, AudioEvent::Started)) { break; }
                anyhow::ensure!(Instant::now() < deadline, "{mode_name} routing did not start: {events:?}");
                thread::sleep(Duration::from_millis(20));
            }
            anyhow::ensure!(events.iter().any(|event| matches!(event,
                AudioEvent::PhysicalOutput(id) if id == &output.id)), "Verified physical binding was not published");
            thread::sleep(Duration::from_millis(100));
            let expected: Vec<_> = initial.iter().enumerate().map(|(role, id)| {
                if role == 1 || id == &cable.id { output.id.clone() } else { id.clone() }
            }).collect();
            started_defaults = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
            if mode == 0 {
                anyhow::ensure!(started_defaults == expected, "Windows did not normalize exactly the selected cable roles");
                anyhow::ensure!(endpoint.scalar()? < 0.001, "Windows target zero was not applied");
            } else {
                anyhow::ensure!(started_defaults == vec![cable.id.clone(); 3], "Full did not route all three roles to the selected cable");
                anyhow::ensure!((endpoint.scalar()? - scalar).abs() < 0.001 && endpoint.muted()? == muted,
                    "Full target zero changed physical volume or mute");
            }
            anyhow::ensure!(!engine.poll().iter().any(|event| matches!(event,
                AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_) | AudioEvent::RestoreWarning(_))),
                "{mode_name} worker failed after starting");
            anyhow::ensure!(settings.output_id.is_empty(), "System default intent changed");
            engine.shutdown()?;
            let stopped_events = engine.poll();
            anyhow::ensure!(stopped_events.iter().any(|event| matches!(event, AudioEvent::Stopped))
                && !stopped_events.iter().any(|event| matches!(event,
                    AudioEvent::Error(_) | AudioEvent::DeviceInvalidated(_) | AudioEvent::RestoreWarning(_))),
                "{mode_name} worker did not stop cleanly: {stopped_events:?}");
            stopped_defaults = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
            anyhow::ensure!(stopped_defaults == expected, "{mode_name} Stop did not apply its committed restoration policy");
            anyhow::ensure!((endpoint.scalar()? - scalar).abs() < 0.001 && endpoint.muted()? == muted,
                "{mode_name} Stop did not restore volume and mute");
            anyhow::ensure!(!engine_data.join("recovery.json").exists(), "{mode_name} recovery marker remains");
            cleanup.restore()?;
            staging.restore()?;
            Ok(())
        })();
        let restored: Vec<_> = (0..3).map(core_audio::default_id).collect::<Result<_>>()?;
        let volume_restored = (endpoint.scalar()? - scalar).abs() < 0.001 && endpoint.muted()? == muted;
        crate::settings::write_json(&directory.join("result.json"), &serde_json::json!({
            "status": if result.is_ok() && restored == initial && volume_restored { "passed" } else { "failed" },
            "error": result.as_ref().err().map(|error| format!("{error:#}")),
            "mode": mode_name,
            "scope": "Actual audio engine, staged selected VB-CABLE multimedia default, remembered verified physical fallback, mode-specific start and Stop role policy, exact fixture baseline restoration; configured target zero, no injected audio, no listening or hardware-amplitude assessment",
            "initial_defaults": initial, "started_defaults": started_defaults, "stopped_defaults": stopped_defaults,
            "restored_defaults": restored, "volume_and_mute_restored": volume_restored,
            "output_id": output.id, "selected_cable": cable.id,
        }))?;
        eprintln!("{mode_name} routing roundtrip evidence: {}", directory.join("result.json").display());
        result?;
        anyhow::ensure!(restored == initial && volume_restored, "Routing fixture did not restore its exact baseline");
        Ok(())
    }

    #[test]
    fn typed_device_invalidation_survives_context_and_preserves_raw_diagnostics() {
        let error = anyhow::Error::new(windows::core::Error::from_hresult(
            windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED))
            .context("capture or render operation").context("audio worker");
        let expected = format!("{error:#}");
        match engine_error_event(&error) {
            AudioEvent::DeviceInvalidated(raw) => assert_eq!(raw, expected),
            other => panic!("Typed invalidation must retain its event kind: {other:?}"),
        }
    }

    #[test]
    fn other_hresult_and_literal_code_remain_generic_errors() {
        for error in [
            anyhow::Error::new(windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32)))
                .context("different Windows failure"),
            anyhow::anyhow!("Unrelated text containing 0x88890004"),
        ] {
            let expected = format!("{error:#}");
            match engine_error_event(&error) {
                AudioEvent::Error(raw) => assert_eq!(raw, expected),
                other => panic!("Only the exact typed HRESULT may be mapped: {other:?}"),
            }
        }
    }

    fn observed(capture: &CaptureObservation, age_ms: u64) -> AudioSnapshot {
        let age = Duration::from_millis(age_ms);
        AudioSnapshot {
            source: measurement_source(true, capture.packets > 0, age, false),
            capture: capture.reading_at(age),
            ..Default::default()
        }
    }

    #[test]
    fn capture_observation_separates_absent_zero_nonfinite_and_raw_nonzero() {
        let mut capture = CaptureObservation::default();
        assert_eq!(observed(&capture, 1000).capture.state, CaptureState::AwaitingPackets);
        capture.record_at(Duration::ZERO, 0, &[1.0]);
        assert_eq!(capture.packets, 0);
        // Synthetic zero padding has no capture read and never calls record.
        assert_eq!(observed(&capture, 1000).source, MeasurementSource::Unavailable);
        capture.record_at(Duration::ZERO, 1, &[0.0, -0.0]);
        assert_eq!(observed(&capture, 0).capture.state, CaptureState::ZeroSamples);
        assert_eq!(capture.nonzero_packets, 0);
        assert_eq!(observed(&capture, 0).source, MeasurementSource::Sampled);
        capture.record_at(Duration::ZERO, 1, &[0.0, f32::NAN, f32::INFINITY]);
        assert_eq!(observed(&capture, 0).capture.state, CaptureState::NonfiniteSamples);
        assert_eq!(capture.nonzero_packets, 0);
        // A finite subthreshold sample is packet evidence, not audible signal.
        capture.record_at(Duration::ZERO, 1, &[f32::NAN, f32::MIN_POSITIVE]);
        assert_eq!(observed(&capture, 0).capture.state, CaptureState::NonzeroSamples);
        assert_eq!(capture.nonzero_packets, 1);
        assert_eq!(observed(&capture, 251).capture.state, CaptureState::NoRecentPackets);
        assert_eq!(observed(&capture, 251).source, MeasurementSource::Unavailable);
        capture.record_at(Duration::from_millis(300), 0, &[1.0]);
        assert_eq!(capture.packets, 3);
        assert_eq!(observed(&capture, 301).capture.state, CaptureState::NoRecentPackets);
        let unavailable = CaptureObservation { available: false, ..Default::default() };
        assert_eq!(observed(&unavailable, 0).capture.state, CaptureState::CaptureUnavailable);
    }

    #[test]
    fn diagnostics_report_first_packets_then_coalesce_quiet_transitions() {
        let mut capture = CaptureObservation::default();
        let mut reporter = DiagnosticTransitions::default();
        let mut report = |ms, snapshot: AudioSnapshot| reporter.should_report(Duration::from_millis(ms), &snapshot, false);
        assert!(report(0, observed(&capture, 0)));
        assert!(!report(1, observed(&capture, 0)));
        capture.record_at(Duration::ZERO, 1, &[0.0]);
        assert!(report(10, observed(&capture, 0))); // first real, silent packet
        capture.record_at(Duration::ZERO, 1, &[0.01]);
        assert!(report(20, observed(&capture, 0))); // first finite raw nonzero
        capture.record_at(Duration::ZERO, 1, &[0.0]);
        assert!(!report(30, observed(&capture, 0)));
        capture.record_at(Duration::ZERO, 1, &[0.01]);
        assert!(!report(40, observed(&capture, 0))); // already reported this state
        capture.record_at(Duration::ZERO, 1, &[0.0]);
        assert!(report(1020, observed(&capture, 0))); // last emitted, not last observed
        assert!(!report(1030, observed(&capture, 251)));
        assert!(report(2020, observed(&capture, 251))); // stable loss of recent packets
        capture.record_at(Duration::ZERO, 1, &[0.01]);
        assert!(!report(2030, observed(&capture, 0)));
        assert!(report(3020, observed(&capture, 0))); // recovery survives cooldown
        assert!(!report(5000, observed(&capture, 0))); // no periodic spam
    }

    #[test]
    fn pause_keeps_capture_evidence_but_stop_and_rebind_reset_milestones() {
        let mut capture = CaptureObservation::default();
        let mut reporter = DiagnosticTransitions::default();
        capture.record_at(Duration::ZERO, 1, &[0.01]);
        let value = observed(&capture, 0);
        assert!(reporter.should_report(Duration::ZERO, &value, false));
        assert!(reporter.should_report(Duration::from_millis(10), &value, true));
        capture.record_at(Duration::ZERO, 1, &[0.0]); // capture continues during pause
        let paused = observed(&capture, 0);
        assert_eq!(paused.capture.packets, 2);
        assert_eq!(paused.capture.nonzero_packets, 1);
        assert!(reporter.should_report(Duration::from_millis(20), &paused, false));
        let store = Mutex::new(paused);
        snapshot(&store, AudioSnapshot::default());
        let stopped = *store.lock().unwrap();
        assert_eq!(stopped.capture.packets, 0);
        assert!(!reporter.should_report(Duration::from_millis(30), &stopped, false));
        let mut next = CaptureObservation::default();
        next.binding = capture.binding + Duration::from_secs(1);
        assert!(reporter.should_report(Duration::from_millis(40), &observed(&next, 0), false));
        next.record_at(Duration::ZERO, 1, &[0.01]);
        assert!(reporter.should_report(Duration::from_millis(50), &observed(&next, 0), false));
        // Windows follows another default without a stopped snapshot in between.
        let rebound = CaptureObservation { binding: next.binding + Duration::from_secs(1), ..Default::default() };
        assert!(reporter.should_report(Duration::from_millis(60), &observed(&rebound, 0), false));
        assert_eq!(observed(&rebound, 0).capture.state, CaptureState::AwaitingPackets);
    }

    fn classify(capture: bool, packet: bool, age_ms: u64, peak: f32) -> MeasurementSource {
        let age = Duration::from_millis(age_ms);
        measurement_source(capture, packet, age, uses_peak_fallback(capture, age, peak))
    }

    #[test]
    fn windows_capture_source_transitions() {
        assert_eq!(classify(false, false, 0, 0.0), MeasurementSource::PeakEstimate);
        assert_eq!(classify(true, false, 0, 0.0), MeasurementSource::Unavailable);
        assert_eq!(classify(true, false, 250, 0.5), MeasurementSource::Unavailable);
        assert_eq!(classify(true, true, 0, 0.0), MeasurementSource::Sampled);
        assert_eq!(classify(true, true, 250, 0.0), MeasurementSource::Sampled);
        assert_eq!(classify(true, true, 251, 0.001), MeasurementSource::Unavailable);
        assert_eq!(classify(true, true, 251, 0.0011), MeasurementSource::PeakEstimate);
        assert_eq!(classify(true, true, 0, 0.0), MeasurementSource::Sampled);
    }

    #[test]
    fn silence_mute_and_reset_do_not_change_source_meaning() {
        let silent_sample = AudioSnapshot {
            silent: true,
            muted: true,
            source: classify(true, true, 0, 0.0),
            ..Default::default()
        };
        assert_eq!(silent_sample.source, MeasurementSource::Sampled);
        assert!(silent_sample.silent && silent_sample.muted);
        assert_eq!(AudioSnapshot::default().source, MeasurementSource::Unavailable);
        assert_eq!(classify(true, false, 0, 0.0), MeasurementSource::Unavailable);
    }
}
