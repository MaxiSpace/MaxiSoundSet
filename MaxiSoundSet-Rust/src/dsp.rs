use std::collections::VecDeque;

pub fn db(amplitude: f64) -> f64 {
    20.0 * amplitude.max(1e-10).log10()
}
pub fn target_db(target: i32) -> f64 {
    let n = target.clamp(1, 200) as f64;
    if n <= 100. {
        -50.0 + n * 0.34
    } else {
        -16.0 + (n - 100.0) * 0.12
    }
}
pub fn amplitude(target: i32) -> f64 {
    if target <= 0 {
        0.0
    } else {
        10.0_f64.powf(target_db(target) / 20.0)
    }
}
pub fn index(amplitude: f64) -> i32 {
    if amplitude < 1e-5 {
        0
    } else {
        let level = db(amplitude);
        let n = if level <= -16.0 {
            (level + 50.0) / 0.34
        } else {
            100.0 + (level + 16.0) / 0.12
        };
        n.round().clamp(0.0, 200.0) as i32
    }
}
pub fn smooth(current: f64, desired: f64, dt: f64, speed: i32) -> f64 {
    let error = desired - current;
    if error.abs() <= 0.25 {
        return current;
    }
    let large = error.abs() > 3.;
    let (down, up, tau_down, tau_up) = match speed {
        0 => (120., 24., 0.025, 0.18),
        2 => (40., 6., 0.09, 0.65),
        _ => (80., 12., 0.045, 0.32),
    };
    let rate = if error < 0. {
        if large {
            down
        } else {
            down * 0.3
        }
    } else {
        if large {
            up
        } else {
            up * 0.3
        }
    };
    let tau = if error < 0. { tau_down } else { tau_up };
    current + (error * (1. - (-dt / tau).exp())).clamp(-rate * dt, rate * dt)
}
/// Blend automatic correction with the original programme dynamics.
/// 0 = High (full correction), 1 = Balanced, 2 = Low.
pub fn steady_correction(wanted: f64, intensity: i32) -> f64 {
    wanted
        * match intensity {
            0 => 1.0,
            2 => 0.4,
            _ => 0.7,
        }
}

pub fn manual_gain_db(target: i32, boost_db: i32) -> f64 {
    if target <= 0 {
        -80.0
    } else {
        (target_db(target.clamp(1, 200)) - target_db(100))
            .clamp(-80.0, boost_db.clamp(0, 36) as f64)
    }
}
pub use crate::loudness::ProgrammeLevel;
#[derive(Default)]
pub struct WindowsLeveler {
    programme: ProgrammeLevel,
    pub reference: f64,
    pub level: f64,
    pub correction: f64,
    pub wanted: f64,
}
impl WindowsLeveler {
    // RMS and target now share the same absolute scale as the virtual-cable worker.
    #[cfg(test)]
    pub fn update(&mut self, rms: f64, dt: f64, target: i32, speed: i32) -> f64 {
        self.update_with_intensity(rms, dt, target, speed, 0)
    }
    pub fn update_with_intensity(
        &mut self,
        rms: f64,
        dt: f64,
        target: i32,
        speed: i32,
        intensity: i32,
    ) -> f64 {
        self.level = self.programme.update(rms, dt);
        self.update_measured_with_intensity(
            self.level,
            rms,
            self.programme.may_boost(),
            dt,
            target,
            speed,
            intensity,
        )
    }
    #[cfg(test)]
    pub fn update_measured(
        &mut self,
        level: f64,
        rms: f64,
        may_boost: bool,
        dt: f64,
        target: i32,
        speed: i32,
    ) -> f64 {
        self.update_measured_with_intensity(level, rms, may_boost, dt, target, speed, 0)
    }
    pub fn update_measured_with_intensity(
        &mut self,
        level: f64,
        rms: f64,
        may_boost: bool,
        dt: f64,
        target: i32,
        speed: i32,
        intensity: i32,
    ) -> f64 {
        self.level = level;
        self.reference = amplitude(100);
        if rms > 0.001 && target > 0 {
            self.wanted = db(amplitude(target.clamp(1, 100))) - db(self.level.max(1e-10));
            let desired = steady_correction(self.wanted, intensity);
            if desired <= self.correction || may_boost {
                self.correction = smooth(self.correction, desired, dt, speed);
            }
        }
        self.correction
    }
    pub fn constrain(&mut self, min: f64, max: f64) {
        self.correction = self.correction.clamp(min, max);
    }
}
#[derive(Clone, Copy)]
pub struct DspConfig {
    pub target: i32,
    pub boost_db: i32,
    pub speed: i32,
    pub intensity: i32,
    pub enabled: bool,
    pub profile: i32,
    pub leveling: bool,
    pub treble_smoothing: bool,
    pub eq_bands: [f32; 10],
}
impl Default for DspConfig {
    fn default() -> Self {
        Self {
            target: 100,
            boost_db: 24,
            speed: 1,
            intensity: 0,
            enabled: true,
            profile: 1,
            leveling: true,
            treble_smoothing: true,
            eq_bands: [0.; 10],
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct MeterReading {
    pub input: f32,
    pub output: f32,
    pub gain_db: f32,
    pub limited: bool,
    pub boost_limited: bool,
    pub sibilance_db: f32,
}

pub struct Normalizer {
    rate: usize,
    channels: usize,
    delay_frames: usize,
    delay: Vec<f32>,
    energies: Vec<f64>,
    peak_queue: VecDeque<(u64, f64)>,
    frame: u64,
    position: usize,
    energy_position: usize,
    energy_sum: f64,
    gain_db: f64,
    limiter_gain: f64,
    output_energy: f64,
    detector: crate::loudness::Detector,
    output_detector: crate::loudness::Detector,
    noise_boost: f64,
    enhancer: crate::enhancer::Enhancer,
    pub meters: MeterReading,
}
impl Normalizer {
    pub fn new(rate: usize, channels: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (8000..=384000).contains(&rate) && (1..=8).contains(&channels),
            "Unsupported audio format: {rate} Hz / {channels} channels"
        );
        let delay_frames = (rate / 200).max(1);
        Ok(Self {
            rate,
            channels,
            delay_frames,
            delay: vec![0.0; delay_frames * channels],
            energies: vec![0.0; (rate / 50).max(1)],
            peak_queue: VecDeque::with_capacity(delay_frames + 2),
            frame: 0,
            position: 0,
            energy_position: 0,
            energy_sum: 0.0,
            gain_db: 0.0,
            limiter_gain: 1.0,
            output_energy: 0.0,
            detector: crate::loudness::Detector::new(rate, channels),
            output_detector: crate::loudness::Detector::new(rate, channels),
            noise_boost: 0.0,
            enhancer: crate::enhancer::Enhancer::new(rate, channels),
            meters: MeterReading::default(),
        })
    }
    pub fn process(&mut self, samples: &mut [f32], config: DspConfig) {
        assert_eq!(samples.len() % self.channels, 0, "Unaligned audio packet");
        self.enhancer.set_custom(config.eq_bands);
        self.enhancer
            .set_treble_smoothing(config.enabled && config.treble_smoothing);
        self.enhancer
            .process(samples, if config.enabled { config.profile } else { 0 });
        self.meters.sibilance_db = self.enhancer.reduction_db;
        let target = config.target.clamp(0, 200);
        let target_amp = amplitude(target);
        let boost = config.boost_db.clamp(0, 36) as f64;
        let dt = 1.0 / self.rate as f64;
        let release = (-dt / 0.25).exp();
        let out_smooth = (-dt / 0.06).exp();
        self.meters.limited = false;
        self.meters.boost_limited = false;
        for input in samples.chunks_exact_mut(self.channels) {
            let mut power = 0.0;
            for s in input.iter_mut() {
                if !s.is_finite() {
                    *s = 0.0;
                }
                *s = s.clamp(-4.0, 4.0);
                power += (*s as f64).powi(2);
            }
            power /= self.channels as f64;
            self.energy_sum += power - self.energies[self.energy_position];
            self.energies[self.energy_position] = power;
            self.energy_position = (self.energy_position + 1) % self.energies.len();
            let rms = (self.energy_sum.max(0.0)
                / (self.energies.len() as u64).min(self.frame + 1) as f64)
                .sqrt();
            let level = self.detector.frame(input, dt);
            let desired = if !config.enabled {
                0.0
            } else if !config.leveling {
                let wanted = target_db(target.max(1)) - target_db(100);
                self.meters.boost_limited |= target > 0 && wanted > boost;
                manual_gain_db(target, config.boost_db)
            } else if target > 0
                && rms > 0.001
                && self.detector.rms > 0.001
                && self.frame >= self.energies.len() as u64
            {
                let wanted = steady_correction(target_db(target) - db(level), config.intensity);
                self.meters.boost_limited |= wanted > boost;
                if wanted > self.gain_db && !self.detector.programme.may_boost() {
                    self.gain_db
                } else {
                    wanted.clamp(-80.0, boost)
                }
            } else {
                self.gain_db
            };
            self.gain_db = if config.enabled && config.leveling {
                smooth(self.gain_db, desired, dt, config.speed).clamp(-80.0, boost)
            } else if config.enabled {
                self.gain_db += (desired - self.gain_db) * (1.0 - (-dt / 0.06).exp());
                self.gain_db.clamp(-80.0, boost)
            } else {
                self.gain_db * (-dt / 0.08).exp()
            };
            // Fade out only positive gain on background noise; retain the programme
            // estimate through silence so every new phrase does not restart the AGC.
            let noise_ceiling =
                boost * ((db(rms.min(self.detector.rms)) + 60.0) / 12.0).clamp(0.0, 1.0);
            self.noise_boost += (noise_ceiling - self.noise_boost) * (1.0 - (-dt / 0.045).exp());
            let effective_db = if config.enabled && config.leveling {
                self.gain_db.min(self.noise_boost)
            } else {
                self.gain_db
            };
            let gain = if config.enabled && target == 0 {
                0.0
            } else {
                10.0_f64.powf(effective_db / 20.0)
            };
            let mut peak = 0.0_f64;
            for (c, s) in input.iter_mut().enumerate() {
                let incoming = (*s as f64 * gain) as f32;
                let i = self.position * self.channels + c;
                *s = self.delay[i];
                self.delay[i] = incoming;
                peak = peak.max(incoming.abs() as f64);
            }
            while self
                .peak_queue
                .front()
                .is_some_and(|(time, _)| *time + (self.delay_frames as u64) < self.frame)
            {
                self.peak_queue.pop_front();
            }
            while self
                .peak_queue
                .back()
                .is_some_and(|(_, value)| *value <= peak)
            {
                self.peak_queue.pop_back();
            }
            self.peak_queue.push_back((self.frame, peak));
            let future_peak = self.peak_queue.front().map_or(0.0, |(_, p)| *p);
            let mut needed = if future_peak > 0.944 {
                0.938 / future_peak
            } else {
                1.0
            };
            // A fast RMS guard follows the selected steadying intensity. High keeps
            // sustained overshoots within +3 dB; Balanced and Low intentionally
            // preserve progressively wider dynamics. Peak protection stays fixed.
            if config.enabled && config.leveling && target > 0 && self.detector.rms > 0.001 {
                let dynamic_headroom = match config.intensity {
                    0 => 3.0,
                    2 => 12.0,
                    _ => 7.0,
                };
                needed = needed.min(
                    target_amp * 10.0_f64.powf(dynamic_headroom / 20.0)
                        / (self.detector.rms * gain).max(1e-10),
                );
            }
            self.limiter_gain = if needed < self.limiter_gain {
                needed + (self.limiter_gain - needed) * (-dt / 0.00035).exp()
            } else {
                needed + (self.limiter_gain - needed) * release
            };
            self.meters.limited |= self.limiter_gain < 0.999;
            let mut output_power = 0.0;
            for s in input.iter_mut() {
                *s = if config.enabled && target == 0 {
                    0.0
                } else {
                    (*s as f64 * self.limiter_gain).clamp(-0.944, 0.944) as f32
                };
                output_power += (*s as f64).powi(2);
            }
            self.output_energy = self.output_energy * out_smooth
                + output_power / self.channels as f64 * (1.0 - out_smooth);
            self.meters.input = self.detector.rms as f32;
            self.meters.output = self.output_detector.measure(input) as f32;
            self.meters.gain_db = (effective_db + db(self.limiter_gain.max(1e-10))) as f32;
            self.position = (self.position + 1) % self.delay_frames;
            self.frame += 1;
        }
    }
}

pub struct SampleQueue {
    data: VecDeque<f32>,
    capacity: usize,
    pub dropped: usize,
}
impl SampleQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn add(&mut self, samples: &[f32], channels: usize) {
        if samples.len() > self.capacity {
            self.dropped += samples.len();
            return;
        }
        let overflow = (self.data.len() + samples.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            let drop = overflow.div_ceil(channels) * channels;
            self.data.drain(..drop);
            self.dropped += drop;
        }
        self.data.extend(samples.iter().copied());
    }
    pub fn take(&mut self, output: &mut [f32]) -> usize {
        let actual = output.len().min(self.data.len());
        for s in output.iter_mut() {
            *s = self.data.pop_front().unwrap_or(0.0);
        }
        actual
    }
    pub fn trim(&mut self, maximum: usize, channels: usize) {
        let drop = self.data.len().saturating_sub(maximum) / channels * channels;
        if drop > 0 {
            self.data.drain(..drop);
            self.dropped += drop;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(n: &mut Normalizer, config: DspConfig, amp: f64, seconds: f64) -> f64 {
        let rate = n.rate;
        let channels = n.channels;
        let total = (rate as f64 * seconds) as usize;
        let mut energy = 0.0;
        let mut count = 0;
        for start in (0..total).step_by(240) {
            let frames = 240.min(total - start);
            let mut block = vec![0.0; frames * channels];
            for (j, frame) in block.chunks_exact_mut(channels).enumerate() {
                let value = (amp
                    * (2.0 * std::f64::consts::PI * 440.0 * (start + j) as f64 / rate as f64).sin())
                    as f32;
                frame.fill(value);
            }
            n.process(&mut block, config);
            for (j, frame) in block.chunks_exact(channels).enumerate() {
                if start + j > total.saturating_sub(rate / 2) {
                    energy += (frame[0] as f64).powi(2);
                    count += 1;
                }
            }
        }
        (energy / count.max(1) as f64).sqrt()
    }
    #[test]
    fn enhancement_works_with_manual_unity_volume() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig {
            target: 100,
            leveling: false,
            profile: 1,
            ..DspConfig::default()
        };
        let mut block: Vec<f32> = (0..96000)
            .map(|i| {
                (0.25 * (2.0 * std::f64::consts::PI * 7000.0 * i as f64 / 48000.0).sin()) as f32
            })
            .collect();
        n.process(&mut block, config);
        let rms = (block[72000..]
            .iter()
            .map(|s| (*s as f64).powi(2))
            .sum::<f64>()
            / 24000.0)
            .sqrt();
        assert!(
            rms > 0.06 && rms < 0.10,
            "Enhancement was inactive at manual unity volume: {rms}"
        );
        assert!(n.meters.sibilance_db > 5.0 && n.meters.gain_db.abs() < 0.001);
    }
    #[test]
    fn system_profile_does_not_disable_leveling_and_switching_it_off_restores_loudness() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig {
            profile: 0,
            boost_db: 36,
            ..DspConfig::default()
        };
        let out = tone(&mut n, config, 0.01, 16.0);
        assert!(db(out / amplitude(100)).abs() < 0.65);
        let dry = tone(
            &mut n,
            DspConfig {
                leveling: false,
                ..config
            },
            0.2,
            2.0,
        );
        assert!((dry - 0.2 / 2.0_f64.sqrt()).abs() < 0.002);
        assert!(n.meters.sibilance_db == 0.0);
    }
    #[test]
    fn quiet_is_boosted_to_target() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let out = tone(
            &mut n,
            DspConfig {
                boost_db: 36,
                ..Default::default()
            },
            0.01,
            16.0,
        );
        assert!(db(out / amplitude(100)).abs() < 0.65);
        assert!(out > 0.04);
    }
    #[test]
    fn loud_is_reduced_to_target() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let out = tone(&mut n, DspConfig::default(), 0.8, 6.0);
        assert!(db(out / amplitude(100)).abs() < 0.65);
    }
    #[test]
    fn silence_stays_silent_after_tail() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(&mut n, DspConfig::default(), 0.01, 3.0);
        n.process(&mut vec![0.0; 4096], DspConfig::default());
        let mut zero = vec![0.0; 4096];
        n.process(&mut zero, DspConfig::default());
        assert!(zero.iter().all(|s| *s == 0.0));
    }
    #[test]
    fn zero_target_mutes_immediately() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(&mut n, DspConfig::default(), 0.5, 0.1);
        let mut block = vec![0.5; 2000];
        n.process(
            &mut block,
            DspConfig {
                target: 0,
                ..DspConfig::default()
            },
        );
        assert!(block.iter().all(|s| *s == 0.0));
    }
    #[test]
    fn boost_ceiling_is_enforced() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(
            &mut n,
            DspConfig {
                target: 200,
                boost_db: 6,
                ..DspConfig::default()
            },
            0.005,
            5.0,
        );
        assert!(n.meters.gain_db <= 6.001 && n.meters.boost_limited);
    }
    #[test]
    fn limiter_preserves_stereo_during_loud_burst() {
        let mut n = Normalizer::new(48000, 2).unwrap();
        let config = DspConfig {
            target: 200,
            boost_db: 36,
            ..DspConfig::default()
        };
        tone(&mut n, config, 0.004, 8.0);
        let mut block = vec![0.0; 40000];
        for (i, f) in block.chunks_exact_mut(2).enumerate() {
            f[0] = if i % 50 == 0 { 1.0 } else { -0.95 };
            f[1] = f[0] * 0.37;
        }
        n.process(&mut block, config);
        for (i, f) in block.chunks_exact(2).enumerate() {
            assert!(f[0].abs() <= 0.944001 && f[1].abs() <= 0.944001);
            if i >= n.delay_frames {
                assert!((f[1] - f[0] * 0.37).abs() < 1e-6);
            }
        }
    }
    #[test]
    fn noise_floor_does_not_keep_boost() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(&mut n, DspConfig::default(), 0.004, 5.0);
        let out = tone(&mut n, DspConfig::default(), 0.0001, 1.0);
        assert!(out <= 0.0001 / 2.0_f64.sqrt() * 1.05);
    }
    #[test]
    fn invalid_samples_are_sanitized() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let mut s = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0];
        n.process(&mut s, DspConfig::default());
        assert!(s.iter().all(|v| v.is_finite()));
    }
    #[test]
    fn pause_preserves_input_level() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let out = tone(
            &mut n,
            DspConfig {
                enabled: false,
                ..DspConfig::default()
            },
            0.2,
            2.0,
        );
        assert!((out - 0.2 / 2.0_f64.sqrt()).abs() < 0.002);
    }
    #[test]
    fn packet_boundaries_do_not_change_output() {
        let mut a: Vec<f32> = (0..5000)
            .map(|i| (0.05 * (i as f64 * 0.2).sin()) as f32)
            .collect();
        let mut b = a.clone();
        let mut n = Normalizer::new(48000, 1).unwrap();
        let mut m = Normalizer::new(48000, 1).unwrap();
        n.process(&mut a, DspConfig::default());
        for part in b.chunks_mut(127) {
            m.process(part, DspConfig::default());
        }
        assert_eq!(a, b);
    }
    #[test]
    fn queue_survives_overflow_and_underrun() {
        let mut q = SampleQueue::new(10);
        q.add(&[1., 2., 3., 4., 5., 6., 7., 8.], 2);
        q.add(&[9., 10., 11., 12.], 2);
        let mut out = [0.; 12];
        assert_eq!(q.take(&mut out), 10);
        assert_eq!(out[0], 3.);
        assert_eq!(out[9], 12.);
        assert_eq!(out[10], 0.);
    }
    #[test]
    fn custom_eq_works_with_leveling_off_and_peak_protection_on() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let mut gains = [0.; 10];
        gains[5] = 6.;
        let cfg = DspConfig {
            target: 100,
            leveling: false,
            profile: 7,
            eq_bands: gains,
            ..DspConfig::default()
        };
        let mut audio: Vec<f32> = (0..144000)
            .map(|i| (0.05 * (2. * std::f64::consts::PI * 1000. * i as f64 / 48000.).sin()) as f32)
            .collect();
        n.process(&mut audio, cfg);
        let output = (audio[120000..]
            .iter()
            .map(|x| (*x as f64).powi(2))
            .sum::<f64>()
            / 24000.)
            .sqrt();
        assert!(
            (output - 0.05 / 2.0_f64.sqrt() * 2.).abs() < 0.002,
            "output={output}"
        );
        let mut cfg = cfg;
        cfg.eq_bands = [12.; 10];
        tone(&mut n, cfg, 0.8, 2.);
        assert!(
            n.meters.output <= 0.95 && n.meters.limited,
            "limiter did not protect EQ boost"
        );
    }

    #[test]
    fn steadying_intensity_preserves_progressively_more_dynamics() {
        let run = |intensity| {
            let mut n = Normalizer::new(48000, 1).unwrap();
            tone(
                &mut n,
                DspConfig {
                    profile: 0,
                    intensity,
                    ..Default::default()
                },
                0.8,
                6.0,
            )
        };
        let high = db(run(0));
        let balanced = db(run(1));
        let low = db(run(2));
        assert!((high - target_db(100)).abs() < 0.7, "high={high}");
        assert!(
            high < balanced && balanced < low,
            "{high}, {balanced}, {low}"
        );
        assert!(
            low - high > 4.0,
            "Low must retain substantially more dynamics"
        );
    }

    #[test]
    fn manual_volume_and_boost_work_without_steadying() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let input = 0.01 / 2.0_f64.sqrt();
        let boosted = tone(
            &mut n,
            DspConfig {
                target: 200,
                boost_db: 12,
                leveling: false,
                profile: 0,
                ..Default::default()
            },
            0.01,
            2.0,
        );
        assert!(
            (db(boosted / input) - 12.0).abs() < 0.2,
            "boosted={boosted}"
        );
        let mut n = Normalizer::new(48000, 1).unwrap();
        let muted = tone(
            &mut n,
            DspConfig {
                target: 0,
                leveling: false,
                profile: 0,
                ..Default::default()
            },
            0.2,
            0.5,
        );
        assert_eq!(muted, 0.0);
    }

    #[test]
    fn programme_drop_holds_boost_but_releases_for_sustained_quiet() {
        let mut p = ProgrammeLevel::default();
        for _ in 0..200 {
            p.update(0.2, 0.02);
        }
        p.update(0.02, 0.02);
        assert!(!p.may_boost());
        for _ in 0..7 {
            p.update(0.02, 0.02);
        }
        assert!(!p.may_boost(), "Short breaths must not trigger upward gain");
        for _ in 0..60 {
            p.update(0.02, 0.02);
        }
        assert!(
            p.may_boost(),
            "Sustained quiet content must remain boostable"
        );
        p.update(0.3, 0.02);
        assert!(p.may_boost());
    }
    #[test]
    fn windows_leveler_holds_upward_correction_on_short_dip() {
        let mut p = WindowsLeveler::default();
        for _ in 0..200 {
            p.update(0.2, 0.02, 100, 1);
        }
        let before = p.correction;
        for _ in 0..8 {
            p.update(0.02, 0.02, 100, 1);
        }
        assert!((p.correction - before).abs() < 0.001);
        for _ in 0..100 {
            p.update(0.02, 0.02, 100, 1);
        }
        assert!(p.correction > before + 0.5);
    }
    #[test]
    fn sustained_burst_is_guarded_without_clipping_and_recovers_quickly() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig {
            profile: 0,
            boost_db: 36,
            ..Default::default()
        };
        tone(&mut n, config, 0.04, 8.);
        let mut reached = false;
        let mut peak = 0_f32;
        for block_index in 0..160 {
            let start = block_index * 240;
            let mut block: Vec<f32> = (start..start + 240)
                .map(|i| {
                    (0.4 * (2. * std::f64::consts::PI * 440. * i as f64 / 48000.).sin()) as f32
                })
                .collect();
            n.process(&mut block, config);
            peak = peak.max(block.iter().fold(0_f32, |p, s| p.max(s.abs())));
            let error = db(n.meters.output as f64 / amplitude(100));
            if block_index >= 12 {
                assert!(
                    error < 3.6,
                    "Sustained overshoot must not remain above guard: {error}"
                );
            }
            if block_index >= 20 && error.abs() < 1. {
                reached = true;
            }
        }
        assert!(
            peak <= 0.94401 && reached,
            "Burst must recover within800ms without sample clipping"
        );
    }
    #[test]
    fn quiet_step_recovers_promptly_and_subthreshold_noise_gets_no_positive_gain() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig {
            profile: 0,
            boost_db: 36,
            ..Default::default()
        };
        tone(&mut n, config, 0.4, 8.);
        tone(&mut n, config, 0.04, 3.);
        assert!((db(n.meters.output as f64) - target_db(100)).abs() < 1.);
        tone(&mut n, config, 0.0001, 2.);
        assert!(
            n.meters.gain_db <= 0.01,
            "Background noise must not retain positive gain"
        );
    }
    #[test]
    fn processing_modes_converge_to_same_measured_target() {
        let rate = 48000;
        let mut n = Normalizer::new(rate, 1).unwrap();
        let mut detector = crate::loudness::Detector::new(rate, 1);
        let mut windows = WindowsLeveler::default();
        let config = DspConfig {
            profile: 0,
            boost_db: 36,
            ..Default::default()
        };
        for index in 0..600 {
            let start = index * 480;
            let mut packet: Vec<f32> = (start..start + 480)
                .map(|i| {
                    (0.4 * (2. * std::f64::consts::PI * 440. * i as f64 / rate as f64).sin()) as f32
                })
                .collect();
            let level = detector.block(&packet, rate, 1);
            windows.update_measured(
                level,
                detector.rms,
                detector.programme.may_boost(),
                0.01,
                100,
                1,
            );
            windows.constrain(-80., 0.);
            n.process(&mut packet, config);
        }
        let native = detector.rms * 10_f64.powf(windows.correction / 20.);
        assert!((db(native) - target_db(100)).abs() < 0.6);
        assert!(
            (db(n.meters.output as f64 / native)).abs() < 0.4,
            "The two paths must share the same target when native attenuation is sufficient"
        );
    }
    #[test]
    fn full_200_is_above_windows_100_and_full_can_boost_below_native_ceiling() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig {
            profile: 0,
            target: 200,
            boost_db: 36,
            ..Default::default()
        };
        tone(&mut n, config, 0.8, 4.);
        let full = n.meters.output as f64;
        assert!((db(full) - target_db(200)).abs() < 0.6);
        assert!(db(full / amplitude(100)) > 11.0);
        let mut native = WindowsLeveler::default();
        for _ in 0..300 {
            native.update(0.01, 0.02, 100, 1);
            native.constrain(-80., 0.);
        }
        assert!(native.wanted > 20. && native.correction == 0.);
        let boosted = tone(
            &mut n,
            DspConfig {
                target: 100,
                ..config
            },
            0.02,
            5.,
        );
        assert!(
            boosted > 0.13,
            "Digital path must boost input beyond native unity"
        );
    }
    #[test]
    fn all_target_values_round_trip() {
        assert_eq!(amplitude(0), 0.0);
        for i in 1..=200 {
            assert_eq!(index(amplitude(i)), i);
        }
    }
    #[test]
    fn windows_100_targets_same_absolute_level_as_virtual_cable() {
        let mut model = WindowsLeveler::default();
        for _ in 0..1500 {
            model.update(0.5, 0.02, 100, 1);
        }
        assert!((model.reference - amplitude(100)).abs() < 1e-10);
        assert!((model.correction - db(amplitude(100) / 0.5)).abs() < 0.3);
    }
    #[test]
    fn windows_target_uses_shared_scale_and_caps_boost_above_100() {
        let mut model = WindowsLeveler::default();
        for _ in 0..1000 {
            model.update(0.3, 0.02, 50, 1);
        }
        assert!((model.correction - db(amplitude(50) / 0.3)).abs() < 0.3);
        for _ in 0..1500 {
            model.update(0.3, 0.02, 200, 1);
        }
        assert!(
            (model.correction - db(amplitude(100) / 0.3)).abs() < 0.3
                && (model.wanted - db(amplitude(100) / 0.3)).abs() < 0.01,
            "Windows mode must cap target 200 at 100"
        );
    }
    #[test]
    fn windows_ignores_noise_and_bounds_single_transient_response() {
        let mut model = WindowsLeveler::default();
        for _ in 0..300 {
            model.update(0.0001, 0.02, 100, 1);
        }
        assert_eq!(model.correction, 0.0);
        for _ in 0..100 {
            model.update(0.2, 0.02, 100, 1);
        }
        let before = model.correction;
        model.update(1.0, 0.02, 100, 1);
        assert!((model.correction - before).abs() <= 1.61);
        for _ in 0..500 {
            model.update(0.0001, 0.02, 100, 1);
        }
        assert!((model.reference - amplitude(100)).abs() < 1e-8);
    }
    #[test]
    fn short_dynamic_passages_do_not_pump_gain() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        let config = DspConfig::default();
        tone(&mut n, config, 0.08, 8.0);
        let before = n.meters.gain_db;
        tone(&mut n, config, 0.02, 0.1);
        assert!((n.meters.gain_db - before).abs() < 0.25);
        n.process(&mut vec![0.0; 48000], config);
        let stored = n.gain_db;
        tone(&mut n, config, 0.08, 0.1);
        assert!((n.gain_db - stored).abs() < 0.25);
    }
    #[test]
    fn target_changes_ramp_instead_of_jump() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(&mut n, DspConfig::default(), 0.08, 8.0);
        let before = n.meters.gain_db;
        tone(
            &mut n,
            DspConfig {
                target: 200,
                ..DspConfig::default()
            },
            0.08,
            0.1,
        );
        assert!(n.meters.gain_db > before && n.meters.gain_db - before <= 1.21);
    }
    #[test]
    fn unmute_reuses_gain_instead_of_recovering_from_minus_80_db() {
        let mut n = Normalizer::new(48000, 1).unwrap();
        tone(&mut n, DspConfig::default(), 0.08, 8.0);
        let before = n.gain_db;
        tone(
            &mut n,
            DspConfig {
                target: 0,
                ..DspConfig::default()
            },
            0.08,
            5.0,
        );
        assert!((n.gain_db - before).abs() < 0.01);
        let out = tone(&mut n, DspConfig::default(), 0.08, 0.2);
        assert!(out > 0.03);
    }
    #[test]
    fn windows_ceiling_does_not_accumulate_unavailable_boost() {
        let mut model = WindowsLeveler::default();
        for _ in 0..1000 {
            model.update(0.2, 0.02, 100, 1);
            model.constrain(-60.0, 0.0);
        }
        for _ in 0..1000 {
            model.update(0.05, 0.02, 100, 1);
            model.constrain(-60., 0.);
        }
        assert!(model.wanted > 6.0 && model.correction == 0.0);
        for _ in 0..400 {
            model.update(0.8, 0.02, 100, 1);
            model.constrain(-60.0, 0.0);
        }
        assert!(model.correction < -5.0);
    }
}
