# GitHub와 릴리스 계약

## CI 증거 범위

`.github/workflows/verify.yml`은 pull request와 `main` push에서 다음을 수행합니다.

- macOS 14와 Windows Server 2022에서 TypeScript build, Rust format·clippy·test
- 세 flavor의 macOS universal DMG를 임시 설치·기동하고, Windows Server 2022에서 x64 NSIS 설치·기동·제거 smoke를 수행

macOS 14 runner 결과는 macOS 13 실제 동작 증거가 아니며, Windows runner 결과도 Windows용 compile·test·package 증거일 뿐 Windows 10 22H2 실제 동작 증거가 아닙니다. 저장소 보호 규칙은 모든 검사와 bundle job이 성공할 때만 통과하는 aggregate `verify`를 요구하고 Actions 권한은 read-only로 유지합니다. 이 저장소에는 자동 push, dependency PR, release workflow가 없습니다.

## Updater와 서명

공용 Tauri 설정은 일반 개발·CI bundle에서 updater artifact를 만들지 않습니다. 승인된 production build만 다음 환경과 명령으로 서명 가능한 updater artifact를 만듭니다. endpoint URL path는 선택한 `<flavor>-<target>.json`으로 끝나야 합니다.

- 공통 필수: `TAURI_UPDATER_PUBLIC_KEY`, HTTPS `TAURI_UPDATER_ENDPOINT`, updater artifact의 정확한 HTTPS `TAURI_UPDATER_ARTIFACT_URL`, `TAURI_SIGNING_PRIVATE_KEY`; 암호화된 updater key라면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`도 설정
- macOS 필수: Developer ID Application인 `APPLE_SIGNING_IDENTITY`와 notarization용 `APPLE_API_ISSUER`·`APPLE_API_KEY`·절대 경로 `APPLE_API_KEY_PATH`. RuntimeAtlas에는 같은 팀의 Developer ID Installer인 `INSTALLER_SIGN_IDENTITY`도 필요
- Windows 필수: `WINDOWS_CERTIFICATE_THUMBPRINT`, HTTPS `WINDOWS_TIMESTAMP_URL`; 인증서와 credentials는 Windows certificate store 또는 서명 공급자 계약에 따라 별도 준비

```sh
npm run release -- token-meter universal-apple-darwin
npm run release -- runtime-atlas universal-apple-darwin
npm run release -- dev-console universal-apple-darwin
npm run release -- token-meter x86_64-pc-windows-msvc
npm run release -- runtime-atlas x86_64-pc-windows-msvc
npm run release -- dev-console x86_64-pc-windows-msvc
```

기본 산출물은 flavor 충돌을 피하도록 `target/releases/<flavor>` 아래에 생성되며, 명시한 `CARGO_TARGET_DIR`가 있으면 그 경로를 사용합니다. macOS target은 서명·공증·staple한 `app,dmg`, updater `app.tar.gz`·`.sig`와 `<flavor>-universal-apple-darwin.json`을 생성·검증합니다. RuntimeAtlas는 서명·공증·staple한 전역 CLI 포함 PKG도 `bundle/pkg`에 생성합니다. Windows target은 Tauri v2 updater이기도 한 NSIS `.exe`의 Authenticode signer·timestamp·제품명·버전, `.exe.sig`, `<flavor>-x86_64-pc-windows-msvc.json`을 검증합니다. manifest는 `version`, `url`, `.sig` 내용만 가진 해당 flavor·target 전용 응답이며 게시하지 않습니다. 스크립트는 private `0700` 임시 디렉터리의 `0600` overlay를 종료 시 삭제하고, private updater key와 Apple API credential을 필요한 Tauri bundler·signer·notary 호출에만 전달합니다. prebuild와 새 산출물을 실행하는 검증 환경에서는 이 값을 제거합니다.

TokenMeter의 로컬 macOS updater 변조 거부와 정상 설치 흐름은 임시 key·localhost·격리된 HOME을 쓰는 다음 harness로 검증합니다. 출력된 workspace 명령으로 `tampered` 확인 후 `valid` 설치를 확인하고 반드시 stop합니다.

```sh
npm run qa:updater:macos
```

Windows production release 검증과 x64 updater QA는 production과 같은 flavor identity를 사용하므로 기존 HKCU/HKLM 설치, 같은 이름의 process, 실제 `%LOCALAPPDATA%` feature data가 모두 없는 폐기 가능한 전용 Windows 10 22H2+ 호스트에서만 실행합니다. harness는 임시 updater key·localhost endpoint와 build에 고정된 격리 data root·webview directory, 격리된 설치·`HOME`·`CODEX_HOME`을 사용합니다. 출력된 순서로 tampered 거부, valid 설치·restart·ProductVersion·sidecar를 확인하고 반드시 stop하여 exact 설치 경로 process와 공식 uninstaller를 정리합니다. 이 검증은 Authenticode 증거가 아닙니다.

```powershell
npm run qa:updater:windows -- start token-meter
npm run qa:updater:windows -- start runtime-atlas
npm run qa:updater:windows -- start dev-console
```

## 릴리스 게이트

외부 push, PR, release, 배포는 자동으로 수행하지 않습니다. 승인된 릴리스 작업도 다음 증거가 모두 있을 때만 진행합니다.

- CI의 모든 check와 세 flavor package build 통과
- macOS 13 이상에서 세 앱의 실제 기능 흐름 확인
- Windows 10 22H2 이상 x64에서 세 앱의 test·설치·실행·업데이트 실제 확인
- macOS code signing·notarization·stapling과 Windows Authenticode 검증
- 각 flavor의 HTTPS manifest에서 잘못된 signature 거부와 올바른 signed update 설치 확인
- version, endpoint, updater public key, release artifact가 같은 flavor·target을 가리키는지 검토

Windows 호스트를 쓰기 위한 CI push/PR과 signing secret 등록은 각각 명시적 승인 후에만 수행합니다.
