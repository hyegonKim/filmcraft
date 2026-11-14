//! Round trips through FilmCraft's own H.265 decoder: every decoded picture must equal the
//! encoder's reconstruction sample for sample (any syntax or reconstruction mismatch shows), and
//! follow the source closely.

use filmcraft_hevcenc::{ColorDescription, Encoder, EncoderConfig, Frame, Packet, RateControl};

/// A moving test picture: gradients, a bright disc, edges and a little deterministic noise.
fn picture(w: usize, h: usize, t: usize, bd: u32) -> [Vec<u16>; 3] {
    let max = (1u32 << bd) - 1;
    let scale = |v: f64| -> u16 { (v.clamp(0.0, 1.0) * f64::from(max)).round() as u16 };
    let mut seed = (t as u32).wrapping_mul(2_654_435_761).wrapping_add(17);
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        f64::from(seed % 1000) / 1000.0 - 0.5
    };
    let (cx, cy, r) = (w as f64 * (0.3 + 0.02 * t as f64), h as f64 * 0.5, h.min(w) as f64 * 0.2);
    let mut y = vec![0u16; w * h];
    for j in 0..h {
        for i in 0..w {
            let (fi, fj) = (i as f64, j as f64);
            let mut v = 0.15 + 0.5 * fi / w as f64 + 0.2 * fj / h as f64;
            if (fi - cx).powi(2) + (fj - cy).powi(2) < r * r {
                v = 0.9;
            }
            if (i / 16 + j / 16) % 7 == 0 {
                v -= 0.25;
            }
            y[j * w + i] = scale(v + 0.02 * noise());
        }
    }
    let (cw, ch) = (w / 2, h / 2);
    let u = (0..cw * ch).map(|k| scale(0.4 + 0.2 * (k % cw) as f64 / cw.max(1) as f64)).collect();
    let v = (0..cw * ch).map(|k| scale(0.6 - 0.2 * (k / cw.max(1)) as f64 / ch.max(1) as f64)).collect();
    [y, u, v]
}

/// The packets as an Annex B stream (parameter sets first).
fn annexb(enc: &Encoder, packets: &[Packet]) -> Vec<u8> {
    let (vps, sps, pps) = enc.parameter_sets();
    let mut out = Vec::new();
    for ps in [vps, sps, pps] {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(ps);
    }
    for p in packets {
        let mut d = &p.data[..];
        while d.len() >= 4 {
            let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&d[4..4 + n]);
            d = &d[4 + n..];
        }
    }
    out
}

fn psnr(a: &[u16], b: &[u16], bd: u32) -> f64 {
    let max = f64::from((1u32 << bd) - 1);
    let mse = a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (max * max / mse).log10() }
}

fn planes_of(p: &filmcraft_hevc::Picture) -> [Vec<u16>; 3] {
    let wide = |pl: &filmcraft_hevc::Plane| -> Vec<u16> {
        match pl {
            filmcraft_hevc::Plane::U8(v) => v.iter().map(|&s| u16::from(s)).collect(),
            filmcraft_hevc::Plane::U16(v) => v.clone(),
        }
    };
    [wide(&p.y), wide(&p.u), wide(&p.v)]
}

/// Encode `frames` pictures, decode them, and check each decoded picture equals the encoder's
/// reconstruction; returns (bytes, worst luma PSNR against the source).
fn round_trip(cfg: EncoderConfig, frames: usize) -> (usize, f64) {
    let (w, h, bd) = (cfg.width as usize, cfg.height as usize, cfg.bit_depth);
    let mut enc = Encoder::new(cfg).expect("encoder");
    let mut packets = Vec::new();
    let mut recons = Vec::new();
    let mut sources = Vec::new();
    for t in 0..frames {
        let [y, u, v] = picture(w, h, t, bd);
        packets.push(enc.encode(&Frame { y: &y, u: &u, v: &v, y_stride: w, uv_stride: w / 2 }).expect("encode"));
        recons.push(enc.reconstruction().expect("reconstruction"));
        sources.push(y);
    }
    let stream = annexb(&enc, &packets);
    let mut dec = filmcraft_hevc::Decoder::new();
    let mut pics = dec.decode(&stream, 0).unwrap_or_else(|e| panic!("decode: {e}"));
    pics.extend(dec.flush());
    assert_eq!(pics.len(), frames, "every picture decodes");
    let mut worst = 99.0f64;
    for (i, p) in pics.iter().enumerate() {
        assert_eq!((p.width as usize, p.height as usize), (w, h), "picture {i} size");
        let got = planes_of(p);
        for c in 0..3 {
            if got[c] != recons[i][c] {
                let first = got[c].iter().zip(&recons[i][c]).position(|(a, b)| a != b).unwrap_or(0);
                let pw = if c == 0 { w } else { w / 2 };
                panic!("picture {i} plane {c}: decoder differs from the encoder's reconstruction first at ({}, {})", first % pw, first / pw);
            }
        }
        worst = worst.min(psnr(&got[0], &sources[i], bd));
    }
    (stream.len(), worst)
}

#[test]
fn intra_pictures_decode_to_the_reconstruction() {
    let mut cfg = EncoderConfig::new(352, 288, 25, 1);
    cfg.keyint = 1;
    let (bytes, worst) = round_trip(cfg, 3);
    eprintln!("intra 352x288: {bytes} bytes, worst luma PSNR {worst:.1} dB");
    assert!(worst > 30.0, "{worst:.1} dB");
}

#[test]
fn p_pictures_decode_to_the_reconstruction() {
    let mut cfg = EncoderConfig::new(352, 288, 25, 1);
    cfg.keyint = 8;
    let (bytes, worst) = round_trip(cfg, 12);
    eprintln!("IP 352x288: {bytes} bytes, worst luma PSNR {worst:.1} dB");
    assert!(worst > 30.0, "{worst:.1} dB");
}

#[test]
fn main10_hdr_signal_decodes_to_the_reconstruction() {
    let mut cfg = EncoderConfig::new(320, 240, 30, 1);
    cfg.bit_depth = 10;
    cfg.keyint = 6;
    cfg.color = ColorDescription { primaries: 9, transfer: 16, matrix: 9 };
    let (bytes, worst) = round_trip(cfg, 8);
    eprintln!("Main 10 320x240: {bytes} bytes, worst luma PSNR {worst:.1} dB");
    assert!(worst > 30.0, "{worst:.1} dB");
}

#[test]
fn sizes_off_the_coding_grid_crop_and_decode() {
    for (w, h) in [(322, 182), (66, 34), (8, 8), (2, 2), (130, 18)] {
        let mut cfg = EncoderConfig::new(w, h, 24, 1);
        cfg.keyint = 3;
        cfg.slices = 3;
        let (_, worst) = round_trip(cfg, 4);
        assert!(worst > 25.0, "{w}x{h}: {worst:.1} dB");
    }
}

#[test]
fn average_bitrate_lands_near_the_target() {
    let mut cfg = EncoderConfig::new(352, 288, 25, 1);
    cfg.keyint = 25;
    cfg.rate = RateControl::Average { kbps: 600, max_kbps: 900 };
    let (bytes, worst) = round_trip(cfg, 50);
    let kbps = bytes as f64 * 8.0 / 2.0 / 1000.0;
    eprintln!("ABR 600 kbps: {kbps:.0} kbps, worst luma PSNR {worst:.1} dB");
    assert!((300.0..1200.0).contains(&kbps), "{kbps:.0} kbps");
}

#[test]
fn hostile_configurations_are_refused() {
    for (w, h) in [(0, 16), (16, 0), (15, 16), (16, 17), (20000, 16)] {
        assert!(Encoder::new(EncoderConfig::new(w, h, 30, 1)).is_err(), "{w}x{h}");
    }
    assert!(Encoder::new(EncoderConfig::new(16, 16, 0, 1)).is_err());
    let mut cfg = EncoderConfig::new(16, 16, 30, 1);
    cfg.bit_depth = 12;
    assert!(Encoder::new(cfg).is_err());
    let mut enc = Encoder::new(EncoderConfig::new(16, 16, 30, 1)).unwrap();
    let short = vec![0u16; 10];
    assert!(enc.encode(&Frame { y: &short, u: &short, v: &short, y_stride: 16, uv_stride: 8 }).is_err());
}
