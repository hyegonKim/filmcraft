# FilmCraft H.265·8K 재시험 — Apple M1(VideoToolbox), 2026-11-23

Windows RTX 4070 Ti 재시험(`h265-rtx-4070-ti-e-retest-2026-11-23.md`)은 macOS VideoToolbox를 다시 돌리지 않았다. 같은 날 이 Mac에서 H.265와 8K 내보내기 시험을 다시 실행했다. 첫 실행 기록은 [`h265-validation-2026-11-14.md`](h265-validation-2026-11-14.md)에 있다.

## 소스와 환경

- 소스: [PR #23](https://github.com/hyegonKim/ArtCraft/pull/23)의 `65f716ba`. main `7603d9c7`(PR #21 병합)에 `a6b921d2`가 되돌린 파일 53개를 복구한 것이고, 2026-11-24에 같은 트리로 main `08439e6b`에 병합됐다. H.265 관련 크레이트에서 main과 다른 곳은 `crates/hevc/src/filter.rs`의 clippy 정리(동작 같음)와 Pro Apps 내보내기 코드 삭제뿐이다.
- Apple M1(8 CPU), RAM 16GiB, macOS 27.2.1, Rust/Cargo 1.99.1, 외부 FFmpeg/ffprobe 9.0.2.
- 릴리스 프로필, `--locked`, `FILMCRAFT_REQUIRE_ORACLES=1`. 2026-11-23 23:27에 시작해 11-24 01:10(KST)에 끝났다. 대부분은 컴파일 시간이다.

## 실제 출력 8개 시나리오

`hevc_delivery_validation`의 `delivery_matrix_and_hardware_initialization_failure`가 통과했다(1 passed, 0 failed, 32.95초). 각 출력에 대해 테스트가 ffprobe로 `hevc`/`hvc1`, 해상도, 프레임 수, 프로필, 픽셀 형식, 전달 함수, 원색을 확인했고, FFmpeg가 `-xerror`로 오류와 경고 없이 끝까지 디코딩했다.

| 시나리오 | 해상도 | 프레임 | 소요 초 | VideoToolbox 프레임 | 세션 | 거절 | SHA-256 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| software-4k-sdr | 3840×2160 | 4 | 4.251 | 0 | 0 | 0 | 33e9c037d027eb7a5fb154063fca1e0adfbbd0c02cc834147165a0b658148221 |
| software-4k-pq | 3840×2160 | 4 | 4.282 | 0 | 0 | 0 | 93a164bb89a295f63c06e9259d16c9e6ab0e5accf1656c637cd5546d5ff034b4 |
| software-4k-hlg | 3840×2160 | 4 | 2.941 | 0 | 0 | 0 | e620c4f62a688be9fb52e72e247d990e57238e6e53e76559f928ebbe954604da |
| software-8k-sdr | 7680×4320 | 2 | 3.898 | 0 | 0 | 0 | baecf48ed22b158826aec6a9d11b812ceec4ea78203dbf146c1308f1cf3c7ea0 |
| software-8k-pq | 7680×4320 | 2 | 7.475 | 0 | 0 | 0 | cbe5f0b3af8dfcbaa15e30a47e65dff49a872a5a172b6794a299cec4676dc17b |
| auto-4k-pq | 3840×2160 | 4 | 1.824 | 4 | 1 | 0 | eb41c64387ba3cb8240521f0592363bd45b042011e37bfd8404c80ef4443b8c8 |
| auto-8k-pq | 7680×4320 | 2 | 5.026 | 2 | 1 | 0 | a19c97903024e2ad31c9b7dd667f2b1abdaa588d55d792d8d65695f53aa05373 |
| injected-hardware-failure-pq | 1280×720 | 8 | 0.841 | 0 | 0 | 1 | fc7b4d20f8769c47bee23732f69409be6b5f6f14d09869e2277a6d9d89491350 |

`auto-` 두 개는 모든 프레임이 VideoToolbox 세션 하나로 인코딩됐고 Main 10, `yuv420p10le`, `smpte2084`, `bt2020`이다. 실패 주입은 오류를 내는 팩토리가 한 번 불리고 VideoToolbox가 비정사각 픽셀을 거절해(거절 1) 자체 인코더가 8프레임을 만들었다.

소요 시간은 렌더링·색 변환·인코딩·컨테이너 생성을 합친 값이고 다른 작업이 함께 돌고 있었다. 인코더 속도나 실시간 8K 재생의 근거가 아니다.

## 그 밖의 시험

| 명령 | 결과 |
| --- | --- |
| `cargo test --locked --release -p filmcraft-platform --test videotoolbox_hevc_hdr_export` | 3 통과, 0 실패 |
| `cargo test --locked --release -p filmcraft-hevcenc` | 23 통과, 0 실패, 2 건너뜀(`moving_source_quality_and_speed`, `encode_speed`) |
| `cargo test --locked --release -p filmcraft-hevc --test it` | 61 통과, 0 실패, 4 건너뜀(fixture 생성·coverage 보고·성능 2개) |

`videotoolbox_hevc_hdr_export`의 PQ·HLG 출력(1280×720, 24프레임)은 FFmpeg와 FilmCraft 디코더의 결과가 Y·Cb·Cr 모두 코드 값 차이 0이었다. ProRes HDR 출력과 비교한 최저 휘도 PSNR은 PQ 58.3dB, HLG 57.2dB다.

## 하지 않은 것

- Windows 시험의 6개 24프레임 묶음(4K·8K × SDR·PQ·HLG)과 같은 구성은 아니다. 이 Mac에서 VideoToolbox로 간 출력은 PQ 4K 4프레임과 PQ 8K 2프레임, 그리고 720p PQ·HLG뿐이다. VideoToolbox의 4K·8K SDR·HLG 출력은 이번에 만들지 않았다.
- 데스크톱 앱의 내보내기 화면, 긴 영상의 메모리·안정성, 8K 재생 프레임 속도는 시험하지 않았다.
- `delivery_quality`, `perf`, `fuzz_hostile`은 다시 돌리지 않았다.
- Linux는 시험하지 않았다. Windows의 NVENC 개별 테스트(`it nvenc`, `nvenc_hevc_hdr_export`, `nvenc_hevc_warm`)는 PC에서만 돌릴 수 있다.

## 재현

`filmcraft` 디렉터리에서 실행한다. 출력 폴더는 저장소 밖의 절대 경로로 한다.

```sh
export FILMCRAFT_REQUIRE_ORACLES=1
FILMCRAFT_DELIVERY_DIR=/absolute/delivery cargo test --locked --release -p filmcraft-platform --test hevc_delivery_validation -- --ignored --nocapture --test-threads=1
cargo test --locked --release -p filmcraft-platform --test videotoolbox_hevc_hdr_export -- --nocapture --test-threads=1
cargo test --locked --release -p filmcraft-hevcenc
cargo test --locked --release -p filmcraft-hevc --test it
```
