//! Audio-local agreement for Korean filler edits. Recognition stays intact when checks abstain.
use filmcraft_project::{Transcript, Word};
use filmcraft_time::{Tick, TimeRange};

pub fn target(w: &Word) -> bool {
    matches!(w.normalized().as_str(), "음" | "어" | "으음" | "어어")
}

/// Brief, tightly agreeing sounds need a usable video-frame interior. Longer or less stable
/// sounds keep the original 100 ms word-edge margin.
pub fn word_padding(word: &Word) -> Tick {
    crate::seconds_tick(if word.end.0.saturating_sub(word.start.0) <= crate::seconds_tick(0.3).0 { 0.08 } else { 0.1 })
}

/// Require the same lexical form in a separate cropped decode and keep uncertain edges.
/// Agreement does not establish semantic dispensability; the user still reviews every cut.
pub fn agreed_bounds(word: &Word, local: &Transcript, offset: Tick) -> Option<TimeRange> {
    let padding = word_padding(word);
    if word.end <= word.start {
        return None;
    }
    local
        .words
        .iter()
        .filter(|other| other.normalized() == word.normalized())
        .filter_map(|other| {
            let local_start = Tick(other.start.0.checked_add(offset.0)?);
            let local_end = Tick(other.end.0.checked_add(offset.0)?);
            // A repeated lexical guess is insufficient when its audio alignment moves.
            // Token scores are uncalibrated; a low score only tightens the timing rule.
            if !word.confidence.is_finite() || !other.confidence.is_finite() {
                return None;
            }
            let brief = padding == crate::seconds_tick(0.08);
            if brief && other.end.0.checked_sub(other.start.0)? > crate::seconds_tick(0.3).0 {
                return None;
            }
            let seconds = if brief {
                0.04
            } else if word.confidence.min(other.confidence) < 0.5 {
                0.1
            } else {
                0.3
            };
            let tolerance = Tick::from_seconds_f64(seconds).0 as u64;
            if local_start.0.abs_diff(word.start.0) > tolerance || local_end.0.abs_diff(word.end.0) > tolerance {
                return None;
            }
            let a = local_start.max(word.start);
            let b = local_end.min(word.end);
            let start = Tick(a.0.checked_add(padding.0)?);
            let end = Tick(b.0.checked_sub(padding.0)?);
            if end <= start
                || local.words.iter().any(|w| {
                    let a = w.start.0.checked_add(offset.0);
                    let b = w.end.0.checked_add(offset.0);
                    !target(w) && a.zip(b).is_some_and(|(a, b)| a < end.0 && b > start.0)
                })
            {
                return None;
            }
            Some(TimeRange::from_bounds(start, end))
        })
        .max_by_key(|r| r.duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn w(text: &str, a: f64, b: f64) -> Word {
        Word::new(text, Tick::from_seconds_f64(a), Tick::from_seconds_f64(b))
    }
    #[test]
    fn hallucinated_or_disagreeing_fillers_never_receive_edit_bounds() {
        let word = w("어", 1.0, 1.5);
        let mut t = Transcript::default();
        t.words.push(w("오늘", 1.0, 1.5));
        assert!(agreed_bounds(&word, &t, Tick::ZERO).is_none());
        t.words[0] = w("어", 2.0, 2.5);
        assert!(agreed_bounds(&word, &t, Tick::ZERO).is_none());
        t.words[0] = w("어", 1.0, 1.1);
        assert!(agreed_bounds(&word, &t, Tick::ZERO).is_none());
    }
    #[test]
    fn repeated_filler_guess_with_drifting_alignment_is_not_editable() {
        let word = w("음", 10.0, 10.7);
        let mut t = Transcript::default();
        t.words.push(w("음", 8.0, 10.7));
        assert!(agreed_bounds(&word, &t, Tick::ZERO).is_none());
        t.words[0] = w("음", 10.0, 11.3);
        assert!(agreed_bounds(&word, &t, Tick::ZERO).is_none());
    }
    #[test]
    fn independent_boundaries_are_intersected_and_edges_retained() {
        let word = w("음,", 10.0, 11.0);
        let mut t = Transcript::default();
        t.words.push(w("음", 1.2, 1.8));
        let cut = agreed_bounds(&word, &t, Tick::from_seconds_f64(9.0)).unwrap();
        assert_eq!(cut.start, Tick::from_seconds_f64(10.3));
        assert_eq!(cut.end(), Tick::from_seconds_f64(10.7));
    }
}

#[cfg(test)]
mod score_tests {
    use super::*;
    #[test]
    fn uncalibrated_scores_do_not_override_matching_audio_boundaries() {
        let mut word = Word::new("음", Tick::ZERO, Tick::from_seconds_f64(0.72));
        word.confidence = 0.48;
        let mut other = word.clone();
        other.confidence = 0.42;
        let transcript = Transcript { words: vec![other], ..Default::default() };
        assert!(agreed_bounds(&word, &transcript, Tick::ZERO).is_some());
    }
}

#[cfg(test)]
mod uncertainty_tests {
    use super::*;
    #[test]
    fn uncertain_word_with_drifting_end_is_not_editable() {
        let mut word = Word::new("음", Tick::from_seconds_f64(1.0), Tick::from_seconds_f64(2.24));
        word.confidence = 0.4;
        let mut other = Word::new("음", Tick::from_seconds_f64(1.0), Tick::from_seconds_f64(2.01));
        other.confidence = 0.72;
        let transcript = Transcript { words: vec![other], ..Default::default() };
        assert!(agreed_bounds(&word, &transcript, Tick::ZERO).is_none());
    }
}

/// Alternate crop sizes are useful when a short decode omits a filler. They must preserve
/// the surrounding lexical context as well as satisfying the same strict timing check.
pub fn context_matches(words: &[Word], index: usize, local: &Transcript, offset: Tick) -> bool {
    let Some(before) = words.get(..index).and_then(|ws| ws.iter().rev().find(|w| !target(w) && w.end > w.start)) else { return false };
    let Some(after) = words.get(index.saturating_add(1)..).and_then(|ws| ws.iter().find(|w| !target(w) && w.end > w.start)) else { return false };
    let immediate = [before, after].iter().all(|word| {
        let middle = word.start.0.saturating_add(word.end.0.saturating_sub(word.start.0) / 2);
        local.words.iter().any(|w| {
            let other = w.start.0.saturating_add(w.end.0.saturating_sub(w.start.0) / 2).saturating_add(offset.0);
            w.end > w.start && w.normalized() == word.normalized() && middle.abs_diff(other) <= Tick::from_seconds_f64(0.5).0 as u64
        })
    });
    if immediate {
        return true;
    }
    // A local decoder may merge or omit the nearest ordinary word. Require two ordered
    // anchors on EACH side before accepting that alternative; one distant match is not enough.
    let mut previous: Vec<_> = words.get(..index).unwrap_or_default().iter().rev().filter(|w| !target(w) && w.end > w.start).take(3).collect();
    previous.reverse();
    let following: Vec<_> = words.get(index.saturating_add(1)..).unwrap_or_default().iter().filter(|w| !target(w) && w.end > w.start).take(3).collect();
    let mut next = 0;
    let mut side_matches = |anchors: &[&Word]| {
        let mut count = 0;
        for word in anchors {
            let middle = word.start.0.saturating_add(word.end.0.saturating_sub(word.start.0) / 2);
            if let Some(found) = local.words.iter().enumerate().skip(next).find_map(|(i, w)| {
                let other = w.start.0.saturating_add(w.end.0.saturating_sub(w.start.0) / 2).saturating_add(offset.0);
                (w.end > w.start && w.normalized() == word.normalized() && middle.abs_diff(other) <= crate::seconds_tick(0.5).0 as u64).then_some(i)
            }) {
                count += 1;
                next = found.saturating_add(1);
            }
        }
        count >= 2
    };
    side_matches(&previous) && side_matches(&following)
}

/// Recover a badly aligned primary filler only when two fixed crops agree on the complete
/// lexical item and both ordinary neighbors. No expansion beyond the original word is allowed.
pub fn recovered_bounds(
    words: &[Word],
    index: usize,
    first: &Transcript,
    first_offset: Tick,
    second: &Transcript,
    second_offset: Tick,
) -> Option<(TimeRange, Tick)> {
    let primary = words.get(index)?;
    if !target(primary)
        || !primary.confidence.is_finite()
        || !context_matches(words, index, first, first_offset)
        || !context_matches(words, index, second, second_offset)
    {
        return None;
    }
    let locate = |transcript: &Transcript, offset: Tick| -> Option<Word> {
        let mut found = None;
        for word in &transcript.words {
            if word.normalized() != primary.normalized() || word.end <= word.start || !word.confidence.is_finite() {
                continue;
            }
            let start = Tick(word.start.0.checked_add(offset.0)?);
            let end = Tick(word.end.0.checked_add(offset.0)?);
            if start >= primary.end || end <= primary.start {
                continue;
            }
            if found.is_some() {
                return None;
            }
            let mut word = word.clone();
            word.start = start;
            word.end = end;
            found = Some(word);
        }
        found
    };
    let a = locate(first, first_offset)?;
    let b = locate(second, second_offset)?;
    let primary_padding = word_padding(primary);
    let primary_start = Tick(primary.start.0.checked_add(primary_padding.0)?);
    let primary_end = Tick(primary.end.0.checked_sub(primary_padding.0)?);
    let brief = crate::seconds_tick(0.08);
    let padding = word_padding(&a).max(word_padding(&b));
    if [primary, &a, &b].iter().any(|w| word_padding(w) == brief) {
        // Brief sounds keep agreement at their actual edges, including the primary
        // alignment. Clipping must never turn a long guess into a verified short sound.
        let tolerance = crate::seconds_tick(0.04).0 as u64;
        if [primary, &a, &b].iter().any(|w| word_padding(w) != brief)
            || [(primary, &a), (primary, &b), (&a, &b)]
                .iter()
                .any(|(left, right)| left.start.0.abs_diff(right.start.0) > tolerance || left.end.0.abs_diff(right.end.0) > tolerance)
        {
            return None;
        }
    }
    let interior = |word: &Word| {
        let edge = word_padding(word);
        let start = Tick(word.start.0.checked_add(edge.0)?).max(primary_start);
        let end = Tick(word.end.0.checked_sub(edge.0)?).min(primary_end);
        (end > start).then_some((start, end))
    };
    let (a_start, a_end) = interior(&a)?;
    let (b_start, b_end) = interior(&b)?;
    // Decoders can absorb different amounts of silence outside the primary span.
    // Compare only their independently padded, primary-bounded possible edit regions;
    // both must still cover the cut, with all ordinary words and acoustic edges protected.
    let tolerance = crate::seconds_tick(0.08).0 as u64;
    if a_start.0.abs_diff(b_start.0) > tolerance || a_end.0.abs_diff(b_end.0) > tolerance {
        return None;
    }
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    let duration = end.0.checked_sub(start.0)?;
    if duration <= 0 {
        return None;
    }
    let bounds = TimeRange::new(start, Tick(duration));
    let overlaps = |word: &Word, offset: Tick| {
        !target(word) && word.start.0.checked_add(offset.0).zip(word.end.0.checked_add(offset.0)).is_some_and(|(a, b)| a < bounds.end().0 && b > bounds.start.0)
    };
    if words.iter().any(|w| overlaps(w, Tick::ZERO))
        || first.words.iter().any(|w| overlaps(w, first_offset))
        || second.words.iter().any(|w| overlaps(w, second_offset))
    {
        return None;
    }
    Some((bounds, padding))
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    fn w(s: &str, a: f64, b: f64) -> Word {
        Word::new(s, crate::seconds_tick(a), crate::seconds_tick(b))
    }
    #[test]
    fn absorbed_leading_silence_may_differ_outside_the_retained_primary_interior() {
        let words = vec![w("앞말", 1.5, 2.4), w("음", 2.68, 3.38), w("뒷말", 3.96, 4.12)];
        let first = Transcript { words: vec![w("앞말", 1.5, 2.14), w("음", 2.14, 3.98), words[2].clone()], ..Default::default() };
        let second = Transcript { words: vec![w("앞말", 1.5, 1.78), w("음", 1.78, 3.98), words[2].clone()], ..Default::default() };
        let (range, _) = recovered_bounds(&words, 1, &first, Tick::ZERO, &second, Tick::ZERO).unwrap();
        assert_eq!(range.start, crate::seconds_tick(2.78));
        assert_eq!(range.end(), crate::seconds_tick(3.28));
    }
    #[test]
    fn clipped_agreement_does_not_approve_a_brief_primary_against_long_local_guesses() {
        let words = vec![w("앞말", 1.0, 1.4), w("음", 2.0, 2.28), w("뒷말", 3.0, 3.4)];
        let local = Transcript { words: vec![words[0].clone(), w("음", 1.5, 2.8), words[2].clone()], ..Default::default() };
        assert!(recovered_bounds(&words, 1, &local, Tick::ZERO, &local, Tick::ZERO).is_none());
    }
    #[test]
    fn recovery_retains_primary_edges_even_when_both_crops_extend_outside_them() {
        let words = vec![w("앞말", 0.0, 0.3), w("음", 0.8, 1.5), w("다음", 1.9, 2.2)];
        let local = Transcript { words: vec![words[0].clone(), w("음", 0.4, 1.8), words[2].clone()], ..Default::default() };
        let (range, _) = recovered_bounds(&words, 1, &local, Tick::ZERO, &local, Tick::ZERO).unwrap();
        assert_eq!(range.start, crate::seconds_tick(0.9));
        assert_eq!(range.end(), crate::seconds_tick(1.4));
    }
    #[test]
    fn primary_timing_error_needs_two_anchored_crops_and_no_ordinary_overlap() {
        let words = vec![w("앞말", 0.0, 0.3), w("음", 0.4, 1.5), w("다음", 1.6, 1.9)];
        let first = Transcript { words: vec![words[0].clone(), w("음", 0.8, 1.3), words[2].clone()], ..Default::default() };
        assert!(agreed_bounds(&words[1], &first, Tick::ZERO).is_none());
        assert!(recovered_bounds(&words, 1, &first, Tick::ZERO, &first, Tick::ZERO).is_some());
        let mut second = first.clone();
        second.words[1].start = crate::seconds_tick(0.95);
        assert!(recovered_bounds(&words, 1, &first, Tick::ZERO, &second, Tick::ZERO).is_none());
        second = first.clone();
        second.words.push(w("일반말", 1.0, 1.1));
        assert!(recovered_bounds(&words, 1, &first, Tick::ZERO, &second, Tick::ZERO).is_none());
        second = first.clone();
        second.words[2].text = "다른말".into();
        assert!(recovered_bounds(&words, 1, &first, Tick::ZERO, &second, Tick::ZERO).is_none());
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    fn w(s: &str, a: f64, b: f64) -> Word {
        Word::new(s, Tick::from_seconds_f64(a), Tick::from_seconds_f64(b))
    }
    #[test]
    fn alternate_context_must_preserve_both_neighbors() {
        let words = vec![w("앞말", 0.0, 0.3), w("음", 0.4, 1.0), w("다음", 1.1, 1.4)];
        let mut local = Transcript { words: words.clone(), ..Default::default() };
        assert!(context_matches(&words, 1, &local, Tick::ZERO));
        local.words[2].text = "다른말".into();
        assert!(!context_matches(&words, 1, &local, Tick::ZERO));
        local.words = words.clone();
        local.words[0].start = Tick::from_seconds_f64(5.0);
        local.words[0].end = Tick::from_seconds_f64(5.3);
        assert!(!context_matches(&words, 1, &local, Tick::ZERO));
    }
    #[test]
    fn confirmed_filler_cannot_cut_an_overlapping_ordinary_word() {
        let word = w("음", 0.0, 1.0);
        let local = Transcript { words: vec![word.clone(), w("겹친말", 0.3, 0.7)], ..Default::default() };
        assert!(agreed_bounds(&word, &local, Tick::ZERO).is_none());
    }
    #[test]
    fn a_missing_nearest_neighbor_requires_two_ordered_anchors_on_each_side() {
        let words = vec![
            w("앞하나", 0.0, 0.2),
            w("앞둘", 0.3, 0.5),
            w("가까운앞", 0.6, 0.8),
            w("음", 0.9, 1.4),
            w("가까운뒤", 1.5, 1.7),
            w("뒤하나", 1.8, 2.0),
            w("뒤둘", 2.1, 2.3),
        ];
        let mut local = Transcript { words: words.clone(), ..Default::default() };
        local.words[2].text = "합친앞말".into();
        local.words[4].text = "합친뒷말".into();
        assert!(context_matches(&words, 3, &local, Tick::ZERO));
        local.words.swap(5, 6);
        assert!(!context_matches(&words, 3, &local, Tick::ZERO));
        local.words.swap(5, 6);
        local.words[1].text = "다른말".into();
        assert!(!context_matches(&words, 3, &local, Tick::ZERO));
    }
}

/// A repeated word in a silent gap is still a hallucination. Keep only the longest sustained
/// above-floor acoustic island, retaining 60 ms on both acoustic edges and never expanding the
/// independently confirmed interval. This is an energy gate, not phoneme/semantic recognition.
pub fn audible_bounds(levels: &[f32], threshold: f32, confirmed: TimeRange, padding: Tick) -> Option<TimeRange> {
    if !threshold.is_finite() {
        return None;
    }
    let padding = padding.0;
    if padding <= 0 {
        return None;
    }
    let acoustic_padding = Tick::from_seconds_f64(0.06).0;
    let frame = crate::TICKS_PER_SAMPLE * i64::from(crate::SAMPLE_RATE / 100);
    let raw_start = confirmed.start.0.checked_sub(padding)?.max(0);
    let raw_end = confirmed.end().0.checked_add(padding)?;
    let a = usize::try_from(raw_start.checked_add(frame - 1)? / frame).ok()?;
    let b = usize::try_from(raw_end / frame).ok()?.min(levels.len());
    let frames = levels.get(a..b)?;
    let mut best = None;
    let mut run = None;
    let mut last = 0usize;
    let mut count = 0usize;
    let consider = |best: &mut Option<(usize, usize)>, start: usize, end: usize, active: usize| {
        let duration = end.saturating_sub(start);
        if duration >= 20 && active.saturating_mul(4) >= duration.saturating_mul(3) && best.is_none_or(|(a, b)| duration > b - a) {
            *best = Some((start, end));
        }
    };
    for (i, level) in frames.iter().enumerate() {
        if !level.is_finite() || *level < threshold {
            continue;
        }
        if let Some(start) = run {
            if i.saturating_sub(last) > 3 {
                consider(&mut best, start, last + 1, count);
                run = Some(i);
                count = 0;
            }
        } else {
            run = Some(i);
        }
        last = i;
        count += 1;
    }
    if let Some(start) = run {
        consider(&mut best, start, last + 1, count);
    }
    let (start, end) = best?;
    let start = i64::try_from(a.checked_add(start)?).ok()?.checked_mul(frame)?.checked_add(acoustic_padding)?;
    let end = i64::try_from(a.checked_add(end)?).ok()?.checked_mul(frame)?.checked_sub(acoustic_padding)?;
    let start = Tick(start).max(confirmed.start);
    let end = Tick(end).min(confirmed.end());
    (end > start).then(|| TimeRange::from_bounds(start, end))
}

#[cfg(test)]
mod acoustic_tests {
    use super::*;
    fn bounds(a: f64, b: f64) -> TimeRange {
        TimeRange::from_bounds(Tick::from_seconds_f64(a), Tick::from_seconds_f64(b))
    }
    #[test]
    fn agreement_in_silence_never_authorizes_a_cut() {
        assert!(audible_bounds(&vec![-80.0; 200], -55.0, bounds(0.1, 1.9), crate::seconds_tick(0.1)).is_none());
    }
    #[test]
    fn preceding_word_tail_and_gap_are_preserved() {
        let mut levels = vec![-80.0; 200];
        levels[..20].fill(-25.0); // previous ordinary word's tail
        levels[70..150].fill(-25.0); // sustained independently recognized filler
        assert_eq!(audible_bounds(&levels, -55.0, bounds(0.1, 1.5), crate::seconds_tick(0.1)), Some(bounds(0.76, 1.44)));
    }
    #[test]
    fn short_vocalization_is_preserved_when_no_safe_interior_exists() {
        let mut levels = vec![-80.0; 100];
        levels[30..45].fill(-25.0);
        assert!(audible_bounds(&levels, -55.0, bounds(0.1, 0.9), crate::seconds_tick(0.1)).is_none());
    }
}

#[cfg(test)]
mod short_core_tests {
    use super::*;
    #[test]
    fn a_sustained_short_filler_has_a_frame_editable_interior() {
        let mut levels = vec![-80.0; 160];
        levels[70..94].fill(-25.0);
        let confirmed = TimeRange::from_bounds(Tick::from_seconds_f64(0.1), Tick::from_seconds_f64(1.5));
        let cut = audible_bounds(&levels, -55.0, confirmed, crate::seconds_tick(0.1)).unwrap();
        let rate = filmcraft_time::FrameRate { num: 24, den: 1 };
        let start = rate.snap(cut.start) + rate.frame_duration();
        let end = rate.snap(cut.end());
        assert!(end > start, "240ms of actual voice needs a usable core after inward video-frame rounding");
    }
}

#[cfg(test)]
mod short_word_margin_tests {
    use super::*;
    #[test]
    fn tightly_agreed_short_word_keeps_a_video_frame_interior() {
        let word = Word::new("어", Tick::from_seconds_f64(1.01), Tick::from_seconds_f64(1.26));
        let local = Transcript { words: vec![word.clone()], ..Default::default() };
        let bounds = agreed_bounds(&word, &local, Tick::ZERO).unwrap();
        let rate = filmcraft_time::FrameRate { num: 24, den: 1 };
        let mut start = rate.snap(bounds.start);
        if start < bounds.start {
            start += rate.frame_duration();
        }
        assert!(rate.snap(bounds.end()) > start, "a stable 250ms filler must retain an inward frame-aligned core");
    }
}

#[cfg(test)]
mod brief_disagreement_tests {
    use super::*;
    #[test]
    fn brief_words_need_tighter_timing_agreement() {
        let word = Word::new("어", crate::seconds_tick(1.0), crate::seconds_tick(1.28));
        let other = Word::new("어", crate::seconds_tick(1.06), crate::seconds_tick(1.34));
        let local = Transcript { words: vec![other], ..Default::default() };
        assert!(agreed_bounds(&word, &local, Tick::ZERO).is_none());
    }
}
