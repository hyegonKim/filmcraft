# filmcraft-hevcenc

A clean-room, pure-Rust H.265 / HEVC encoder for FilmCraft's export path, written from the public
ITU-T H.265 specification (ISO/IEC 23008-2). Layer L0: it depends only on `filmcraft-bitstream`,
`thiserror` and (optionally, feature `threads`, on by default) `rayon`. No `unsafe`. The export
uses it for every H.265 job a hardware encoder (VideoToolbox, NVENC) declines or cannot open, so
H.265 exports on every machine.

## Usage

```rust
use filmcraft_hevcenc::{ColorDescription, Encoder, EncoderConfig, Frame, RateControl};

let mut cfg = EncoderConfig::new(3840, 2160, 30, 1);
cfg.bit_depth = 10;                                           // 8 (Main) or 10 (Main 10)
cfg.rate = RateControl::Average { kbps: 40_000, max_kbps: 48_000 }; // or ConstantQp(u8)
cfg.keyint = 60;                                              // an IDR every 60 pictures
cfg.slices = 34;                                              // fixed: the stream is the same on every machine
cfg.color = ColorDescription { primaries: 9, transfer: 16, matrix: 9 }; // BT.2020 PQ (default BT.709)
cfg.mastering_display = Some(mdcv_payload);                   // HDR10 SEI on every IDR (24 bytes)
cfg.content_light = Some((1000, 400));                        // (MaxCLL, MaxFALL)
let mut enc = Encoder::new(cfg)?;
let hvcc = enc.hvcc();                                        // HEVCDecoderConfigurationRecord for `hvcC`
for pic in pictures {
    let frame = Frame { y: &pic.y, u: &pic.u, v: &pic.v, y_stride: 3840, uv_stride: 1920 };
    let packet = enc.encode(&frame)?;                         // packet.data, packet.key, packet.index
}
```

- Input: planar 4:2:0 `u16` samples of the configured depth, with strides; any even size up to
  16384 each way. Sizes off the 8-sample coding grid are padded and cropped by the SPS conformance
  window. Short planes are an error, never a panic.
- Output: one packet per picture, 4-byte length-prefixed NAL units (MP4 samples), in output order
  (there is no reordering). The VPS / SPS / PPS are only in `parameter_sets()` and `hvcc()`.
- `reconstruction()` returns the last picture as a decoder outputs it (the tests compare decoders
  with it).

## Features

| Area | Implemented |
|---|---|
| Profiles | Main (8-bit) and Main 10, 4:2:0, Main tier; level the lowest whose limits take the picture size, frame rate, bitrate ceiling and slice count (Tables A.6, A.8) |
| Parameter sets | VPS, SPS (conformance window, VUI: sample aspect ratio, colour description, limited range, timing), PPS, `hvcC`; prefix SEI with the mastering display colour volume and content light level |
| Pictures | IDR every keyframe interval, P pictures predicted from the previous picture (one reference, no TMVP), no B-frames |
| Coding tree | 32x32 CTUs, coding units 32 / 16 / 8 chosen per CTU, 2Nx2N partitions, one transform unit per coding unit (DCT, luma 8..32, chroma 4..16) |
| Intra | 11 candidate modes (planar, DC, angular) by SATD, reference substitution and filtering exactly as the decoder, most-probable-mode coding |
| Inter | hexagon integer motion search, half- and quarter-sample refinement by SATD, merge / skip (one candidate) and AMVP with the spec's candidate derivation; motion compensation identical to the decoder's (8-tap luma, 4-tap chroma) |
| Residual | forward DCT, dead-zone quantisation (intra 1/3, inter 1/6), decoder-exact reconstruction; no sign data hiding, no transform skip |
| Entropy | CABAC throughout (context initialisation per slice type and QP, all residual syntax) |
| Loop filters | deblocking and SAO off, so the reconstruction is exactly what any decoder outputs |
| Rate control | constant QP, or a one-pass average bitrate steering each picture's QP with a ceiling (keyframes a few steps finer) |
| Threading | slices of whole CTU rows coded in parallel (feature `threads`); a fixed slice count gives the same stream on every machine |

## Verification

`cargo test --release -p filmcraft-hevcenc` (ffmpeg / ffprobe only as external oracles; tests skip
without them).

- `tests/roundtrip.rs`: FilmCraft's own decoder (`filmcraft-hevc`) decodes intra, P, Main 10 HDR,
  off-grid sizes and average-bitrate streams to exactly the encoder's reconstruction; hostile
  configurations are refused; the average bitrate lands near its target.
- `tests/oracle.rs`: ffmpeg (`-xerror`) decodes Main, Main 10 PQ (average bitrate) and an odd-sized
  4:3-pixel stream with no message, to exactly the reconstruction; ffprobe reads the profile, size,
  sample aspect ratio, limited range, colour description and the HDR10 SEI.
- Unit tests: forward / inverse transforms, intra prediction against the reference formulas, scan
  and QP tables, the level choice and the SEI payloads.
- Export (`crates/export`): an H.265 export decodes back through `filmcraft-codecs` with the
  matte's colour; HDR PQ / HLG exports are Main 10 with `colr`, `mdcv` / `clli` and the SEI; the
  file is byte-identical whatever the core count, batch size or render-ahead.

## Performance

`cargo test --release -p filmcraft-hevcenc --test perf -- --ignored --nocapture` on an Apple M1
(8 cores), the old benchmark slice count (one per two CTU rows), average bitrate. These historical
measurements predate the export's current one-per-128-rows / maximum-16 policy; rerun the
benchmark for current-policy numbers:

| Picture | Speed |
|---|---|
| 1920x1080 8-bit | 4.8 frames/s |
| 3840x2160 10-bit | 1.2 frames/s |
| 7680x4320 8-bit | 0.33 frames/s |

### Current delivery-policy run (2026-11-14)

After aligning `tests/perf.rs` with the export policy (one slice per 128 luma rows, at most 16),
this Apple M1 / 16 GiB run measured the following. Frame generation is included. These are short
synthetic clips on a machine with other applications running, not real-time or long-form guarantees.

| Picture | Frames | Seconds | Frames/s |
| --- | --- | --- | --- |
| 1920x1080 8-bit | 30 | 9.22 | 3.25 |
| 3840x2160 10-bit | 8 | 10.21 | 0.78 |
| 7680x4320 8-bit | 3 | 11.87 | 0.25 |

The explicit `delivery_quality` test also writes source/reconstruction files and a quality/bitrate
CSV for a 120-frame synthetic 720p sweep. Its encode-only timings exclude source generation and I/O;
they are not directly comparable with the table above. See
[the independent validation report](../../docs/h265-validation-2026-11-14.md) for the procedure,
quality measurements and limitations.

## Limitations / not yet implemented

- No B-frames, deblocking or SAO: files are larger than a hardware or x265 encoder's at the same
  quality. Adding the in-loop filters needs the reconstruction to run them too.
- No rectangular or asymmetric partitions, no transform tree below the coding unit, one merge
  candidate, no rate-distortion optimised quantisation.
- No 4:2:2 / 4:4:4, no 12-bit, no interlaced output.
- No two-pass or constant-quality (CRF) mode; the export refuses those for H.265.
