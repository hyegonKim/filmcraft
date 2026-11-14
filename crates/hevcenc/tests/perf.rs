//! Encoding speed (ignored by default; `cargo test --release -p filmcraft-hevcenc --test perf -- --ignored --nocapture`).

use std::time::Instant;

use filmcraft_hevcenc::{Encoder, EncoderConfig, Frame, RateControl};

fn run(w: usize, h: usize, frames: usize, bd: u32) {
    let mut cfg = EncoderConfig::new(w as u32, h as u32, 30, 1);
    cfg.bit_depth = bd;
    cfg.keyint = 30;
    // the export's slice count: one per two CTU rows
    cfg.slices = h.div_ceil(64).max(1);
    cfg.rate = RateControl::Average { kbps: (w * h / 200) as u32, max_kbps: (w * h / 120) as u32 };
    let mut enc = Encoder::new(cfg).unwrap();
    let max = f64::from((1u32 << bd) - 1);
    let mut bytes = 0;
    let t0 = Instant::now();
    for t in 0..frames {
        let y: Vec<u16> = (0..w * h)
            .map(|k| {
                let (i, j) = ((k % w) as f64, (k / w) as f64);
                ((0.5 + 0.3 * ((i + 4.0 * t as f64) / 31.0).sin() * ((j + t as f64) / 19.0).cos()) * max) as u16
            })
            .collect();
        let c = vec![(max / 2.0) as u16; (w / 2) * (h / 2)];
        bytes += enc.encode(&Frame { y: &y, u: &c, v: &c, y_stride: w, uv_stride: w / 2 }).unwrap().data.len();
    }
    let s = t0.elapsed().as_secs_f64();
    eprintln!("{w}x{h} {bd}-bit: {frames} frames in {s:.2} s = {:.2} fps, {bytes} bytes", frames as f64 / s);
}

#[test]
#[ignore]
fn encode_speed() {
    run(1920, 1080, 30, 8);
    run(3840, 2160, 8, 10);
    run(7680, 4320, 3, 8);
}
