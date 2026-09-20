//! Shared K-weighted short-window detector and adaptive programme envelope.
//! BS.1770 coefficient design; channel-normalized proxy, not an integrated LUFS meter.
use std::f64::consts::PI;
#[derive(Clone, Copy)]
struct Coeff {
    b: [f64; 3],
    a: [f64; 2],
}
#[derive(Clone, Copy, Default)]
struct State {
    z1: f64,
    z2: f64,
}
impl State {
    fn tick(&mut self, x: f64, c: Coeff) -> f64 {
        let y = c.b[0] * x + self.z1;
        self.z1 = c.b[1] * x - c.a[0] * y + self.z2;
        self.z2 = c.b[2] * x - c.a[1] * y;
        if self.z1.abs() < 1e-25 {
            self.z1 = 0.;
        }
        if self.z2.abs() < 1e-25 {
            self.z2 = 0.;
        }
        y
    }
}
pub struct Detector {
    coeff: [Coeff; 2],
    states: Vec<[State; 2]>,
    powers: Vec<f64>,
    position: usize,
    count: usize,
    sum: f64,
    pub rms: f64,
    pub programme: ProgrammeLevel,
}
impl Detector {
    pub fn new(rate: usize, channels: usize) -> Self {
        let r = rate as f64;
        let k = (PI * 1681.974450955533 / r).tan();
        let q = 0.7071752369554196;
        let vh = 10_f64.powf(3.999843853973347 / 20.);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1. + k / q + k * k;
        let shelf = Coeff {
            b: [
                (vh + vb * k / q + k * k) / a0,
                2. * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            a: [2. * (k * k - 1.) / a0, (1. - k / q + k * k) / a0],
        };
        let k = (PI * 38.13547087602444 / r).tan();
        let q = 0.5003270373238773;
        let a0 = 1. + k / q + k * k;
        let highpass = Coeff {
            b: [1., -2., 1.],
            a: [2. * (k * k - 1.) / a0, (1. - k / q + k * k) / a0],
        };
        Self {
            coeff: [shelf, highpass],
            states: vec![[State::default(); 2]; channels],
            powers: vec![0.; (rate / 50).max(1)],
            position: 0,
            count: 0,
            sum: 0.,
            rms: 0.,
            programme: ProgrammeLevel::default(),
        }
    }
    pub fn measure(&mut self, frame: &[f32]) -> f64 {
        let mut power = 0.;
        for (sample, state) in frame.iter().zip(&mut self.states) {
            let x = if sample.is_finite() {
                (*sample as f64).clamp(-4., 4.)
            } else {
                0.
            };
            let y = state[0].tick(x, self.coeff[0]);
            let y = state[1].tick(y, self.coeff[1]);
            power += y * y;
        }
        power /= self.states.len() as f64;
        self.sum += power - self.powers[self.position];
        self.powers[self.position] = power;
        self.position = (self.position + 1) % self.powers.len();
        self.count = (self.count + 1).min(self.powers.len());
        self.rms = (self.sum.max(0.) / self.count as f64).sqrt();
        self.rms
    }
    pub fn frame(&mut self, frame: &[f32], dt: f64) -> f64 {
        let rms = self.measure(frame);
        self.programme.update(rms, dt)
    }
    pub fn block(&mut self, samples: &[f32], rate: usize, channels: usize) -> f64 {
        let mut level = 0.;
        for frame in samples.chunks_exact(channels) {
            level = self.frame(frame, 1. / rate as f64);
        }
        level
    }
}
#[derive(Default)]
pub struct ProgrammeLevel {
    fast: f64,
    slow: f64,
    initialized: bool,
    boost_hold: f64,
    falling: bool,
    transition: f64,
    fast_tracking: bool,
    fast_weight: f64,
    pub level: f64,
}
impl ProgrammeLevel {
    pub fn may_boost(&self) -> bool {
        self.boost_hold <= 0.
    }
    pub fn update(&mut self, rms: f64, dt: f64) -> f64 {
        self.boost_hold = (self.boost_hold - dt).max(0.);
        if rms > 0.001 {
            let power = rms * rms;
            if !self.initialized {
                self.fast = power;
                self.slow = power;
                self.initialized = true;
            }
            if power < self.fast * 0.25 {
                if !self.falling {
                    self.boost_hold = 0.18;
                }
                self.falling = true;
            } else if power >= self.fast * 0.5 {
                self.falling = false;
            }
            let tau = if power > self.fast { 0.02 } else { 0.12 };
            self.fast += (power - self.fast) * (1. - (-dt / tau).exp());
            self.slow += (power - self.slow) * (1. - (-dt / 0.6).exp());
            let ratio = self.fast.max(1e-20) / self.slow.max(1e-20);
            // A new level persists before fast tracking; peaks are handled by the lookahead guard.
            if ratio > 2. || ratio < 0.5 {
                self.transition += dt;
            } else {
                self.transition = 0.;
            }
            if self.transition >= 0.035 {
                self.fast_tracking = true;
            }
            if ratio > 0.7943 && ratio < 1.2589 {
                self.fast_tracking = false;
            }
            let weight = if self.fast_tracking { 1.0 } else { 0.0 };
            self.fast_weight += (weight - self.fast_weight) * (1.0 - (-dt / 0.04).exp());
            let fast_mix = 0.25 + 0.75 * self.fast_weight;
            self.level = (self.fast * fast_mix + self.slow * (1.0 - fast_mix)).sqrt();
        }
        self.level
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weighting_matches_bs1770_at_48k_and_is_stable_at_all_supported_rates() {
        let d = Detector::new(48000, 1);
        assert!((d.coeff[0].b[0] - 1.53512485958697).abs() < 1e-12);
        assert!((d.coeff[1].a[0] + 1.99004745483398).abs() < 1e-12);
        for rate in [8000, 16000, 44100, 48000, 96000, 192000, 384000] {
            let mut d = Detector::new(rate, 2);
            for i in 0..rate {
                let s = 0.2 * (2. * PI * 440. * i as f64 / rate as f64).sin() as f32;
                let l = d.frame(&[s, s], 1. / rate as f64);
                assert!(l.is_finite() && l < 2.);
            }
            assert!(d.rms > 0.1 && d.rms < 0.2);
        }
    }
    #[test]
    fn packet_boundaries_do_not_change_detector() {
        let source: Vec<f32> = (0..9600)
            .map(|i| (0.2 * (2. * PI * 440. * i as f64 / 48000.).sin()) as f32)
            .collect();
        let mut a = Detector::new(48000, 1);
        let mut b = Detector::new(48000, 1);
        a.block(&source, 48000, 1);
        for chunk in source.chunks(137) {
            b.block(chunk, 48000, 1);
        }
        assert!((a.rms - b.rms).abs() < 1e-12);
        assert!((a.programme.level - b.programme.level).abs() < 1e-12);
    }
}
