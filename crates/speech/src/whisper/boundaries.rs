//! Conservative repairs of ordinary words that absorbed preceding speech and a long pause.
//! Energy proposes a crop; recognition of the complete word and following context verifies it.
use filmcraft_project::{Transcript, Word};
use filmcraft_time::{Tick, TimeRange};

pub struct Probe {
    pub index: usize,
    pub start: Tick,
    gap: Tick,
}

pub struct EndProbe {
    pub index: usize,
    pub end: Tick,
    gap: usize,
}

const MAX_END_PROBES: usize = 32;

/// Prefix crops complement suffix repairs: a long word may have absorbed another voice
/// after a quiet gap. Recognition, not energy alone, must confirm the complete prefix word.
pub fn end_probes(words: &[Word], levels: &[f32], threshold: f32) -> Vec<EndProbe> {
    let frame = crate::TICKS_PER_SAMPLE * i64::from(crate::SAMPLE_RATE / 100);
    let mut out = Vec::new();
    if !threshold.is_finite() {
        return out;
    }
    for (index, word) in words.iter().enumerate() {
        let length = word.end.0.saturating_sub(word.start.0);
        if word.start < Tick::ZERO || super::fillers::target(word) || length <= crate::seconds_tick(1.0).0 || length >= crate::seconds_tick(5.0).0 {
            continue;
        }
        let Some(a) = word.start.0.checked_add(frame - 1).and_then(|x| usize::try_from(x / frame).ok()) else { continue };
        let Ok(b) = usize::try_from(word.end.0 / frame) else { continue };
        let Some(window) = levels.get(a..b.min(levels.len())) else { continue };
        let mut begin = None;
        let mut best = None;
        for i in 0..=window.len() {
            if window.get(i).is_some_and(|x| x.is_finite() && *x < threshold) {
                if begin.is_none() {
                    begin = Some(i);
                }
            } else if let Some(start) = begin.take() {
                let gap = i.saturating_sub(start);
                if gap >= 24 && (6..=90).contains(&start) && window.len().saturating_sub(i) >= 6 && best.is_none_or(|(_, size)| gap > size) {
                    best = Some((start, gap));
                }
            }
        }
        if let Some((start, gap)) = best {
            let Some(end) = a
                .checked_add(start)
                .and_then(|x| i64::try_from(x).ok())
                .and_then(|x| x.checked_mul(frame))
                .and_then(|x| x.checked_add(crate::seconds_tick(0.06).0))
            else {
                continue;
            };
            let probe = EndProbe { index, end: Tick(end).min(word.end), gap };
            if out.len() < MAX_END_PROBES {
                out.push(probe);
            } else if let Some((smallest, _)) = out.iter().enumerate().min_by_key(|(_, p)| p.gap)
                && let Some(slot) = out.get_mut(smallest)
                && probe.gap > slot.gap
            {
                *slot = probe;
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(p.gap));
    out.truncate(MAX_END_PROBES);
    out
}

/// End repair needs the complete word plus two preceding anchors in their original order.
/// The word's start stays intact; repeated or uncertain matches abstain.
pub fn verified_end(words: &[Word], index: usize, local: &Transcript, offset: Tick) -> Option<Tick> {
    let word = words.get(index)?;
    let before: Vec<_> = words.get(..index)?.iter().rev().filter(|w| !super::fillers::target(w) && w.end > w.start).take(4).collect();
    if before.len() < 2 || word.end <= word.start {
        return None;
    }
    let mut result = None;
    for (i, candidate) in local.words.iter().enumerate().rev().take(4) {
        if candidate.normalized() != word.normalized() || !candidate.confidence.is_finite() || candidate.confidence < 0.5 || candidate.end <= candidate.start {
            continue;
        }
        let start = shifted(candidate.start, offset)?;
        let end = shifted(candidate.end, offset)?;
        if start.0.abs_diff(word.start.0) > crate::seconds_tick(0.15).0 as u64 || end <= word.start || end > word.end {
            continue;
        }
        let mut previous = i;
        let mut matches = 0;
        for anchor in &before {
            let Some(at) = middle(anchor, Tick::ZERO) else { continue };
            if let Some(found) = local.words.get(..previous)?.iter().rposition(|w| {
                w.end > w.start
                    && w.normalized() == anchor.normalized()
                    && middle(w, offset).is_some_and(|t| t.0.abs_diff(at.0) <= crate::seconds_tick(0.3).0 as u64)
            }) {
                matches += 1;
                previous = found;
            }
        }
        let end = Tick(end.0.checked_add(crate::seconds_tick(0.03).0)?).min(word.end);
        if matches < 2 || word.end - end < crate::seconds_tick(0.2) || end - word.start < crate::seconds_tick(0.06) {
            continue;
        }
        if result.is_some() {
            return None;
        }
        result = Some(end);
    }
    result
}

/// Attention timing can end before a word's last voiced syllable even when the decoder
/// emits the complete lexical word. Retain the entire acoustic prefix, including its margin.
/// This only guards a lexically verified end; it never authorizes a repair on energy alone.
pub fn guarded_end(word: &Word, verified: Tick, prefix_end: Tick) -> Option<Tick> {
    let end = verified.max(prefix_end).min(word.end);
    let removed = word.end.0.checked_sub(end.0)?;
    let retained = end.0.checked_sub(word.start.0)?;
    (removed >= crate::seconds_tick(0.2).0 && retained >= crate::seconds_tick(0.06).0).then_some(end)
}

/// Largest internal quiet gap in a suspiciously long ordinary word. This alone is never a repair.
pub fn probes(words: &[Word], levels: &[f32], threshold: f32) -> Vec<Probe> {
    let frame = crate::TICKS_PER_SAMPLE * i64::from(crate::SAMPLE_RATE / 100);
    if !threshold.is_finite() {
        return Vec::new();
    }
    let mut probes = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let length = word.end.0.saturating_sub(word.start.0);
        if word.start < Tick::ZERO || super::fillers::target(word) || length <= crate::seconds_tick(1.0).0 || length >= crate::seconds_tick(5.0).0 {
            continue;
        }
        let Some(a) = word.start.0.max(0).checked_add(frame - 1).and_then(|t| usize::try_from(t / frame).ok()) else { continue };
        let Ok(b) = usize::try_from(word.end.0.max(0) / frame) else { continue };
        let Some(window) = levels.get(a..b.min(levels.len())) else { continue };
        let mut begin = None;
        let mut best = None;
        for i in 0..=window.len() {
            if window.get(i).is_some_and(|v| v.is_finite() && *v < threshold) {
                if begin.is_none() {
                    begin = Some(i);
                }
            } else if let Some(start) = begin.take() {
                let duration = i - start;
                let suffix = window.len() - i;
                if duration >= 24 && start >= 6 && (6..=90).contains(&suffix) && best.is_none_or(|(a, b)| duration > b - a) {
                    best = Some((start, i));
                }
            }
        }
        if let Some((begin, end)) = best {
            let Some(at) = a.checked_add(end).and_then(|v| i64::try_from(v).ok()).and_then(|v| v.checked_mul(frame)) else { continue };
            let Some(start) = at.checked_sub(crate::seconds_tick(0.06).0) else { continue };
            probes.push(Probe { index, start: Tick(start).max(word.start), gap: Tick((end - begin) as i64 * frame) });
        }
    }
    probes.sort_by_key(|p| std::cmp::Reverse(p.gap));
    probes.truncate(256);
    probes
}

fn shifted(t: Tick, offset: Tick) -> Option<Tick> {
    Some(Tick(t.0.checked_add(offset.0)?))
}
fn middle(w: &Word, offset: Tick) -> Option<Tick> {
    shifted(Tick(w.start.0.checked_add(w.end.0.checked_sub(w.start.0)? / 2)?), offset)
}

/// The proposed crop is unprompted: the word must actually be decoded from the suffix, with
/// two ordered ordinary-word anchors near their original times. Never expand a word's bounds.
pub fn verified_range(words: &[Word], index: usize, local: &Transcript, offset: Tick) -> Option<TimeRange> {
    let word = words.get(index)?;
    if word.start < Tick::ZERO || word.end <= word.start || super::fillers::target(word) {
        return None;
    }
    let end_limit = Tick(word.end.0.checked_add(crate::seconds_tick(0.2).0)?);
    let after: Vec<&Word> = words.get(index.checked_add(1)?..)?.iter().filter(|w| !super::fillers::target(w) && w.end > w.start).take(4).collect();
    if after.len() < 2 {
        return None;
    }
    let mut result = None;
    for (i, candidate) in local.words.iter().take(4).enumerate() {
        if candidate.normalized() != word.normalized() || !candidate.confidence.is_finite() || candidate.confidence < 0.5 || candidate.end <= candidate.start {
            continue;
        }
        let begin = shifted(candidate.start, offset)?;
        let finish = shifted(candidate.end, offset)?;
        if begin < word.start || finish > end_limit || finish - begin < crate::seconds_tick(0.06) {
            continue;
        }
        let mut next = i + 1;
        let mut matches = 0;
        for anchor in &after {
            let Some(at) = middle(anchor, Tick::ZERO) else { continue };
            if let Some(found) = local.words.iter().enumerate().skip(next).find_map(|(j, w)| {
                (w.end > w.start
                    && w.normalized() == anchor.normalized()
                    && middle(w, offset).is_some_and(|t| t.0.abs_diff(at.0) <= crate::seconds_tick(0.3).0 as u64))
                .then_some(j)
            }) {
                matches += 1;
                next = found + 1;
            }
        }
        if matches < 2 {
            continue;
        }
        let start = Tick(begin.0.saturating_sub(crate::seconds_tick(0.03).0)).max(word.start);
        if start - word.start < crate::seconds_tick(0.2) {
            continue;
        }
        // An end moves only when a separately decoded ordinary word occupies the old tail.
        let tail = local.words.iter().skip(i + 1).any(|w| {
            if super::fillers::target(w) || w.normalized() == word.normalized() {
                return false;
            }
            let Some(a) = shifted(w.start, offset) else { return false };
            let Some(b) = shifted(w.end, offset) else { return false };
            b.min(word.end) - a.max(finish) >= crate::seconds_tick(0.08)
        });
        let end = if tail { Tick(finish.0.checked_add(crate::seconds_tick(0.03).0)?).min(word.end) } else { word.end };
        if end <= start {
            continue;
        }
        if result.is_some() {
            return None;
        } // repeated words are ambiguous
        result = Some(TimeRange::from_bounds(start, end));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn w(text: &str, a: f64, b: f64) -> Word {
        Word::new(text, crate::seconds_tick(a), crate::seconds_tick(b))
    }
    #[test]
    fn prefix_repair_needs_complete_word_and_ordered_preceding_context() {
        let words = vec![w("앞의", 0.0, 0.3), w("문맥", 0.4, 0.7), w("그렇죠", 0.8, 3.0)];
        let mut local = Transcript { words: vec![w("앞의", 0.0, 0.3), w("문맥", 0.4, 0.7), w("그렇죠", 0.8, 1.2)], ..Default::default() };
        let mut levels = vec![-20.0; 300];
        levels[120..240].fill(-80.0);
        assert_eq!(end_probes(&words, &levels, -50.0)[0].end, crate::seconds_tick(1.26));
        assert_eq!(verified_end(&words, 2, &local, Tick::ZERO), Some(crate::seconds_tick(1.23)));
        local.words[2].text = "그렇".into();
        assert!(verified_end(&words, 2, &local, Tick::ZERO).is_none());
        local.words[2].text = "그렇죠".into();
        local.words.swap(0, 1);
        assert!(verified_end(&words, 2, &local, Tick::ZERO).is_none());
    }
    #[test]
    fn end_rechecks_have_a_fixed_job_budget() {
        let mut words = Vec::new();
        let mut levels = vec![-20.0; 12800];
        for i in 0..64 {
            words.push(w("일반말", i as f64 * 2.0, i as f64 * 2.0 + 1.8));
            levels[i * 200 + 30..i * 200 + 130].fill(-80.0);
        }
        assert_eq!(end_probes(&words, &levels, -50.0).len(), MAX_END_PROBES);
    }
    #[test]
    fn lexical_end_before_the_last_voiced_syllable_keeps_the_full_acoustic_prefix() {
        let word = w("그렇죠", 0.8, 3.0);
        assert_eq!(guarded_end(&word, crate::seconds_tick(1.03), crate::seconds_tick(1.66)), Some(crate::seconds_tick(1.66)));
        assert!(guarded_end(&word, crate::seconds_tick(1.03), crate::seconds_tick(2.9)).is_none());
    }
    #[test]
    fn energy_is_only_a_proposal_and_a_complete_suffix_word_is_required() {
        let words = vec![w("시작말", 0.0, 1.5), w("다음", 1.6, 1.9), w("문장", 2.0, 2.3)];
        let mut levels = vec![-20.0; 230];
        levels[30..110].fill(-80.0);
        let proposals = probes(&words, &levels, -50.0);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].start, crate::seconds_tick(1.04));
        let mut local = Transcript { words: vec![w("시작말", 0.06, 0.46), w("다음", 0.56, 0.86), w("문장", 0.96, 1.26)], ..Default::default() };
        let range = verified_range(&words, 0, &local, crate::seconds_tick(1.04)).unwrap();
        assert_eq!(range.start, crate::seconds_tick(1.07));
        assert_eq!(range.end(), words[0].end);
        local.words[0].text = "작말".into();
        assert!(verified_range(&words, 0, &local, crate::seconds_tick(1.04)).is_none());
    }
    #[test]
    fn a_pause_or_missing_neighbors_cannot_move_a_correct_word() {
        let words = vec![w("정상말", 0.0, 1.5), w("다음", 1.6, 1.9), w("문장", 2.0, 2.3)];
        let local = Transcript { words: vec![w("다른말", 0.06, 0.46), w("다음", 0.56, 0.86), w("문장", 0.96, 1.26)], ..Default::default() };
        assert!(verified_range(&words, 0, &local, crate::seconds_tick(1.04)).is_none());
        let local = Transcript { words: vec![w("정상말", 0.06, 0.46)], ..Default::default() };
        assert!(verified_range(&words, 0, &local, crate::seconds_tick(1.04)).is_none());
    }
}
