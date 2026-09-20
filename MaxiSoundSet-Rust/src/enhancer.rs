//! Stereo-linked dynamic sibilance reduction and smoothly switched tonal profiles.
//! Biquad equations follow the RBJ Audio EQ Cookbook (W3C Working Group Note).
use std::f64::consts::PI;

#[derive(Clone, Copy)]
struct Coefficients {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}
impl Coefficients {
    fn normalized(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
    fn band(rate: f64, frequency: f64, q: f64) -> Self {
        let w = 2.0 * PI * frequency / rate;
        let a = w.sin() / (2.0 * q);
        Self::normalized(a, 0.0, -a, 1.0 + a, -2.0 * w.cos(), 1.0 - a)
    }
    fn peak(rate: f64, frequency: f64, q: f64, gain: f64) -> Self {
        let w = 2.0 * PI * frequency.min(rate * 0.4) / rate;
        let alpha = w.sin() / (2.0 * q);
        let a = 10.0_f64.powf(gain / 40.0);
        Self::normalized(
            1.0 + alpha * a,
            -2.0 * w.cos(),
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * w.cos(),
            1.0 - alpha / a,
        )
    }
}
#[derive(Clone, Copy, Default)]
struct Filter {
    z1: f64,
    z2: f64,
}
impl Filter {
    fn tick(&mut self, x: f64, c: Coefficients) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        // Flush vanishing filter tails to avoid denormal CPU spikes.
        if self.z1.abs() < 1e-25 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-25 {
            self.z2 = 0.0;
        }
        y
    }
}

pub const EQ_FREQUENCIES: [f64; 10] = [
    31.5, 63., 125., 250., 500., 1000., 2000., 4000., 8000., 16000.,
];
const IDENTITY: Coefficients = Coefficients {
    b0: 1.,
    b1: 0.,
    b2: 0.,
    a1: 0.,
    a2: 0.,
};

pub struct Enhancer {
    rate: f64,
    custom: [Coefficients; 10],
    custom_target: [Coefficients; 10],
    custom_filters: Vec<[Filter; 10]>,
    custom_gains: [f32; 10],
    coefficient_fade: f64,
    channels: usize,
    profiles: [[Coefficients; 3]; 7],
    eq: Vec<[[Filter; 3]; 7]>,
    detector: Vec<Filter>,
    band: Coefficients,
    weights: [f64; 8],
    fade: f64,
    envelope_attack: f64,
    envelope_release: f64,
    cut_attack: f64,
    cut_release: f64,
    band_energy: f64,
    total_energy: f64,
    cut_db: f64,
    supports_sibilance: bool,
    smoothing_enabled: bool,
    pub reduction_db: f32,
}
impl Enhancer {
    pub fn new(rate: usize, channels: usize) -> Self {
        let r = rate as f64;
        let dt = 1.0 / r;
        // System, Gentle (flat), Voice, Music, Cinema, Warm, Gaming.
        // Broad, modest tonal EQ keeps the presets useful on a whole mix.
        let gains = [
            [0., 0., 0.],
            [0., 0., 0.],
            [-1.5, 2., -1.],
            [1.5, 0., -0.5],
            [2., 1., -1.5],
            [0., -0.5, -3.],
            [-2., 1.5, -0.5],
        ];
        let profiles = gains.map(|g| {
            [
                Coefficients::peak(r, 120., 0.7, g[0]),
                Coefficients::peak(r, 1800., 0.9, g[1]),
                Coefficients::peak(r, 6500., 0.65, g[2]),
            ]
        });
        Self {
            rate: r,
            custom: [IDENTITY; 10],
            custom_target: [IDENTITY; 10],
            custom_filters: vec![[Filter::default(); 10]; channels],
            custom_gains: [0.; 10],
            coefficient_fade: (-dt / 0.04).exp(),
            channels,
            profiles,
            eq: vec![[[Filter::default(); 3]; 7]; channels],
            detector: vec![Filter::default(); channels],
            band: Coefficients::band(r, 7000.0_f64.min(r * 0.35), 0.75),
            weights: [1., 0., 0., 0., 0., 0., 0., 0.],
            fade: (-dt / 0.08).exp(),
            envelope_attack: (-dt / 0.004).exp(),
            envelope_release: (-dt / 0.07).exp(),
            cut_attack: (-dt / 0.008).exp(),
            cut_release: (-dt / 0.12).exp(),
            band_energy: 0.,
            total_energy: 0.,
            cut_db: 0.,
            supports_sibilance: rate >= 16000,
            smoothing_enabled: true,
            reduction_db: 0.,
        }
    }
    pub fn set_treble_smoothing(&mut self, enabled: bool) {
        self.smoothing_enabled = enabled;
    }
    pub fn set_custom(&mut self, gains: [f32; 10]) {
        let gains = gains.map(|g| {
            if g.is_finite() {
                g.clamp(-12., 12.)
            } else {
                0.
            }
        });
        if gains == self.custom_gains {
            return;
        }
        self.custom_gains = gains;
        for i in 0..10 {
            // Bands too close to Nyquist are bypassed, not moved to another frequency.
            self.custom_target[i] = if EQ_FREQUENCIES[i] < self.rate * 0.45 {
                Coefficients::peak(self.rate, EQ_FREQUENCIES[i], 1.41421356237, gains[i] as f64)
            } else {
                IDENTITY
            };
        }
    }
    pub fn process(&mut self, samples: &mut [f32], profile: i32) {
        assert_eq!(samples.len() % self.channels, 0);
        let profile = profile.clamp(0, 7) as usize;
        let caps = [0., 6., 8., 4., 6., 8., 5., 6.];
        let mut bands = [0.0; 8];
        let mut dry = [0.0; 8];
        for frame in samples.chunks_exact_mut(self.channels) {
            // Smooth stable normalized biquad coefficients over 40ms, preserving
            // filter histories. The stable denominator region is convex.
            for (c, t) in self.custom.iter_mut().zip(self.custom_target) {
                let f = self.coefficient_fade;
                c.b0 = t.b0 + (c.b0 - t.b0) * f;
                c.b1 = t.b1 + (c.b1 - t.b1) * f;
                c.b2 = t.b2 + (c.b2 - t.b2) * f;
                c.a1 = t.a1 + (c.a1 - t.a1) * f;
                c.a2 = t.a2 + (c.a2 - t.a2) * f;
            }
            for (i, w) in self.weights.iter_mut().enumerate() {
                *w = if i == profile {
                    1.0 + (*w - 1.0) * self.fade
                } else {
                    *w * self.fade
                };
            }
            if self.weights[profile] > 1.0 - 1e-8 {
                self.weights = [0.; 8];
                self.weights[profile] = 1.;
            }
            let wet = 1.0 - self.weights[0];
            let cap: f64 = self.weights.iter().zip(caps).map(|(w, c)| w * c).sum();
            let mut band_power = 0.;
            let mut total_power = 0.;
            for (channel, s) in frame.iter_mut().enumerate() {
                let x = if s.is_finite() {
                    (*s as f64).clamp(-4., 4.)
                } else {
                    0.
                };
                let mut shaped = x * self.weights[0];
                for p in 1..7 {
                    let mut y = x;
                    for f in 0..3 {
                        y = self.eq[channel][p][f].tick(y, self.profiles[p][f]);
                    }
                    shaped += self.weights[p] * y;
                }
                let mut custom = x;
                for f in 0..10 {
                    custom = self.custom_filters[channel][f].tick(custom, self.custom[f]);
                }
                shaped += self.weights[7] * custom;
                dry[channel] = shaped;
                bands[channel] = self.detector[channel].tick(shaped, self.band);
                total_power += shaped * shaped;
                band_power += bands[channel] * bands[channel];
            }
            total_power /= self.channels as f64;
            band_power /= self.channels as f64;
            let follow = if band_power > self.band_energy {
                self.envelope_attack
            } else {
                self.envelope_release
            };
            self.band_energy = band_power + (self.band_energy - band_power) * follow;
            let follow = if total_power > self.total_energy {
                self.envelope_attack
            } else {
                self.envelope_release
            };
            self.total_energy = total_power + (self.total_energy - total_power) * follow;
            // Trigger on high-frequency dominance, with an absolute floor. Low
            // voiced material alone is left untouched; this is not a treble cut.
            let ratio_db = 10.0
                * (self.band_energy / self.total_energy.max(1e-20))
                    .max(1e-20)
                    .log10();
            let floor = ((10.0 * self.band_energy.max(1e-20).log10() + 55.0) / 10.0).clamp(0., 1.);
            let desired = if self.supports_sibilance && self.smoothing_enabled {
                ((ratio_db + 9.0) * 0.85).clamp(0., cap) * floor
            } else {
                0.
            };
            let follow = if desired > self.cut_db {
                self.cut_attack
            } else {
                self.cut_release
            };
            self.cut_db = desired + (self.cut_db - desired) * follow;
            let band_gain = (10.0_f64.powf(-self.cut_db / 20.0) - 1.0) * wet;
            for (channel, s) in frame.iter_mut().enumerate() {
                *s = (dry[channel] + band_gain * bands[channel]) as f32;
            }
            self.reduction_db = (self.cut_db * wet) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_eq_boosts_and_cuts_selected_band_with_stereo_preserved() {
        let measure = |gain: f32| {
            let mut e = Enhancer::new(48000, 2);
            let mut gains = [0.; 10];
            gains[5] = gain;
            e.set_custom(gains);
            let mut energy = 0.;
            for block in 0..300 {
                let mut audio: Vec<f32> = (0..480)
                    .flat_map(|i| {
                        let x = (0.05 * (2. * PI * 1000. * (block * 480 + i) as f64 / 48000.).sin())
                            as f32;
                        [x, x * 0.37]
                    })
                    .collect();
                e.process(&mut audio, 7);
                if block >= 250 {
                    for f in audio.chunks_exact(2) {
                        energy += (f[0] as f64).powi(2);
                        assert!((f[1] - f[0] * 0.37).abs() < 1e-6);
                    }
                }
            }
            (energy / (50. * 480.)).sqrt()
        };
        let flat = measure(0.);
        assert!((flat - 0.05 / 2.0_f64.sqrt()).abs() < 0.0001);
        assert!((20. * (measure(6.) / flat).log10() - 6.).abs() < 0.15);
        assert!((20. * (measure(-6.) / flat).log10() + 6.).abs() < 0.15);
    }
    #[test]
    fn custom_eq_retuning_is_smooth_and_bypass_restores_dry() {
        let mut e = Enhancer::new(48000, 1);
        let signal = |i: usize| (0.05 * (2. * PI * 1000. * i as f64 / 48000. + 0.47).sin()) as f32;
        let mut warm: Vec<_> = (0..48000).map(signal).collect();
        e.process(&mut warm, 7);
        let mut gains = [0.; 10];
        gains[5] = 12.;
        e.set_custom(gains);
        let mut audio: Vec<_> = (48000..144000).map(signal).collect();
        let dry = audio.clone();
        e.process(&mut audio, 7);
        assert!(
            (audio[0] - dry[0]).abs() < 0.0001,
            "EQ gain jumped on retune"
        );
        assert!(audio.iter().all(|x| x.is_finite() && x.abs() < 0.25));
        let mut bypass: Vec<_> = (144000..240000).map(signal).collect();
        let dry = bypass.clone();
        e.process(&mut bypass, 0);
        assert!(bypass[bypass.len() - 480..]
            .iter()
            .zip(&dry[dry.len() - 480..])
            .all(|(a, b)| (a - b).abs() < 1e-7));
    }
    #[test]
    fn custom_eq_unsupported_high_bands_are_bypassed_and_extremes_stay_finite() {
        let mut low = Enhancer::new(8000, 1);
        let mut gains = [0.; 10];
        gains[9] = 12.;
        low.set_custom(gains);
        let mut samples: Vec<_> = (0..24000)
            .map(|i| (0.05 * (2. * PI * 1000. * i as f64 / 8000.).sin()) as f32)
            .collect();
        let dry = samples.clone();
        low.process(&mut samples, 7);
        assert!(samples.iter().zip(dry).all(|(a, b)| (a - b).abs() < 1e-7));
        for rate in [8000, 16000, 44100, 48000, 96000, 192000, 384000] {
            let mut e = Enhancer::new(rate, 2);
            for step in 0..30 {
                e.set_custom([if step % 2 == 0 { 12. } else { -12. }; 10]);
                let mut s: Vec<_> = (0..(rate / 100) * 2)
                    .map(|i| (0.001 * (i as f64 * 0.03).sin()) as f32)
                    .collect();
                e.process(&mut s, 7);
                assert!(
                    s.iter().all(|x| x.is_finite() && x.abs() < 4.),
                    "unstable at {rate}"
                );
            }
        }
    }

    fn tone(frequency: f64, profile: i32) -> (f64, f32) {
        let mut e = Enhancer::new(48000, 2);
        let mut energy = 0.;
        let mut count = 0;
        for chunk in 0..400 {
            let mut block: Vec<f32> = (0..240)
                .flat_map(|i| {
                    let x = (0.2 * (2. * PI * frequency * (chunk * 240 + i) as f64 / 48000.).sin())
                        as f32;
                    [x, x * 0.37]
                })
                .collect();
            e.process(&mut block, profile);
            if chunk > 300 {
                for frame in block.chunks_exact(2) {
                    energy += (frame[0] as f64).powi(2);
                    count += 1;
                    assert!((frame[1] - frame[0] * 0.37).abs() < 1e-6);
                }
            }
        }
        ((energy / count as f64).sqrt(), e.reduction_db)
    }
    #[test]
    fn gentle_reduces_sibilant_band_but_preserves_voice_body() {
        let (s, cut) = tone(7000., 1);
        let (body, body_cut) = tone(1000., 1);
        assert!(s < 0.09 && cut > 5.0, "s={s}, cut={cut}");
        assert!(
            (body - 0.2 / 2.0_f64.sqrt()).abs() < 0.001 && body_cut < 0.1,
            "body={body}, cut={body_cut}"
        );
    }
    #[test]
    fn system_profile_is_exactly_dry() {
        let mut e = Enhancer::new(48000, 2);
        let mut signal: Vec<f32> = (0..10000)
            .map(|i| ((i as f64 * 0.3).sin() * 0.2) as f32)
            .collect();
        let dry = signal.clone();
        e.process(&mut signal, 0);
        assert_eq!(signal, dry);
    }
    #[test]
    fn profiles_have_different_tonal_balance() {
        let (system, _) = tone(120., 0);
        let (music, _) = tone(120., 3);
        let (voice, _) = tone(1800., 2);
        let (dry_voice, _) = tone(1800., 0);
        assert!(music > system * 1.1 && voice > dry_voice * 1.1);
        let (gaming_bass, _) = tone(120., 6);
        let (warm_upper, _) = tone(6500., 5);
        let (dry_upper, _) = tone(6500., 0);
        assert!(gaming_bass < system * 0.85);
        assert!(warm_upper < dry_upper * 0.6);
    }
    #[test]
    fn treble_smoothing_can_be_disabled_without_bypassing_the_profile() {
        let mut e = Enhancer::new(48000, 1);
        e.set_treble_smoothing(false);
        let mut signal: Vec<f32> = (0..96000)
            .map(|i| {
                (0.1 * (2.0 * PI * 120.0 * i as f64 / 48000.0).sin()
                    + 0.08 * (2.0 * PI * 7000.0 * i as f64 / 48000.0).sin()) as f32
            })
            .collect();
        e.process(&mut signal, 3);
        let bass_rms = (signal[72000..]
            .iter()
            .enumerate()
            .map(|(i, sample)| {
                let reference = (2.0 * PI * 120.0 * (72000 + i) as f64 / 48000.0).sin();
                *sample as f64 * reference
            })
            .sum::<f64>()
            .abs()
            * 2.0
            / 24000.0) as f32;
        assert!(bass_rms > 0.11, "music profile was bypassed: {bass_rms}");
        assert!(e.reduction_db < 0.001, "smoothing remained active");
    }
    #[test]
    fn profile_switches_are_faded_and_bypass_settles_exactly() {
        let mut e = Enhancer::new(48000, 1);
        let mut unchanged = Enhancer::new(48000, 1);
        let signal =
            |i: usize| (0.2 * (2.0 * PI * 1800.0 * i as f64 / 48000.0 + 0.47).sin()) as f32;
        let mut block: Vec<_> = (0..48000).map(signal).collect();
        let mut same = block.clone();
        e.process(&mut block, 4);
        unchanged.process(&mut same, 4);
        let mut next: Vec<_> = (48000..48000 * 3).map(signal).collect();
        let dry = next.clone();
        let mut continuing = next[..240].to_vec();
        unchanged.process(&mut continuing, 4);
        e.process(&mut next, 0);
        assert!((next[0] - continuing[0]).abs() < 0.0001);
        assert!(next[next.len() - 480..]
            .iter()
            .zip(&dry[dry.len() - 480..])
            .all(|(a, b)| (a - b).abs() < 1e-7));
    }
    #[test]
    fn sibilant_overlay_is_reduced_without_dimming_voice_body() {
        let mut e = Enhancer::new(48000, 1);
        let mut signal: Vec<f32> = (0..96000)
            .map(|i| {
                let t = i as f64 / 48000.;
                (0.2 * (2. * PI * 1000. * t).sin() + 0.15 * (2. * PI * 7000. * t).sin()) as f32
            })
            .collect();
        e.process(&mut signal, 1);
        let amplitude = |frequency: f64| {
            let (mut re, mut im) = (0., 0.);
            for (i, s) in signal.iter().enumerate().skip(72000) {
                let a = 2. * PI * frequency * i as f64 / 48000.;
                re += *s as f64 * a.cos();
                im += *s as f64 * a.sin();
            }
            2. * re.hypot(im) / 24000.
        };
        assert!((amplitude(1000.) - 0.2).abs() < 0.01);
        assert!(amplitude(7000.) < 0.115);
    }
    #[test]
    fn short_sibilant_burst_is_softened_and_voice_recovers() {
        let mut e = Enhancer::new(48000, 1);
        // Test a short consonant with the selected profile already active.
        // Turning the whole enhancer on has a separate intentional 80ms fade.
        e.process(&mut vec![0.0; 48000], 1);
        let mut audio: Vec<f32> = (0..24000)
            .map(|i| {
                let t = i as f64 / 48000.;
                let s = if (0.04..0.15).contains(&t) {
                    0.25 * (2. * PI * 7000. * t).sin()
                } else {
                    0.
                };
                (0.2 * (2. * PI * 1000. * t).sin() + s) as f32
            })
            .collect();
        e.process(&mut audio, 1);
        let component = |frequency: f64, start: usize, end: usize| {
            let (mut re, mut im) = (0., 0.);
            for (i, x) in audio.iter().enumerate().take(end).skip(start) {
                let a = 2. * PI * frequency * i as f64 / 48000.;
                re += *x as f64 * a.cos();
                im += *x as f64 * a.sin();
            }
            2. * re.hypot(im) / (end - start) as f64
        };
        assert!(component(7000., 2640, 5040) < 0.17);
        assert!((component(1000., 16800, 21600) - 0.2).abs() < 0.003);
    }
    #[test]
    fn detector_does_not_raise_noise_or_make_silence() {
        let mut e = Enhancer::new(48000, 2);
        let mut zero = vec![0.; 48000];
        e.process(&mut zero, 1);
        assert!(zero.iter().all(|x| *x == 0.));
        let mut invalid = [f32::NAN, f32::INFINITY];
        e.process(&mut invalid, 2);
        assert!(invalid.iter().all(|x| x.is_finite()));
    }
}
