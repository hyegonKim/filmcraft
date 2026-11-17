# HEVC and 8K on RTX 4070 Ti — 2026-11-17

FilmCraft successfully exported and hardware-decoded 4K and 8K HEVC in SDR, PQ and HLG on
Windows. All 24 pictures of every export used NVENC, with one encoder session and zero declines.
The independent 8K GPU compositor check also passed, with both frames submitted to the GPU and
reported RGB p99 error 0, mean error 0.000 against the CPU reference after PNG export.

## Sources and environment

- Committed source: ArtCraft `db2d0aca327d86fd425766f7b2b3ba2b4fee6f69`, FilmCraft 0.15.0,
  plus this PR's opt-in validation tests. This includes the earlier macOS H.265 work.
- Also tested: the existing, uncommitted FilmCraft 0.20.1 workspace. Its broad changes are **not**
  included in this PR and are not an immutable revision; those results do not certify a future
  0.20.1 release. The GPU test wrapper needed its existing `FrameRenderer::caps` implementation
  to compile against that workspace's newer interface.
- Windows x86-64; NVIDIA GeForce RTX 4070 Ti, 12,282 MiB; driver 621.14.
- Rust/Cargo 1.99.1; test profile (not release).
- External test oracles only: FFmpeg/ffprobe `N-126479-g08cd8df29d-20260908`.
- GPU compositor: Vulkan, `DiscreteGpu`, explicitly reported as RTX 4070 Ti.

No changes to the native encoder, decoder, compositor, dependencies or workflows are needed for
these checks. Generated movies are retained locally and are not shipped or committed.

## Actual HEVC deliveries

Each file contains 24 frames at 24 fps, generated with FilmCraft's OceanSunset source and exported
through `filmcraft_export::export`. Target bitrate is 40 Mbps for 4K and 100 Mbps for 8K. The source
is procedural, not camera footage. Each row passed in **both** source trees described above.

| Dimensions | Signal | HEVC profile / pixel format | Transfer / primaries | NVENC frames | Windows GPU decoded frames |
|---|---|---|---|---:|---:|
| 3840×2160 | SDR | Main / yuv420p | BT.709 / BT.709 | 24 | 24 |
| 3840×2160 | PQ | Main 10 / yuv420p10le | SMPTE ST 2084 / BT.2020 | 24 | 24 |
| 3840×2160 | HLG | Main 10 / yuv420p10le | ARIB STD-B67 / BT.2020 | 24 | 24 |
| 7680×4320 | SDR | Main / yuv420p | BT.709 / BT.709 | 24 | 24 |
| 7680×4320 | PQ | Main 10 / yuv420p10le | SMPTE ST 2084 / BT.2020 | 24 | 24 |
| 7680×4320 | HLG | Main 10 / yuv420p10le | ARIB STD-B67 / BT.2020 | 24 | 24 |

For every file, ffprobe counted exactly 24 decoded frames and confirmed the dimensions, `hvc1`
sample entry, profile, pixel format and transfer. FFmpeg's software decode completed with
`-xerror`, no diagnostics and no errors. The Windows check used `MfDecoder` directly, so it could
not silently switch through `HybridDecoder` to FilmCraft's software decoder. It checked every
output frame's size, bit depth and presentation timestamp, including the drained pictures.

The raw export counters, file sizes and elapsed times are in
[`main-delivery.csv`](validation/rtx-2026-11-17/main-delivery.csv) and
[`local-0201-delivery.csv`](validation/rtx-2026-11-17/local-0201-delivery.csv).
[`sha256.csv`](validation/rtx-2026-11-17/sha256.csv) identifies the retained movies. These timings
include CPU generation, rendering, colour conversion and muxing; other builds/tests ran concurrently.
They are **not NVENC throughput benchmarks or evidence of real-time 8K playback**.

## GPU rendering

`gpu_8k_export_matches_cpu_without_fallback` exported the same two 7680×4320 frames as PNG sequences
with CPU rendering and with automatic GPU rendering. It required a physical GPU adapter and checked
the actual compositor-submission counter increased by two, preventing a silent CPU fallback from
passing. Both frames had RGB p99 error **0**, mean error **0.000**, in both source trees.

This uses an SDR colour matte scaled from 320×180 to 8K. It validates full-size render targets,
readback, export integration and CPU parity. HDR HEVC encoding above uses the export pipeline's
HDR rendering path; it is not evidence that all HDR processing or all effects run on the GPU.

## Other validation and limits

- PR source: targeted clippy (`filmcraft-platform`, `filmcraft-gpu`, all targets, `-D warnings`),
  dependency layers and asset attribution checks passed.
- PR source: `cargo test -p filmcraft-platform -p filmcraft-gpu -p filmcraft-hevcenc`
  reported 222 unit/integration tests and one doctest passed, zero failed, nine ignored; the three explicit hardware validation runs
  passed separately. As in the local workspace, the unavailable HEVC 4:2:2 hardware test skipped
  its parity body.
- Local 0.20.1: platform tests reported 151 passed, zero failed; GPU tests 57 passed, zero failed,
  four intentionally ignored performance tests. The HEVC 4:2:2 hardware-decoder test reported
  an unavailable decoder and skipped its parity body; this is not a 4:2:2 hardware-support claim.
- The new hardware delivery, hardware decode and 8K GPU tests are ignored by default because they
  require physical hardware, external tools, retained files and substantial memory. The explicit
  runs above all passed, including against the PR's committed-source baseline.
- `cargo xtask ci` was attempted. It stopped at existing formatting differences in
  `crates/engine/src/export_tools.rs` and `crates/interchange/src/prproj.rs` from `db2d0aca`.
  The unchanged baseline problem is also documented in [PR #12](https://github.com/hyegonKim/ArtCraft/pull/12).
  The files changed by this PR pass `rustfmt --check` independently.
- This session did not operate the desktop export UI, rerun macOS VideoToolbox, test Linux,
  measure long-duration stability, or establish live 8K playback frame rate. Previous macOS UI
  results are recorded separately in [`h265-validation-2026-11-14.md`](h265-validation-2026-11-14.md).

## Reproduce on Windows

Run from `filmcraft/` in a PowerShell session. Set the oracle paths to your own external tools and
choose a new output directory outside the repository. This strict mode requires Windows and an NVENC
HEVC Main 10 encoder. On other operating systems it fails before starting an export. Leave
`FILMCRAFT_REQUIRE_NVENC` unset for the software/failure matrix.

```powershell
$env:FILMCRAFT_FFMPEG = 'C:\tools\ffmpeg\bin\ffmpeg.exe'
$env:FILMCRAFT_FFPROBE = 'C:\tools\ffmpeg\bin\ffprobe.exe'
$env:FILMCRAFT_REQUIRE_ORACLES = '1'
$env:FILMCRAFT_DELIVERY_DIR = Join-Path $env:TEMP 'filmcraft-hevc-rtx-deliveries'
$env:FILMCRAFT_REQUIRE_NVENC = '1'
cargo test -p filmcraft-platform --test hevc_delivery_validation -- --ignored --nocapture --test-threads=1
cargo test -p filmcraft-platform --test hevc_delivery_decode -- --ignored --nocapture --test-threads=1
cargo test -p filmcraft-gpu --test export_pipeline gpu_8k_export_matches_cpu_without_fallback -- --ignored --nocapture --test-threads=1
Remove-Item Env:FILMCRAFT_REQUIRE_NVENC
```

To exercise the pre-existing software and injected-failure matrix separately, unset
`FILMCRAFT_REQUIRE_NVENC` and run the first command in a fresh process, with a separate output
directory. That matrix was not rerun as part of the six NVENC deliveries.
