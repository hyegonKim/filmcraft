//! cpal audio output: the playback master clock. Settings ▸ Audio Hardware picks the host
//! ("Device Class"), the output device, the I/O buffer size and the sample rate.
//!
//! The stream lives on its own thread ([`device_thread`]), never on the UI thread: opening a
//! device takes ~20 ms warm and 0.5-1 s from a cold audio engine on WASAPI, and closing one
//! joins its audio thread, so a drag or Play that opened the output synchronously stuttered at
//! its start (#2281). [`AudioOut::start`] and [`AudioOut::stop`] only swap what the open stream
//! plays. When no stream is open, `start` asks for one and returns at once: the sound follows
//! when the device is ready (playback runs on the wall clock meanwhile and a scrub is silent).
//! A stream with nothing to play stays open for [`IDLE_CLOSE`], so the next scrub or Play
//! starts at once, then closes.
//!
//! A stream the device drops while playing (an ALSA xrun the backend does not recover from, which
//! cpal reports as `poll() returned POLLERR`; an unplugged device) is restarted by
//! [`AudioOut::recover`], keeping the played-frame clock and the play-ahead mixer, so sound
//! resumes where it stopped instead of playback going silent (#840).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use filmcraft_engine::settings::AudioHardwarePrefs;
use filmcraft_ui_egui::{AudioDevices, AudioOut, AudioReadiness};

/// The playback mixer callback handed to [`AudioOut::start`].
type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

/// How long an open stream with nothing to play stays open, so that the next scrub or Play
/// plays at once instead of opening the device again.
const IDLE_CLOSE: Duration = Duration::from_secs(5);

pub struct CpalOut {
    /// Requests for the device thread; `None` when the thread could not be started.
    tx: Option<mpsc::Sender<Cmd>>,
    /// What this handle, the device thread and the stream callback share.
    link: Arc<Link>,
    watchdog: Watchdog,
    /// Time base for the watchdog.
    epoch: Instant,
    hw: AudioHardwarePrefs,
    /// Sequence sample rate for "Attempt to force hardware to document sample rate".
    document_rate: Option<u32>,
}

/// What the UI thread, the device thread and the stream callback share.
struct Link {
    /// What the stream plays: the running playback's mixer, or nothing (silence).
    slot: Mutex<Option<Fill>>,
    /// Frames played since the mixer in `slot` was put there (the playback clock).
    played: AtomicU64,
    health: Health,
    state: Mutex<DeviceState>,
    /// The fallback picked when the default device could not be opened, as (host, device) names,
    /// so later lookups (`devices`, `configure`, opening the stream) reuse it instead of probing
    /// every PCM.
    fallback: Mutex<Option<(String, String)>>,
}

impl Link {
    fn state(&self) -> MutexGuard<'_, DeviceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Put `fill` in the slot (or empty it), restarting the clock; returns the mixer it replaced,
    /// to be dropped outside the lock (dropping a play-ahead mixer ends its thread).
    fn swap_fill(&self, fill: Option<Fill>) -> Option<Fill> {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        // under the slot lock, so no callback of the old mixer counts into the new clock
        self.played.store(0, Ordering::SeqCst);
        std::mem::replace(&mut *slot, fill)
    }
}

/// Whether a stream is open, as the UI thread sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Closed,
    Opening,
    Open,
}

/// The device's state, shared by the UI thread and the device thread.
struct DeviceState {
    phase: Phase,
    /// The last request sent to the device thread. The thread reports the outcome of a request
    /// only while it is still the latest, so a slow open that finishes after a newer request
    /// (a close for a settings change) does not overwrite that request's state.
    issued: u64,
    /// `start` gave the stream something to play, and `stop` has not taken it back.
    playing: bool,
    /// When the stream last ran out of things to play.
    idle_since: Instant,
    /// The format the next `start` mixes at: the open stream's, or the configured device's.
    rate: u32,
    channels: u16,
    /// Status-bar note: the host's default device could not be opened and a fallback device is
    /// in use, or no output device was found (playing without sound).
    note: Option<Note>,
}

/// A stream to open: the settings, and the (sample rate, channels) a mixer was already set up
/// for (`None`: whatever the device runs at).
#[derive(Clone, Debug)]
struct OpenRequest {
    hw: AudioHardwarePrefs,
    document_rate: Option<u32>,
    expect: Option<(u32, u16)>,
}

/// What the device thread is asked to do; `seq` is the request's [`DeviceState::issued`] number.
enum Cmd {
    /// Open a stream, replacing the one that is open.
    Open { request: OpenRequest, seq: u64 },
    /// Close the stream.
    Close { seq: u64 },
}

/// An open stream (`S`: a `cpal::Stream`, a stand-in in tests) and its format.
struct Opened<S> {
    stream: S,
    rate: u32,
    channels: u16,
    note: Option<Note>,
}

/// Why a stream could not be opened, and the device's format when that was the reason.
#[derive(Debug)]
struct OpenError {
    message: String,
    format: Option<(u32, u16)>,
}

impl From<String> for OpenError {
    fn from(message: String) -> Self {
        Self { message, format: None }
    }
}

/// A hardware note for the status bar, translated when it is shown.
#[derive(Clone, Debug, PartialEq)]
enum Note {
    /// The default output device cannot be opened; this named device is used instead.
    Fallback(String),
    /// No output device at all: playback runs without sound.
    NoDevice,
}

impl Note {
    /// Localized status text for a fallback device (name interpolated through `fmt`) or for no output device.
    fn text(&self) -> String {
        use filmcraft_ui_egui::i18n::{fmt, t};
        match self {
            Note::Fallback(device) => fmt(t("Audio hardware: the default output device cannot be opened; using '{device}'"), &[("device", device)]),
            Note::NoDevice => t("No audio output device found: playing without sound (check Settings > Audio Hardware)").to_string(),
        }
    }
}

/// Whether the settings name a sample rate the device does not already run at, so its supported
/// configurations have to be listed. `0` is the settings' "device default".
fn needs_rate_search(default_rate: u32, want: u32) -> bool {
    want != 0 && want != default_rate
}

/// The stream configuration the settings ask for, falling back to the device default.
///
/// Listing a device's configurations means probing every format and rate in the driver:
/// 150-215 ms on WASAPI (measured), while opening the stream itself costs ~20 ms warm (0.5-1 s
/// from a cold audio engine). So the list is only asked for when the settings name a rate the
/// device does not already run at (#2633).
fn stream_config(hw: &AudioHardwarePrefs, document_rate: Option<u32>, dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    let default = dev.default_output_config().map_err(|e| e.to_string())?;
    let want = if hw.force_document_rate { document_rate.unwrap_or(hw.sample_rate) } else { hw.sample_rate };
    if !needs_rate_search(default.sample_rate().0, want) {
        return Ok(default);
    }
    let exact = dev.supported_output_configs().ok().and_then(|it| {
        it.filter(|c| c.channels() > 0 && (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&want))
            .max_by_key(|c| (c.channels() == default.channels(), c.sample_format() == cpal::SampleFormat::F32, c.sample_format() == default.sample_format()))
            .and_then(|c| c.try_with_sample_rate(cpal::SampleRate(want)))
    });
    Ok(exact.unwrap_or(default))
}

/// The host named in the settings (empty or unknown = the default host).
fn host(name: &str) -> cpal::Host {
    if !name.is_empty()
        && let Some(id) = cpal::available_hosts().into_iter().find(|h| h.name() == name)
        && let Ok(h) = cpal::host_from_id(id)
    {
        return h;
    }
    cpal::default_host()
}

/// The output device the settings ask for, with a status-bar note when a fallback is in use.
struct DeviceChoice {
    device: cpal::Device,
    /// The host's default device could not be opened: this device is the fallback.
    fallback: Option<Note>,
}

/// Fallback order when the default device cannot be opened: the sound-server bridges first
/// (`pipewire`, `pulse`: they share the card with every other app), then other named PCMs, and
/// raw `hw:`/`plughw:` devices last (opening one takes the card exclusively and silences the
/// rest of the desktop).
fn fallback_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n == "pipewire" || n.starts_with("pipewire:") {
        0
    } else if n == "pulse" || n.starts_with("pulse:") {
        1
    } else if n.starts_with("hw:") || n.starts_with("plughw:") {
        3
    } else {
        2
    }
}

/// The output device named in the settings (empty or missing = the host's default). When the
/// default cannot be opened — a broken ALSA `default` (missing `99-pipewire-default.conf`,
/// #23/#106) — a fallback that can, so the app is not silently mute. The fallback list is probed
/// only after the default fails, in [`fallback_rank`] order, and the pick is cached in `cache`.
fn output_device(h: &cpal::Host, name: &str, cache: &Mutex<Option<(String, String)>>) -> Option<DeviceChoice> {
    if !name.is_empty()
        && let Ok(mut devs) = h.output_devices()
        && let Some(d) = devs.find(|d| d.name().is_ok_and(|n| n == name))
    {
        return Some(DeviceChoice { device: d, fallback: None });
    }
    if let Some(d) = h.default_output_device()
        && d.default_output_config().is_ok()
    {
        return Some(DeviceChoice { device: d, fallback: None });
    }
    let host_name = h.id().name().to_string();
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    let mut devs: Vec<(String, cpal::Device)> = h.output_devices().ok()?.filter_map(|d| d.name().ok().map(|n| (n, d))).collect();
    if let Some((cached_host, cached)) = cache.as_ref()
        && *cached_host == host_name
        && let Some(i) = devs.iter().position(|(n, _)| n == cached)
    {
        let (found, d) = devs.swap_remove(i);
        return Some(DeviceChoice { device: d, fallback: Some(Note::Fallback(found)) });
    }
    devs.sort_by_key(|(n, _)| fallback_rank(n));
    let (found, d) = devs.into_iter().find(|(_, d)| d.default_output_config().is_ok())?;
    log::warn!("the default audio output device cannot be opened; falling back to '{found}'");
    *cache = Some((host_name, found.clone()));
    Some(DeviceChoice { device: d, fallback: Some(Note::Fallback(found)) })
}

impl CpalOut {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let fallback = Mutex::new(None);
        let choice = output_device(&host, "", &fallback);
        let cfg = choice.as_ref().and_then(|c| c.device.default_output_config().ok());
        let note = match choice {
            Some(c) => c.fallback,
            None => {
                log::warn!("no audio output device found");
                Some(Note::NoDevice)
            }
        };
        let (rate, channels) = cfg.as_ref().map_or((48_000, 2), |c| (c.sample_rate().0, c.channels()));
        Self::with_opener(rate, channels, note, fallback, IDLE_CLOSE, open_stream)
    }

    /// A handle whose device thread opens streams with `open`, closing an idle one after `idle`.
    fn with_opener<S, F>(rate: u32, channels: u16, note: Option<Note>, fallback: Mutex<Option<(String, String)>>, idle: Duration, open: F) -> Self
    where
        S: 'static,
        F: FnMut(&Arc<Link>, &OpenRequest) -> Result<Opened<S>, OpenError> + Send + 'static,
    {
        let state = DeviceState { phase: Phase::Closed, issued: 0, playing: false, idle_since: Instant::now(), rate, channels, note };
        let link = Arc::new(Link { slot: Mutex::new(None), played: AtomicU64::new(0), health: Health::default(), state: Mutex::new(state), fallback });
        let (tx, rx) = mpsc::channel();
        let thread_link = link.clone();
        let spawned = std::thread::Builder::new().name("filmcraft-audio-device".into()).spawn(move || device_thread(rx, thread_link, idle, open));
        let tx = match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                log::warn!("the audio device thread could not be started: {e}");
                None
            }
        };
        Self { tx, link, watchdog: Watchdog::default(), epoch: Instant::now(), hw: AudioHardwarePrefs::default(), document_rate: None }
    }

    /// Ask the device thread to open a stream on the current settings, replacing the open one.
    fn request_open(&self, st: &mut DeviceState, expect: Option<(u32, u16)>) -> Result<(), String> {
        let tx = self.tx.as_ref().ok_or("the audio device thread is not running")?;
        let seq = st.issued.wrapping_add(1);
        let request = OpenRequest { hw: self.hw.clone(), document_rate: self.document_rate, expect };
        tx.send(Cmd::Open { request, seq }).map_err(|_| "the audio device thread has stopped")?;
        st.issued = seq;
        st.phase = Phase::Opening;
        Ok(())
    }

    /// Ask the device thread to close the stream.
    fn request_close(&self, st: &mut DeviceState) {
        let seq = st.issued.wrapping_add(1);
        if let Some(tx) = &self.tx
            && tx.send(Cmd::Close { seq }).is_ok()
        {
            st.issued = seq;
        }
        st.phase = Phase::Closed;
    }
}

/// Owns the stream: opens and closes it as asked (cpal streams stay on the thread that made
/// them), and closes a stream that had nothing to play for `idle`. Runs until the handle is
/// dropped.
fn device_thread<S, F>(rx: mpsc::Receiver<Cmd>, link: Arc<Link>, idle: Duration, mut open: F)
where
    F: FnMut(&Arc<Link>, &OpenRequest) -> Result<Opened<S>, OpenError>,
{
    let mut stream: Option<S> = None;
    // the request handled last
    let mut handled = 0u64;
    let poll = (idle / 4).clamp(Duration::from_millis(5), Duration::from_millis(500));
    loop {
        let cmd = match rx.recv_timeout(poll) {
            Ok(cmd) => Some(cmd),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // A driver that panics must not take the thread, and every later open, with it.
        let step = catch_unwind(AssertUnwindSafe(|| match cmd {
            Some(Cmd::Open { request, seq }) => {
                handled = seq;
                // the old stream (and its audio thread) goes first: only one stream ever runs the mixer
                stream = None;
                link.health.clear_errors();
                let opened = open(&link, &request);
                let mut st = link.state();
                // An open overtaken by a newer request reports nothing: a settings change already
                // set the format its device runs at, and a newer open reports its own.
                let latest = st.issued == seq;
                match opened {
                    Ok(o) => {
                        if latest {
                            (st.rate, st.channels, st.note) = (o.rate, o.channels, o.note);
                            st.phase = Phase::Open;
                            // idle from now, not from the last playback: a stream that Play
                            // prepared long after that would otherwise close before Play starts
                            st.idle_since = Instant::now();
                        }
                        stream = Some(o.stream);
                    }
                    Err(e) => {
                        log::warn!("audio output unavailable: {}", e.message);
                        if latest {
                            // a device that now runs another format: the next `start` mixes at it
                            if let Some((rate, channels)) = e.format {
                                (st.rate, st.channels) = (rate, channels);
                            }
                            st.phase = Phase::Closed;
                        }
                    }
                }
            }
            Some(Cmd::Close { seq }) => {
                handled = seq;
                stream = None;
            }
            None => {
                let mut st = link.state();
                if stream.is_some() && idle_stream_closes(&st, handled, idle) {
                    st.phase = Phase::Closed;
                    drop(st);
                    stream = None;
                }
            }
        }));
        if step.is_err() {
            log::warn!("the audio device thread recovered from a panic; the output is closed");
            stream = None;
            let mut st = link.state();
            if st.issued == handled {
                st.phase = Phase::Closed;
            }
        }
    }
}

/// Whether the open stream has had nothing to play for `idle` (and no request is pending), so
/// the device thread closes it.
fn idle_stream_closes(st: &DeviceState, handled: u64, idle: Duration) -> bool {
    st.issued == handled && st.phase == Phase::Open && !st.playing && st.idle_since.elapsed() >= idle
}

/// Open the output device `request` names and play the link's slot on it (device thread).
fn open_stream(link: &Arc<Link>, request: &OpenRequest) -> Result<Opened<cpal::Stream>, OpenError> {
    let OpenRequest { hw, document_rate, expect } = request;
    let host = host(&hw.device_class);
    let DeviceChoice { device: dev, fallback } = output_device(&host, &hw.default_output, &link.fallback).ok_or_else(|| "no output device".to_string())?;
    let cfg = stream_config(hw, *document_rate, &dev)?;
    let (rate, channels) = (cfg.sample_rate().0, cfg.channels());
    if channels == 0 || rate == 0 {
        return Err("the output device returned an invalid channel count or sample rate".to_string().into());
    }
    if let Some(want) = expect
        && *want != (rate, channels)
    {
        return Err(OpenError { message: format!("the output device now runs at {rate} Hz with {channels} channels"), format: Some((rate, channels)) });
    }
    let mut config: cpal::StreamConfig = cfg.clone().into();
    if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size()
        && (*min..=*max).contains(&hw.buffer_size)
    {
        config.buffer_size = cpal::BufferSize::Fixed(hw.buffer_size);
    }
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            let ch = usize::from(channels);
            let (callback, errors) = (link.clone(), link.clone());
            dev.build_output_stream(
                &config,
                move |buf: &mut [f32], _| render(&callback.slot, buf, ch, &callback.played, &callback.health),
                move |e| stream_error(&errors.health, e),
                None,
            )
        }
        cpal::SampleFormat::I8 => converted_stream::<i8>(&dev, &config, link),
        cpal::SampleFormat::I16 => converted_stream::<i16>(&dev, &config, link),
        cpal::SampleFormat::I24 => converted_stream::<cpal::I24>(&dev, &config, link),
        cpal::SampleFormat::I32 => converted_stream::<i32>(&dev, &config, link),
        cpal::SampleFormat::I64 => converted_stream::<i64>(&dev, &config, link),
        cpal::SampleFormat::U8 => converted_stream::<u8>(&dev, &config, link),
        cpal::SampleFormat::U16 => converted_stream::<u16>(&dev, &config, link),
        cpal::SampleFormat::U32 => converted_stream::<u32>(&dev, &config, link),
        cpal::SampleFormat::U64 => converted_stream::<u64>(&dev, &config, link),
        cpal::SampleFormat::F64 => converted_stream::<f64>(&dev, &config, link),
        other => return Err(format!("unsupported sample format {other:?}").into()),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(Opened { stream, rate, channels, note: fallback })
}

fn converted_stream<T>(dev: &cpal::Device, config: &cpal::StreamConfig, link: &Arc<Link>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let (callback, errors) = (link.clone(), link.clone());
    let mut scratch = Vec::new();
    dev.build_output_stream(
        config,
        move |buf: &mut [T], _| {
            // Reuse the conversion buffer. A pathological driver buffer must not panic the audio thread.
            if buf.len() > 1_048_576 || scratch.try_reserve(buf.len().saturating_sub(scratch.len())).is_err() {
                buf.fill(T::EQUILIBRIUM);
                if !callback.health.failed.swap(true, Ordering::SeqCst) {
                    eprintln!("filmcraft: unable to allocate the audio conversion buffer");
                }
                return;
            }
            scratch.resize(buf.len(), 0.0);
            render(&callback.slot, &mut scratch, channels, &callback.played, &callback.health);
            convert_samples(&scratch, buf);
        },
        move |e| stream_error(&errors.health, e),
        None,
    )
}

/// What the stream callback reports to the UI thread.
#[derive(Default)]
struct Health {
    /// The device went away (unplugged): restart the stream at once.
    lost: AtomicBool,
    /// A backend error since the last check (cpal keeps the stream alive after these): restart
    /// only if the stream also stopped consuming samples.
    errored: AtomicBool,
    /// Unrecoverable: the mixer panicked, a buffer could not be allocated, or restarts ran out.
    /// [`AudioOut::played_frames`] reports `None` and playback goes on without sound.
    failed: AtomicBool,
    /// The first error of a stream has been printed.
    reported: AtomicBool,
}

impl Health {
    fn clear_errors(&self) {
        self.lost.store(false, Ordering::SeqCst);
        self.errored.store(false, Ordering::SeqCst);
        self.reported.store(false, Ordering::SeqCst);
    }
}

/// How long a stream that reported an error may go without consuming samples before it is
/// restarted. Well under the UI's `AUDIO_STALL_S`, which gives up on the device clock.
const STALL_RESTART: Duration = Duration::from_millis(300);
/// At most this many restarts within [`RESTART_WINDOW`]; a device that keeps failing gives up.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(10);

/// What [`Watchdog::check`] decides about the running stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Healthy,
    Restart,
    GiveUp,
}

/// Decides when a stream that reported errors is restarted (UI thread, every frame).
#[derive(Default)]
struct Watchdog {
    /// Played-frame count last seen, and when it last changed.
    seen: (u64, Duration),
    /// An error was reported and the stream has not consumed samples since.
    pending_error: bool,
    /// When recent restarts happened (at most [`MAX_RESTARTS`] kept).
    restarts: Vec<Duration>,
}

impl Watchdog {
    fn reset(&mut self, now: Duration) {
        *self = Self { seen: (0, now), ..Self::default() };
    }

    fn check(&mut self, now: Duration, played: u64, errored: bool, lost: bool) -> Verdict {
        if played != self.seen.0 {
            self.seen = (played, now);
            self.pending_error = false;
        }
        // An error that arrives with the stream still consuming samples is only a hiccup.
        self.pending_error |= errored;
        let stalled = now.saturating_sub(self.seen.1) >= STALL_RESTART;
        if !(lost || (self.pending_error && stalled)) {
            return Verdict::Healthy;
        }
        self.restarts.retain(|t| now.saturating_sub(*t) < RESTART_WINDOW);
        if self.restarts.len() >= MAX_RESTARTS {
            return Verdict::GiveUp;
        }
        self.restarts.push(now);
        self.seen = (played, now);
        self.pending_error = false;
        Verdict::Restart
    }
}

/// One device callback: run the mixer in `slot` into `buf` and advance the clock. Never panics: a
/// panicking mixer plays silence and fails the stream. With nothing to play, or while the UI
/// thread swaps the mixer, it plays silence without advancing the clock.
fn render(slot: &Mutex<Option<Fill>>, buf: &mut [f32], channels: usize, played: &AtomicU64, health: &Health) {
    let mut guard = match slot.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            buf.fill(0.0);
            return;
        }
    };
    let Some(fill) = guard.as_mut() else {
        buf.fill(0.0);
        return;
    };
    if health.failed.load(Ordering::SeqCst) || catch_unwind(AssertUnwindSafe(|| fill(buf, channels))).is_err() {
        buf.fill(0.0);
        if !health.failed.swap(true, Ordering::SeqCst) {
            eprintln!("filmcraft: the audio mixer failed; playing without sound");
        }
        return;
    }
    played.fetch_add((buf.len() / channels.max(1)) as u64, Ordering::SeqCst);
}

fn stream_error(health: &Health, e: cpal::StreamError) {
    match e {
        cpal::StreamError::DeviceNotAvailable => health.lost.store(true, Ordering::SeqCst),
        cpal::StreamError::BackendSpecific { .. } => health.errored.store(true, Ordering::SeqCst),
    }
    if !health.reported.swap(true, Ordering::SeqCst) {
        eprintln!("filmcraft: audio stream error: {e}");
    }
}

fn convert_samples<T: cpal::Sample + cpal::FromSample<f32>>(input: &[f32], output: &mut [T]) {
    // Integer conversion represents [-1, 1). In particular I24 cannot represent +1 exactly.
    let upper = f32::from_bits(1.0_f32.to_bits() - 1);
    for (sample, converted) in input.iter().zip(output) {
        *converted = T::from_sample(if sample.is_finite() { sample.clamp(-1.0, upper) } else { 0.0 });
    }
}

impl AudioOut for CpalOut {
    /// Hands `fill` to the open stream at once. With no stream open, asks the device thread for
    /// one and returns without waiting: the sound starts when it is open (#2281).
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        drop(self.link.swap_fill(Some(fill)));
        self.link.health.failed.store(false, Ordering::SeqCst);
        self.watchdog.reset(self.epoch.elapsed());
        let mut st = self.link.state();
        st.playing = true;
        if st.phase == Phase::Closed {
            // the caller set its mixer up for this format
            let expect = (st.rate, st.channels);
            if let Err(e) = self.request_open(&mut st, Some(expect)) {
                st.playing = false;
                drop(st);
                drop(self.link.swap_fill(None));
                return Err(e);
            }
        }
        Ok(st.rate)
    }
    /// Silences the stream; it stays open for [`IDLE_CLOSE`] so the next start plays at once.
    fn stop(&mut self) {
        drop(self.link.swap_fill(None));
        let mut st = self.link.state();
        if st.playing {
            st.playing = false;
            st.idle_since = Instant::now();
        }
    }
    fn readiness(&self) -> AudioReadiness {
        match self.link.state().phase {
            Phase::Open => AudioReadiness::Ready,
            Phase::Opening => AudioReadiness::Opening,
            Phase::Closed => AudioReadiness::Closed,
        }
    }
    fn prepare(&mut self) {
        let mut st = self.link.state();
        if st.phase == Phase::Closed
            && let Err(e) = self.request_open(&mut st, None)
        {
            log::warn!("audio output unavailable: {e}");
        }
    }
    fn recover(&mut self) {
        if self.link.state().phase != Phase::Open || self.link.health.failed.load(Ordering::SeqCst) {
            return;
        }
        let lost = self.link.health.lost.swap(false, Ordering::SeqCst);
        let errored = self.link.health.errored.swap(false, Ordering::SeqCst);
        match self.watchdog.check(self.epoch.elapsed(), self.link.played.load(Ordering::SeqCst), errored, lost) {
            Verdict::Healthy => {}
            Verdict::Restart => {
                log::warn!("the audio output stream {}; restarting it", if lost { "lost its device" } else { "stopped after an error" });
                // A new stream on the same mixer and clock, opened on the device thread. If it
                // cannot be opened the output reports no clock and playback goes on silently.
                let mut st = self.link.state();
                let expect = (st.rate, st.channels);
                if let Err(e) = self.request_open(&mut st, Some(expect)) {
                    log::warn!("the audio output stream could not be restarted: {e}");
                    self.link.health.failed.store(true, Ordering::SeqCst);
                }
            }
            Verdict::GiveUp => {
                log::warn!("the audio output stream keeps failing ({MAX_RESTARTS} restarts within {} s)", RESTART_WINDOW.as_secs());
                self.link.health.failed.store(true, Ordering::SeqCst);
                // the next start opens a fresh stream
                let mut st = self.link.state();
                self.request_close(&mut st);
            }
        }
    }
    fn sample_rate(&self) -> u32 {
        self.link.state().rate
    }
    fn channels(&self) -> usize {
        usize::from(self.link.state().channels)
    }
    /// The clock: frames played since `start`, held at 0 while the stream opens. `None` when
    /// nothing was started, the stream could not be opened or restarted, or the mixer failed.
    fn played_frames(&self) -> Option<u64> {
        if self.link.health.failed.load(Ordering::SeqCst) {
            return None;
        }
        let st = self.link.state();
        (st.playing && st.phase != Phase::Closed).then(|| self.link.played.load(Ordering::SeqCst))
    }
    fn devices(&self) -> AudioDevices {
        let h = host(&self.hw.device_class);
        let outputs = h.output_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let inputs = h.input_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let output_channels = output_device(&h, &self.hw.default_output, &self.link.fallback)
            .and_then(|c| stream_config(&self.hw, self.document_rate, &c.device).ok())
            .map(|cfg| cfg.channels())
            .unwrap_or(0);
        AudioDevices { hosts: cpal::available_hosts().iter().map(|h| h.name().to_string()).collect(), inputs, outputs, output_channels }
    }
    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        let changed = self.hw != *hw || self.document_rate != document_rate;
        self.hw = hw.clone();
        self.document_rate = document_rate;
        // the rate playback mixes at must be known before `start`
        let host = host(&self.hw.device_class);
        let found = output_device(&host, &self.hw.default_output, &self.link.fallback)
            .and_then(|choice| stream_config(&self.hw, self.document_rate, &choice.device).ok().map(|cfg| (cfg, choice.fallback)));
        let mut st = self.link.state();
        if let Some((cfg, note)) = found {
            (st.rate, st.channels, st.note) = (cfg.sample_rate().0, cfg.channels(), note);
        }
        // a stream open on the old settings closes; the next start opens the new device
        if changed && st.phase != Phase::Closed {
            self.request_close(&mut st);
        }
    }
    fn note(&self) -> Option<String> {
        self.link.state().note.as_ref().map(Note::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::Sample;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn fallback_prefers_sound_servers_over_raw_hardware() {
        let mut names = vec!["plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0", "sysdefault:CARD=PCH", "pulse", "pipewire"];
        names.sort_by_key(|n| fallback_rank(n));
        assert_eq!(names, ["pipewire", "pulse", "sysdefault:CARD=PCH", "plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0"]);
    }

    #[test]
    fn notes_name_the_fallback_device() {
        assert!(Note::Fallback("pipewire".into()).text().contains("'pipewire'"));
        assert!(!Note::NoDevice.text().is_empty());
    }

    /// #2633: the rate search costs ~190 ms on WASAPI, so the device default is taken as is.
    #[test]
    fn the_device_default_rate_is_used_without_listing_every_configuration() {
        assert!(!needs_rate_search(48_000, 48_000), "the settings ask for the rate the device runs at");
        assert!(!needs_rate_search(44_100, 0), "0 is the device default");
        assert!(needs_rate_search(48_000, 44_100), "another rate has to be searched for");
        assert!(needs_rate_search(48_000, 96_000));
    }

    #[test]
    fn integer_conversion_clips_and_preserves_interleaved_channels() {
        let mut signed = [0_i16; 8];
        convert_samples(&[-2.0, 2.0, -0.5, 0.5, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY], &mut signed);
        assert_eq!(signed, [i16::MIN, i16::MAX, -16_384, 16_384, 0, 0, 0, 0]);
        let mut unsigned = [0_u16; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut unsigned);
        assert_eq!(unsigned, [0, 32_768, u16::MAX]);
    }

    #[test]
    fn full_scale_24_bit_samples_remain_representable() {
        let mut samples = [cpal::I24::EQUILIBRIUM; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut samples);
        assert_eq!(samples.map(|s| s.inner()), [-8_388_608, 0, 8_388_607]);
    }

    #[test]
    fn stream_errors_ask_for_a_restart_instead_of_failing_playback() {
        let health = Health::default();
        stream_error(&health, cpal::StreamError::BackendSpecific { err: cpal::BackendSpecificError { description: "`alsa::poll()` returned POLLERR".into() } });
        assert!(health.errored.load(Ordering::SeqCst) && !health.lost.load(Ordering::SeqCst));
        stream_error(&health, cpal::StreamError::DeviceNotAvailable);
        assert!(health.lost.load(Ordering::SeqCst));
        assert!(!health.failed.load(Ordering::SeqCst), "a stream error alone must not silence playback");
        let output = CpalOut::new();
        assert_eq!(output.played_frames(), None, "no stream, no clock");
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_error_while_samples_keep_flowing_is_only_a_hiccup() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(10), 480, true, false), Verdict::Healthy);
        // the stream keeps consuming samples after the error: never restarted
        for (i, t) in (20..2_000).step_by(20).enumerate() {
            assert_eq!(w.check(ms(t), 480 * (i as u64 + 2), false, false), Verdict::Healthy);
        }
    }

    #[test]
    fn a_stream_that_errors_and_stops_consuming_is_restarted() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(10), 480, false, false), Verdict::Healthy);
        // POLLERR: the backend reports errors and no longer calls back
        assert_eq!(w.check(ms(20), 480, true, false), Verdict::Healthy);
        assert_eq!(w.check(ms(200), 480, true, false), Verdict::Healthy);
        assert_eq!(w.check(ms(310), 480, false, false), Verdict::Restart);
        // the restarted stream gets a fresh grace period
        assert_eq!(w.check(ms(400), 480, false, false), Verdict::Healthy);
        // a stall without any reported error is left to the UI's own watchdog
        assert_eq!(w.check(ms(2_000), 480, false, false), Verdict::Healthy);
    }

    #[test]
    fn a_lost_device_restarts_at_once_and_repeated_failures_give_up() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(5), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(10), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(15), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(20), 0, false, true), Verdict::GiveUp);
        // sporadic failures far apart keep being recovered
        assert_eq!(w.check(ms(10_006), 0, false, true), Verdict::Restart);
    }

    #[test]
    fn the_device_callback_never_panics_and_counts_played_frames() {
        let (played, health) = (AtomicU64::new(0), Health::default());
        let fill: Mutex<Option<Fill>> = Mutex::new(Some(Box::new(|buf: &mut [f32], _| buf.fill(0.25))));
        let mut buf = [1.0_f32; 8];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!((buf, played.load(Ordering::SeqCst)), ([0.25; 8], 4));
        // a mixer the UI thread is swapping plays silence without moving the clock
        {
            let _held = fill.lock().unwrap();
            render(&fill, &mut buf, 2, &played, &health);
        }
        assert_eq!((buf, played.load(Ordering::SeqCst)), ([0.0; 8], 4));
        // zero channels from a broken driver must not divide by zero
        render(&fill, &mut buf, 0, &played, &health);
        assert_eq!(played.load(Ordering::SeqCst), 12);
    }

    #[test]
    fn a_panicking_mixer_plays_silence_and_fails_the_stream() {
        let (played, health) = (AtomicU64::new(0), Health::default());
        let fill: Mutex<Option<Fill>> = Mutex::new(Some(Box::new(|buf: &mut [f32], _| {
            buf.fill(0.5);
            panic!("mixer bug");
        })));
        let mut buf = [1.0_f32; 4];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!(buf, [0.0; 4]);
        assert_eq!(played.load(Ordering::SeqCst), 0);
        assert!(health.failed.load(Ordering::SeqCst));
        // later callbacks stay silent instead of re-entering the broken mixer
        buf = [1.0; 4];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!(buf, [0.0; 4]);
    }

    #[test]
    fn nothing_to_play_is_silence_without_moving_the_clock() {
        let (played, health) = (AtomicU64::new(0), Health::default());
        let empty: Mutex<Option<Fill>> = Mutex::new(None);
        let mut buf = [1.0_f32; 8];
        render(&empty, &mut buf, 2, &played, &health);
        assert_eq!((buf, played.load(Ordering::SeqCst)), ([0.0; 8], 0));
        assert!(!health.failed.load(Ordering::SeqCst));
    }

    /// A stand-in for a `cpal::Stream`, counting how many are open.
    struct FakeStream(Arc<AtomicUsize>);

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Streams opened so far, and streams open now.
    type Counters = (Arc<AtomicUsize>, Arc<AtomicUsize>);

    fn opened_stream(counters: &Counters) -> Result<Opened<FakeStream>, OpenError> {
        counters.0.fetch_add(1, Ordering::SeqCst);
        counters.1.fetch_add(1, Ordering::SeqCst);
        Ok(Opened { stream: FakeStream(counters.1.clone()), rate: 48_000, channels: 2, note: None })
    }

    /// An output whose device takes `delay` to open, fails its first `fail` opens, and closes a
    /// stream idle for `idle`.
    fn fake(delay: Duration, fail: usize, idle: Duration) -> (CpalOut, Counters) {
        let counters: Counters = Default::default();
        let device = counters.clone();
        let mut fails = fail;
        let out = CpalOut::with_opener(48_000, 2, None, Mutex::new(None), idle, move |_: &Arc<Link>, _: &OpenRequest| {
            std::thread::sleep(delay);
            if fails > 0 {
                fails -= 1;
                return Err(OpenError::from("the device is busy".to_string()));
            }
            opened_stream(&device)
        });
        (out, counters)
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !done() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting until {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// What the stream's callback plays next: `frames` stereo frames.
    fn callback(out: &CpalOut, frames: usize) -> Vec<f32> {
        let mut buf = vec![9.0; frames * 2];
        render(&out.link.slot, &mut buf, 2, &out.link.played, &out.link.health);
        buf
    }

    fn constant(level: f32) -> Fill {
        Box::new(move |buf: &mut [f32], _| buf.fill(level))
    }

    /// #2281: a slow device never holds up `start`, which a scrub or Play calls on the UI thread.
    /// The output reports that it is opening, its clock holds at 0, and the sound follows.
    #[test]
    fn start_returns_at_once_while_a_slow_device_opens() {
        let (mut out, (opened, _)) = fake(Duration::from_millis(400), 0, IDLE_CLOSE);
        let t0 = Instant::now();
        assert_eq!(out.start(constant(0.5)), Ok(48_000));
        assert!(t0.elapsed() < Duration::from_millis(200), "start waited for the device: {:?}", t0.elapsed());
        assert_eq!(out.readiness(), AudioReadiness::Opening);
        assert_eq!(out.played_frames(), Some(0), "the clock holds while the device opens");
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert_eq!(callback(&out, 4), [0.5; 8], "the stream plays what start handed it");
        assert_eq!(out.played_frames(), Some(4));
    }

    /// Starting again on an open stream swaps the mixer and restarts the clock without opening
    /// the device again. Stop silences it, and it stays open until it has been idle a while.
    #[test]
    fn an_open_stream_is_reused_and_closes_once_idle() {
        let (mut out, (opened, live)) = fake(Duration::ZERO, 0, Duration::from_millis(150));
        out.prepare();
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        out.start(constant(0.25)).unwrap();
        callback(&out, 10);
        assert_eq!(out.played_frames(), Some(10));
        out.start(constant(0.75)).unwrap();
        assert_eq!(out.readiness(), AudioReadiness::Ready, "no reopen");
        assert_eq!(out.played_frames(), Some(0), "a new mixer restarts the clock");
        assert_eq!(callback(&out, 2), [0.75; 4]);
        out.stop();
        assert_eq!(callback(&out, 2), [0.0; 4], "stopped: silence");
        assert_eq!(out.played_frames(), None, "and no clock");
        assert_eq!(out.readiness(), AudioReadiness::Ready, "the stream stays open for the next start");
        wait_until("the idle stream closes", || out.readiness() == AudioReadiness::Closed);
        wait_until("the stream is dropped", || live.load(Ordering::SeqCst) == 0);
        out.start(constant(0.5)).unwrap();
        wait_until("the stream reopens", || out.readiness() == AudioReadiness::Ready);
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    /// A device that cannot be opened: no clock (playback goes on silently and retries), and the
    /// next start opens it again.
    #[test]
    fn a_failed_open_reports_no_clock_and_the_next_start_retries() {
        let (mut out, (opened, _)) = fake(Duration::from_millis(20), 1, IDLE_CLOSE);
        out.start(constant(0.5)).unwrap();
        assert_eq!(out.played_frames(), Some(0));
        wait_until("the open fails", || out.readiness() == AudioReadiness::Closed);
        assert_eq!(out.played_frames(), None, "no stream, no clock");
        out.start(constant(0.5)).unwrap();
        assert_eq!(out.readiness(), AudioReadiness::Opening);
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        assert_eq!((opened.load(Ordering::SeqCst), out.played_frames()), (1, Some(0)));
    }

    /// A slow open that finishes after the output was closed (Settings ▸ Audio Hardware changed)
    /// does not report itself open; the next start opens the device on the new settings.
    #[test]
    fn an_open_overtaken_by_a_close_stays_closed() {
        let (gate, wait) = mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let counters: Counters = Default::default();
        let device = counters.clone();
        let mut out = CpalOut::with_opener(48_000, 2, None, Mutex::new(None), IDLE_CLOSE, move |_: &Arc<Link>, _: &OpenRequest| {
            let _ = wait.lock().unwrap().recv();
            opened_stream(&device)
        });
        out.prepare();
        {
            let mut st = out.link.state();
            out.request_close(&mut st);
        }
        gate.send(()).unwrap();
        wait_until("the overtaken open finishes and its stream is closed", || counters.0.load(Ordering::SeqCst) == 1 && counters.1.load(Ordering::SeqCst) == 0);
        assert_eq!(out.readiness(), AudioReadiness::Closed);
        gate.send(()).unwrap();
        out.start(constant(0.5)).unwrap();
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        assert_eq!(counters.1.load(Ordering::SeqCst), 1);
    }

    /// #840 with the stream on its own thread: a lost device is reopened there, on the same mixer
    /// and clock, the old stream closing first.
    #[test]
    fn a_lost_device_is_reopened_on_the_same_mixer_and_clock() {
        let (mut out, (opened, live)) = fake(Duration::from_millis(10), 0, IDLE_CLOSE);
        out.start(constant(0.5)).unwrap();
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        callback(&out, 100);
        out.link.health.lost.store(true, Ordering::SeqCst);
        out.recover();
        assert_eq!(out.readiness(), AudioReadiness::Opening, "reopened on the device thread");
        assert_eq!(out.played_frames(), Some(100), "the clock holds meanwhile");
        wait_until("the stream is reopened", || out.readiness() == AudioReadiness::Ready);
        assert_eq!((opened.load(Ordering::SeqCst), live.load(Ordering::SeqCst)), (2, 1), "the old stream went first");
        assert_eq!(callback(&out, 1), [0.5; 2], "the same mixer");
        assert_eq!(out.played_frames(), Some(101));
    }

    /// Play prepares the output long after the last playback stopped: the stream stays open for
    /// the idle time counted from when it opened, so Play still finds it open.
    #[test]
    fn a_stream_prepared_long_after_playback_stays_open_for_the_idle_time() {
        let idle = Duration::from_secs(1);
        let (mut out, (opened, _)) = fake(Duration::ZERO, 0, idle);
        // the last playback ended a minute ago
        out.link.state().idle_since = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
        out.prepare();
        wait_until("the stream is open", || out.readiness() == AudioReadiness::Ready);
        let open_at = Instant::now();
        // the device thread checks for idle streams every 250 ms
        std::thread::sleep(Duration::from_millis(400));
        let readiness = out.readiness();
        out.start(constant(0.5)).unwrap();
        // (a machine so loaded that the sleep overran the idle time proves nothing either way)
        if open_at.elapsed() < idle {
            assert_eq!(readiness, AudioReadiness::Ready, "the prepared stream closed before its idle time ran out");
            assert_eq!(opened.load(Ordering::SeqCst), 1, "Play plays on the prepared stream");
        }
    }

    /// A slow open that a settings change overtook does not put its device's format back: the next
    /// start mixes at the format of the device the settings now name.
    #[test]
    fn an_overtaken_open_keeps_the_new_settings_format() {
        let (gate, wait) = mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let counters: Counters = Default::default();
        let device = counters.clone();
        let mut out = CpalOut::with_opener(48_000, 2, None, Mutex::new(None), IDLE_CLOSE, move |_: &Arc<Link>, _: &OpenRequest| {
            let _ = wait.lock().unwrap().recv();
            opened_stream(&device)
        });
        out.prepare();
        {
            // what `configure` does for a device that runs at 96 kHz in 6 channels
            let mut st = out.link.state();
            (st.rate, st.channels) = (96_000, 6);
            out.request_close(&mut st);
        }
        gate.send(()).unwrap();
        wait_until("the overtaken open finishes", || counters.0.load(Ordering::SeqCst) == 1 && counters.1.load(Ordering::SeqCst) == 0);
        assert_eq!((out.sample_rate(), out.channels()), (96_000, 6));
        assert_eq!(out.readiness(), AudioReadiness::Closed);
    }
}
