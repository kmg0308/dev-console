# GitHub와 릴리스 계약

## CI 증거 범위

`.github/workflows/verify.yml`은 pull request와 `main` push에서 다음을 수행합니다.

- macOS 14와 Windows Server 2022에서 TypeScript build, Rust format·clippy·test
- macOS universal `app,dmg`와 Windows x64 NSIS를 세 flavor별 unsigned build

macOS 14 runner 결과는 macOS 13 실제 동작 증거가 아니며, Windows runner 결과도 Windows용 compile·test·package 증거일 뿐 Windows 10 22H2 실제 동작 증거가 아닙니다. 저장소 보호 규칙은 모든 검사와 bundle job이 성공할 때만 통과하는 aggregate `verify`를 요구하고 Actions 권한은 read-only로 유지합니다. 이 저장소에는 자동 push, dependency PR, release workflow가 없습니다.

## Updater와 서명

공용 Tauri 설정은 일반 개발·CI bundle에서 updater artifact를 만들지 않습니다. 승인된 production build만 다음 환경과 명령으로 서명 가능한 updater artifact를 만듭니다. endpoint는 해당 flavor와 target의 manifest만 제공해야 합니다.

- 공통 필수: `TAURI_UPDATER_PUBLIC_KEY`, HTTPS `TAURI_UPDATER_ENDPOINT`, `TAURI_SIGNING_PRIVATE_KEY`; 암호화된 updater key라면 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`도 설정
- macOS 필수: `APPLE_SIGNING_IDENTITY`와 notarization용 `APPLE_API_ISSUER`·`APPLE_API_KEY`·절대 경로 `APPLE_API_KEY_PATH`, 또는 `APPLE_ID`·`APPLE_PASSWORD`·`APPLE_TEAM_ID`
- Windows 필수: `WINDOWS_CERTIFICATE_THUMBPRINT`, HTTPS `WINDOWS_TIMESTAMP_URL`; 인증서와 credentials는 Windows certificate store 또는 서명 공급자 계약에 따라 별도 준비

```sh
npm run release -- token-meter universal-apple-darwin
npm run release -- runtime-atlas universal-apple-darwin
npm run release -- dev-console universal-apple-darwin
npm run release -- token-meter x86_64-pc-windows-msvc
npm run release -- runtime-atlas x86_64-pc-windows-msvc
npm run release -- dev-console x86_64-pc-windows-msvc
```

macOS target은 `app,dmg`, Windows target은 `nsis`만 생성합니다. 스크립트는 private `0700` 임시 디렉터리의 `0600` overlay에 updater public key·endpoint와 필요한 공개 서명 설정만 넣고 종료 시 삭제합니다. private updater key와 password는 환경에만 유지합니다.

TokenMeter의 로컬 macOS updater 변조 거부와 정상 설치 흐름은 임시 key·localhost·격리된 HOME을 쓰는 다음 harness로 검증합니다. 출력된 workspace 명령으로 `tampered` 확인 후 `valid` 설치를 확인하고 반드시 stop합니다.

```sh
npm run qa:updater:macos
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
