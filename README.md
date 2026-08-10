# DevConsole

개발 작업의 토큰 사용량과 Git 실행 환경을 확인하는 macOS·Windows 데스크톱 앱입니다.

## 어떤 앱을 받으면 되나요?

- **TokenMeter**: Codex, Claude Code, Hermes Agent의 토큰 사용량과 Codex 사용 한도를 확인합니다.
- **Runtime Atlas**: Git 저장소와 작업 폴더별 프로세스, 포트, Docker 컨테이너를 확인하고 등록한 명령을 실행합니다.
- **DevConsole**: TokenMeter와 Runtime Atlas를 한 앱에서 사용합니다.

## 다운로드

| 앱 | macOS | Windows x64 |
| --- | --- | --- |
| TokenMeter | [![TokenMeter macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/TokenMeter_0.2.3_universal.dmg) | [![TokenMeter Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/TokenMeter_0.2.3_x64-setup.exe) |
| Runtime Atlas | [![Runtime Atlas macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/RuntimeAtlas_0.2.3_universal.dmg) | [![Runtime Atlas Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/RuntimeAtlas_0.2.3_x64-setup.exe) |
| DevConsole | [![DevConsole macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/DevConsole_0.2.3_universal.dmg) | [![DevConsole Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/download/v0.2.3/DevConsole_0.2.3_x64-setup.exe) |

현재 배포 파일은 코드 서명이 없습니다. 반드시 이 저장소의 GitHub Releases에서 받은 파일만 실행하세요.

- **macOS**: DMG에서 앱을 **응용 프로그램**으로 옮긴 뒤, 앱을 Control-클릭하고 **열기**를 선택합니다.
- **Windows**: 설치 파일을 실행합니다. SmartScreen이 표시되면 출처를 확인한 뒤 **추가 정보 → 실행**을 선택합니다.

## 지원 환경

| 운영체제 | 버전 |
| --- | --- |
| macOS | macOS 13 이상, Apple Silicon 또는 Intel |
| Windows | Windows 10 22H2 이상, x64 |

## 개발

Node.js 24와 `rustup`이 필요합니다. Rust 버전은 `rust-toolchain.toml`에서 자동으로 선택됩니다. Windows에서는 Visual Studio Build Tools의 **Desktop development with C++**와 Windows SDK도 설치합니다.

```sh
npm ci
npm run tauri:dev:dev-console
```

단독 앱은 `npm run tauri:dev:token-meter` 또는 `npm run tauri:dev:runtime-atlas`로 실행합니다.

```sh
npm run check
cargo test --workspace --all-features --locked
```

서명과 릴리스 설정은 [GitHub 설정](docs/github-setup.md)을 참고하세요.
