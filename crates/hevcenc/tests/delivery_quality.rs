//! Explicit reproducible synthetic-source quality/speed measurement, not a camera-footage benchmark.
use filmcraft_hevcenc::{Encoder, EncoderConfig, Frame, RateControl};
use std::io::{Read, Write};
use std::time::Instant;

#[test]
#[ignore = "120-frame 720p quality sweep; requires external ffmpeg and FILMCRAFT_DELIVERY_DIR"]
fn moving_source_quality_and_speed() {
    let dir = std::path::PathBuf::from(std::env::var_os("FILMCRAFT_DELIVERY_DIR").expect("set output directory"));
    std::fs::create_dir_all(&dir).unwrap();
    let ff = filmcraft_testkit::ffmpeg_or_skip("HEVC quality").expect("ffmpeg required");
    let source = dir.join("source-720.yuv");
    let generated = std::process::Command::new(ff)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=24", "-frames:v", "120", "-pix_fmt", "yuv420p", "-f", "rawvideo"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));
    let (w, h, frames) = (1280usize, 720usize, 120usize);
    let mut rows = String::from("target_kbps,actual_kbps,seconds,fps,bytes,psnr_all,psnr_y,worst_frame_psnr_y,slices\n");
    for kbps in [1000, 2000, 4000] {
        let mut cfg = EncoderConfig::new(w as u32, h as u32, 24, 1);
        cfg.rate = RateControl::Average { kbps, max_kbps: kbps * 3 / 2 };
        cfg.keyint = 48;
        cfg.slices = h.div_ceil(128).clamp(1, 16);
        let mut enc = Encoder::new(cfg.clone()).unwrap();
        let mut output = std::fs::File::create(dir.join(format!("software-{kbps}.hevc"))).unwrap();
        let mut recon_file = std::fs::File::create(dir.join(format!("software-{kbps}.recon.yuv"))).unwrap();
        for ps in [enc.parameter_sets().0, enc.parameter_sets().1, enc.parameter_sets().2] {
            output.write_all(&[0, 0, 0, 1]).unwrap();
            output.write_all(ps).unwrap();
        }
        let mut input = std::fs::File::open(&source).unwrap();
        let mut buf = vec![0u8; w * h * 3 / 2];
        let mut seconds = 0.0;
        let (mut squared, mut y_squared, mut worst) = (0.0f64, 0.0f64, 999.0f64);
        let mut bytes = 0usize;
        for _ in 0..frames {
            input.read_exact(&mut buf).unwrap();
            let wide: Vec<u16> = buf.iter().map(|&x| u16::from(x)).collect();
            let (y, uv) = wide.split_at(w * h);
            let (u, v) = uv.split_at(w * h / 4);
            let start = Instant::now();
            let packet = enc.encode(&Frame { y, u, v, y_stride: w, uv_stride: w / 2 }).unwrap();
            seconds += start.elapsed().as_secs_f64();
            bytes += packet.data.len();
            let mut data = packet.data.as_slice();
            while !data.is_empty() {
                assert!(data.len() >= 4);
                let n = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
                assert!(n <= data.len() - 4);
                output.write_all(&[0, 0, 0, 1]).unwrap();
                output.write_all(&data[4..4 + n]).unwrap();
                data = &data[4 + n..];
            }
            let rec = enc.reconstruction().unwrap();
            let mut offset = 0;
            for (plane, samples) in rec.iter().enumerate() {
                let mut sum = 0.0;
                let mut raw = Vec::with_capacity(samples.len());
                for &sample in samples {
                    assert!(sample <= 255);
                    let delta = f64::from(sample) - f64::from(buf[offset]);
                    sum += delta * delta;
                    raw.push(sample as u8);
                    offset += 1;
                }
                recon_file.write_all(&raw).unwrap();
                squared += sum;
                if plane == 0 {
                    y_squared += sum;
                    worst = worst.min(10.0 * (255.0 * 255.0 * (w * h) as f64 / sum).log10());
                }
            }
        }
        let psnr_all = 10.0 * (255.0 * 255.0 * (w * h * 3 / 2 * frames) as f64 / squared).log10();
        let psnr_y = 10.0 * (255.0 * 255.0 * (w * h * frames) as f64 / y_squared).log10();
        assert!(psnr_y.is_finite() && psnr_y > 20.0, "catastrophic distortion: {psnr_y}");
        let actual = bytes as f64 * 8.0 * 24.0 / frames as f64 / 1000.0;
        let row = format!("{kbps},{actual},{seconds},{},{bytes},{psnr_all},{psnr_y},{worst},{}\n", frames as f64 / seconds, cfg.slices);
        eprint!("{row}");
        rows.push_str(&row);
    }
    std::fs::write(dir.join("quality.csv"), rows).unwrap();
}
