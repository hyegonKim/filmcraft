//! Whisper speech recognition in pure Rust (candle, CPU).
//!
//! Transcription follows the procedure described with the model (sequential 30-second windows):
//!
//! 1. The whole clip's log-mel spectrogram is computed once ([`crate::mel`]), padded with 30 s of
//!    silence. A window of 3000 frames starting at `seek` is encoded.
//! 2. **Language**: unless given, the first window's decoder logits after `<|startoftranscript|>`
//!    are compared over the language tokens and the most likely language is used.
//! 3. **Decoding** is greedy with timestamp tokens: the first token must be a timestamp (≤ 1 s),
//!    timestamps come in pairs and never go backwards, timestamps win whenever their summed
//!    probability beats the best text token, and special tokens are suppressed. Windows whose
//!    `<|nospeech|>` probability exceeds 0.6 while the text is improbable are skipped. A loop
//!    guard stops a window that keeps repeating itself.
//! 4. `seek` advances to the last complete timestamp pair (or the whole window).
//! 5. **Word timestamps**: the window's text tokens are run once more through the decoder
//!    (`<|notimestamps|>` prompt) and the cross-attention logits of the model's alignment heads
//!    (`generation_config.json`; default: every head of the second half of the decoder) are
//!    softmaxed over the window's audio frames, standardised per token, median-filtered (width 7)
//!    and averaged; dynamic time warping through the negated matrix gives each token's start frame
//!    (20 ms resolution). Tokens are grouped into words at spaces, punctuation joins its word.
//!
//! 6. Word bounds are tightened past silent frames ([`crate::vad`]), so pauses stay pauses.
//!
//! Optional speaker labelling runs afterwards ([`crate::diarize`]).

mod align;
mod boundaries;
mod fillers;
mod model;
pub mod tokenizer;

use std::path::Path;
use std::sync::Mutex;

use candle_core::Tensor;
use filmcraft_project::{Transcript, Word};

use crate::{Options, ProgressFn, SpeechError, Transcriber, sample_tick};
use model::{Config, Model};
use tokenizer::Tokenizer;

/// Mel frames per window (30 s).
const N_FRAMES: usize = 3000;
/// Mel frames per timestamp step (0.02 s).
const FRAMES_PER_TS: usize = 2;
const SAMPLES_PER_FRAME: usize = crate::mel::HOP;
const MAX_TOKENS: usize = 224;

fn merr(e: candle_core::Error) -> SpeechError {
    SpeechError::Model(e.to_string())
}

pub struct Whisper {
    id: String,
    model: Mutex<Model>,
    tok: Tokenizer,
    alignment_heads: Vec<(usize, usize)>,
    suppress: Vec<u32>,
    begin_suppress: Vec<u32>,
}

impl Whisper {
    /// Load a model directory (`config.json`, `generation_config.json`, `tokenizer.json`,
    /// `model.safetensors`).
    pub fn load(dir: &Path, id: &str) -> Result<Self, SpeechError> {
        let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| SpeechError::Model(format!("{}: {e}", dir.join(n).display())));
        let cfg: Config = serde_json::from_slice(&read("config.json")?).map_err(|e| SpeechError::Model(format!("config.json: {e}")))?;
        let gen_cfg: serde_json::Value = serde_json::from_slice(&read("generation_config.json")?).unwrap_or_default();
        let tok = Tokenizer::from_json(&String::from_utf8_lossy(&read("tokenizer.json")?))?;
        let weights = read("model.safetensors")?;
        let alignment_heads = gen_cfg["alignment_heads"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| Some((p[0].as_u64()? as usize, p[1].as_u64()? as usize))).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| (cfg.decoder_layers / 2..cfg.decoder_layers).flat_map(|l| (0..cfg.decoder_attention_heads).map(move |h| (l, h))).collect());
        let mut suppress: Vec<u32> =
            gen_cfg["suppress_tokens"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|v| v as u32)).collect()).unwrap_or_default();
        suppress.extend(tok.non_text_specials());
        suppress.sort_unstable();
        suppress.dedup();
        let begin_suppress = gen_cfg["begin_suppress_tokens"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|v| v as u32)).collect())
            .unwrap_or_else(|| vec![220, tok.eot]);
        let model = Model::load(cfg, weights).map_err(merr)?;
        Ok(Self { id: id.to_string(), model: Mutex::new(model), tok, alignment_heads, suppress, begin_suppress })
    }

    pub fn languages(&self) -> Vec<String> {
        self.tok.languages.iter().map(|l| l.0.clone()).collect()
    }

    fn window(mel: &[f32], n_mels: usize, frames: usize, seek: usize) -> Vec<f32> {
        let mut w = vec![0f32; n_mels * N_FRAMES];
        for m in 0..n_mels {
            let end = (seek + N_FRAMES).min(frames);
            if seek < end {
                w[m * N_FRAMES..m * N_FRAMES + end - seek].copy_from_slice(&mel[m * frames + seek..m * frames + end]);
            }
        }
        w
    }

    fn detect_language(&self, m: &mut Model, xa: &Tensor) -> Result<String, SpeechError> {
        m.decoder.reset();
        let (logits, _) = m.decoder.forward(&[self.tok.sot], xa, false, None).map_err(merr)?;
        let l = Model::last_logits(&logits).map_err(merr)?;
        let best = self.tok.languages.iter().max_by(|a, b| l[a.1 as usize].total_cmp(&l[b.1 as usize])).map(|x| x.0.clone());
        Ok(best.unwrap_or_else(|| "en".into()))
    }

    /// Apply the timestamp rules and suppression to `logits` for the next token after `seq`
    /// (sampled tokens of this window, prompt excluded).
    fn constrain(&self, logits: &mut [f32], seq: &[u32], max_ts: Option<u32>) {
        let tb = self.tok.timestamp_begin as usize;
        let ninf = f32::NEG_INFINITY;
        for &s in &self.suppress {
            if let Some(v) = logits.get_mut(s as usize) {
                *v = ninf;
            }
        }
        if seq.is_empty() {
            for &s in &self.begin_suppress {
                if let Some(v) = logits.get_mut(s as usize) {
                    *v = ninf;
                }
            }
        }
        let last_ts = seq.last().is_some_and(|&t| self.tok.is_timestamp(t));
        let penult_ts = seq.len() < 2 || self.tok.is_timestamp(seq[seq.len() - 2]);
        if last_ts {
            if penult_ts {
                logits[tb..].iter_mut().for_each(|v| *v = ninf);
            } else {
                logits[..self.tok.eot as usize].iter_mut().for_each(|v| *v = ninf);
            }
        }
        if let Some(&last) = seq.iter().rev().find(|&&t| self.tok.is_timestamp(t)) {
            let lim = (if last_ts && !penult_ts { last } else { last + 1 } as usize).min(logits.len());
            logits[tb..lim].iter_mut().for_each(|v| *v = ninf);
        }
        if seq.is_empty() {
            logits[..tb].iter_mut().for_each(|v| *v = ninf);
            if let Some(m) = max_ts {
                let cut = (tb + m as usize + 1).min(logits.len());
                logits[cut..].iter_mut().for_each(|v| *v = ninf);
            }
        }
        // timestamps win when their total probability beats every text token
        let max = logits.iter().copied().fold(ninf, f32::max);
        if max.is_finite() {
            let lse = |s: &[f32]| -> f32 {
                let m = s.iter().copied().fold(ninf, f32::max);
                if !m.is_finite() {
                    return ninf;
                }
                m + s.iter().map(|v| (v - m).exp()).sum::<f32>().ln()
            };
            let ts = lse(&logits[tb..]);
            let best_text = logits[..tb].iter().copied().fold(ninf, f32::max);
            if ts > best_text {
                logits[..tb].iter_mut().for_each(|v| *v = ninf);
            }
        }
    }

    /// Greedy decode of one window. Returns sampled tokens (no prompt, no end-of-text), the mean
    /// log-probability and the no-speech probability.
    fn decode_window(
        &self,
        m: &mut Model,
        xa: &Tensor,
        prompt: &[u32],
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<(Vec<u32>, Vec<f32>, f32, f32), SpeechError> {
        if cancelled() {
            return Err(SpeechError::Cancelled);
        }
        m.decoder.reset();
        let mut seq: Vec<u32> = Vec::new();
        let mut token_logprobs = Vec::new();
        let (logits, _) = m.decoder.forward(prompt, xa, true, None).map_err(merr)?;
        // no-speech probability from the start-of-transcript position
        let first = logits.get(0).and_then(|l| l.get(0)).and_then(|l| l.to_vec1::<f32>()).map_err(merr)?;
        let no_speech = self.tok.no_speech.map(|ns| softmax_at(&first, ns as usize)).unwrap_or(0.0);
        let mut cur = Model::last_logits(&logits).map_err(merr)?;
        let mut sum_lp = 0f32;
        for _ in 0..MAX_TOKENS.min(m.cfg.max_target_positions / 2) {
            if cancelled() {
                return Err(SpeechError::Cancelled);
            }
            let lp = log_softmax(&cur);
            self.constrain(&mut cur, &seq, Some(50));
            let next = argmax(&cur);
            sum_lp += lp[next as usize];
            if next == self.tok.eot {
                break;
            }
            seq.push(next);
            token_logprobs.push(lp[next as usize]);
            if repeating(&seq, self.tok.timestamp_begin) {
                break;
            }
            let (logits, _) = m.decoder.forward(&[next], xa, true, None).map_err(merr)?;
            cur = Model::last_logits(&logits).map_err(merr)?;
        }
        let avg = sum_lp / (seq.len() + 1) as f32;
        Ok((seq, token_logprobs, avg, no_speech))
    }
}

fn softmax_at(x: &[f32], i: usize) -> f32 {
    let m = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let s: f32 = x.iter().map(|v| (v - m).exp()).sum();
    x.get(i).map(|v| (v - m).exp() / s).unwrap_or(0.0)
}

fn log_softmax(x: &[f32]) -> Vec<f32> {
    let m = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let lse = m + x.iter().map(|v| (v - m).exp()).sum::<f32>().ln();
    x.iter().map(|v| v - lse).collect()
}

fn argmax(x: &[f32]) -> u32 {
    let mut best = 0;
    for (i, v) in x.iter().enumerate() {
        if *v > x[best] {
            best = i;
        }
    }
    best as u32
}

/// The tail of `seq` (text tokens) repeats a short pattern many times: a decoding loop.
fn repeating(seq: &[u32], tb: u32) -> bool {
    let text: Vec<u32> = seq.iter().copied().filter(|&t| t < tb).collect();
    for n in 1..=8 {
        let reps = if n == 1 { 12 } else { 5 };
        if text.len() < n * reps {
            continue;
        }
        let tail = &text[text.len() - n * reps..];
        if (1..reps).all(|r| tail[r * n..(r + 1) * n] == tail[..n]) {
            return true;
        }
    }
    false
}

impl Transcriber for Whisper {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        self.transcribe_pass(audio, opts, progress, true)
    }
}

impl Whisper {
    fn transcribe_pass(&self, audio: &[f32], opts: &Options, progress: ProgressFn, verify: bool) -> Result<Transcript, SpeechError> {
        let mut m = self.model.lock().unwrap_or_else(|e| e.into_inner());
        let n_mels = m.cfg.num_mel_bins;
        let filler_context = if opts.preserve_fillers { self.tok.filler_context()? } else { Vec::new() };
        let content_frames = audio.len() / SAMPLES_PER_FRAME;
        if !progress(0.0, "Analysing audio") {
            return Err(SpeechError::Cancelled);
        }
        let trace = std::env::var_os("FILMCRAFT_SPEECH_TRACE").is_some();
        let clock = std::time::Instant::now();
        let mut language = opts.language.clone().filter(|l| !l.is_empty() && l != "auto");
        if language.as_deref().is_some_and(|l| self.tok.multilingual() && self.tok.language_token(l).is_none()) {
            return Err(SpeechError::Model(format!("the model does not know the language `{}`", language.unwrap_or_default())));
        }
        let mut words: Vec<Word> = Vec::new();
        let mut seek = 0usize;
        while seek < content_frames {
            let frac = seek as f32 / content_frames.max(1) as f32;
            if !progress(frac * 0.95, &format!("Transcribing {:.0}%", frac * 100.0)) {
                return Err(SpeechError::Cancelled);
            }
            let seg_frames = N_FRAMES.min(content_frames - seek);
            let start = seek * SAMPLES_PER_FRAME;
            let end = ((seek + seg_frames) * SAMPLES_PER_FRAME).min(audio.len());
            let slice = &audio[start..end];
            // Exact digital silence needs no inference; quiet speech is never discarded here.
            if slice.iter().all(|x| x.abs() <= 1e-8) {
                seek += seg_frames;
                continue;
            }
            // Bound the spectrogram allocation to one Whisper window, even for hour-long files.
            let mut padded = slice.to_vec();
            padded.resize(N_FRAMES * SAMPLES_PER_FRAME + SAMPLES_PER_FRAME, 0.0);
            let (frames, mel) = crate::mel::log_mel(&padded, n_mels);
            if !progress(frac * 0.95, "Encoding audio") {
                return Err(SpeechError::Cancelled);
            }
            let win = Self::window(&mel, n_mels, frames, 0);
            let x = Tensor::from_vec(win, (1, n_mels, N_FRAMES), &m.device).map_err(merr)?;
            let xa = m.encoder.forward(&x).map_err(merr)?;
            if trace {
                eprintln!("encode {:.3}s", clock.elapsed().as_secs_f64());
            }
            let mut prompt = vec![self.tok.sot];
            if self.tok.multilingual() {
                if language.is_none() {
                    language = Some(self.detect_language(&mut m, &xa)?);
                }
                let lang = language.as_deref().unwrap_or("en");
                prompt.push(self.tok.language_token(lang).unwrap_or(self.tok.sot + 1));
                prompt.push(self.tok.transcribe);
            }
            let mut decode_prompt = if language.as_deref() == Some("ko") { filler_context.clone() } else { Vec::new() };
            decode_prompt.extend_from_slice(&prompt);
            let (toks, logprobs, avg_lp, no_speech) = self.decode_window(&mut m, &xa, &decode_prompt, &mut || !progress(frac * 0.95, "Decoding speech"))?;
            if trace {
                eprintln!("decode {} tokens {:.3}s", toks.len(), clock.elapsed().as_secs_f64());
            }
            let skip = no_speech > 0.6 && avg_lp < -1.0;
            // where the next window starts
            let is_ts: Vec<bool> = toks.iter().map(|&t| self.tok.is_timestamp(t)).collect();
            let single_ending = is_ts.len() >= 2 && !is_ts[is_ts.len() - 2] && is_ts[is_ts.len() - 1];
            let last_pair = (1..is_ts.len()).rev().find(|&i| is_ts[i] && is_ts[i - 1]);
            let advance = match last_pair {
                Some(i) if !single_ending => ((toks[i - 1] - self.tok.timestamp_begin) as usize * FRAMES_PER_TS).clamp(1, seg_frames),
                _ => seg_frames,
            };
            let text: Vec<u32> = toks.iter().copied().filter(|&t| t < self.tok.eot).collect();
            if !skip && !text.is_empty() {
                // the text up to the last complete segment (what `advance` covers)
                let upto = match last_pair {
                    Some(i) if !single_ending => i,
                    _ => toks.len(),
                };
                let text: Vec<u32> = toks[..upto].iter().copied().filter(|&t| t < self.tok.eot).collect();
                let text_logprobs: Vec<f32> = toks[..upto].iter().zip(&logprobs).filter(|(t, _)| **t < self.tok.eot).map(|(_, lp)| *lp).collect();
                let mut text_index = 0;
                let mut segment_tokens = Vec::new();
                for (i, &token) in toks[..upto].iter().enumerate() {
                    if self.tok.is_timestamp(token) && toks.get(i + 1).is_some_and(|t| *t < self.tok.eot) {
                        segment_tokens.push(text_index);
                    }
                    if token < self.tok.eot {
                        text_index += 1;
                    }
                }
                let mut anchors = Vec::new();
                let mut open = None;
                let mut index = 0;
                for &token in &toks[..upto] {
                    if self.tok.is_timestamp(token) {
                        let frame = ((token - self.tok.timestamp_begin) as usize * FRAMES_PER_TS).min(advance);
                        if let Some((begin, at)) = open.take() {
                            if index > begin {
                                anchors.push(align::Segment { tokens: begin..index, frames: at..frame });
                            } else {
                                open = Some((index, frame));
                            }
                        } else {
                            open = Some((index, frame));
                        }
                    } else if token < self.tok.eot {
                        index += 1;
                    }
                }
                if let Some((begin, at)) = open
                    && index > begin
                {
                    anchors.push(align::Segment { tokens: begin..index, frames: at..advance });
                }
                let mut aprompt = prompt.clone();
                aprompt.push(self.tok.no_timestamps);
                if !progress(frac * 0.95, "Aligning words") {
                    return Err(SpeechError::Cancelled);
                }
                let times = align::align(&mut m, &xa, &aprompt, &text, self.tok.eot, &self.alignment_heads, advance, &anchors).map_err(merr)?;
                let offset = seek * SAMPLES_PER_FRAME;
                let limit = (seek + advance) * SAMPLES_PER_FRAME;
                let mut window_words = Vec::new();
                let mut segment_words = Vec::new();
                for (wtext, r) in tokenizer::group_words(&self.tok, &text) {
                    let spoken = tokenizer::confidence_tokens(&self.tok, &text, r.clone());
                    let (s, e) = (times[r.start].0, times[r.end - 1].1);
                    let a = (offset + s * SAMPLES_PER_FRAME).min(limit);
                    let b = (offset + e * SAMPLES_PER_FRAME).min(limit).max(a);
                    if segment_tokens.contains(&r.start) {
                        segment_words.push(window_words.len());
                    }
                    let mut word = Word::new(wtext, sample_tick(a as i64), sample_tick(b as i64));
                    word.confidence = if b > a { align::word_probability(&text_logprobs[spoken]) } else { 0.0 };
                    window_words.push(word);
                }
                align::limit_boundary_words(&mut window_words, &segment_words);
                words.extend(window_words);
                if trace {
                    eprintln!("window seek={seek} advance={advance} segment_starts={segment_words:?}");
                }
            }
            if trace {
                eprintln!("align {:.3}s", clock.elapsed().as_secs_f64());
            }
            seek += advance;
        }
        let mut t = Transcript { language: language.unwrap_or_else(|| "en".into()), source: self.id.clone(), speakers: Vec::new(), words };
        t.normalize();
        for word in &mut t.words {
            if word.end <= word.start {
                word.confidence = 0.0;
            }
        }
        crate::vad::tighten_words(audio, &mut t.words);
        if opts.diarize && !t.words.is_empty() {
            if !progress(0.96, "Labelling speakers") {
                return Err(SpeechError::Cancelled);
            }
            let p = crate::diarize::Params { max_speakers: opts.max_speakers, ..Default::default() };
            crate::diarize::diarize(audio, &mut t, &p);
        }
        drop(m);
        if verify && t.language == "ko" {
            self.refine_boundaries(audio, &mut t, opts, progress)?;
            self.check_fillers(audio, &mut t, opts, progress)?;
        }
        if !progress(1.0, "Done") {
            return Err(SpeechError::Cancelled);
        }
        Ok(t)
    }

    fn refine_boundaries(&self, audio: &[f32], t: &mut Transcript, opts: &Options, progress: ProgressFn) -> Result<(), SpeechError> {
        let levels = crate::vad::frame_db(audio);
        let original = t.words.clone();
        let probes = boundaries::probes(&original, &levels, crate::vad::threshold(&levels));
        let total = probes.len().max(1) as f32;
        for (n, probe) in probes.iter().enumerate() {
            let status = format!("Checking word boundaries {}/{}", n + 1, probes.len());
            if !progress(0.96 + 0.005 * n as f32 / total, &status) {
                return Err(SpeechError::Cancelled);
            }
            let Some(word) = original.get(probe.index) else { continue };
            let a = usize::try_from((probe.start.0 / crate::TICKS_PER_SAMPLE).max(0)).map_err(|e| SpeechError::Model(e.to_string()))?;
            let b = usize::try_from((word.end.0 / crate::TICKS_PER_SAMPLE).max(0))
                .map_err(|e| SpeechError::Model(e.to_string()))?
                .saturating_add(crate::SAMPLE_RATE as usize * 2)
                .min(audio.len());
            let Some(crop) = audio.get(a..b) else { continue };
            let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: false, ..opts.clone() };
            let local = self.transcribe_pass(crop, &options, &mut |fraction, _| progress(0.96 + 0.005 * (n as f32 + fraction) / total, &status), false)?;
            if let Some(range) = boundaries::verified_range(&original, probe.index, &local, sample_tick(a as i64)) {
                t.words[probe.index].start = range.start;
                t.words[probe.index].end = range.end();
                t.words[probe.index].confidence = t.words[probe.index].confidence.min(0.5);
            }
        }
        self.refine_ends(audio, t, opts, progress)
    }

    fn refine_ends(&self, audio: &[f32], t: &mut Transcript, opts: &Options, progress: ProgressFn) -> Result<(), SpeechError> {
        let levels = crate::vad::frame_db(audio);
        // An immutable second-stage snapshot preserves the already verified starts. The
        // prefix word must agree with that start, so it cannot choose a different occurrence.
        let original = t.words.clone();
        let end_probes = boundaries::end_probes(&original, &levels, crate::vad::threshold(&levels));
        let total = end_probes.len().max(1) as f32;
        for (n, probe) in end_probes.iter().enumerate() {
            let step = n as f32;
            let status = format!("Checking word ends {}/{}", n + 1, end_probes.len());
            if !progress(0.965 + 0.005 * step / total, &status) {
                return Err(SpeechError::Cancelled);
            }
            let Some(word) = original.get(probe.index) else { continue };
            let a = usize::try_from((word.start.0 / crate::TICKS_PER_SAMPLE).max(0))
                .map_err(|e| SpeechError::Model(e.to_string()))?
                .saturating_sub(crate::SAMPLE_RATE as usize * 2);
            let b = usize::try_from((probe.end.0 / crate::TICKS_PER_SAMPLE).max(0)).map_err(|e| SpeechError::Model(e.to_string()))?.min(audio.len());
            let Some(crop) = audio.get(a..b) else { continue };
            let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: false, ..opts.clone() };
            let local = self.transcribe_pass(crop, &options, &mut |fraction, _| progress(0.965 + 0.005 * (step + fraction) / total, &status), false)?;
            if let Some(end) =
                boundaries::verified_end(&original, probe.index, &local, sample_tick(a as i64)).and_then(|end| boundaries::guarded_end(word, end, probe.end))
            {
                t.words[probe.index].end = end;
                t.words[probe.index].confidence = t.words[probe.index].confidence.min(0.5);
            }
        }
        Ok(())
    }

    fn check_fillers(&self, audio: &[f32], t: &mut Transcript, opts: &Options, progress: ProgressFn) -> Result<(), SpeechError> {
        let levels = crate::vad::frame_db(audio);
        let speech_threshold = crate::vad::threshold(&levels);
        let targets: Vec<usize> = t.words.iter().enumerate().filter_map(|(i, w)| fillers::target(w).then_some(i)).collect();
        for &i in &targets {
            t.words[i].filler_checked = true;
            t.words[i].filler_bounds = None;
            t.words[i].filler_bounds_revision = 0;
        }
        for (n, &i) in targets.iter().take(2048).enumerate() {
            let word = &t.words[i];
            let length = word.end.0.saturating_sub(word.start.0);
            if length <= 0 || length > crate::seconds_tick(2.5).0 {
                continue;
            }
            let start_sample = (word.start.0 / crate::TICKS_PER_SAMPLE).max(0) as usize;
            let end_sample = (word.end.0 / crate::TICKS_PER_SAMPLE).max(0) as usize;
            let total = targets.len().clamp(1, 2048) as f32;
            let status = format!("Checking filler boundaries {}/{}", n + 1, targets.len().min(2048));
            let mut contradicted = None;
            for (attempt, context) in [2, 4, 1].into_iter().enumerate() {
                let a = start_sample.saturating_sub(crate::SAMPLE_RATE as usize * context);
                let b = end_sample.saturating_add(crate::SAMPLE_RATE as usize * context).min(audio.len());
                let Some(crop) = audio.get(a..b) else { continue };
                let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: true, ..opts.clone() };
                let local = self.transcribe_pass(
                    crop,
                    &options,
                    &mut |fraction, _| progress(0.97 + 0.02 * (n as f32 + (attempt as f32 + fraction) / 3.0) / total, &status),
                    false,
                )?;
                let offset = sample_tick(a as i64);
                if let Some((previous, previous_offset)) = contradicted.take() {
                    if let Some((bounds, padding)) = fillers::recovered_bounds(&t.words, i, &previous, previous_offset, &local, offset) {
                        t.words[i].filler_bounds = fillers::audible_bounds(&levels, speech_threshold, bounds, padding);
                    }
                    break;
                }
                if attempt > 0 && !fillers::context_matches(&t.words, i, &local, offset) {
                    continue;
                }
                if let Some(bounds) = fillers::agreed_bounds(&t.words[i], &local, offset) {
                    t.words[i].filler_bounds = fillers::audible_bounds(&levels, speech_threshold, bounds, fillers::word_padding(&t.words[i]));
                    break;
                }
                // A primary timing contradiction needs agreement from the next fixed crop,
                // including both ordinary neighbors. Never shop across further contexts.
                if local.words.iter().any(|w| w.normalized() == t.words[i].normalized()) {
                    if fillers::context_matches(&t.words, i, &local, offset) {
                        contradicted = Some((local, offset));
                    } else {
                        break;
                    }
                }
            }
            if t.words[i].filler_bounds.is_some() {
                t.words[i].filler_bounds_revision = filmcraft_project::transcript::FILLER_BOUNDS_REVISION;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repetition_guard() {
        let tb = 1000;
        assert!(repeating(&[5; 12], tb));
        assert!(!repeating(&[5, 5, 5, 6, 7, 8], tb));
        let mut s = Vec::new();
        for _ in 0..5 {
            s.extend([1, 2, 3]);
        }
        assert!(repeating(&s, tb));
        assert!(!repeating(&[1, 2, 3, 4, 5, 6, 7], tb));
    }
}

#[cfg(test)]
mod local_filler_probe {
    use super::*;
    #[test]
    #[ignore = "local model diagnostic; media and transcripts stay outside the repository"]
    fn cropped_audio_filler_diagnostic() {
        let audio_path = std::env::var("FILMCRAFT_PROBE_AUDIO").unwrap();
        let transcript_path = std::env::var("FILMCRAFT_PROBE_TRANSCRIPT").unwrap();
        let output = std::path::PathBuf::from(std::env::var("FILMCRAFT_PROBE_OUTPUT").unwrap());
        std::fs::create_dir_all(&output).unwrap();
        let raw = std::fs::read(audio_path).unwrap();
        let audio: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let transcript: Transcript = serde_json::from_slice(&std::fs::read(transcript_path).unwrap()).unwrap();
        let model = Whisper::load(&std::path::PathBuf::from(std::env::var("FILMCRAFT_PROBE_MODEL").unwrap()), "whisper-small").unwrap();
        let context = std::env::var("FILMCRAFT_PROBE_CONTEXT_SECONDS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2).clamp(1, 4);
        for (i, w) in transcript.words.iter().enumerate().filter(|(_, w)| fillers::target(w)) {
            let a = ((w.start.0 / crate::TICKS_PER_SAMPLE).max(0) as usize).saturating_sub(crate::SAMPLE_RATE as usize * context);
            let b = ((w.end.0 / crate::TICKS_PER_SAMPLE).max(0) as usize).saturating_add(crate::SAMPLE_RATE as usize * context).min(audio.len());
            let Some(crop) = audio.get(a..b) else { continue };
            let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: true, ..Default::default() };
            let local = model.transcribe_pass(crop, &options, &mut |_, _| true, false).unwrap();
            let result = serde_json::json!({"index":i,"original":w,"offset":sample_tick(a as i64),"local":local,"agreedBounds":fillers::agreed_bounds(w,&local,sample_tick(a as i64))});
            std::fs::write(output.join(format!("{i}.json")), serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        }
    }
}

#[cfg(test)]
mod local_filler_recheck {
    use super::*;
    #[test]
    #[ignore = "replay actual verified model endpoints through the new deterministic acoustic guard"]
    fn replay_saved_word_end_guard() {
        let raw = std::fs::read(std::env::var("FILMCRAFT_PROBE_AUDIO").unwrap()).unwrap();
        let audio: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let original: Transcript = serde_json::from_slice(&std::fs::read(std::env::var("FILMCRAFT_PROBE_ORIGINAL").unwrap()).unwrap()).unwrap();
        let mut t: Transcript = serde_json::from_slice(&std::fs::read(std::env::var("FILMCRAFT_PROBE_TRANSCRIPT").unwrap()).unwrap()).unwrap();
        let levels = crate::vad::frame_db(&audio);
        let probes = boundaries::end_probes(&original.words, &levels, crate::vad::threshold(&levels));
        let mut changed = Vec::new();
        assert_eq!(original.words.len(), t.words.len());
        for (i, (before, after)) in original.words.iter().zip(&mut t.words).enumerate() {
            assert_eq!(before.text, after.text);
            assert_eq!(before.start, after.start);
            if before.end == after.end {
                continue;
            }
            let probe = probes.iter().find(|p| p.index == i).unwrap();
            if let Some(end) = boundaries::guarded_end(before, after.end, probe.end) {
                after.end = end;
            } else {
                after.end = before.end;
                after.confidence = before.confidence;
            }
            changed.push(i);
        }
        // Cached filler checks remain valid only when none of their context anchors or
        // approved ordinary-word overlap checks changed. Otherwise re-decode those fillers.
        for (i, word) in t.words.iter().enumerate().filter(|(_, w)| fillers::target(w)) {
            let before = original.words[..i].iter().enumerate().rev().filter(|(_, w)| !fillers::target(w) && w.end > w.start).take(3);
            let after = original.words.iter().enumerate().skip(i + 1).filter(|(_, w)| !fillers::target(w) && w.end > w.start).take(3);
            assert!(before.chain(after).all(|(j, _)| !changed.contains(&j)), "changed filler anchor needs a new local decode");
            if let Some(bounds) = word.filler_bounds {
                assert!(changed.iter().all(|j| t.words[*j].start >= bounds.end() || t.words[*j].end <= bounds.start));
            }
        }
        std::fs::write(std::env::var("FILMCRAFT_PROBE_OUTPUT").unwrap(), serde_json::to_vec_pretty(&t).unwrap()).unwrap();
    }
    #[test]
    #[ignore = "private real-audio boundary and filler pipeline, retaining primary recognition text"]
    fn recheck_saved_boundaries_and_fillers() {
        let raw = std::fs::read(std::env::var("FILMCRAFT_PROBE_AUDIO").unwrap()).unwrap();
        let audio: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let mut transcript: Transcript = serde_json::from_slice(&std::fs::read(std::env::var("FILMCRAFT_PROBE_TRANSCRIPT").unwrap()).unwrap()).unwrap();
        let before = transcript.words.clone();
        let model = Whisper::load(&std::path::PathBuf::from(std::env::var("FILMCRAFT_PROBE_MODEL").unwrap()), "whisper-small").unwrap();
        let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: true, ..Default::default() };
        let mut last = String::new();
        let mut progress = |_: f32, status: &str| {
            if last != status {
                eprintln!("{status}");
                last = status.to_string();
            }
            true
        };
        // Build13 already verified starts; rerun exactly the new second stage, not an
        // additional suffix-repair pass that would differ from fresh transcription.
        model.refine_ends(&audio, &mut transcript, &options, &mut progress).unwrap();
        model.check_fillers(&audio, &mut transcript, &options, &mut progress).unwrap();
        assert_eq!(before.len(), transcript.words.len());
        for (a, b) in before.iter().zip(&transcript.words) {
            assert_eq!(a.text, b.text);
            assert!(b.start >= a.start && b.end <= a.end);
        }
        std::fs::write(std::env::var("FILMCRAFT_PROBE_OUTPUT").unwrap(), serde_json::to_vec_pretty(&transcript).unwrap()).unwrap();
    }
    #[test]
    #[ignore = "recheck private real-audio evaluation without repeating primary decoding"]
    fn recheck_saved_transcript_fillers() {
        let raw = std::fs::read(std::env::var("FILMCRAFT_PROBE_AUDIO").unwrap()).unwrap();
        let audio: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let mut transcript: Transcript = serde_json::from_slice(&std::fs::read(std::env::var("FILMCRAFT_PROBE_TRANSCRIPT").unwrap()).unwrap()).unwrap();
        let before: Vec<_> = transcript.words.iter().map(|w| (w.text.clone(), w.start, w.end)).collect();
        let model = Whisper::load(&std::path::PathBuf::from(std::env::var("FILMCRAFT_PROBE_MODEL").unwrap()), "whisper-small").unwrap();
        let options = Options { language: Some("ko".into()), diarize: false, preserve_fillers: true, ..Default::default() };
        model.check_fillers(&audio, &mut transcript, &options, &mut |_, _| true).unwrap();
        assert_eq!(before, transcript.words.iter().map(|w| (w.text.clone(), w.start, w.end)).collect::<Vec<_>>());
        std::fs::write(std::env::var("FILMCRAFT_PROBE_OUTPUT").unwrap(), serde_json::to_vec_pretty(&transcript).unwrap()).unwrap();
    }
}

#[cfg(test)]
mod local_boundary_probe {
    use super::*;
    #[test]
    #[ignore = "private real-audio diagnostic; no evaluation labels are supplied to the model"]
    fn cropped_audio_boundary_diagnostic() {
        let raw = std::fs::read(std::env::var("FILMCRAFT_PROBE_AUDIO").unwrap()).unwrap();
        let audio: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let t: Transcript = serde_json::from_slice(&std::fs::read(std::env::var("FILMCRAFT_PROBE_TRANSCRIPT").unwrap()).unwrap()).unwrap();
        let index: usize = std::env::var("FILMCRAFT_PROBE_WORD_INDEX").unwrap().parse().unwrap();
        let levels = crate::vad::frame_db(&audio);
        let probe = boundaries::probes(&t.words, &levels, crate::vad::threshold(&levels)).into_iter().find(|p| p.index == index).unwrap();
        let a = (probe.start.0 / crate::TICKS_PER_SAMPLE).max(0) as usize;
        let b = ((t.words[index].end.0 / crate::TICKS_PER_SAMPLE).max(0) as usize).saturating_add(crate::SAMPLE_RATE as usize * 2).min(audio.len());
        let model = Whisper::load(&std::path::PathBuf::from(std::env::var("FILMCRAFT_PROBE_MODEL").unwrap()), "whisper-small").unwrap();
        let local = model
            .transcribe_pass(
                &audio[a..b],
                &Options { language: Some("ko".into()), diarize: false, preserve_fillers: false, ..Default::default() },
                &mut |_, _| true,
                false,
            )
            .unwrap();
        let offset = sample_tick(a as i64);
        let result = serde_json::json!({"index":index,"original":t.words[index],"cropStart":offset,"local":local,"verifiedRange":boundaries::verified_range(&t.words,index,&local,offset)});
        std::fs::write(std::env::var("FILMCRAFT_PROBE_OUTPUT").unwrap(), serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }
}
