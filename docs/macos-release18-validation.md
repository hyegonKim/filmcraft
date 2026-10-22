# FilmCraft Release 빌드18 설치·검증 결과

검증일: 2026-10-18. Apple Silicon / 이 Mac의 macOS 27.0.1에서 확인했다.

기존 `/Applications/FilmCraft.app`은 0.6.0, 개발용 Debug 빌드 `20261018.17`이었다.
현재는 0.6.0 **Release Build 18**로 교체했다. 실행 중인 바이너리가 상단에 `release #18`,
Help → About FilmCraft에 버전·빌드 번호·날짜·기반 커밋을 표시한다.
Finder 정보는 디스크 버전이며, 열어 둔 구버전은 종료·재실행 전까지 이전 코드를 실행할 수 있다.

## 설치 파일

- [PKG 설치 프로그램](../dist/release/filmcraft-0.6.0-18-macos-aarch64.pkg)
- [DMG 설치 이미지](../dist/release/filmcraft-0.6.0-18-macos-aarch64.dmg)
- [빌드·SHA-256 명세](../dist/release/BUILD18-manifest.json)
- [설치 방법·재현 환경](macos-installation.md)

PKG·DMG를 풀어 내부 앱의 모든 파일 해시를 Release 원본과 대조했고 모두 같았다.
DMG 무결성·앱 코드 서명 무결성 검사를 통과했다. 사용자별 절대 경로, 전사 모델, 검증 영상,
FFmpeg를 앱에 동봉하지 않았다. 아이콘·폰트·프로젝트 라이선스와 출처를 포함했다.
Whisper와 macOS 한국어·영어 TTS가 컴파일되어 있음을 설치 앱의 런타임에서 확인했다.

시스템 Installer의 관리자 인증 UI는 실행하지 않았다. 검증된 패키지의 앱을 원자적으로 복사해
현재 앱을 교체했고, 이전 앱은 검증 폴더의 `.app.backup`에 보존했다.
패키지의 실행 중 앱 보호 스크립트를 실제 설치 앱이 열린 상태에서 실행해 설치 거부를 확인했다.
다른 세션의 앱을 강제로 종료하지 않았다.

## 실제 설치 앱 확인

빌드17에서 새로 전사한 실제 발화 10분·60분 프로젝트를 **빌드18 설치 실행 파일**로 열었다.
원본 프로젝트에 저장하지 않고 별도 검증 사본을 사용했다.

| 확인 | 10분 프로젝트 | 60분 프로젝트 |
|---|---|---|
| 마커 편집·Undo·Redo | 전체 프로젝트 JSON 비교 통과 | 전체 프로젝트 JSON 비교 통과 |
| 저장·재열기 | 전체 프로젝트 JSON 비교 통과 | 전체 프로젝트 JSON 비교 통과 |
| 내보내기 | 0~3초 | 3500~3503초 |
| 출력 검사 | 3초·72프레임, H.264/AAC 전체 디코딩 | 3초·72프레임, H.264/AAC 전체 디코딩 |
| 원본 보존 | 프로젝트 해시 유지, 영상은 기존 검증 SHA-256과 일치 | 프로젝트 해시 유지, 영상은 기존 검증 SHA-256과 일치 |

초기 검증 시 스크립트가 내보내기 작업을 `transcript.jobs`에서 조회해 중단됐다.
조회 대상을 `jobs.list`로 수정하고 최종 검사를 모두 다시 통과했다. 앱 바이너리는 변경하지 않았다.

설치본 실행 검사는 `scripts/verify_macos_release.py`로 통과했다. About·컨트롤 채널·명령행·번들 정보의
빌드 일치를 실제 실행에서 대조했다. 별도 headless `chrome_ui` 모음은 중복 컴파일을 중단했으며,
해당 모음이 통과했다고 보고하지 않는다.

## 확인 범위와 남은 제한

이번 빌드는 설치 형식·식별 표시·최적화 컴파일을 바꿨다. **Release는 기능 완성 보증이 아니다.**
전체 길이 새 전사·전사 정확도·추임새 제거·잠금/복수 트랙 조합을 빌드18에서 다시 돌린 검사가 아니다.
그 결과와 남은 UI 지연·전사/경계 제약은 [빌드17 보고서](real-korean-validation.md)를 따른다.
3초 출력 검사를 10분·60분 전체 출력 검증으로 표시하지 않는다. 다른 Mac·Intel·macOS 13 실행은 미검증이다.

현재 Developer ID Application / Developer ID Installer 서명·Apple 공증은 없다.
앱은 ad-hoc 서명, PKG는 미서명이며 `spctl`은 실제로 `rejected`, `pkgutil`은 `no signature`를 반환했다.
외부 Mac에서 경고 없이 설치 가능한 정식 공개 배포 상태는 아니다. Gatekeeper 설정은 변경하지 않았다.

근거는 `real-korean-validation/release18/installation.json`, `runtime-final/report.json`,
`source-preservation.json`, 실제 About 캡처와 설치본 해시 명세에 보존했다.
원본 영상 문맥: https://www.youtube.com/watch?v=XwY9bW2r3hU · 실제 발화 출처: https://www.openslr.org/113/ .
이번 작업에서 원본 YouTube 영상을 다시 시청·분석하지 않았다.
