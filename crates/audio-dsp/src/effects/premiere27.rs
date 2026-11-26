//! Premiere Pro 27.0 (`27.0.0.88`) native audio processors:
//! - [`AudioLeveling`] (`"audio_leveling"`): K-weighted LUFS/RMS sliding-window gain rider
//!   replacing legacy `VolumeRider`, with noise-floor gate freeze and speed/amount controls.
//! - [`EnhanceStems`] (`"enhance_stems"`): real-time 4-stem complementary separator and mixer
//!   (`Dialogue`, `Music`, `Ambience`, `SFX`, 0–200 %).

use crate::biquad::{Biquad, Coeffs, FilterType};
use crate::{AudioEffect, ParamSpec, ParamValues, Smoothed, Unit, block_len, db_to_gain, flush, flush32, param_plumbing};

fn coef_ms(ms: f64, sr: f64) -> f64 {
    if ms <= 0.0 { 0.0 } else { (-1.0 / (ms * 0.001 * sr).max(1.0)).exp() }
}

/// Sliding-window K-weighted LUFS/RMS gain rider (`Audio Leveling`, replacing `VolumeRider`).
pub struct AudioLeveling {
    pv: ParamValues,
    sr: f64,
    chans: usize,
    k_shelf: Coeffs,
    k_hp: Coeffs,
    bq_shelf: Vec<Biquad>,
    bq_hp: Vec<Biquad>,
    target_lufs: Smoothed,
    amount: Smoothed,
    speed: Smoothed,
    max_gain: Smoothed,
    gate_threshold: Smoothed,
    env_power: f64,
    gain_db: f64,
}

impl AudioLeveling {
    pub const PARAMS: &'static [ParamSpec] = &[
        ParamSpec::new("target_lufs", "Target Level", -60.0, 0.0, -23.0, Unit::Decibels),
        ParamSpec::new("amount", "Amount", 0.0, 100.0, 75.0, Unit::Percent),
        ParamSpec::new("speed", "Speed", 0.0, 100.0, 50.0, Unit::Percent),
        ParamSpec::new("max_gain", "Max Gain", 0.0, 24.0, 12.0, Unit::Decibels),
        ParamSpec::new("gate_threshold", "Noise Gate", -80.0, -20.0, -55.0, Unit::Decibels),
    ];

    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let sr = (sample_rate as f64).max(1000.0);
        let ch = channels.max(1);
        // ITU-R BS.1770 K-weighting stage 1 (+4 dB high shelf at 1681 Hz) + stage 2 (RLB high-pass at 38 Hz).
        let k_shelf = Coeffs::design(FilterType::HighShelf, sr, 1681.97, std::f64::consts::FRAC_1_SQRT_2, 4.0);
        let k_hp = Coeffs::design(FilterType::HighPass, sr, 38.13, 0.5, 0.0);
        let mut s = Self {
            pv: ParamValues::new(Self::PARAMS),
            sr,
            chans: ch,
            k_shelf,
            k_hp,
            bq_shelf: vec![Biquad::new(); ch],
            bq_hp: vec![Biquad::new(); ch],
            target_lufs: Smoothed::with_ms(-23.0, sample_rate, 20.0),
            amount: Smoothed::with_ms(75.0, sample_rate, 20.0),
            speed: Smoothed::with_ms(50.0, sample_rate, 20.0),
            max_gain: Smoothed::with_ms(12.0, sample_rate, 20.0),
            gate_threshold: Smoothed::with_ms(-55.0, sample_rate, 20.0),
            env_power: 0.0,
            gain_db: 0.0,
        };
        s.apply_params(true);
        s
    }

    fn apply_params(&mut self, snap: bool) {
        self.target_lufs.set(self.pv.v("target_lufs"));
        self.amount.set(self.pv.v("amount"));
        self.speed.set(self.pv.v("speed"));
        self.max_gain.set(self.pv.v("max_gain"));
        self.gate_threshold.set(self.pv.v("gate_threshold"));
        if snap {
            self.target_lufs.snap();
            self.amount.snap();
            self.speed.snap();
            self.max_gain.snap();
            self.gate_threshold.snap();
        }
    }
}

impl AudioEffect for AudioLeveling {
    param_plumbing!("audio_leveling");

    fn reset(&mut self) {
        for b in &mut self.bq_shelf {
            b.reset();
        }
        for b in &mut self.bq_hp {
            b.reset();
        }
        self.env_power = 0.0;
        self.gain_db = 0.0;
        self.apply_params(true);
    }

    fn process(&mut self, channels: &mut [&mut [f32]]) {
        let n = block_len(channels);
        if n == 0 || channels.is_empty() {
            return;
        }
        let active_ch = channels.len().min(self.chans);
        for i in 0..n {
            let target = self.target_lufs.tick() as f64;
            let amt = (self.amount.tick() as f64 * 0.01).clamp(0.0, 1.0);
            let spd = (self.speed.tick() as f64 * 0.01).clamp(0.0, 1.0);
            let max_g = (self.max_gain.tick() as f64).clamp(0.0, 24.0);
            let gate_db = self.gate_threshold.tick() as f64;

            let win_ms = 900.0 - 820.0 * spd;
            let det_c = coef_ms(win_ms, self.sr);
            let rider_c = coef_ms(win_ms * 0.65, self.sr);

            let mut p_sum = 0.0f64;
            for (c, ch_buf) in channels.iter().take(active_ch).enumerate() {
                let x = (ch_buf[i] as f64).clamp(-1e4, 1e4);
                let s1 = self.bq_shelf[c].tick(&self.k_shelf, x);
                let kw = self.bq_hp[c].tick(&self.k_hp, s1);
                p_sum += kw * kw;
            }
            let mean_sq = p_sum / active_ch as f64;
            self.env_power = flush(det_c * self.env_power + (1.0 - det_c) * mean_sq);
            let lufs_est = -0.691 + 10.0 * (self.env_power + 1e-20).log10();

            let desired_db = if lufs_est >= gate_db { ((target - lufs_est) * amt).clamp(-max_g, max_g) } else { 0.0 };
            self.gain_db = flush(rider_c * self.gain_db + (1.0 - rider_c) * desired_db);
            let lin = db_to_gain(self.gain_db as f32);
            for ch_buf in channels.iter_mut() {
                ch_buf[i] = flush32(ch_buf[i] * lin);
            }
        }
    }
}

/// Real-time 4-stem complementary separator & mixer (`Dialogue`, `Music`, `Ambience`, `SFX`).
///
/// Decomposes the signal into four exact-reconstruction stems (`S_dialogue + S_music + S_ambience +
/// S_sfx == input` at 100 % on all stems):
/// - **Ambience**: stereo side image (`L - M`, `R - M`) + sub-105 Hz diffuse room bed.
/// - **SFX**: fast-crest transient component isolated from the mid/mono core.
/// - **Dialogue**: vocal formant band (`220 Hz .. 4200 Hz`) of the sustained center channel.
/// - **Music**: harmonic low-end + upper-harmonic tonal residual of the sustained center channel.
pub struct EnhanceStems {
    pv: ParamValues,
    chans: usize,
    hp_voice: Coeffs,
    lp_voice: Coeffs,
    lp_bed: Coeffs,
    bq_hp: Vec<Biquad>,
    bq_lp: Vec<Biquad>,
    bq_bed: Vec<Biquad>,
    fast_c: f64,
    slow_c: f64,
    fast_env: f64,
    slow_env: f64,
    dialogue: Smoothed,
    music: Smoothed,
    ambience: Smoothed,
    sfx: Smoothed,
}

impl EnhanceStems {
    pub const PARAMS: &'static [ParamSpec] = &[
        ParamSpec::new("dialogue", "Dialogue", 0.0, 200.0, 100.0, Unit::Percent),
        ParamSpec::new("music", "Music", 0.0, 200.0, 100.0, Unit::Percent),
        ParamSpec::new("ambience", "Ambience", 0.0, 200.0, 100.0, Unit::Percent),
        ParamSpec::new("sfx", "SFX", 0.0, 200.0, 100.0, Unit::Percent),
    ];

    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let sr = (sample_rate as f64).max(1000.0);
        let ch = channels.max(1);
        let hp_voice = Coeffs::design(FilterType::HighPass, sr, 220.0, std::f64::consts::FRAC_1_SQRT_2, 0.0);
        let lp_voice = Coeffs::design(FilterType::LowPass, sr, 4200.0, std::f64::consts::FRAC_1_SQRT_2, 0.0);
        let lp_bed = Coeffs::design(FilterType::LowPass, sr, 105.0, std::f64::consts::FRAC_1_SQRT_2, 0.0);
        let mut s = Self {
            pv: ParamValues::new(Self::PARAMS),
            chans: ch,
            hp_voice,
            lp_voice,
            lp_bed,
            bq_hp: vec![Biquad::new(); ch],
            bq_lp: vec![Biquad::new(); ch],
            bq_bed: vec![Biquad::new(); ch],
            fast_c: coef_ms(1.8, sr),
            slow_c: coef_ms(55.0, sr),
            fast_env: 0.0,
            slow_env: 0.0,
            dialogue: Smoothed::with_ms(100.0, sample_rate, 15.0),
            music: Smoothed::with_ms(100.0, sample_rate, 15.0),
            ambience: Smoothed::with_ms(100.0, sample_rate, 15.0),
            sfx: Smoothed::with_ms(100.0, sample_rate, 15.0),
        };
        s.apply_params(true);
        s
    }

    fn apply_params(&mut self, snap: bool) {
        self.dialogue.set(self.pv.v("dialogue"));
        self.music.set(self.pv.v("music"));
        self.ambience.set(self.pv.v("ambience"));
        self.sfx.set(self.pv.v("sfx"));
        if snap {
            self.dialogue.snap();
            self.music.snap();
            self.ambience.snap();
            self.sfx.snap();
        }
    }
}

impl AudioEffect for EnhanceStems {
    param_plumbing!("enhance_stems");

    fn reset(&mut self) {
        for b in &mut self.bq_hp {
            b.reset();
        }
        for b in &mut self.bq_lp {
            b.reset();
        }
        for b in &mut self.bq_bed {
            b.reset();
        }
        self.fast_env = 0.0;
        self.slow_env = 0.0;
        self.apply_params(true);
    }

    fn process(&mut self, channels: &mut [&mut [f32]]) {
        let n = block_len(channels);
        if n == 0 || channels.is_empty() {
            return;
        }
        let active_ch = channels.len().min(self.chans);
        for i in 0..n {
            let gd = (self.dialogue.tick() * 0.01) as f64;
            let gm = (self.music.tick() * 0.01) as f64;
            let ga = (self.ambience.tick() * 0.01) as f64;
            let gs = (self.sfx.tick() * 0.01) as f64;

            let mid = if channels.len() >= 2 { 0.5 * (channels[0][i] as f64 + channels[1][i] as f64) } else { channels[0][i] as f64 };
            let mag = mid.abs().min(1e4);
            self.fast_env = flush(self.fast_c * self.fast_env + (1.0 - self.fast_c) * mag);
            self.slow_env = flush(self.slow_c * self.slow_env + (1.0 - self.slow_c) * mag);
            let crest = if self.fast_env > 1e-7 { ((self.fast_env - 1.55 * self.slow_env) / (self.fast_env + 1e-7)).clamp(0.0, 0.85) } else { 0.0 };

            for (c, ch_buf) in channels.iter_mut().enumerate() {
                let x = ch_buf[i] as f64;
                if c >= active_ch {
                    ch_buf[i] = flush32((x * gm) as f32);
                    continue;
                }
                let side = if active_ch >= 2 && c < 2 { x - mid } else { 0.0 };
                let core = x - side;
                let bed = self.bq_bed[c].tick(&self.lp_bed, core) * 0.35;
                let amb_stem = side + bed;
                let rem = core - bed;
                let sfx_stem = rem * crest;
                let sustained = rem - sfx_stem;
                let hp = self.bq_hp[c].tick(&self.hp_voice, sustained);
                let dlg_stem = self.bq_lp[c].tick(&self.lp_voice, hp);
                let mus_stem = sustained - dlg_stem;
                let y = gd * dlg_stem + gm * mus_stem + ga * amb_stem + gs * sfx_stem;
                ch_buf[i] = flush32(y as f32);
            }
        }
    }
}
