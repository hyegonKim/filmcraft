//! 4:2:2 H.265 (the camera formats of Sony XAVC HS, Panasonic and Canon: Main 4:2:2 10) imported
//! through the software decoder: MP4 (`hvc1`) and Matroska files ffmpeg makes with libx265 give
//! 4:2:2 frames of the stream's bit depth whose every sample is ffmpeg's (ffmpeg only as an
//! external oracle and fixture generator).

use crate::common;

use common::*;
use filmcraft_frame::{Chroma, PixelData};

#[test]
fn hevc_422_imports_match_ffmpeg() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let (w, h) = (320usize, 240usize);
    for (pix_fmt, bits, ext) in [("yuv422p10le", 10u32, "mp4"), ("yuv422p10le", 10, "mkv"), ("yuv422p", 8, "mp4")] {
        let name = format!("hevc422_{pix_fmt}.{ext}");
        let mut args = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=24:duration=1,noise=alls=10:allf=t",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:bframes=3:keyint=12",
            "-pix_fmt",
            pix_fmt,
        ];
        if ext == "mp4" {
            args.extend_from_slice(&["-tag:v", "hvc1"]);
        }
        let Some(path) = fixture(&ff, &name, &args) else {
            eprintln!("SKIPPED ({name}): ffmpeg without libx265");
            return;
        };
        let src = filmcraft_codecs::open_bytes(&name, bytes(&path)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let v = src.info().video.clone().unwrap();
        assert_eq!((v.width as usize, v.height as usize), (w, h), "{name}");
        let raw = ffmpeg_frames(&ff, &path, pix_fmt);
        let bps = if bits > 8 { 2 } else { 1 };
        let frame_bytes = (w * h + 2 * (w / 2) * h) * bps;
        assert_eq!(raw.len() % frame_bytes, 0, "{name}: ffmpeg made 4:2:2 frames");
        let n = raw.len() / frame_bytes;
        assert_eq!(n, 24, "{name}");
        for (i, want) in raw.chunks(frame_bytes).enumerate() {
            let f = src.video_frame(filmcraft_media::FrameRequest::full(v.frame_rate.tick_of(i as i64))).unwrap();
            let (chroma, depth) = match &f.data {
                PixelData::Yuv8 { chroma, .. } => (*chroma, 8),
                PixelData::Yuv16 { chroma, bits, .. } => (*chroma, *bits),
                _ => panic!("{name}: frame {i} is not Y'CbCr"),
            };
            assert_eq!((chroma, depth), (Chroma::C422, bits), "{name}: frame {i}");
            assert!(planes(&f) == raw_planes(want, w, h, w / 2, h, bps), "{name}: frame {i} differs from ffmpeg's");
        }
        eprintln!("{name}: {n} frames, every sample equals ffmpeg's");
    }
}
