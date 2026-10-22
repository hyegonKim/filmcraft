//! Optional local Korean / English voices via macOS's fixed `/usr/bin/say` service client.
//! No shell, network service, voice assets or third-party executable is bundled.
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use filmcraft_media::MediaSource;

use crate::{Audio, Gender, MAX_SECONDS, Params, SAMPLE_RATE, TtsError, Voice, VoiceInfo, check_text, script};

pub const INFO: VoiceInfo = VoiceInfo {
    id: "macos-yuna",
    name: "유나 (Mac)",
    language: "ko-KR",
    gender: Gender::Female,
    engine: "system",
    description: "Local Korean voice from macOS. Requires the installed Yuna voice. No upload.",
    license: "System voice; Apple terms apply",
    installed: true,
};

pub const ENGLISH_INFO: VoiceInfo = VoiceInfo {
    id: "macos-samantha",
    name: "Samantha (Mac)",
    language: "en-US",
    description: "Local English voice from macOS. Requires the installed Samantha voice. No upload.",
    ..INFO
};

pub struct SystemVoice {
    pub info: VoiceInfo,
    pub system_name: &'static str,
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, TtsError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let dir = std::env::temp_dir().join(format!("filmcraft-yuna-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(failed(e)),
            }
        }
        Err(failed("could not reserve a temporary directory"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn failed(e: impl std::fmt::Display) -> TtsError {
    TtsError::Model(format!(
        "macOS speech: {e}. Check that the selected system voice (Yuna or Samantha) is installed in System Settings > Accessibility > Spoken Content."
    ))
}

impl Voice for SystemVoice {
    fn info(&self) -> VoiceInfo {
        self.info
    }
    fn synthesize(&self, text: &str, params: &Params) -> Result<Audio, TtsError> {
        check_text(text)?;
        params.check()?;
        // Only FilmCraft's documented pause markers are interpreted. User text cannot inject
        // macOS speech commands, even though it never reaches a shell.
        if text.contains("[[") || text.contains("]]") {
            return Err(TtsError::Invalid("macOS speech control tags are not accepted; use [pause 1s]".into()));
        }
        let segments = script::parse(text);
        if segments.len() > 256 {
            return Err(TtsError::Invalid("at most 256 speech/pause segments are supported".into()));
        }
        let scratch = Scratch::new()?;
        let input = scratch.0.join("script.txt");
        let output = scratch.0.join("speech.wav");
        let max_samples = (MAX_SECONDS * f64::from(SAMPLE_RATE)) as usize;
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut samples = Vec::new();
        for segment in segments {
            match segment {
                script::Segment::Pause { millis } => {
                    let n = u64::from(millis) * u64::from(SAMPLE_RATE) / 1000;
                    let end = samples.len().checked_add(n as usize).filter(|&n| n <= max_samples).ok_or(TtsError::AudioTooLong)?;
                    samples.resize(end, 0.0);
                }
                script::Segment::Speech(text) => {
                    // pbas is the documented macOS base-pitch control, in Hz; pace uses say's
                    // words-per-minute option. All control values originate from checked numbers.
                    let pitch = 200.0 * 2.0_f64.powf(params.semitones / 12.0);
                    let mut file = std::fs::File::create(&input).map_err(failed)?;
                    write!(file, "[[pbas {pitch:.2}]]{text}").map_err(failed)?;
                    drop(file);
                    let mut child = Command::new("/usr/bin/say")
                        .args(["-v", self.system_name, "-r", &(180.0 * params.pace).round().to_string(), "--file-format=WAVE", "--data-format=LEF32@24000"])
                        .arg("-f")
                        .arg(&input)
                        .arg("-o")
                        .arg(&output)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(failed)?;
                    loop {
                        let outcome = child.try_wait();
                        if !matches!(outcome, Ok(None)) {
                            match outcome {
                                Ok(Some(status)) if status.success() => break,
                                other => {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    return Err(failed(format!("speech service failed: {other:?}")));
                                }
                            }
                        }
                        let too_large = std::fs::metadata(&output).is_ok_and(|m| m.len() > max_samples as u64 * 4 + 8192);
                        if Instant::now() >= deadline || too_large {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(failed("speech exceeded its time or size limit"));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    let size = std::fs::metadata(&output).map_err(failed)?.len();
                    if size > max_samples as u64 * 4 + 8192 {
                        return Err(TtsError::AudioTooLong);
                    }
                    let bytes = std::fs::read(&output).map_err(failed)?;
                    let source = filmcraft_media::wav::WavSource::parse("Korean speech", bytes.into()).map_err(failed)?;
                    let count = source.info().duration.to_units_floor(i64::from(SAMPLE_RATE));
                    let count = usize::try_from(count).map_err(failed)?;
                    if samples.len().checked_add(count).is_none_or(|n| n > max_samples) {
                        return Err(TtsError::AudioTooLong);
                    }
                    let mut audio = source.audio(0, count, SAMPLE_RATE).map_err(failed)?;
                    let mono = audio.channels.pop().ok_or_else(|| failed("speech output has no audio channel"))?;
                    if mono.is_empty() || mono.iter().any(|s| !s.is_finite()) {
                        return Err(failed("speech output is empty or non-finite"));
                    }
                    samples.extend(mono);
                    std::fs::remove_file(&output).map_err(failed)?;
                }
            }
        }
        Ok(Audio { samples, sample_rate: SAMPLE_RATE })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_hostile_scripts_before_running_service() {
        let v = SystemVoice { info: INFO, system_name: "Yuna" };
        for text in ["", "[pause 1s]", "[[rate 900]]안녕", &"가".repeat(crate::MAX_TEXT_BYTES)] {
            assert!(v.synthesize(text, &Params::default()).is_err());
        }
        assert!(v.synthesize("안녕", &Params { pace: f64::NAN, semitones: 0.0 }).is_err());
    }
    #[test]
    fn installed_korean_voice_speaks_and_preserves_pause() {
        if std::env::var_os("FILMCRAFT_TEST_SYSTEM_VOICE").is_none() {
            return;
        }
        let v = SystemVoice { info: INFO, system_name: "Yuna" };
        let a = v.synthesize("안녕하세요.[pause 1s]한국어 음성입니다.", &Params::default()).expect("installed Yuna");
        assert_eq!(a.sample_rate, SAMPLE_RATE);
        assert!(a.seconds() > 2.0);
        assert!(a.samples.iter().any(|s| s.abs() > 0.01));
        assert!(a.samples.windows(SAMPLE_RATE as usize).any(|w| w.iter().all(|&s| s == 0.0)));
        let english = SystemVoice { info: ENGLISH_INFO, system_name: "Samantha" };
        let a = english.synthesize("Hello.[pause 1s]This is English narration.", &Params::default()).expect("installed Samantha");
        assert!(a.seconds() > 2.0 && a.samples.iter().any(|s| s.abs() > 0.01));
    }
}
