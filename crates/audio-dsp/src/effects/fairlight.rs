//! DaVinci Resolve Fairlight [`DialogueLeveler`] (`"dialogue_leveler"`):
//! Dual-window K-weighted vocal envelope rider with soft-whisper boost curves, background room-tone
//! freeze gate, and soft ceiling protection.

use crate::biquad::{Biquad, Coeffs, FilterType};
use crate::{AudioEffect, ParamSpec, ParamValues, Smoothed, Unit, block_len, db_to_gain, flush, flush32, param_plumbing};

fn coef_ms(ms: f64, sr: f64) -> f64 {
    if ms <= 0.0 { 0.0 } else { (-1.0 / (ms * 0.001 * sr).max(1.0)).exp() }
}

/// Fairlight-style real-time broadcast dialogue leveler.
pub struct DialogueLeveler {
    pv: ParamValues,
    sr: f64,
    chans: usize,
    voice_hp: Coeffs,
    voice_shelf: Coeffs,
    bq_hp: Vec<Biquad>,
    bq_shelf: Vec<Biquad>,
    target_lufs: Smoothed,
    mode: Smoothed,
    background_protect: Smoothed,
    output_gain: Smoothed,
    fast_power: f64,
    slow_power: f64,
    rider_db: f64,
}

impl DialogueLeveler {
    pub const PARAMS: &'static [ParamSpec] = &[
        ParamSpec::new("target_lufs", "Target Level", -48.0, -6.0, -20.0, Unit::Decibels),
        ParamSpec::choice(
            "mode",
            "Dynamics Profile",
            &["Optimize Moderate Levels", "Allow Wider Dynamics", "More Lift for Low Levels", "Lift Soft Whispery Sources"],
            0,
        ),
        ParamSpec::new("background_protect", "Background Noise Protection", 0.0, 100.0, 60.0, Unit::Percent),
        ParamSpec::new("output_gain", "Output Trim", -18.0, 18.0, 0.0, Unit::Decibels),
    ];

    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let sr = (sample_rate as f64).max(1000.0);
        let ch = channels.max(1);
        let voice_hp = Coeffs::design(FilterType::HighPass, sr, 85.0, std::f64::consts::FRAC_1_SQRT_2, 0.0);
        let voice_shelf = Coeffs::design(FilterType::HighShelf, sr, 1800.0, std::f64::consts::FRAC_1_SQRT_2, 3.5);
        let mut s = Self {
            pv: ParamValues::new(Self::PARAMS),
            sr,
            chans: ch,
            voice_hp,
            voice_shelf,
            bq_hp: vec![Biquad::new(); ch],
            bq_shelf: vec![Biquad::new(); ch],
            target_lufs: Smoothed::with_ms(-20.0, sample_rate, 25.0),
            mode: Smoothed::with_ms(0.0, sample_rate, 10.0),
            background_protect: Smoothed::with_ms(60.0, sample_rate, 25.0),
            output_gain: Smoothed::with_ms(0.0, sample_rate, 15.0),
            fast_power: 0.0,
            slow_power: 0.0,
            rider_db: 0.0,
        };
        s.apply_params(true);
        s
    }

    fn apply_params(&mut self, snap: bool) {
        self.target_lufs.set(self.pv.v("target_lufs"));
        self.mode.set(self.pv.v("mode"));
        self.background_protect.set(self.pv.v("background_protect"));
        self.output_gain.set(self.pv.v("output_gain"));
        if snap {
            self.target_lufs.snap();
            self.mode.snap();
            self.background_protect.snap();
            self.output_gain.snap();
        }
    }
}

impl AudioEffect for DialogueLeveler {
    param_plumbing!("dialogue_leveler");

    fn reset(&mut self) {
        for b in &mut self.bq_hp {
            b.reset();
        }
        for b in &mut self.bq_shelf {
            b.reset();
        }
        self.fast_power = 0.0;
        self.slow_power = 0.0;
        self.rider_db = 0.0;
        self.apply_params(true);
    }

    fn process(&mut self, channels: &mut [&mut [f32]]) {
        let n = block_len(channels);
        if n == 0 || channels.is_empty() {
            return;
        }
        let active_ch = channels.len().min(self.chans);
        let fast_c = coef_ms(85.0, self.sr);
        let slow_c = coef_ms(620.0, self.sr);

        for i in 0..n {
            let target = self.target_lufs.tick() as f64;
            let profile = (self.mode.tick().round() as i32).clamp(0, 3);
            let bg = (self.background_protect.tick() as f64 * 0.01).clamp(0.0, 1.0);
            let trim_db = self.output_gain.tick() as f64;

            let (strength, max_lift_db, rider_ms) = match profile {
                1 => (0.58, 10.0, 340.0), // Allow Wider Dynamics
                2 => (0.85, 16.0, 180.0), // More Lift for Low Levels
                3 => (0.94, 21.0, 125.0), // Lift Soft Whispery Sources
                _ => (0.76, 13.5, 220.0), // Optimize Moderate Levels
            };
            let rider_c = coef_ms(rider_ms, self.sr);
            let gate_db = -66.0 + bg * 24.0;

            let mut p_sum = 0.0f64;
            for (c, ch_buf) in channels.iter().take(active_ch).enumerate() {
                let x = (ch_buf[i] as f64).clamp(-1e4, 1e4);
                let hp = self.bq_hp[c].tick(&self.voice_hp, x);
                let v = self.bq_shelf[c].tick(&self.voice_shelf, hp);
                p_sum += v * v;
            }
            let mean_sq = p_sum / active_ch as f64;
            self.fast_power = flush(fast_c * self.fast_power + (1.0 - fast_c) * mean_sq);
            self.slow_power = flush(slow_c * self.slow_power + (1.0 - slow_c) * mean_sq);
            let blended = 0.65 * self.fast_power + 0.35 * self.slow_power;
            let level_db = -0.691 + 10.0 * (blended + 1e-20).log10();

            let desired = if level_db >= gate_db {
                ((target - level_db) * strength).clamp(-14.0, max_lift_db)
            } else {
                // Hold gentle decay toward unity during room-tone pauses instead of pumping noise up.
                self.rider_db * 0.9995
            };
            self.rider_db = flush(rider_c * self.rider_db + (1.0 - rider_c) * desired);
            let gain = db_to_gain((self.rider_db + trim_db) as f32);
            for ch_buf in channels.iter_mut() {
                let y = ch_buf[i] * gain;
                // Soft hyperbolic ceiling at 0 dBFS so lifted peaks never clip harshly.
                ch_buf[i] = flush32(if y.abs() > 0.92 { y.signum() * (0.92 + 0.08 * ((y.abs() - 0.92) * 6.0).tanh()) } else { y });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DialogueLeveler;
    use crate::AudioEffect;

    #[test]
    fn lifts_quiet_speech_and_protects_room_tone_silence() {
        let mut fx = DialogueLeveler::new(48_000.0, 1);
        fx.set_param("target_lufs", -18.0);
        fx.set_param("mode", 3.0);
        let mut quiet: Vec<f32> = (0..24_000).map(|n| 0.02 * ((n as f32 * 440.0 * std::f32::consts::TAU) / 48_000.0).sin()).collect();
        let before_rms = (quiet.iter().skip(12_000).map(|s| s * s).sum::<f32>() / 12_000.0).sqrt();
        fx.process(&mut [&mut quiet]);
        let after_rms = (quiet.iter().skip(12_000).map(|s| s * s).sum::<f32>() / 12_000.0).sqrt();
        assert!(after_rms > before_rms * 2.0, "expected speech lift: {before_rms} -> {after_rms}");

        fx.reset();
        let mut room_noise = vec![0.00005f32; 12_000];
        fx.process(&mut [&mut room_noise]);
        assert!(room_noise.iter().all(|s| s.abs() < 0.001), "room tone should stay gated");
    }
}
