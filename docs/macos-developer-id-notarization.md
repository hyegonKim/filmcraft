# FilmCraft 빌드18 Developer ID 서명·공증 진행 상태

기준: 2026-10-18. **Developer ID 서명 완료, Apple 공증 미완료**이다.
다른 Mac에서 경고 없이 설치·실행할 수 있는 배포 완료 상태로 표시하지 않는다.

## 실제 완료한 작업

사용자의 실행 직전 승인 후 Xcode 개인 팀에서 Developer ID Application과 Developer ID Installer
인증서를 생성했다. 개인키는 로컬 키체인에 보관하며 내보내지 않았다.
배포 앱·DMG·CLI에는 Developer ID Application, PKG에는 Developer ID Installer로 서명했다.
앱의 Hardened Runtime, 인증서 체인과 보안 타임스탬프를 확인했다.

서명된 앱과 PKG를 추출한 앱의 `codesign --verify --strict --deep` 검사를 통과했다.
실행 코드·데이터 13개 섹션의 SHA-256이 이전 실제 검증된 설치 빌드18과 모두 일치한다.
프로그램 코드를 다시 컴파일하거나 원본 프로젝트·음성을 수정하지 않았다.
기존 `/Applications/FilmCraft.app`은 아직 이전에 검증한 ad-hoc 서명 빌드18이다.
새 서명 후보는 별도 디렉터리에 보존했다.

## 발견·수정한 패키징 문제

Documents의 File Provider가 서명 후 `.app`에 FinderInfo를 추가해 서명 무결성을 깨뜨리는 현상을
확인했다. 최초 실패 근거를 보존하고, 동기화되지 않는 임시 폴더에서 다시 생성해 무결성 검사를 통과했다.
기본 패키징 작업 위치도 Library/Caches/FilmCraftPackaging으로 옮겼다.
Gatekeeper·인증서 검증·보안 설정을 완화하지 않았다.

공증용 키체인 프로필 `MACOS_NOTARY_PROFILE`과 선택적인 `MACOS_NOTARY_KEYCHAIN` 지원을 추가했다.
`MACOS_REQUIRE_NOTARIZATION=1`이면 서명·공증 설정이 빠졌을 때 실패하며,
프로필 인증도 빌드·산출물 교체 전에 검사한다. 누락 설정을 거부하는 검사를 통과했다.
공증 후 PKG의 Gatekeeper install 평가도 필수 단계로 넣었다.

## 후속 검사: DMG 내부 서명 오류 수정

후속 조회에서도 기본 키체인과 명시적인 로그인 키체인이 `FilmCraft-notary`를 찾지 못했다.
사용자에게 로컬 Terminal의 실제 인증 종료 코드·오류 문구를 요청했다. 설정 명령은 이제
로그인 키체인을 명시하고, 저장 후 같은 키체인으로 인증을 재검사한 뒤에만 성공을 표시한다.

공증 대기 중 DMG를 실제로 마운트해 추가 문제를 발견했다. 원본 앱은 strict 서명 검사를 통과했지만
HFS `makehybrid` 이미지 안의 앱에는 `com.apple.FinderInfo`가 생겨 같은 검사가 실패했다.
동일한 원본 앱을 APFS 이미지에 넣자 FinderInfo 없이 검사를 통과했다.
패키징을 APFS로 바꾸고, 이미지 생성 후 읽기 전용 마운트와 내부 앱 strict 서명 검사를 필수화했다.
새 APFS DMG 자체에도 Developer ID 서명을 적용했다. 프로그램 코드는 변경하지 않았다.
추가 근거: `dmg-signature-diagnosis.json`, `apfs-dmg-regression.json`과 갱신된 산출물 해시.

## 남은 실제 차단 조건

사용자가 로컬 저장 완료를 알려 주었으나 `notarytool history --keychain-profile FilmCraft-notary`와
명시적인 로그인 키체인 조회 모두 `No Keychain password item found for profile`을 반환했다.
인증 정보가 없으므로 Apple 공증 제출 자체를 시작하지 않았다.
공증용 앱 전용 암호의 생성·입력은 사용자가 로컬에서 직접 해야 하며 채팅으로 받지 않는다.
다른 이름으로 저장했다면 프로필 이름만 확인하면 된다.

컴퓨터 제어 도구는 Terminal 앱 접근을 안전상 거부했다. 별도 로컬 설정 명령 파일을 준비했고
사용자에게 마지막 성공/오류 문구와 프로필 이름을 요청한 상태다.
앱·PKG의 실제 Gatekeeper 평가는 **rejected / Unnotarized Developer ID**이다.
Apple Accepted·티켓 첨부·최종 Gatekeeper 승인·다른 Mac 실행은 아직 확인하지 않았다.

## 현재 산출물 — 공증 전 후보

- [서명된 PKG](../dist/developer-id-build18/filmcraft-0.6.0-18-macos-aarch64.pkg)
- [서명된 DMG](../dist/developer-id-build18/filmcraft-0.6.0-18-macos-aarch64.dmg)
- [서명 상태·산출물 해시](../dist/developer-id-build18/SIGNING-STATUS-build18.json)

공증 프로필 인증이 성공하면 준비된 로컬 재개 명령으로 앱·DMG·PKG·CLI를 제출한다.
Apple Accepted 뒤 티켓을 첨부하고 앱/DMG/PKG Gatekeeper 평가와 최종 설치본 실행 검사를 마무리한다.
이 문서는 긴 한국어 영상 전사 정확도의 추가 검증이 아니다.
