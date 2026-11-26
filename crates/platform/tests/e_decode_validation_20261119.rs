//! Decode the retained NVENC delivery matrix through FilmCraft's actual Windows GPU decoder.
//! Run after hevc_delivery_validation with FILMCRAFT_REQUIRE_NVENC=1, using the same output directory.
#![cfg(target_os = "windows")]

use filmcraft_codecs::VideoDecoder;
use filmcraft_platform::media_foundation::{MfDecoder, stream_info};

#[test]
#[ignore = "requires the retained NVENC delivery files and a Windows HEVC hardware decoder"]
fn hardware_decodes_every_4k_and_8k_delivery_frame() {
    let dir = std::path::PathBuf::from(std::env::var_os("FILMCRAFT_DELIVERY_DIR").expect("set FILMCRAFT_DELIVERY_DIR"));
    for (size, width, height) in [("4k", 3840, 2160), ("8k", 7680, 4320)] {
        for signal in ["sdr", "pq", "hlg"] {
            let name = format!("nvenc-{size}-{signal}");
            let bytes = std::fs::read(dir.join(format!("{name}.mp4"))).expect("run the NVENC delivery matrix first");
            let file = filmcraft_isobmff::open(bytes.clone()).unwrap();
            let index = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
            let track = &file.tracks[index];
            let info = stream_info(&track.entries[0]).expect("HEVC stream info");
            // Use MfDecoder directly: this test cannot succeed through HybridDecoder's fallback.
            let mut decoder = MfDecoder::new(info).expect("a Windows HEVC GPU decoder is required");
            let mut pts = Vec::new();
            let mut check = |frames: Vec<filmcraft_codecs::DecodedFrame>| {
                for decoded in frames {
                    assert_eq!((decoded.frame.width, decoded.frame.height), (width, height), "{name}: decoded dimensions");
                    match &decoded.frame.data {
                        filmcraft_frame::PixelData::Yuv8 { .. } => assert_eq!(signal, "sdr", "{name}: 8-bit output"),
                        filmcraft_frame::PixelData::Yuv16 { bits, .. } => {
                            assert_ne!(signal, "sdr", "{name}: HDR output");
                            assert_eq!(*bits, 10, "{name}: 10-bit output");
                        }
                        _ => panic!("{name}: unexpected decoder pixel format"),
                    }
                    pts.push(decoded.pts);
                }
            };
            for (sample, metadata) in track.samples.iter().enumerate() {
                let data = file.read_sample(&bytes, index, sample).unwrap();
                check(decoder.decode(&data, metadata.pts).unwrap());
            }
            check(decoder.flush());
            let mut expected: Vec<_> = track.samples.iter().map(|s| s.pts).collect();
            expected.sort_unstable();
            assert_eq!(expected.len(), 24, "{name}: delivery fixture length");
            assert_eq!(pts, expected, "{name}: every frame in presentation order, including drain");
            eprintln!("{name}: {} hardware-decoded frames via {}", pts.len(), decoder.name());
        }
    }
}
