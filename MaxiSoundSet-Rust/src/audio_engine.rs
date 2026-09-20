use crate::{
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
#[derive(Clone, Copy, Default)]
pub struct AudioSnapshot {
    pub meters: MeterReading,
    pub muted: bool,
    pub silent: bool,
    pub reference: f32,
}
#[derive(Debug)]
pub enum AudioEvent {
    Started,
    Stopped,
    Error(String),
    RestoreWarning(String),
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
            if let Err(e) = result { log::error!("Audio engine error: {e:#}"); let _ = events.send(AudioEvent::Error(format!("{e:#}"))); }
            if data.join("recovery.json").exists() { let _ = events.send(AudioEvent::RestoreWarning("Audio restoration is incomplete. Reconnect the previous device or select speakers in Windows sound settings.".into())); }
            *snapshot.lock().unwrap_or_else(|p| p.into_inner()) = AudioSnapshot::default(); let _ = events.send(AudioEvent::Stopped); log::info!("Audio engine stopped");
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
    *store.lock().unwrap_or_else(|p| p.into_inner()) = value;
}
fn run_system(
    s: &Settings,
    data: &Path,
    controls: &Controls,
    store: &Mutex<AudioSnapshot>,
    events: &Sender<AudioEvent>,
) -> Result<()> {
    let id = if s.output_id.is_empty() {
        core_audio::default_id(1)?
    } else {
        s.output_id.clone()
    };
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
    let mut last_debug = Instant::now();
    let mut engaged = false;
    let mut was_zero = false;
    log::info!(
        "Windows shared-scale leveler started: device={}, target={}, loopback={}",
        ep.id,
        s.target,
        capture.is_some()
    );
    let _ = events.send(AudioEvent::Started);
    while !controls.stop.load(Ordering::Relaxed) {
        let dt = last.elapsed().as_secs_f64().clamp(0.001, 0.1);
        last = Instant::now();
        if s.output_id.is_empty() && device_check.elapsed() > Duration::from_secs(1) {
            device_check = Instant::now();
            let id = core_audio::default_id(1)?;
            if id != ep.id {
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
                log::info!("Following new default output: {}", ep.id);
            }
        }
        let mut measured = false;
        if let Some(c) = capture.as_ref() {
            while c.next_packet()? > 0 {
                c.read(&mut samples)?;
                if let Some(d) = detector.as_mut() {
                    d.block(&samples, c.format.rate, c.format.channels);
                }
                measured = true;
                last_packet = Instant::now();
            }
        }
        let peak = ep.peak()?;
        let muted = ep.muted()?;
        let fallback = capture.is_none()
            || (last_packet.elapsed() > Duration::from_millis(250) && peak > 0.001);
        let (rms, level, may_boost) = if fallback {
            let rms = peak as f64 / std::f64::consts::SQRT_2;
            leveler.update_with_intensity(
                rms,
                dt,
                controls.target.load(Ordering::Relaxed),
                controls.speed.load(Ordering::Relaxed),
                controls.intensity.load(Ordering::Relaxed),
            );
            (rms, leveler.level, true)
        } else if let Some(d) = detector.as_ref() {
            (d.rms, d.programme.level, d.programme.may_boost())
        } else {
            (0., 0., false)
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
                    leveler.update_measured_with_intensity(
                        level,
                        rms,
                        may_boost,
                        dt,
                        target,
                        controls.speed.load(Ordering::Relaxed),
                        controls.intensity.load(Ordering::Relaxed),
                    )
                };
                let guard = dsp::target_db(target) + 3. - dsp::db(rms);
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
    anyhow::ensure!(
        !s.source_id.is_empty(),
        "Install VB-CABLE from Audio components first"
    );
    anyhow::ensure!(
        !s.output_id.is_empty(),
        "Select a real playback device for full boost"
    );
    anyhow::ensure!(
        s.source_id != s.output_id,
        "Virtual input and playback output must be different devices"
    );
    let mut capture = CaptureStream::new(&s.source_id)?;
    let rate = capture.format.rate;
    let channels = capture.format.channels;
    let mut render = RenderStream::new(&s.output_id, &capture.format)?;
    let mut dsp = Normalizer::new(rate, channels)?;
    let mut input = Vec::with_capacity(rate * channels);
    let mut output = vec![0.0; render.frames * channels];
    let mut queue = SampleQueue::new(rate / 2 * channels);
    let output_ep = Endpoint::new(&s.output_id)?;
    let output_original = output_ep.scalar()?;
    let output_max_db = output_ep.range()?.1;
    let mut originals = Vec::new();
    if s.auto_route {
        for role in 0..3 {
            let id = core_audio::default_id(role)?;
            originals.push(if id == s.source_id {
                s.output_id.clone()
            } else {
                id
            });
        }
    }
    let mut recovery = RecoveryGuard::new(
        RecoveryState {
            volume_id: s.output_id.clone(),
            original_scalar: output_original,
            source_id: if s.auto_route {
                s.source_id.clone()
            } else {
                String::new()
            },
            original_defaults: originals,
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
    if s.auto_route {
        for role in 0..3 {
            core_audio::set_default(&s.source_id, role)?;
        }
    }
    log::info!(
        "Full boost started: {} Hz, {} channels, source={}, output={}, auto_route={}",
        rate,
        channels,
        s.source_id,
        s.output_id,
        s.auto_route
    );
    let _ = events.send(AudioEvent::Started);
    let clock = Instant::now();
    let mut last_packet = 0.0;
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
            capture.read(&mut input)?;
            dsp.process(&mut input, controls.dsp_config());
            queue.add(&input, channels);
            last_packet = clock.elapsed().as_secs_f64();
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
