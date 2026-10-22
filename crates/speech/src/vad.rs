//! Energy-based tightening of word bounds.
//!
//! Attention alignment places every word boundary on the next word's start, so silence between
//! words is absorbed into the word before it (and leading silence into the first word). Pause
//! detection and text-based editing need the silences, so each word's start and end are moved
//! inward past 10 ms frames whose level is below a threshold set between the clip's noise floor
//! (10th percentile of the levels of frames above −100 dBFS) and its speech level (95th percentile): 30 % of the way up,
//! and at least 6 dB above the floor. A word keeps at least 60 ms.

use filmcraft_project::Word;

use crate::{SAMPLE_RATE, TICKS_PER_SAMPLE};

const FRAME: usize = (SAMPLE_RATE / 100) as usize;
const MIN_WORD_FRAMES: usize = 6;

/// Frame levels in dBFS (10 ms RMS).
pub fn frame_db(audio: &[f32]) -> Vec<f32> {
    audio
        .chunks(FRAME)
        .map(|c| {
            let e = c.iter().map(|v| v * v).sum::<f32>() / c.len().max(1) as f32;
            10.0 * (e + 1e-12).log10()
        })
        .collect()
}

/// The speech/silence threshold in dBFS.
pub fn threshold(db: &[f32]) -> f32 {
    thresholds(db).0
}

fn thresholds(db: &[f32]) -> (f32, f32) {
    if db.is_empty() {
        return (-60.0, -60.0);
    }
    // digital silence (< −100 dBFS) would drag the floor far below the recording's own noise
    let mut s: Vec<f32> = db.iter().copied().filter(|v| *v > -100.0).collect();
    if s.is_empty() {
        return (-60.0, -60.0);
    }
    s.sort_by(f32::total_cmp);
    let q = |p: f32| s[((s.len() - 1) as f32 * p) as usize];
    let (floor, speech) = (q(0.10), q(0.95));
    let high = (floor + 0.3 * (speech - floor)).max(floor + 6.0);
    let low = (floor + 0.12 * (speech - floor)).max(floor + 3.0).min(high);
    (high, low)
}

/// Move word bounds inward past silent frames (see the module docs).
pub fn tighten_words(audio: &[f32], words: &mut [Word]) {
    let db = frame_db(audio);
    if db.is_empty() {
        return;
    }
    let (th, edge_th) = thresholds(&db);
    let frame_ticks = TICKS_PER_SAMPLE * FRAME as i64;
    for w in words {
        let a = (w.start.0 / frame_ticks).max(0) as usize;
        let b = (((w.end.0 + frame_ticks - 1) / frame_ticks).max(0) as usize).min(db.len());
        if b <= a + MIN_WORD_FRAMES {
            continue;
        }
        let mut s = a;
        while s + MIN_WORD_FRAMES < b && db[s] < th {
            s += 1;
        }
        let mut e = b;
        while e > s + MIN_WORD_FRAMES && db[e - 1] < th {
            e -= 1;
        }
        // nothing voiced at all: leave the word alone
        if (s..e).all(|f| db[f] < th) {
            continue;
        }
        // Keep quiet consonant edges near confirmed speech, not arbitrary low-level
        // breathing/noise far ahead of it. Limit hysteresis to 30 ms at either edge.
        let first = s;
        while s > a && first - s < 3 && db[s - 1] >= edge_th {
            s -= 1;
        }
        let last = e;
        while e < b && e - last < 3 && db[e] >= edge_th {
            e += 1;
        }
        if s > a {
            w.start = filmcraft_time::Tick(s as i64 * frame_ticks);
        }
        if e < b {
            w.end = filmcraft_time::Tick(e as i64 * frame_ticks).max(w.start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seconds_tick;

    #[test]
    fn silence_is_trimmed_from_words() {
        // 0.0–0.5 silence, 0.5–1.0 tone, 1.0–2.0 silence, 2.0–2.5 tone
        let mut audio: Vec<f32> = (0..40_000).map(|i| if i % 2 == 0 { 0.0001 } else { 0.0 }).collect();
        for i in (8_000..16_000).chain(32_000..40_000) {
            audio[i] = 0.3 * (i as f32 * 0.2).sin();
        }
        let mut w = vec![Word::new("a", seconds_tick(0.0), seconds_tick(2.0)), Word::new("b", seconds_tick(2.0), seconds_tick(2.5))];
        tighten_words(&audio, &mut w);
        assert_eq!(w[0].start, seconds_tick(0.5));
        assert_eq!(w[0].end, seconds_tick(1.0));
        assert_eq!((w[1].start, w[1].end), (seconds_tick(2.0), seconds_tick(2.5)));
    }
}

#[cfg(test)]
mod quiet_edges {
    use super::*;
    #[test]
    fn weak_onset_and_release_are_not_trimmed_like_silence() {
        // A quiet consonant next to a stronger vowel must not disappear at a hard RMS gate.
        let mut audio = vec![0.0001; 16000];
        for (i, sample) in audio.iter_mut().enumerate().take(11200).skip(4800) {
            let amplitude = if (5280..10720).contains(&i) { 0.3 } else { 0.0005 };
            *sample = amplitude * (i as f32 * 0.2).sin();
        }
        let mut words = vec![Word::new("quiet onset", crate::seconds_tick(0.0), crate::seconds_tick(1.0))];
        tighten_words(&audio, &mut words);
        assert!(words[0].start <= crate::seconds_tick(0.3));
        assert!(words[0].end >= crate::seconds_tick(0.7));
    }
}

#[cfg(test)]
mod distant_noise {
    use super::*;
    #[test]
    fn distant_quiet_noise_is_not_absorbed_as_a_word_onset() {
        let mut audio = vec![0.0001; 16000];
        for (i, sample) in audio.iter_mut().enumerate().take(14400).skip(3200) {
            let amplitude = if i >= 11200 { 0.3 } else { 0.0005 };
            *sample = amplitude * (i as f32 * 0.2).sin();
        }
        let mut words = vec![Word::new("voice after quiet noise", crate::seconds_tick(0.0), crate::seconds_tick(1.0))];
        tighten_words(&audio, &mut words);
        assert!(words[0].start >= crate::seconds_tick(0.67));
        assert!(words[0].start <= crate::seconds_tick(0.7));
    }
}
