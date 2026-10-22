# Transcripts and text-based editing

FilmCraft's Text panel ▸ **Transcript** tab shows the dialogue of the open sequence as text. Select
words to mark In/Out, then extract or lift them; remove filler words and long pauses in one step;
turn the transcript into captions. Every action is an engine command (`transcript.*`), so the CLI,
the control channel and MCP agents can do the same.

## Model

- A **transcript** belongs to a media item (`Project::transcripts`, saved in the `.fcproj` since
  schema v9). It lists **words** with media-time bounds (`Tick`s), an optional speaker index and a
  confidence, plus the speaker names and the language. Because the times are media time, the
  transcript stays valid however the clip is trimmed, moved, sped up or reused.
- The **sequence transcript** is derived, never stored (`filmcraft_edit::transcript::sequence_words`):
  audio tracks are read top first; a word is heard through the first enabled clip whose range
  covers the word's midpoint (duplicates of the same dialogue on lower tracks read once); disabled,
  reversed and frame-hold clips contribute nothing.
- Speaker names come from the clip transcripts, so renaming "Speaker 1" in every transcript renames
  it across the sequence.

## Commands

| Command | What it does |
|---|---|
| `transcript.generate` | Transcribe media items (`items`, else the Project selection, else the media of the sequence's audio clips). Params: `model` (preferences default), `language` (`auto` = detect), `diarize`, `maxSpeakers`, `wait` (false). Returns a background job; commits one undo step on success. |
| `transcript.set` | Store a transcript you bring (JSON: `language`, `speakers`, `words` with `text`/`start`/`end`/`speaker`); it is sorted and made well formed. |
| `transcript.delete` | Remove transcripts. |
| `transcript.inspect` | The sequence transcript: words (index, text, sequence times, speaker, clip), paragraphs, speakers, the word at the playhead. |
| `transcript.search` | Word-index ranges matching a phrase (case and punctuation ignored; the last word may be a prefix). |
| `transcript.select` | Mark In/Out around words `from..=to` (frame-snapped outward) and move the playhead there. |
| `transcript.extract` / `transcript.lift` | Extract (ripple) or lift the words' frames on the targeted tracks. |
| `transcript.renameSpeaker` | Rename a speaker by name (every transcript) or by index in one `item`. |
| `transcript.removeFillers` | Ripple-delete filler words (`fillers`, default um/uh/erm/…; phrases such as "you know" allowed). |
| `transcript.removePauses` | Ripple-delete pauses longer than `minSeconds`, keeping `keepSeconds` of air on both sides. |
| `transcript.createCaptions` | Lay the words out as captions on a new caption track (`maxChars`, `lines`, `minSeconds`, `maxSeconds`, `gapFrames`). |
| `transcript.models` / `transcript.downloadModel` | List the speech models (size, licence, installed) / download one. |

## Speech recognition

Recognition goes through the `Transcriber` trait (`crates/speech`). The built-in recogniser is
OpenAI's Whisper, run in pure Rust on [candle](https://github.com/huggingface/candle) on the CPU,
with timestamp decoding, language detection and word times from cross-attention alignment (see the
`filmcraft_speech::whisper` module docs). Speakers are labelled by clustering per-chunk MFCC
statistics (`filmcraft_speech::diarize`); no model is involved.

Both are **optional features**, off by default and never built for the web:

- `whisper` (on `filmcraft-speech`, `filmcraft-engine`, and the `filmcraft` / `filmcraft-cli`
  apps, where it also enables downloads): candle inference.
- `download` (`speech-download` on the engine): HTTPS downloads with rustls + RustCrypto and the
  operating system's certificate verifier.

Without `whisper`, and with no recogniser installed, `transcript.generate` and Transcribe Sequence
are disabled, with "speech-to-text is not available in this build" as the reason (`describe`,
`command_list {"enabled_only": true}` and the menus show it); with Automatically transcribe clips
on, `file.import` reports the same reason as a `transcription: …` entry in its `errors`.
Without `speech-download`, `transcript.downloadModel` is disabled the same way. Transcripts can
still be imported with `transcript.set` and edited with every other command. Hosts and tests can
install any recogniser in `Session::transcriber`, which enables transcription in any build.

### Models

Weights are **never** bundled or committed. They are downloaded on request into
`<data dir>/models/<id>/` (see `filmcraft_engine::autosave::default_data_dir`), each file pinned to a
revision of OpenAI's Hugging Face repositories and checked against its SHA-256:

| Id | Languages | Licence |
|---|---|---|
| `whisper-tiny` | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-base` (default) | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-small` | multilingual | MIT (OpenAI); HF conversion Apache-2.0 |

### Testing

Unit and engine tests use a fake `Transcriber` (`FixedTranscriber`), so CI needs no model. The
end-to-end test `crates/speech/tests/whisper_model.rs` (feature `whisper`) runs only when weights are
in `target/models/<id>/` (or `$FILMCRAFT_MODELS_DIR`) and speech samples (mono 16 kHz f32 with a
reference `.txt`) are in `target/fixtures/speech/`; it reports the word error rate and otherwise
prints SKIPPED. Measured on 2026-10-01: `whisper-tiny`, English, 12.2 % WER over 797 words of
the local speech samples (LibriSpeech read speech and dialogue clips) (about 9.5 minutes for the run in a release build on an
Apple-silicon laptop CPU).

## Limits

- Native builds transcribe on a background worker with progress, Cancel and Retry. `wait:true` and wasm hosts use synchronous execution.
- Track items that refer to a subclip are looked up by the subclip's id, so a transcript made for
  the parent media is not shown through subclip clips yet.

## Reviewed Korean cleanup (local development build)

Build both apps with `--features whisper` to enable local automatic transcription. In Text →
Transcript, **Review pauses / fillers** opens the review window. Download a multilingual model
(base approximately 293 MB; small approximately 970 MB), then **Transcribe Korean**. Models stay
local; inference uses the CPU. Transcription uses the background job controls described below.

**Find candidates** lists word-timestamp gaps and exact filler phrases. They are suggestions,
not acoustic proof of silence: untranslated speech, background music or other speakers can occur
in a gap. Listen to each candidate (with 250 ms of context on either side), then check only the
cuts you want. Nothing is checked automatically. The conservative Korean default is `음, 어,
으음, 어어`; edit the comma-separated list to suit the recording. The pause threshold and retained
breathing space on each side are adjustable.

**Apply selected** ripple-deletes the checked candidates together. One Undo restores every cut,
including linked audio/video and transcripts. A project change invalidates the preview; locked
tracks affected by a ripple cut reject the whole operation. Save and export use the existing commands.

- `transcript.previewCleanup {minSeconds?, keepSeconds?, fillers?}` is read-only and returns a
  `token`, normalized `params`, and `candidates` (`id`, `kind`, `text`, `start`, `end` in ticks).
- `transcript.applyCleanup {token, params, selected:[id]}` recomputes candidates and rejects stale
  previews, altered parameters, duplicate/unknown ids, and locked tracks affected by the ripple before any
  edit. The normalized `params` and `token` must come from the preview.
- `playback.previewRange {start, end}` is a UI command for bounded audition; it does not alter the
  project or add an undo step.

Engine regression tests use known Korean word boundaries to verify cut correctness independently
of recognition quality. The real-model verification uses an original, locally synthesized Korean
speech fixture, then checks project save/reopen and audio/video export with ffprobe/ffmpeg as
external test oracles. Test media belongs in `target/fixtures`, not in the repository.

To rebuild and reproduce on this Mac (workspace MSRV: Rust 1.95):

```sh
cargo +1.95.0 build -p filmcraft -p filmcraft-cli --features whisper
python3 scripts/verify_korean_cleanup.py target/fixtures/korean-speech.mp4
```

The optional `--transcribed-project` argument resumes edit/export checks using a previously
verified transcript; the final fresh run must omit it to exercise actual recognition.

## Korean narration on macOS (local build)

Build with `--features whisper,macos-voices`. The review window's **Open Text to Speech** opens
the narration panel with Korean / Yuna selected; alternatively use Window → Text to Speech,
choose Korean, and write a script. Preview, add to the timeline, and select the resulting clip
to edit it. The existing `tts.*` commands handle one-step undo, WAV persistence and export.

The optional adapter invokes only macOS's `/usr/bin/say`, without a shell, in a private temporary
directory. No voice files or external application are bundled; Yuna must be installed in macOS.
Original text is never uploaded. `macos-yuna` (Korean) and `macos-samantha` (English) support pace, vocal pitch and `[pause 1s]` markers.
It is not a cross-platform Korean neural voice, emotion controller or voice-cloning feature.

See [the Korean comparison and development priorities](korean-feature-comparison.md) for the
video's exact scope, separate Premiere/Audition/Firefly references and validation boundaries.
Run `python3 scripts/verify_korean_tts.py` after building to exercise narration preview, creation,
editing, undo/redo, project save/reopen and H.264/AAC export using the installed voice.

Use `python3 scripts/verify_korean_tts.py --voice macos-samantha` to verify the English narration path.


## Background transcription (P1)

`transcript.generate` and `sequence.transcribe` return `{job}` immediately on native builds.
The Transcript tab and cleanup review dialog show progress, status, Cancel and (after a failure
or cancellation) Retry. `transcript.jobs` returns the same lifecycle for automation; `jobs.list`
and `jobs.cancel {job}` also work. `transcript.retry {job}` freezes the original media targets,
language/model and speaker options; selection changes do not redirect the retry. Only one
transcription runs at a time. Jobs live in the session and are not saved in projects.

Decoding checks cancellation every five seconds of source audio, Whisper checks it between
windows and decoder tokens. Individual model loading, encoding and alignment calls must finish
before their worker can stop; the cancellation request itself does not block the UI. No partial
transcript is committed. Completion applies all results together as one **Transcribe** undo step.
Changing documents cancels outstanding work. Changing a source item or its transcript while
analysis runs rejects that result rather than overwriting the edit; retry analyses the current
source. Unrelated timeline edits are retained.

Whisper computes one 30-second spectrogram at a time and skips only exact digital silence
(absolute samples <= 1e-8), not a speech/noise threshold. Mono audio is decoded in cancellable
five-second blocks; the complete mono buffer is retained for word tightening and speaker
labelling. Input duration is capped at four hours per item with fallible buffer reservation.
For CLI scripts that require immediate transcript results, use `wait:true`; this returns the
previous `{items, skipped}` report plus `job`. Browser/wasm hosts still use synchronous processing.

Reproducible long-file QA: `scripts/make_long_korean_fixtures.py` creates original synthetic
10/60-minute videos; `scripts/verify_long_transcription.py` exercises the actual desktop control
channel and installed local Whisper-small model. Its results live in `target/verification-p1/`.
These sparse speech fixtures test long-file lifecycle and preservation, not continuous speech
throughput or real Korean recording accuracy.

## Real Korean recordings and timing safeguards

The review dialog has an opt-in **Include spoken fillers (experimental)** checkbox
(`text.review.preserveFillers`). `preserveFillers:true` on `transcript.generate` or
`sequence.transcribe` supplies a fixed Korean previous-context prompt for the four default
filler forms. It is off by default, only affects resolved Korean decoding, and is frozen into
Retry along with the original language/model options. It can change omissions and hallucinations;
it does not classify whether a response is semantically expendable. Listen before selecting cuts.

Candidate rows include surrounding transcript text and a reported model score. The score is the
geometric mean of the word's token probabilities, not calibrated recognition accuracy or proof of
safe cutting. Gross boundary-duration repairs cap it at 0.5; zero-duration words report 0.0.
`transcript.inspect` also returns `timingValid` for each word. A zero-duration word remains visible
but cannot select, extract or lift an invented frame. Pause and filler cuts avoid invalid timing
boundaries. Correct the word's timing before using it for an edit.

Word alignment now resolves tied DTW predecessors to a minimum-cost path. An additional guard
limits grossly overlong words at segment/sentence starts, excluding sustained known fillers.
This reduces one failure mode; it is not speaker-aware forced alignment and does not establish
exact word boundaries. Exact lexical matching still needs manual review, particularly Korean
`어`/`음` that can form meaningful responses. The previous synthetic QA is separate from the
[real-interview evaluation and its scope](real-korean-validation.md).

### Locally checked Korean filler intervals and bounded transcript layout

Decoder timestamp segments now constrain the word-alignment DTW search; incomplete/invalid anchors
fall back to full-window alignment. This improves measured boundaries but leaves uncertain words.

For automatic Korean Whisper transcripts, filler edits require a separately decoded crop to contain
the same default target word (음/어/으음/어어), with both boundaries within 300 ms of the primary
pass (100 ms when either model score is below 0.5). For brief words, both durations must
be at most 300 ms and both boundaries must agree within 40 ms. The approved interval is
their intersection with **source audio** retained at each edge: 80 ms for this brief class,
100 ms otherwise, rounded inward to sequence frames. Both passes use the same model: agreement is neither an
independent-model judgement nor evidence that a meaningful response should be removed. Model scores
are not calibrated: a low value only tightens the timing requirement and never establishes safety. At most 2048 targets are reconsidered;
zero-duration or over-2.5-second words receive no approved interval.

Project words persist optional `filler_checked` bookkeeping and `filler_bounds` (a media-time
`TimeRange`), and `filler_bounds_revision` (current policy: 4). `transcript.inspect` exposes
`fillerChecked`, `fillerBounds`, and `fillerBoundsRevision`. Previous-policy approvals require rechecking. A checked
flag alone never authorizes an edit. Missing bounds, including legacy Korean Whisper transcripts,
or an older bounds revision produce review-only suggestions (`actionable:false`); both `applyCleanup` and `removeFillers`
refuse to cut them. Retranscription is needed to obtain checked intervals. Imported/manual
transcripts retain their existing editing behavior. Multiple automatic Korean words cannot be
combined into one phrase cut: checking the individual words does not check the intervening audio.
Select separately confirmed single-word candidates instead.

The Text panel caches the sequence-word mapping by project identity and active sequence, and lays
out up to 500 words per page. Previous/Next/Last and first-search-match navigation retain global
word indexes. Undo/Redo and project/sequence changes invalidate the cache. The engine, captions,
saved project and export still use the complete transcript.


### Korean filler recovery and UI responsiveness (build 20261016.13)

The first local decode uses two seconds of neighboring context. A missing filler may
trigger fixed four-second and then one-second contexts, subject to the ordinary-word
context checks below. A primary timing contradiction permits agreement with only the
next fixed crop; a failed pair stops further retries. Alternate contexts must preserve
matching ordinary-word context near its primary position; the ordered-anchor fallback
and edit-region agreement rules below also apply.

A repeated lexical guess in silence never authorizes a cut. The confirmed intersection
must contain a sustained acoustic island above the adaptive recording noise threshold;
up to 20 ms internal dips are joined, at least 75% of the island must exceed the threshold,
and the island must last at least 200 ms. At least 60 ms is retained at each acoustic edge;
the cut also stays inside the previously confirmed interval, which retains 80 ms for the
brief class and 100 ms otherwise at both independently decoded word edges. The two margins serve different uncertainty bounds. This energy check is not phoneme or semantic recognition.
Short and uncertain sounds remain review-only. Older policy approvals require retranscription.

Displayed punctuation and its alignment remain intact; punctuation/space-only tokens are
excluded from lexical confidence. Discarding their timing outright was rejected because
real held vowels can occupy those attention intervals. Word-bound silence trimming uses
a lower threshold only within 30 ms of stronger speech, preserving quiet consonant edges
without absorbing distant low-level breathing/noise. Zero-duration words are never assigned fabricated
positive intervals.

Transcript search results are cached by project, sequence and query. Menu enablement
uses the same projection rules but stops at the first visible word, avoiding repeated full
transcript copies and sorting. Input control acknowledgements are sent after widgets
process the gesture. Hidden logical updates preserve the queued gesture; backend timestamps
continue to bound retries and captures while egui's last visible-frame time is frozen.
Control requests also unhide and restore a minimized native window without activating it; native
ordering is limited to four requests per second rather than every hidden logical update.
These changes do not guarantee responsiveness under every OS load.


### Verification of absorbed leading speech

After primary Korean decoding, up to 256 suspicious ordinary-word spans are checked in the
background. They must last 1–5 seconds and contain a quiet gap of at least 240 ms, with a
short voiced suffix. Energy only proposes a crop: it never moves a word by itself.
An unprompted decode of the suffix must recognize the complete word with a finite score
of at least 0.5 and at least two ordered ordinary-word anchors among the next four, within
300 ms of their primary midpoints. Repeated qualifying words are ambiguous and skipped.
The score is an additional filter, not a calibrated probability or a semantic guarantee.

Successful bounds stay inside the primary span and retain 30 ms around the locally decoded
word. A trailing edge shrinks only if another decoded ordinary word occupies the old tail.
Checks use an immutable snapshot of primary words, preserving deterministic anchors while
repairs are applied. Text, word count and speaker identity are retained; repaired confidence
is capped at 0.5. Zero-duration words are not fabricated into valid intervals. Progress includes
the checked/total word count and cancellation still discards the entire uncommitted result.
These extra local CPU passes add processing time and do not provide phoneme forced alignment.


Very brief filler words have a stricter agreement rule: both decoded durations must be at
most 300 ms and each boundary difference at most 40 ms. Only this class retains 80 ms at
each independently decoded word edge, allowing a 250 ms sound to keep an inward video-frame
interior. Other words retain 100 ms and the previous 100/300 ms timing rule. The acoustic
island still needs 200 ms of support and retains 60 ms at its edges. Approval revision 4 records the updated local-consensus policy below; older approvals
remain review-only until rechecking or retranscription.

For the real-interview results above, enable **Include spoken fillers (experimental)** before
Transcribe Korean. The saved transcript retains its checked intervals; changing this UI option
does not retroactively re-transcribe it. See [the final-build validation report](real-korean-validation.md).


### Local agreement, trailing boundaries, and track protection (build 20261018.15)

Two fixed local contexts can recover a filler whose primary timing is inaccurate.
Both must identify the complete same filler with matching ordinary-word context.
Their independently padded, primary-bounded possible edit regions must agree within
80 ms. Disagreement outside those retained primary bounds cannot authorize a larger
cut. Brief sounds still require all three raw alignments to last at most 300 ms
and agree at their actual edges within 40 ms. If the immediate
neighbor is merged or omitted, two ordered anchors among the nearest three ordinary
words on each side are required. The proposed interior stays inside the padded primary
and local intervals and may not overlap any ordinary word from the primary or either
verification. A contradictory result permits agreement with only the next fixed crop;
there is no unbounded search for a favorable answer. Existing acoustic support and
retained-edge requirements still apply. Candidates are never automatically checked.

After the leading-edge stage, up to 32 ordinary-word spans with the largest qualifying
internal quiet gaps receive a trailing-edge check. An unprompted prefix decode must
recognize the complete word and two ordered preceding ordinary anchors. A repair must
shorten the primary span by at least 200 ms, never expand it, and retain the entire
acoustic prefix plus its 60 ms margin. This last guard prevents an early model attention
endpoint from cutting the final voiced syllable. Checks use an immutable snapshot of
verified starts; text, count, speakers and starts remain unchanged. This bounded pass
is not complete phoneme alignment and does not fix every boundary error.

Cleanup preview, reviewed application and the legacy filler/pause commands share a
track-safety check. A locked audio/video clip or caption after a proposed cut blocks
ripple cleanup. Overlapping audio is accepted only when it is an aligned duplicate
of the source that supplies the displayed words, including its stream, channel,
effects, speed and source mapping. Unknown overlapping speech is blocked. Built-in
procedural speech-free generators without transcripts have an explicit exception;
imported media is never considered speech-free by filename or missing transcript.
Automatic Korean default-stream transcripts cannot authorize custom clip stream or
channel edits. A rejected cleanup leaves the project and undo history unchanged;
manual user-directed lift/extract remain separate operations.

See [the actual-media validation report](real-korean-validation.md) for measured recall,
boundary errors, locked/multiple-track checks and the retained-primary evaluation scope.
