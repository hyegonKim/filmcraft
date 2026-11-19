# Confirmed E-drive checkout: HEVC and 8K validation — 2026-11-19

The operator confirmed the E-drive ArtCraft checkout as the source to test. Its final working
copy identifies itself as FilmCraft **0.20.1**, with substantial uncommitted changes. The
results below were obtained from that checkout, not inferred from the separate PR checkout's
0.15.0 results. They do not certify an immutable 0.20.1 release or include its broad source
changes in this PR.

## Source consistency before testing

Initially the checkout combined a 0.15.0 workspace manifest with newer loose HEVC files. An
encoder-test dependency inherited `craft-video`, which the old manifest did not declare.
During diagnosis, an existing C-to-E Robocopy job began updating the same checkout to 0.20.1.
There was then a second mismatch: `filmcraft-time` still imported `serde` while its new manifest
used `craft-timeline`. A check using a fresh build directory reproduced that failure.

The pending source/manifests were copied ahead of that existing synchronization, preserving
the previous contents in a separate backup. No E-only files were deleted. The final source
then passed `cargo check -p filmcraft-platform --tests` in the fresh build directory. Earlier
cached runs and the transient 0.15.0 encoder test run are not the evidence for the results below.

The final hardware checks rebuilt the affected libraries from the E-drive source. Their
build output used a separate cache directory; that does not change the source checkout.
Temporary integration-test harnesses came from this PR, including its explicit NVENC mode.
The 0.20.1 GPU wrapper also implemented its existing `FrameRenderer::caps` requirement.
The 313 fingerprinted source/manifest inputs remained unchanged during the final validation;
[e-source-receipt.json](validation/rtx-2026-11-19/e-source-receipt.json) records the input-set
fingerprint and the workspace manifest/lockfile hashes. Full input hashes and backups are retained
locally rather than treating this dirty tree as a new published source revision.

## Environment and results

Windows x86-64, RTX 4070 Ti (12,282 MiB), NVIDIA 621.14, Rust/Cargo 1.99.1, test profile.
The external FFmpeg/ffprobe oracle was `N-126479-g08cd8df29d-20260908`.

All files contain 24 frames at 24 fps, generated using FilmCraft's OceanSunset source and
exported through its normal export pipeline. Every file used exactly one NVENC session,
encoded all 24 frames in hardware and recorded zero declines.

| Size | Signal | HEVC profile | Encoded frames | Windows GPU decoded frames |
|---|---|---|---:|---:|
| 3840×2160 | SDR | Main, 8-bit | 24 | 24 |
| 3840×2160 | PQ | Main 10, 10-bit | 24 | 24 |
| 3840×2160 | HLG | Main 10, 10-bit | 24 | 24 |
| 7680×4320 | SDR | Main, 8-bit | 24 | 24 |
| 7680×4320 | PQ | Main 10, 10-bit | 24 | 24 |
| 7680×4320 | HLG | Main 10, 10-bit | 24 | 24 |

ffprobe confirmed `hvc1`, dimensions, profile, pixel format, transfer and frame count. FFmpeg
decoded every file with `-xerror` and no diagnostics. FilmCraft's `MfDecoder` then decoded all
six directly, without `HybridDecoder` software fallback, checking size, bit depth and every
presentation timestamp including the drained pictures.

The 8K GPU export check selected **RTX 4070 Ti / Vulkan / DiscreteGpu**. Both 7680×4320 PNG
frames were actually composited on the GPU. CPU-reference RGB p99 error was **0**, and mean
error was **0.000**, for both frames.

The software HEVC encoder's 22 unit/integration tests and one doctest also passed, including
Main 10 HDR reconstruction round trips and the external FFmpeg oracle. Two opt-in performance
tests remained ignored; the three explicit hardware validations above ran separately.

The recorded counters and file sizes are in [e-delivery.csv](validation/rtx-2026-11-19/e-delivery.csv);
[e-sha256.csv](validation/rtx-2026-11-19/e-sha256.csv) identifies the retained movies. Timings
include generation, CPU rendering, conversion and muxing while other compilation and disk-copy
work ran. They are not encoder throughput or real-time playback measurements.

## Scope

These are headless export-pipeline, direct GPU-decoder and GPU-compositor checks. The full 0.20.1
workspace test suite, desktop UI, all effects, long-duration stability, real-time 8K playback,
macOS VideoToolbox and Linux were not validated by this E-drive run. HDR encoding does not imply
all HDR rendering runs on the GPU.

## PR baseline verification

The separate PR checkout incorporates main `371019cc8a25a9fc3558c7b2da85a92a81dac09d`.
On Windows with Rust 1.99.1, `FILMCRAFT_REQUIRE_ORACLES=1` and `RUST_TEST_THREADS=1`:

- Workspace fmt and clippy (`--workspace --all-targets -- -D warnings`) passed.
- Native workspace tests reported **4,738 passed, 1 failed, 58 ignored**. The separate i18n
  reporting invocation is not added to these totals.
- The failure was `filmcraft-isobmff`'s `oracle_mux::aac_priming_edit`: ffprobe reported output
  `duration_ts` 96,240, compared with the expected source value 96,000. It failed identically
  when rerun alone. The isobmff source/tests and workspace manifest/lockfile are unchanged from
  the main revision above; this PR does not change AAC timing or weaken that assertion.
- All **9 doctests** passed when run separately.
- Layering, asset attribution (**69 assets**) and WASM checks (**50 crates**) passed separately.

The earlier AV1 vector-download failure was resolved by using the existing conformance-vector
cache; all five vectors then decoded bit-exactly against the external reference. The caption
font fallback test passed both in the workspace run and alone. No test assertions or ignore
attributes were relaxed to obtain these results.

`cargo xtask ci` therefore still returns failure for the AAC test, and the PR remains a draft.
The HEVC/8K hardware results above are successful checks, not a claim that the whole application
test suite is green.

The committed-source baseline and portable reproduction instructions remain in
[the RTX validation report](hevc-8k-rtx-validation-2026-11-17.md).
