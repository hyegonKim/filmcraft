//! Explicit long-running delivery checks; outputs are kept outside the repository.
//! FILMCRAFT_DELIVERY_DIR=/absolute/path cargo test --release -p filmcraft-platform
//! --test hevc_delivery_validation -- --ignored --nocapture --test-threads=1
//! FILMCRAFT_REQUIRE_NVENC=1 selects the strict Windows NVENC matrix and fails on other platforms.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use filmcraft_color::{ColorPipeline, WorkingSpace};
use filmcraft_export::{ExportError, ExportSettings, Format, HardwareEncoding, Progress, VideoEncoder, export, hw_encode_stats};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, Generator, MediaSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::SourceMap;
use filmcraft_time::{FrameRate, Tick, TimeRange};

static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn broken_hardware(format: Format, _: u32, _: u32, _: FrameRate, _: &ExportSettings) -> Option<filmcraft_export::Result<Box<dyn VideoEncoder>>> {
    if format != Format::Hevc {
        return None;
    }
    ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    Some(Err(ExportError::Encode("injected hardware session initialization failure".into())))
}

fn project(w: u32, h: u32, frames: i64, working: WorkingSpace) -> (Arc<Project>, filmcraft_project::ItemId, SourceMap) {
    let rate = FrameRate::FPS_24;
    let mut p = Project::new("HEVC delivery validation");
    let g = GeneratorSource::new(Generator::Demo(DemoScene::OceanSunset), w, h, rate, rate.tick_of(frames));
    let info = g.info().clone();
    let clip = p.add_item(
        &info.name.clone(),
        Label::Iris,
        ItemKind::Media(Box::new(MediaClip {
            media: MediaRef::Generator(g.generator.clone()),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        })),
        None,
    );
    let seq = p.new_sequence(
        "delivery",
        SequenceSettings { width: w, height: h, frame_rate: rate, color: ColorPipeline { working, ..ColorPipeline::REC709 }, ..Default::default() },
        1,
        0,
        None,
    );
    let v = p.make_track_item(clip, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, rate.tick_of(frames)), rate).unwrap();
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(v);
    let mut sources = SourceMap::default();
    sources.0.insert(clip, Arc::new(g));
    (Arc::new(p), seq, sources)
}

fn deliver(name: &str, w: u32, h: u32, frames: i64, working: WorkingSpace, hardware: HardwareEncoding) {
    let dir = std::path::PathBuf::from(std::env::var_os("FILMCRAFT_DELIVERY_DIR").expect("set FILMCRAFT_DELIVERY_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.mp4"));
    let (p, seq, sources) = project(w, h, frames, working);
    let before = hw_encode_stats();
    let start = Instant::now();
    let report = export(
        &p,
        seq,
        &ExportSettings {
            format: Format::Hevc,
            path: path.to_string_lossy().into_owned(),
            include_audio: false,
            hardware_encoding: hardware,
            pixel_aspect: name.starts_with("injected-").then_some((4, 3)),
            bitrate_kbps: if w >= 7680 { 100_000 } else { 40_000 },
            ..Default::default()
        },
        &sources,
        &Progress::default(),
    )
    .unwrap();
    let seconds = start.elapsed().as_secs_f64();
    let after = hw_encode_stats();
    assert_eq!(report.frames, frames as u64);
    let hw_frames = after.frames - before.frames;
    if hardware == HardwareEncoding::Off || name.starts_with("injected-") {
        assert_eq!(hw_frames, 0);
    }
    if name.starts_with("nvenc-") {
        // A valid file alone could hide a software fallback. This explicit hardware check must
        // encode every picture on NVENC, including delayed pictures drained at end of stream.
        assert_eq!(hw_frames, frames as u64, "{name}: all frames must use NVENC");
        assert_eq!(after.sessions - before.sessions, 1, "{name}: one hardware session");
        assert_eq!(after.declined - before.declined, 0, "{name}: no software fallback");
    }
    let probe = filmcraft_testkit::ffprobe_or_skip("HEVC delivery").expect("ffprobe required");
    let out = std::process::Command::new(probe)
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_streams", "-show_format", "-of", "default=nw=1"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    let metadata = String::from_utf8(out.stdout).unwrap();
    let fields: std::collections::HashMap<_, _> = metadata.lines().filter_map(|l| l.split_once('=')).collect();
    let stream = |key: &str| fields.get(key).copied().unwrap_or("");
    assert_eq!(stream("codec_name"), "hevc");
    assert_eq!(stream("codec_tag_string"), "hvc1");
    assert_eq!(stream("width"), w.to_string());
    assert_eq!(stream("height"), h.to_string());
    assert_eq!(stream("nb_read_frames"), frames.to_string());
    let hdr = working != WorkingSpace::Rec709;
    assert_eq!(stream("profile"), if hdr { "Main 10" } else { "Main" });
    assert_eq!(stream("pix_fmt"), if hdr { "yuv420p10le" } else { "yuv420p" });
    assert_eq!(
        stream("color_transfer"),
        match working {
            WorkingSpace::Rec2100Pq => "smpte2084",
            WorkingSpace::Rec2100Hlg => "arib-std-b67",
            _ => "bt709",
        }
    );
    if hdr {
        assert_eq!(stream("color_primaries"), "bt2020");
    }
    let ff = filmcraft_testkit::ffmpeg_or_skip("HEVC delivery").expect("ffmpeg required");
    let decoded = std::process::Command::new(ff).args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "null", "-"]).output().unwrap();
    assert!(decoded.status.success() && decoded.stderr.is_empty(), "{}", String::from_utf8_lossy(&decoded.stderr));
    std::fs::write(dir.join(format!("{name}.probe.txt")), &metadata).unwrap();
    let evidence = format!(
        "name,width,height,frames,seconds,fps,bytes,hardware_frames,hardware_sessions,hardware_declined\n{name},{w},{h},{frames},{seconds},{},{},{hw_frames},{},{}\n",
        frames as f64 / seconds,
        std::fs::metadata(&path).unwrap().len(),
        after.sessions - before.sessions,
        after.declined - before.declined
    );
    std::fs::write(dir.join(format!("{name}.csv")), evidence).unwrap();
    eprintln!("{name}: {seconds:.3}s, {:.3}fps, hardware frames {hw_frames}", frames as f64 / seconds);
}

#[cfg(target_os = "windows")]
fn nvenc_4k_and_8k_delivery() {
    // Unlike normal portable tests, an unavailable GPU is a failure in this opt-in validation.
    assert!(filmcraft_platform::nvenc::hevc_available(), "NVENC HEVC required");
    assert!(filmcraft_platform::nvenc::hevc_hdr_available(), "NVENC HEVC Main 10 required");
    filmcraft_platform::register();
    for (size, w, h) in [("4k", 3840, 2160), ("8k", 7680, 4320)] {
        for (signal, working) in [("sdr", WorkingSpace::Rec709), ("pq", WorkingSpace::Rec2100Pq), ("hlg", WorkingSpace::Rec2100Hlg)] {
            deliver(&format!("nvenc-{size}-{signal}"), w, h, 24, working, HardwareEncoding::Auto);
        }
    }
}

#[test]
#[ignore = "actual 4K/8K exports; requires explicit output directory and external oracles"]
fn delivery_matrix_and_hardware_initialization_failure() {
    if std::env::var("FILMCRAFT_REQUIRE_NVENC").is_ok_and(|v| v == "1") {
        #[cfg(target_os = "windows")]
        {
            // Select the hardware-only matrix before the software/failure matrix mutates the registry.
            nvenc_4k_and_8k_delivery();
            return;
        }
        #[cfg(not(target_os = "windows"))]
        panic!("FILMCRAFT_REQUIRE_NVENC=1 requires Windows");
    }
    // The legacy HEVC contract tries hardware regardless of HardwareEncoding::Off.
    // Test software before registering any hardware factories in this isolated binary.
    for (name, w, h, frames, ws, hw) in [
        ("software-4k-sdr", 3840, 2160, 4, WorkingSpace::Rec709, HardwareEncoding::Off),
        ("software-4k-pq", 3840, 2160, 4, WorkingSpace::Rec2100Pq, HardwareEncoding::Off),
        ("software-4k-hlg", 3840, 2160, 4, WorkingSpace::Rec2100Hlg, HardwareEncoding::Off),
        ("software-8k-sdr", 7680, 4320, 2, WorkingSpace::Rec709, HardwareEncoding::Off),
        ("software-8k-pq", 7680, 4320, 2, WorkingSpace::Rec2100Pq, HardwareEncoding::Off),
        ("auto-4k-pq", 3840, 2160, 4, WorkingSpace::Rec2100Pq, HardwareEncoding::Auto),
        ("auto-8k-pq", 7680, 4320, 2, WorkingSpace::Rec2100Pq, HardwareEncoding::Auto),
    ] {
        if name.starts_with("auto-") {
            filmcraft_platform::register();
        }
        deliver(name, w, h, frames, ws, hw);
    }
    // In its own test binary, with no concurrent registry users: a real export, not a mock success.
    filmcraft_export::register_encoder(broken_hardware);
    deliver("injected-hardware-failure-pq", 1280, 720, 8, WorkingSpace::Rec2100Pq, HardwareEncoding::Auto);
    assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 1);
}

#[cfg(not(target_os = "windows"))]
#[test]
fn requiring_nvenc_rejects_non_windows_before_export() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "delivery_matrix_and_hardware_initialization_failure", "--nocapture"])
        .env("FILMCRAFT_REQUIRE_NVENC", "1")
        .env_remove("FILMCRAFT_DELIVERY_DIR")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "strict NVENC validation unexpectedly passed");
    assert!(stderr.contains("FILMCRAFT_REQUIRE_NVENC=1 requires Windows"), "unexpected failure: {stderr}");
}
