//! ffmpeg (an external oracle only) decodes the encoder's streams to exactly the encoder's
//! reconstruction, and ffprobe reads Main / Main 10 at the right level with the colour description.
//! Skips without ffmpeg.

use std::process::Command;

use filmcraft_hevcenc::{ColorDescription, Encoder, EncoderConfig, Frame, RateControl};

/// BT.2020 primaries (G, B, R) and the D65 white point in 0.00002 units, 1000 / 0.005 cd/m² in
/// 0.0001 units: the mastering_display_colour_volume payload.
const MASTERING: [u8; 24] = {
    let v: [u16; 8] = [8500, 39850, 6550, 2300, 35400, 14600, 15635, 16450];
    let (max, min) = (10_000_000u32.to_be_bytes(), 50u32.to_be_bytes());
    let mut b = [0u8; 24];
    let mut i = 0;
    while i < 8 {
        let [hi, lo] = v[i].to_be_bytes();
        b[2 * i] = hi;
        b[2 * i + 1] = lo;
        i += 1;
    }
    let mut k = 0;
    while k < 4 {
        b[16 + k] = max[k];
        b[20 + k] = min[k];
        k += 1;
    }
    b
};

fn picture(w: usize, h: usize, t: usize, bd: u32) -> [Vec<u16>; 3] {
    let max = f64::from((1u32 << bd) - 1);
    let y = (0..w * h)
        .map(|k| {
            let (i, j) = ((k % w) as f64, (k / w) as f64);
            let v = 0.5 + 0.35 * ((i + 3.0 * t as f64) / 23.0).sin() * ((j - 2.0 * t as f64) / 17.0).cos();
            (v * max).round() as u16
        })
        .collect();
    let (cw, ch) = (w / 2, h / 2);
    let u = (0..cw * ch).map(|k| ((0.45 + 0.1 * ((k % cw) as f64 / 9.0).sin()) * max) as u16).collect();
    let v = (0..cw * ch).map(|k| ((0.55 + 0.1 * ((k / cw) as f64 / 7.0).cos()) * max) as u16).collect();
    [y, u, v]
}

#[test]
fn ffmpeg_decodes_the_reconstruction() {
    let Some(ff) = filmcraft_testkit::ffmpeg_or_skip("hevcenc oracle") else { return };
    let probe = filmcraft_testkit::ffprobe_or_skip("hevcenc oracle");
    let dir = std::env::temp_dir().join(format!("fc-hevcenc-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, w, h, bd, color, rate) in [
        ("main_640x360", 640usize, 360usize, 8u32, ColorDescription::default(), RateControl::ConstantQp(30)),
        ("main10_pq_480x270", 480, 270, 10, ColorDescription { primaries: 9, transfer: 16, matrix: 9 }, RateControl::Average { kbps: 1500, max_kbps: 2500 }),
        ("main_odd_322x182", 322, 182, 8, ColorDescription::default(), RateControl::ConstantQp(22)),
    ] {
        let mut cfg = EncoderConfig::new(w as u32, h as u32, 30, 1);
        cfg.bit_depth = bd;
        cfg.keyint = 10;
        cfg.color = color;
        cfg.rate = rate;
        if name.starts_with("main_odd") {
            cfg.sar = (4, 3);
        }
        if color.transfer == 16 {
            cfg.mastering_display = Some(MASTERING);
            cfg.content_light = Some((1000, 400));
        }
        let mut enc = Encoder::new(cfg).unwrap();
        let (vps, sps, pps) = enc.parameter_sets();
        let mut stream = Vec::new();
        for ps in [vps, sps, pps] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(ps);
        }
        let mut recon = Vec::new();
        for t in 0..16 {
            let [y, u, v] = picture(w, h, t, bd);
            let p = enc.encode(&Frame { y: &y, u: &u, v: &v, y_stride: w, uv_stride: w / 2 }).unwrap();
            let mut d = &p.data[..];
            while d.len() >= 4 {
                let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
                stream.extend_from_slice(&[0, 0, 0, 1]);
                stream.extend_from_slice(&d[4..4 + n]);
                d = &d[4 + n..];
            }
            for plane in enc.reconstruction().unwrap() {
                recon.extend(plane);
            }
        }
        let path = dir.join(format!("{name}.hevc"));
        std::fs::write(&path, &stream).unwrap();
        let pix = if bd > 8 { "yuv420p10le" } else { "yuv420p" };
        let out = Command::new(&ff).args(["-v", "error", "-xerror", "-i"]).arg(&path).args(["-f", "rawvideo", "-pix_fmt", pix, "-"]).output().unwrap();
        assert!(out.status.success() && out.stderr.is_empty(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
        let got: Vec<u16> = if bd > 8 {
            out.stdout.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect()
        } else {
            out.stdout.iter().map(|&b| u16::from(b)).collect()
        };
        assert_eq!(got.len(), recon.len(), "{name}: ffmpeg made {} samples, want {}", got.len(), recon.len());
        let diff = got.iter().zip(&recon).position(|(a, b)| a != b);
        assert!(diff.is_none(), "{name}: ffmpeg's picture differs from the reconstruction at sample {diff:?}");
        if let Some(probe) = &probe {
            let o = Command::new(probe)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=profile,pix_fmt,width,height,level,sample_aspect_ratio,color_primaries,color_transfer,color_space,color_range",
                    "-of",
                    "default=nw=1",
                ])
                .arg(&path)
                .output()
                .unwrap();
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            let profile = if bd > 8 { "profile=Main 10" } else { "profile=Main" };
            assert!(
                s.contains(profile) && s.contains(&format!("pix_fmt={pix}")) && s.contains(&format!("width={w}")) && s.contains(&format!("height={h}")),
                "{name}: {s}"
            );
            assert!(s.contains("color_range=tv"), "{name}: {s}");
            // square pixels are not signalled (aspect_ratio_info_present_flag 0)
            let sar = if name.starts_with("main_odd") { "sample_aspect_ratio=4:3" } else { "sample_aspect_ratio=N/A" };
            assert!(s.contains(sar), "{name}: {s}");
            if color.transfer == 16 {
                assert!(s.contains("color_transfer=smpte2084") && s.contains("color_primaries=bt2020") && s.contains("color_space=bt2020nc"), "{name}: {s}");
            } else {
                assert!(s.contains("color_transfer=bt709"), "{name}: {s}");
            }
            if color.transfer == 16 {
                // the HDR10 SEI: ffprobe reports it as frame side data
                let o = Command::new(probe).args(["-v", "error", "-show_frames", "-read_intervals", "%+#1"]).arg(&path).output().unwrap();
                let f = String::from_utf8_lossy(&o.stdout).to_string();
                assert!(f.contains("Mastering display metadata") && f.contains("max_luminance=10000000/10000"), "{name}: {f}");
                assert!(f.contains("Content light level metadata") && f.contains("max_content=1000") && f.contains("max_average=400"), "{name}: {f}");
            }
            eprintln!("{name}: {} bytes; ffprobe {}", stream.len(), s.replace('\n', " "));
        }
    }
}
