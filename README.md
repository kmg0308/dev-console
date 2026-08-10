# DevConsole

TokenMeter, Runtime Atlas, DevConsole의 canonical monorepo입니다. 세 앱은 하나의 Rust core, React UI, Tauri host를 조합하는 얇은 flavor입니다.

| flavor | 기능 | 추가 산출물 |
| --- | --- | --- |
| `token-meter` | TokenMeter | 없음 |
| `runtime-atlas` | Runtime Atlas | `runtime-atlas`, `runtime-atlas-supervisor` sidecar |
| `dev-console` | 두 기능 | Runtime Atlas sidecar 두 개 |

도메인 코드는 `crates/`, 공용 UI는 `ui/`, 창·IPC·bundle 경계는 `src-tauri/`, flavor 설정은 `apps/`에 있습니다. 지원 기준은 macOS 13 이상과 Windows 10 22H2 이상 x64입니다.

## 다운로드

| 앱 | macOS universal | Windows x64 |
| --- | --- | --- |
| TokenMeter | [![TokenMeter macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/TokenMeter_0.2.1_universal.dmg) | [![TokenMeter Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/TokenMeter_0.2.1_x64-setup.exe) |
| Runtime Atlas | [![Runtime Atlas macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/RuntimeAtlas_0.2.1_universal.dmg) | [![Runtime Atlas Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/RuntimeAtlas_0.2.1_x64-setup.exe) |
| DevConsole | [![DevConsole macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/DevConsole_0.2.1_universal.dmg) | [![DevConsole Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.1/DevConsole_0.2.1_x64-setup.exe) |

`v0.2.1`은 GitHub Actions가 빌드하고 검사한 unsigned 테스트 릴리스입니다. macOS에서는 앱을 Control-클릭한 뒤 **열기**를 선택해야 할 수 있고, Windows에서는 공식 GitHub Release에서 받은 파일인지 확인한 뒤 SmartScreen의 **추가 정보 → 실행**을 선택해야 할 수 있습니다. CI는 macOS 14와 Windows Server 2022에서 실행되므로 macOS 13과 Windows 10 22H2 실제 동작은 직접 확인해야 합니다.

## 개발과 검증

Node.js 24와 `rustup`을 준비합니다. Rust 버전은 `rust-toolchain.toml`에 고정되어 있습니다. Windows 빌드에는 Visual Studio Build Tools의 `Desktop development with C++` 워크로드와 Windows SDK도 필요합니다.

```sh
npm ci
npm run check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

앱을 개발 모드로 실행합니다.

```sh
npm run tauri:dev:token-meter
npm run tauri:dev:runtime-atlas
npm run tauri:dev:dev-console
```

macOS universal unsigned bundle을 만듭니다.

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run tauri:build:token-meter -- --ci --no-sign --target universal-apple-darwin --bundles app,dmg
npm run tauri:build:runtime-atlas -- --ci --no-sign --target universal-apple-darwin --bundles app,dmg
npm run tauri:build:dev-console -- --ci --no-sign --target universal-apple-darwin --bundles app,dmg
```

Windows x64 unsigned installer는 Windows 호스트에서 만듭니다.

```powershell
rustup target add x86_64-pc-windows-msvc
npm run tauri:build:token-meter -- --ci --no-sign --target x86_64-pc-windows-msvc --bundles nsis
npm run tauri:build:runtime-atlas -- --ci --no-sign --target x86_64-pc-windows-msvc --bundles nsis
npm run tauri:build:dev-console -- --ci --no-sign --target x86_64-pc-windows-msvc --bundles nsis
```

## Runtime Atlas macOS PKG

RuntimeAtlas bundle의 CLI를 `/usr/local/bin/runtime-atlas`에도 설치하는 비재배치 PKG를 만들 수 있습니다. 아래 명령은 ad-hoc 앱과 unsigned installer의 로컬 계약 검사입니다.

PKG로 설치된 전역 CLI가 있으면 서명 updater가 앱과 CLI를 함께 교체하며, 권한 승인이 거부되거나 어느 한쪽 교체가 실패하면 둘 다 원래 상태로 되돌립니다. 전역 CLI가 없던 설치는 새로 만들지 않습니다.

```sh
APP=target/universal-apple-darwin/release/bundle/macos/RuntimeAtlas.app
APP_SIGN_IDENTITY=- npm run package:runtime-atlas:macos -- "$APP" dist/RuntimeAtlas-0.2.1.pkg
npm run verify:runtime-atlas:macos-package -- dist/RuntimeAtlas-0.2.1.pkg unsigned none
npm run test:runtime-atlas:macos-package -- "$APP"
```

배포용 PKG는 `npm run release -- runtime-atlas universal-apple-darwin`이 기존 package·verify 스크립트를 재사용해 서명·공증·staple하고 `target/releases/runtime-atlas/universal-apple-darwin/release/bundle/pkg`에 생성합니다.

서명 updater와 릴리스에 필요한 외부 설정은 [GitHub 설정](docs/github-setup.md)에 있습니다.
