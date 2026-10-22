//! Word timing from cross-attention: token-to-audio alignment by dynamic time warping over the
//! averaged, normalised attention of the alignment heads (see the module docs of `whisper`).

use candle_core::{Result, Tensor};

use super::model::Model;

pub struct Segment {
    pub tokens: std::ops::Range<usize>,
    pub frames: std::ops::Range<usize>,
}

/// Start/end (in mel frames from the window start, 10 ms) of every token of `text`.
pub fn align(
    m: &mut Model,
    xa: &Tensor,
    prompt: &[u32],
    text: &[u32],
    eot: u32,
    heads: &[(usize, usize)],
    seg_frames: usize,
    segments: &[Segment],
) -> Result<Vec<(usize, usize)>> {
    let mut tokens = prompt.to_vec();
    tokens.extend_from_slice(text);
    tokens.push(eot);
    m.decoder.reset();
    let (_, qk) = m.decoder.forward(&tokens, xa, false, Some(heads))?;
    m.decoder.reset();
    let Some(qk) = qk else { return Ok(vec![(0, 0); text.len()]) };
    let (h, t, f_all) = qk.dims3()?;
    let f = (seg_frames / 2).clamp(1, f_all);
    let qk = qk.narrow(2, 0, f)?.contiguous()?;
    let w = candle_nn::ops::softmax_last_dim(&qk)?;
    let w: Vec<Vec<Vec<f32>>> = w.to_vec3()?;
    let rows = text.len() + 1;
    let first = prompt.len() - 1;
    let mut matrix = vec![0f32; rows * f];
    for head in w.iter().take(h) {
        // standardise over tokens for every frame
        let mut z = vec![0f32; t * f];
        for j in 0..f {
            let mean = (0..t).map(|i| head[i][j]).sum::<f32>() / t as f32;
            let var = (0..t).map(|i| (head[i][j] - mean).powi(2)).sum::<f32>() / t as f32;
            let sd = var.sqrt().max(1e-8);
            for i in 0..t {
                z[i * f + j] = (head[i][j] - mean) / sd;
            }
        }
        for r in 0..rows {
            let row = median_filter(&z[(first + r) * f..(first + r + 1) * f], 7);
            for j in 0..f {
                matrix[r * f + j] += row[j] / h as f32;
            }
        }
    }
    if let Some(times) = anchored_times(&matrix, rows, f, segments) {
        return Ok(times);
    }
    let neg: Vec<f32> = matrix.iter().map(|v| -v).collect();
    let path = dtw(&neg, rows, f);
    // the frame where each row is first reached
    let mut jump = vec![usize::MAX; rows];
    for &(i, j) in &path {
        if jump[i] == usize::MAX {
            jump[i] = j;
        }
    }
    let mut out = Vec::with_capacity(text.len());
    for i in 0..text.len() {
        let s = jump[i].min(f);
        let e = jump[i + 1].min(f).max(s);
        out.push((s * 2, e * 2));
    }
    Ok(out)
}

/// Decoder timestamps bound each spoken segment; padding/unrecognised neighbouring segments
/// must not be consumed by DTW simply to reach the last frame of the window.
fn anchored_times(matrix: &[f32], rows: usize, frames: usize, segments: &[Segment]) -> Option<Vec<(usize, usize)>> {
    if rows < 2 || frames == 0 || segments.is_empty() {
        return None;
    }
    let mut next = 0;
    let mut result = Vec::with_capacity(rows - 1);
    for segment in segments {
        let range = &segment.tokens;
        if range.start != next || range.end <= range.start || range.end >= rows {
            return None;
        }
        let a = (segment.frames.start / 2).min(frames);
        let b = (segment.frames.end.saturating_add(1) / 2).min(frames);
        if b <= a {
            return None;
        }
        let n = range.end - range.start + 1;
        let width = b - a;
        let mut cost = Vec::with_capacity(n * width);
        for r in range.start..=range.end {
            cost.extend(matrix.get(r * frames + a..r * frames + b)?.iter().map(|v| -v));
        }
        let mut jumps = vec![width; n];
        for (i, j) in dtw(&cost, n, width) {
            jumps[i] = jumps[i].min(j);
        }
        for i in 0..n - 1 {
            result.push(((a + jumps[i]) * 2, (a + jumps[i + 1].max(jumps[i])) * 2));
        }
        next = range.end;
    }
    (next == rows - 1).then_some(result)
}

/// Mean token log probability, separate from timing quality; not calibrated correctness.
pub fn word_probability(logprobs: &[f32]) -> f32 {
    if logprobs.is_empty() || logprobs.iter().any(|p| !p.is_finite()) {
        return 0.0;
    }
    (logprobs.iter().sum::<f32>() / logprobs.len() as f32).exp().clamp(0.0, 1.0)
}

/// DTW's first word can absorb unrecognised speech/pause at a segment boundary. Limit only
/// gross outliers there; ordinary words and sustained vocal fillers keep their full bounds.
/// This is an estimate, not speaker-aware forced alignment (see docs/transcripts.md).
pub fn limit_boundary_words(words: &mut [filmcraft_project::Word], segment_starts: &[usize]) {
    use filmcraft_time::Tick;
    let mut durations: Vec<Tick> = words.iter().map(|w| w.end - w.start).filter(|d| *d > Tick::ZERO).collect();
    if durations.len() < 3 {
        return;
    }
    durations.sort();
    let median = durations[durations.len() / 2].min(Tick::from_seconds_f64(0.7));
    let maximum = (median + median).max(Tick::from_seconds_f64(0.08));
    let outlier = (maximum + maximum).max(Tick::from_seconds_f64(2.0));
    for i in 0..words.len() {
        let boundary =
            i == 0 || segment_starts.contains(&i) || words.get(i.wrapping_sub(1)).is_some_and(|w| w.text.ends_with(['.', '?', '!', '。', '？', '！']));
        let filler = matches!(words[i].normalized().as_str(), "음" | "어" | "으음" | "어어" | "um" | "uh" | "hmm" | "ah");
        if boundary && !filler && words[i].end - words[i].start > outlier {
            words[i].start = words[i].end - maximum;
            words[i].confidence = words[i].confidence.min(0.5);
        }
    }
}

/// Median filter with reflect padding.
pub fn median_filter(x: &[f32], width: usize) -> Vec<f32> {
    let n = x.len();
    if n == 0 || width <= 1 {
        return x.to_vec();
    }
    let half = width / 2;
    let at = |i: isize| -> f32 {
        if n == 1 {
            return x[0];
        }
        let p = 2 * (n as isize - 1);
        let mut j = i.rem_euclid(p);
        if j >= n as isize {
            j = p - j;
        }
        x[j as usize]
    };
    let mut buf = vec![0f32; width];
    (0..n)
        .map(|i| {
            for (k, b) in buf.iter_mut().enumerate() {
                *b = at(i as isize + k as isize - half as isize);
            }
            buf.sort_by(f32::total_cmp);
            buf[half]
        })
        .collect()
}

/// Dynamic time warping through `cost` (`n × m`, row-major), monotonic steps (diagonal, down,
/// right). Returns the path from (0, 0) to (n − 1, m − 1).
pub fn dtw(cost: &[f32], n: usize, m: usize) -> Vec<(usize, usize)> {
    let inf = f32::INFINITY;
    let mut acc = vec![inf; (n + 1) * (m + 1)];
    let mut trace = vec![0u8; (n + 1) * (m + 1)];
    acc[0] = 0.0;
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for j in 1..=m {
        for i in 1..=n {
            let c0 = acc[idx(i - 1, j - 1)];
            let c1 = acc[idx(i - 1, j)];
            let c2 = acc[idx(i, j - 1)];
            let (c, t) = if c0 <= c1 && c0 <= c2 {
                (c0, 0)
            } else if c1 <= c2 {
                (c1, 1)
            } else {
                (c2, 2)
            };
            acc[idx(i, j)] = cost[(i - 1) * m + (j - 1)] + c;
            trace[idx(i, j)] = t;
        }
    }
    for j in 0..=m {
        trace[idx(0, j)] = 2;
    }
    for i in 0..=n {
        trace[idx(i, 0)] = 1;
    }
    let (mut i, mut j) = (n, m);
    let mut path = Vec::new();
    while i > 0 || j > 0 {
        if i > 0 && j > 0 {
            path.push((i - 1, j - 1));
        }
        match trace[idx(i, j)] {
            0 => {
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_speech_is_not_assigned_to_one_short_word() {
        use filmcraft_project::Word;
        use filmcraft_time::Tick;
        let w = |text: &str, a, b| Word::new(text, Tick::from_seconds_f64(a), Tick::from_seconds_f64(b));
        let mut words = vec![w("제", 0.76, 8.46), w("이름은", 8.46, 9.22), w("이라고", 9.22, 10.0), w("하고요.", 10.0, 10.52), w("키는", 11.08, 11.42)];
        let unchanged = words[1..].to_vec();
        limit_boundary_words(&mut words, &[0]);
        assert!(words[0].start >= Tick::from_seconds_f64(7.0));
        assert_eq!(words[0].end, Tick::from_seconds_f64(8.46));
        assert_eq!(&words[1..], unchanged.as_slice());
        assert_eq!(words[0].confidence, 0.5);
    }
    #[test]
    fn sustained_fillers_are_not_truncated_as_alignment_outliers() {
        use filmcraft_project::Word;
        use filmcraft_time::Tick;
        let mut words = vec![
            Word::new("음", Tick::ZERO, Tick::from_seconds_f64(4.0)),
            Word::new("오늘", Tick::from_seconds_f64(4.0), Tick::from_seconds_f64(4.3)),
            Word::new("네", Tick::from_seconds_f64(4.3), Tick::from_seconds_f64(4.6)),
        ];
        let before = words.clone();
        limit_boundary_words(&mut words, &[0]);
        assert_eq!(words, before);
    }
    #[test]
    fn model_scores_are_not_fabricated_as_perfect_confidence() {
        assert_eq!(word_probability(&[]), 0.0);
        assert_eq!(word_probability(&[f32::NAN]), 0.0);
        assert!((word_probability(&[-0.5, -1.5]) - (-1.0f32).exp()).abs() < 1e-6);
    }

    #[test]
    fn median_filter_removes_spikes() {
        assert_eq!(median_filter(&[0.0, 0.0, 9.0, 0.0, 0.0], 3), vec![0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(median_filter(&[1.0, 2.0, 3.0], 1), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn dtw_ties_never_choose_a_more_expensive_predecessor() {
        let cost = [0.0, 0.0, 2.0, 0.0, 2.0, 1.0, 2.0, 0.0, 2.0, 0.0, 0.0, 2.0];
        let path = dtw(&cost, 3, 4);
        let total: f32 = path.iter().map(|(i, j)| cost[i * 4 + j]).sum();
        assert_eq!(total, 3.0, "a tied diagonal/down minimum must not select the costlier right predecessor");
    }

    #[test]
    fn dtw_follows_the_cheap_diagonal_band() {
        // 3 rows × 6 columns, cheap cells at row 0: cols 0-1, row 1: cols 2-3, row 2: cols 4-5
        let mut c = vec![1.0f32; 18];
        for (r, cols) in [(0, [0, 1]), (1, [2, 3]), (2, [4, 5])] {
            for k in cols {
                c[r * 6 + k] = 0.0;
            }
        }
        let p = dtw(&c, 3, 6);
        assert_eq!(p.first(), Some(&(0, 0)));
        assert_eq!(p.last(), Some(&(2, 5)));
        let first_col = |row: usize| p.iter().find(|x| x.0 == row).unwrap().1;
        assert_eq!((first_col(0), first_col(1), first_col(2)), (0, 2, 4));
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::*;
    #[test]
    fn segment_alignment_cannot_absorb_neighbouring_speech_or_window_padding() {
        let matrix = vec![0.0; 4 * 20];
        let segments = [Segment { tokens: 0..1, frames: 4..12 }, Segment { tokens: 1..3, frames: 18..28 }];
        let times = anchored_times(&matrix, 4, 20, &segments).unwrap();
        assert!(times[0].0 >= 4 && times[0].1 <= 12);
        assert!(times[1..].iter().all(|(a, b)| *a >= 18 && *b <= 28));
        assert!(anchored_times(&matrix, 4, 20, &[Segment { tokens: 4..5, frames: 0..10 }]).is_none());
    }
}
