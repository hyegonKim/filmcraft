//! Decoding Whisper token ids to text from the model's `tokenizer.json` (byte-level BPE: each
//! vocabulary entry is a string of "printable" stand-in characters, one per byte, using the
//! GPT-2 byte↔unicode table; special tokens are listed separately as added tokens).
//!
//! Only decoding is needed (Whisper produces tokens; we never encode text), so the BPE merges are
//! not used.

use std::collections::HashMap;

use crate::SpeechError;

pub struct Tokenizer {
    /// Bytes of each regular token, by id.
    bytes: Vec<Vec<u8>>,
    /// Special (added) tokens by content and by id.
    special: HashMap<String, u32>,
    special_by_id: HashMap<u32, String>,
    pub eot: u32,
    pub sot: u32,
    pub transcribe: u32,
    pub translate: Option<u32>,
    pub no_timestamps: u32,
    pub no_speech: Option<u32>,
    /// `<|0.00|>`; every later id is a timestamp in 0.02 s steps.
    pub timestamp_begin: u32,
    /// Language tokens (`"en"` → id), multilingual models only.
    pub languages: Vec<(String, u32)>,
}

/// GPT-2's byte → printable character table, inverted.
fn unicode_to_byte() -> Result<HashMap<char, u8>, SpeechError> {
    let mut bs: Vec<u32> = (b'!' as u32..=b'~' as u32).chain(0xA1..=0xAC).chain(0xAE..=0xFF).collect();
    let mut cs = bs.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    bs.iter()
        .zip(&cs)
        .map(|(&b, &c)| char::from_u32(c).map(|ch| (ch, b as u8)).ok_or_else(|| SpeechError::Model("invalid byte-to-unicode table".into())))
        .collect()
}

impl Tokenizer {
    pub fn from_json(json: &str) -> Result<Self, SpeechError> {
        let bad = |m: &str| SpeechError::Model(format!("tokenizer.json: {m}"));
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| bad(&e.to_string()))?;
        let vocab = v["model"]["vocab"].as_object().ok_or_else(|| bad("no model.vocab"))?;
        let table = unicode_to_byte()?;
        let max = vocab.values().filter_map(|x| x.as_u64()).max().unwrap_or(0) as usize;
        let mut bytes = vec![Vec::new(); max + 1];
        for (tok, id) in vocab {
            let id = id.as_u64().ok_or_else(|| bad("vocab id"))? as usize;
            bytes[id] = tok.chars().filter_map(|c| table.get(&c).copied()).collect();
        }
        let mut special = HashMap::new();
        let mut special_by_id = HashMap::new();
        for a in v["added_tokens"].as_array().ok_or_else(|| bad("no added_tokens"))? {
            let (Some(id), Some(c)) = (a["id"].as_u64(), a["content"].as_str()) else { continue };
            special.insert(c.to_string(), id as u32);
            special_by_id.insert(id as u32, c.to_string());
        }
        let get = |k: &str| special.get(k).copied();
        let need = |k: &str| get(k).ok_or_else(|| bad(&format!("no {k}")));
        let sot = need("<|startoftranscript|>")?;
        let translate = get("<|translate|>");
        let mut languages: Vec<(String, u32)> = special
            .iter()
            .filter(|(k, id)| **id > sot && translate.is_none_or(|t| **id < t) && k.starts_with("<|") && k.ends_with("|>"))
            .map(|(k, id)| (k[2..k.len() - 2].to_string(), *id))
            .collect();
        languages.sort_by_key(|x| x.1);
        Ok(Self {
            eot: need("<|endoftext|>")?,
            sot,
            transcribe: need("<|transcribe|>")?,
            translate,
            no_timestamps: need("<|notimestamps|>")?,
            no_speech: get("<|nospeech|>").or_else(|| get("<|nocaptions|>")),
            timestamp_begin: need("<|0.00|>")?,
            languages,
            bytes,
            special,
            special_by_id,
        })
    }

    /// Fixed, bounded Korean context, not a general text encoder. Every selected vocabulary
    /// piece is validated against its bytes; no additional tokenizer dependency is needed.
    pub fn filler_context(&self) -> Result<Vec<u32>, SpeechError> {
        let previous = self.special.get("<|startofprev|>").copied().ok_or_else(|| SpeechError::Model("missing previous-context token".into()))?;
        let mut out = vec![previous];
        for chunk in [" 음", ",", " 어", ",", " 으음", ",", " 어어", "."] {
            let mut remaining = chunk.as_bytes();
            while !remaining.is_empty() {
                let (id, bytes) = self
                    .bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| !b.is_empty() && remaining.starts_with(b))
                    .max_by_key(|(_, b)| b.len())
                    .ok_or_else(|| SpeechError::Model("cannot encode filler context".into()))?;
                out.push(id as u32);
                remaining = &remaining[bytes.len()..];
            }
        }
        Ok(out)
    }

    pub fn multilingual(&self) -> bool {
        !self.languages.is_empty()
    }

    pub fn language_token(&self, code: &str) -> Option<u32> {
        self.languages.iter().find(|(c, _)| c == code).map(|x| x.1)
    }

    pub fn is_timestamp(&self, id: u32) -> bool {
        id >= self.timestamp_begin
    }

    /// Bytes of a regular token (empty for special tokens).
    pub fn token_bytes(&self, id: u32) -> &[u8] {
        if self.special_by_id.contains_key(&id) {
            return &[];
        }
        self.bytes.get(id as usize).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn decode(&self, ids: &[u32]) -> String {
        let b: Vec<u8> = ids.iter().flat_map(|&i| self.token_bytes(i).iter().copied()).collect();
        String::from_utf8_lossy(&b).into_owned()
    }

    /// Ids of all special tokens that must never be sampled as text (everything special except
    /// end-of-text and the timestamps).
    pub fn non_text_specials(&self) -> Vec<u32> {
        self.special.values().copied().filter(|&id| id != self.eot && id < self.timestamp_begin).collect()
    }
}

/// Group text tokens into words: a token whose text starts with a space starts a new word;
/// punctuation-only tokens join the word before them (opening quotes/brackets the word after).
/// Returns (word text, token index range).
pub fn group_words(tok: &Tokenizer, ids: &[u32]) -> Vec<(String, std::ops::Range<usize>)> {
    let mut groups: Vec<(Vec<u8>, std::ops::Range<usize>)> = Vec::new();
    let mut pending_open: Option<(Vec<u8>, usize)> = None;
    for (i, &id) in ids.iter().enumerate() {
        let b = tok.token_bytes(id);
        if b.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(b);
        let trimmed = text.trim();
        let starts_space = b[0] == b' ';
        let punct = !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_punctuation() || "“”‘’«»¿¡…。，！？：、".contains(c));
        let opening = punct && trimmed.chars().all(|c| "\"'“‘¿¡([{«-".contains(c)) && starts_space;
        if opening {
            let start = pending_open.as_ref().map(|p| p.1).unwrap_or(i);
            let mut acc = pending_open.take().map(|p| p.0).unwrap_or_default();
            acc.extend_from_slice(b);
            pending_open = Some((acc, start));
            continue;
        }
        if let Some((mut pre, start)) = pending_open.take() {
            pre.extend_from_slice(b);
            groups.push((pre, start..i + 1));
            continue;
        }
        match groups.last_mut() {
            Some(g) if !starts_space => {
                g.0.extend_from_slice(b);
                g.1.end = i + 1;
            }
            Some(g) if punct => {
                // " ," style punctuation with a space still belongs to the previous word
                g.0.extend_from_slice(b);
                g.1.end = i + 1;
            }
            _ => groups.push((b.to_vec(), i..i + 1)),
        }
    }
    if let Some((pre, start)) = pending_open {
        groups.push((pre, start..ids.len()));
    }
    groups.into_iter().map(|(b, r)| (String::from_utf8_lossy(&b).trim().to_string(), r)).filter(|(t, _)| !t.is_empty()).collect()
}

/// Tokens used for lexical confidence (text and alignment retain punctuation).
pub fn confidence_tokens(tok: &Tokenizer, ids: &[u32], range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let quiet = |i: usize| {
        ids.get(i).is_some_and(|id| {
            std::str::from_utf8(tok.token_bytes(*id))
                .is_ok_and(|s| !s.is_empty() && s.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation() || "“”‘’«»¿¡…。，！？：、".contains(c)))
        })
    };
    let mut spoken = range.clone();
    while spoken.start < spoken.end && quiet(spoken.start) {
        spoken.start += 1;
    }
    while spoken.end > spoken.start && quiet(spoken.end - 1) {
        spoken.end -= 1;
    }
    if spoken.is_empty() { range } else { spoken }
}

#[cfg(test)]
mod byte_table_tests {
    #[test]
    fn fixed_korean_context_round_trips_without_model_files() {
        use std::collections::HashMap;
        let t = super::Tokenizer {
            bytes: (0..=255u8).map(|b| vec![b]).collect(),
            special: HashMap::from([("<|startofprev|>".to_string(), 256)]),
            special_by_id: HashMap::new(),
            eot: 257,
            sot: 258,
            transcribe: 259,
            translate: None,
            no_timestamps: 260,
            no_speech: None,
            timestamp_begin: 261,
            languages: Vec::new(),
        };
        let tokens = t.filler_context().unwrap();
        assert_eq!(tokens[0], 256);
        assert_eq!(t.decode(&tokens[1..]), " 음, 어, 으음, 어어.");
        assert!(tokens.len() < 64);
    }

    #[test]
    fn punctuation_and_space_tokens_do_not_inflate_lexical_confidence() {
        let t = super::Tokenizer {
            bytes: (0..=255u8).map(|b| vec![b]).collect(),
            special: Default::default(),
            special_by_id: Default::default(),
            eot: 257,
            sot: 258,
            transcribe: 259,
            translate: None,
            no_timestamps: 260,
            no_speech: None,
            timestamp_begin: 261,
            languages: Vec::new(),
        };
        let ids: Vec<_> = " hello.".bytes().map(u32::from).collect();
        let words = super::group_words(&t, &ids);
        assert_eq!(words[0].0, "hello.");
        assert_eq!(super::confidence_tokens(&t, &ids, words[0].1.clone()), 1..6);
        let ids: Vec<_> = " 음,".bytes().map(u32::from).collect();
        assert_eq!(super::confidence_tokens(&t, &ids, 0..ids.len()), 1..4);
    }

    #[test]
    fn unicode_table_covers_every_byte_without_panicking() {
        let table = super::unicode_to_byte().unwrap();
        assert_eq!(table.len(), 256);
        let mut bytes: Vec<u8> = table.values().copied().collect();
        bytes.sort();
        assert_eq!(bytes, (0..=255u8).collect::<Vec<_>>());
    }
}
