# FilmCraft H.265 4K·8K SDR·HLG — Apple M1(VideoToolbox), 2026-11-26

2026-11-23 재시험([`h265-m1-videotoolbox-retest-2026-11-23.md`](h265-m1-videotoolbox-retest-2026-11-23.md))에서 VideoToolbox로 간 4K·8K 출력은 PQ뿐이었다. 납품 테스트의 시나리오 목록에 VideoToolbox(`auto-`) 경우가 PQ 두 개만 있었기 때문이다. VideoToolbox 인코더는 SDR(Main)과 HLG(Main 10)도 받으므로, 목록에 `auto-4k-sdr`, `auto-4k-hlg`, `auto-8k-sdr`, `auto-8k-hlg`와 빠져 있던 `software-8k-hlg`를 더했다([PR #29](https://github.com/hyegonKim/ArtCraft/pull/29)).

PR #29 리뷰에 따라 테스트에 확인 두 가지를 더했다.

- `FILMCRAFT_REQUIRE_HW_ENCODE=1`이면 `auto-` 출력마다 모든 프레임이 하드웨어 세션 하나로 인코딩됐는지 단언한다. 이 변수가 없으면 이전처럼 하드웨어가 거절해도 자체 인코더로 통과한다(하드웨어 인코더가 없는 기계용). Windows 전용 `nvenc-` 시나리오(`FILMCRAFT_REQUIRE_NVENC=1`)는 이와 따로 늘 하드웨어를 단언한다.
- 모든 출력의 원색(`color_primaries`)과 행렬(`color_space`)을 단언한다. SDR은 `bt709`/`bt709`, HDR은 `bt2020`/`bt2020nc`다. 이전에는 원색을 HDR에서만 봤고 행렬은 보지 않았다.

## 소스와 환경

- 소스: main `d2347b1d`에 이 테스트 변경과 [PR #30](https://github.com/hyegonKim/ArtCraft/pull/30)의 macOS 빌드 수정(`crates/platform/src/hardware_encode.rs`)을 더한 것. `d2347b1d` 그대로는 macOS에서 `filmcraft-platform`이 컴파일되지 않는다.
- Apple M1(8 CPU), RAM 16GiB, macOS 27.2.1, Rust/Cargo 1.99.1, 외부 FFmpeg/ffprobe 9.0.2.
- 릴리스 프로필, `--locked`, `FILMCRAFT_REQUIRE_ORACLES=1`, `FILMCRAFT_REQUIRE_HW_ENCODE=1`. 전원 연결, `caffeinate -is`로 잠들지 않게 했다. 2026-11-26 20:12~20:17(KST), 새로 만든 빈 출력 폴더.

## 결과

`hevc_delivery_validation`의 `delivery_matrix_and_hardware_initialization_failure`가 통과했다(1 passed, 0 failed, 56.67초). 테스트는 ffprobe로 `hevc`/`hvc1`, 해상도, 프레임 수, 프로필, 픽셀 형식, 전달 함수, 원색, 행렬을 보고, FFmpeg 디코딩에 오류가 없는지 본다.

| 시나리오 | 해상도 | 프레임 | 소요 초 | VideoToolbox 프레임 | 세션 | 거절 | SHA-256 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| auto-4k-sdr | 3840×2160 | 4 | 1.490 | 4 | 1 | 0 | 61e84be7b02a70620cb0ea77432151a9d807e1797590670269ed1190e96e298d |
| auto-4k-pq | 3840×2160 | 4 | 1.633 | 4 | 1 | 0 | 226689b312dcd892c0f66f7680f847581cddf527a5a211ee292abb6cd2c6d215 |
| auto-4k-hlg | 3840×2160 | 4 | 1.059 | 4 | 1 | 0 | 28a5dc70a59f1d65a74f45080fd3cf562a388335468ca9ab6759af09d781bb54 |
| auto-8k-sdr | 7680×4320 | 2 | 3.637 | 2 | 1 | 0 | 53b03cb4294efd1b6451fc49aac8273568e410de654ef20310b3812bf3de61cd |
| auto-8k-pq | 7680×4320 | 2 | 10.174 | 2 | 1 | 0 | c77d8b395f0d2abf1ca141d2a2ca8fbaf6a86ccdc8a2b544a70f657ba63ae2ad |
| auto-8k-hlg | 7680×4320 | 2 | 2.594 | 2 | 1 | 0 | c58125c20238124e4152cde96ee0b17d20157e69fe72313ef5598ce61df38f06 |
| software-8k-hlg | 7680×4320 | 2 | 5.664 | 0 | 0 | 0 | 6991b41e537c8baeef6ce417db17f4a3f6cc3e323e39b13d13ab3c8eb5993b5a |

- `auto-` 6개는 테스트가 단언한 대로 모든 프레임이 VideoToolbox 세션 하나로 인코딩됐다(출력마다 남은 CSV의 카운터와도 같다). SDR은 Main, `yuv420p`, `bt709`. HLG는 Main 10, `yuv420p10le`, `arib-std-b67`, `bt2020`. PQ는 11-23과 같이 Main 10, `smpte2084`, `bt2020`.
- `software-8k-hlg`는 자체 인코더로 Main 10, `arib-std-b67`, `bt2020`이다.
- 자체 인코더 출력 7개(실패 주입 포함)는 영상 비트스트림과 디코딩된 프레임(framemd5)이 같은 날 `e28a294f` 기반 실행과 같다. 파일은 MP4 `©too` 태그의 버전 문자열(`FilmCraft 0.15.0` → `FilmCraft 0.20.1`) 3바이트만 다르다. 11-23에도 있던 6개는 `e28a294f` 기반 실행에서 11-23 기록과 SHA-256까지 같았다.
- VideoToolbox 출력은 같은 Mac, 같은 소스에서도 실행마다 바이트가 다르다. 이날 세 번 돌린 실행에서 `auto-` 6개의 SHA-256이 매번 모두 달랐다.
- 13개 파일을 모두 `ffmpeg -v warning -xerror`로 다시 디코딩했고, 종료 코드 0에 경고가 없었다.
- 실행 로그에 `SKIPPED`가 없었다.
- 새 하드웨어 단언이 실제로 실패하는지 따로 봤다(`e28a294f` 기반). `auto-4k-sdr`에 비정사각 픽셀을 임시로 줘서 VideoToolbox가 거절하게 하자 `FILMCRAFT_REQUIRE_HW_ENCODE=1`에서 `(0, 0)`과 `(4, 1)`이 다르다며 실패했다. 이 임시 변경은 되돌렸다. 색 태그 단언은 실패하는 경우를 따로 만들어 보지 않았다.

같은 소스로 `videotoolbox_hevc_hdr_export` 3개, `filmcraft-platform` 라이브러리 테스트 37개, 비-Windows에서 `FILMCRAFT_REQUIRE_NVENC=1`을 거절하는 테스트가 통과했다.

## 하지 않은 것

- 클립이 짧고(4K 4프레임, 8K 2프레임) 합성 장면이다. 화질은 재지 않았고, 소요 시간은 렌더링부터 컨테이너까지 합친 값이라 인코더 속도의 근거가 아니다.
- 데스크톱 앱의 내보내기 화면, 긴 영상, 실제 촬영 소스, Linux는 시험하지 않았다.
- 바뀐 테스트를 Windows에서 돌려 보지 않았다. NVENC 경로는 코드상 SDR에 BT.709, HDR에 BT.2020 `colr`을 쓰지만, `nvenc-` 출력이 새 원색·행렬 단언을 통과하는지는 확인하지 않았다.

## 재현

11-23 기록의 재현 절차에서 납품 테스트에 `FILMCRAFT_REQUIRE_HW_ENCODE=1`을 더한다. 하드웨어 HEVC 인코더가 있는 기계에서만 켠다. macOS에서는 PR #30이 main에 들어간 뒤에 돌린다.

```sh
export FILMCRAFT_REQUIRE_ORACLES=1 FILMCRAFT_REQUIRE_HW_ENCODE=1
FILMCRAFT_DELIVERY_DIR=/absolute/new-empty-dir cargo test --locked --release -p filmcraft-platform --test hevc_delivery_validation -- --ignored --nocapture --test-threads=1
```
