# FilmCraft H.265 4K·8K SDR·HLG — Apple M1(VideoToolbox), 2026-11-26

2026-11-23 재시험([`h265-m1-videotoolbox-retest-2026-11-23.md`](h265-m1-videotoolbox-retest-2026-11-23.md))에서 VideoToolbox로 간 4K·8K 출력은 PQ뿐이었다. 납품 테스트의 시나리오 목록에 VideoToolbox(`auto-`) 경우가 PQ 두 개만 있었기 때문이다. VideoToolbox 인코더는 SDR(Main)과 HLG(Main 10)도 받으므로, 목록에 `auto-4k-sdr`, `auto-4k-hlg`, `auto-8k-sdr`, `auto-8k-hlg`와 빠져 있던 `software-8k-hlg`를 더해 다시 돌렸다.

## 소스와 환경

- 소스: main `e28a294f`(PR #28 병합)에 `crates/platform/tests/hevc_delivery_validation.rs`의 시나리오 5개를 더한 것. 테스트 밖의 코드는 main과 같다.
- Apple M1(8 CPU), RAM 16GiB, macOS 27.2.1, Rust/Cargo 1.99.1, 외부 FFmpeg/ffprobe 9.0.2.
- 릴리스 프로필, `--locked`, `FILMCRAFT_REQUIRE_ORACLES=1`. 전원 연결, `caffeinate -is`로 잠들지 않게 했다. 2026-11-26 12:08~12:18(KST), 새로 만든 빈 출력 폴더.

## 결과

`hevc_delivery_validation`의 `delivery_matrix_and_hardware_initialization_failure`가 통과했다(1 passed, 0 failed, 88.48초). 테스트가 확인하는 항목은 11-23 기록과 같다. ffprobe로 `hevc`/`hvc1`, 해상도, 프레임 수, 프로필, 픽셀 형식, 전달 함수, HDR의 원색을 보고, FFmpeg 디코딩에 오류가 없는지 본다.

| 시나리오 | 해상도 | 프레임 | 소요 초 | VideoToolbox 프레임 | 세션 | 거절 | SHA-256 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| auto-4k-sdr | 3840×2160 | 4 | 4.463 | 4 | 1 | 0 | f2e66c1126ad8ea10199babb35730beaeacc9336ef42d8708fe9932a020d62ee |
| auto-4k-pq | 3840×2160 | 4 | 6.773 | 4 | 1 | 0 | e2f6724e0972bae18baa84ca4954459ff59750e6a391e70431b41b19300941f0 |
| auto-4k-hlg | 3840×2160 | 4 | 2.423 | 4 | 1 | 0 | 3f16edc3cac21755e1f233de1ccf62ec69411aa3a3c75073d75ced21649f760d |
| auto-8k-sdr | 7680×4320 | 2 | 5.622 | 2 | 1 | 0 | 2c23f302c1cccd4849a2bca58fd29aaab27743df2088a91531229a45f87bd20f |
| auto-8k-pq | 7680×4320 | 2 | 4.343 | 2 | 1 | 0 | 669a6ca3f5b46a64244a9dc42bdf88cf42278706f4b50ef9f21120ec1afce522 |
| auto-8k-hlg | 7680×4320 | 2 | 6.739 | 2 | 1 | 0 | 410d657b258f9e1d05fb6540ecf167af6e4709483de2d77349baa1baeb2b78e7 |
| software-8k-hlg | 7680×4320 | 2 | 18.370 | 0 | 0 | 0 | 577e725671f16295e90ec225c1dd283c02e93d51bfcd9a3ae1e7f26faa77ad7c |

- `auto-` 6개는 모든 프레임이 VideoToolbox 세션 하나로 인코딩됐다(출력마다 남은 CSV의 카운터). SDR은 Main, `yuv420p`, 원색·전달 함수·행렬 모두 `bt709`. HLG는 Main 10, `yuv420p10le`, `arib-std-b67`, `bt2020`. PQ는 11-23과 같이 Main 10, `smpte2084`, `bt2020`.
- `software-8k-hlg`는 자체 인코더로 Main 10, `arib-std-b67`, `bt2020`이다.
- 11-23에도 있던 자체 인코더 출력 5개(`software-4k-sdr`·`pq`·`hlg`, `software-8k-sdr`·`pq`)와 실패 주입 출력은 SHA-256이 11-23 기록과 바이트 단위로 같다. VideoToolbox 출력은 기계와 OS에 따라 달라질 수 있어 비교하지 않았다.
- 13개 파일을 모두 `ffmpeg -v warning -xerror`로 다시 디코딩했고, 종료 코드 0에 경고가 없었다.
- 실행 로그에 `SKIPPED`가 없었다.

같은 소스로 `videotoolbox_hevc_hdr_export`도 3개 모두 통과했다. 720p PQ·HLG 출력의 수치(ProRes HDR 대비 최저 휘도 PSNR PQ 58.3dB, HLG 57.2dB, 첫 프레임 FFmpeg·FilmCraft 디코더 차이 0)는 11-23과 같다.

## 하지 않은 것

- 클립이 짧고(4K 4프레임, 8K 2프레임) 합성 장면이다. 화질은 재지 않았고, 소요 시간은 렌더링부터 컨테이너까지 합친 값이라 인코더 속도의 근거가 아니다.
- 데스크톱 앱의 내보내기 화면, 긴 영상, 실제 촬영 소스, Linux는 시험하지 않았다.
- Windows NVENC는 다시 돌리지 않았다.

## 재현

11-23 기록의 재현 절차와 같다. 새로 만든 빈 출력 폴더를 쓰고, 통과한 뒤 `auto-*.csv` 6개에서 `hardware_frames`가 프레임 수와 같고 `hardware_sessions`가 1인지 확인한다.
