use filmcraft_project::{Transcript, Word};
use filmcraft_time::{Tick, TimeRange};

#[test]
fn invalid_imported_filler_bounds_are_discarded_without_overflow() {
    let mut word = Word::new("음", Tick::ZERO, Tick::from_seconds_f64(1.0));
    word.filler_checked = true;
    word.filler_bounds = Some(TimeRange { start: Tick(i64::MAX), duration: Tick(i64::MAX) });
    let mut transcript = Transcript { words: vec![word], ..Default::default() };
    transcript.normalize();
    assert!(transcript.words[0].filler_bounds.is_none());
    transcript.check().unwrap();
}

#[test]
fn legacy_word_json_round_trips_without_fabricated_verification() {
    let value = serde_json::json!({"text":"음","start":0,"end":1000});
    let word: Word = serde_json::from_value(value.clone()).unwrap();
    assert!(!word.filler_checked && word.filler_bounds.is_none());
    assert_eq!(serde_json::to_value(word).unwrap(), value);
}
