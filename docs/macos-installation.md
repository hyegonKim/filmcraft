# macOS 설치본과 빌드 구분

앱의 버전 `0.6.0`은 기능 계열, `Build 18`은 개별 빌드 번호다.
`release`는 최적화 배포용 컴파일, `debug`는 개발용 컴파일을 뜻한다.
Release 표시가 기능 완성도나 Adobe 동등성을 뜻하지는 않는다.

앱 상단과 Help → About FilmCraft에서 실행 중인 바이너리의 빌드 번호와
컴파일 종류를 확인한다. About에는 빌드 날짜와 기반 커밋도 표시한다.
Finder → 정보 가져오기는 디스크에 설치된 번들의 버전을 보여준다.
업데이트 전에 열었던 프로세스는 종료·재실행 전까지 이전 코드를 실행할 수 있으므로,
실행 중인 앱에서는 About 표시를 우선한다.

## 설치

작업을 저장하고 FilmCraft를 종료한 후 PKG를 열어 `/Applications/FilmCraft.app`에 설치한다.
DMG를 열어 앱을 Applications 폴더로 복사하는 방식도 제공한다. 두 방식을 중복 실행할 필요는 없다.
PKG는 `/Applications/FilmCraft.app`의 프로세스가 실행 중이면 설치를 중단한다.
이전 개발판의 날짜형 번호·임시 번들 ID에서도 교체되도록, 명시적으로 실행한 PKG는
그 경로의 앱을 통째로 교체한다. 다른 위치의 앱을 찾아 업데이트하지 않는다.
사용자의 프로젝트, 원본 영상, 음성 모델과 환경설정은 설치 파일에 포함하지 않는다.
구버전 `~/Applications/FilmCraft Korean.app`은 별도 앱이므로 새 설치본과 혼동하지 않도록 한다.

휴대 가능한 배포본은 사용자별 절대 경로를 번들 환경변수에 넣지 않는다.
기본 데이터 위치는 `~/Library/Application Support/FilmCraft`이다.
이전 개발판이 사용한 `~/Library/Application Support/FilmCraft Korean`은 삭제하지 않는다.
기존 프로젝트는 저장한 `.fcproj` 파일로 다시 연다.

## 재현 빌드

```sh
FILMCRAFT_BUILD_NUMBER=18 packaging/macos/package.sh --arch aarch64
```

기존 패키징 경로에 Whisper 전사와 macOS 한국어·영어 음성 기능을 포함한다.
앱에 전사 모델이나 macOS 음성 파일을 중복 배포하지 않는다. 모델과 음성은 앱의 기존
다운로드·운영체제 설치 경로를 사용한다. FFmpeg는 검증 도구이며 설치본에 포함하지 않는다.
`--skip-build`를 사용해도 실행 파일의 버전·Release·빌드 번호를 먼저 대조한다.

이 Mac의 Xcode 27 링커가 최적화된 Rust proc-macro에 정렬 오류를 내는 경우,
로컬 검증을 통과한 Rust 제공 LLD와 설치된 macOS 26.5 SDK를 사용할 수 있다.
정확한 환경과 산출물 해시는 배포 검증 결과에 기록한다. 전역 개발 도구 설정은 변경하지 않는다.

이 Mac의 호환 빌드 환경은 다음과 같다. 이 경로와 SDK가 설치된 환경에서만 사용한다.

```sh
RUSTUP_TOOLCHAIN=1.95.0 \
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk \
RUSTFLAGS="-C link-arg=-fuse-ld=$HOME/.rustup/toolchains/1.95.0-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin/gcc-ld/ld64.lld" \
MACOSX_DEPLOYMENT_TARGET=13.0 FILMCRAFT_BUILD_NUMBER=18 \
packaging/macos/package.sh --arch aarch64
```

## 외부 배포 서명

공개 배포에는 앱용 Developer ID Application, PKG용 Developer ID Installer 인증서와
Apple 공증이 필요하다. `MACOS_SIGN_IDENTITY`, `MACOS_INSTALLER_IDENTITY` 및 기존
공증 환경변수를 설정하면 패키징 경로가 서명·공증을 수행한다.
인증서가 없는 로컬 빌드는 앱을 ad-hoc 서명하고 PKG를 미서명 상태로 만든다.
이 경우 다른 Mac에서 Gatekeeper가 차단할 수 있으며 Apple 공증 완료로 표시하지 않는다.
