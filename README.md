# DevConsole

개발 작업의 토큰 사용량과 Git 실행 환경을 확인하는 macOS·Windows 데스크톱 앱입니다.

## 어떤 앱을 받으면 되나요?

- **TokenMeter**: Codex, Claude Code, Hermes Agent의 토큰 사용량과 Codex 사용 한도를 확인합니다.
- **Runtime Atlas**: Git 저장소와 작업 폴더별 프로세스, 포트, Docker 컨테이너를 확인하고 등록한 명령을 실행합니다.
- **DevConsole**: TokenMeter와 Runtime Atlas를 한 앱에서 사용합니다.

Runtime Atlas의 실행 중지는 선택한 실행의 하위 서비스까지 종료합니다. 저장소가 CLI로 등록한 공유 로컬 PostgreSQL 컨테이너는 다른 실행·작업·컨테이너와 DB 접속이 없음을 확인한 경우에만 정상 중지합니다. 조회 실패, 이전 연결의 불확실한 소유자, 영구 데이터 저장을 확인할 수 없는 컨테이너는 유지하며 이유를 표시합니다. DB 데이터·볼륨과 Docker Desktop 앱은 보존하고, 서버와 DB 실행 상태는 별도로 표시합니다. 이 자동 정리는 명시적인 중지에 적용되며 재시작 중에는 DB를 유지합니다. 느린 DB 종료는 최대 45초 기다린 뒤 실제 컨테이너 상태를 재조회합니다. 중지 요청 후 완료 여부를 확인할 수 없으면 유지했다고 단정하지 않고 확인 불가로 표시하며 강제 종료나 자동 재시도는 하지 않습니다.

Docker CLI는 앱이 받은 `PATH`와 macOS에 등록된 Docker 앱에서 찾습니다. 없는 설치 중간 경로는 무시하며, 서로 다른 유효한 Docker 앱이 여러 개면 자동 선택하지 않습니다. 하위 명령에는 앱에 포함된 `RUNTIME_ATLAS_CLI`와 동일한 로컬 데이터 경로를 전달합니다. 저장소의 DB 실행기는 다음 연결 명령을 사용할 수 있습니다. 실행 소유자는 호출한 CLI의 실제 상위 프로세스여야 하며 PID와 시작 시각을 함께 저장합니다. 종료 시 소유권을 해제해도 상태 표시용 DB 연결 정보는 보존됩니다.

```sh
runtime-atlas link database --label my_worktree_db --worktree /absolute/worktree --container shared-local-db --owner-pid <runner-pid>
runtime-atlas unlink database --worktree /absolute/worktree --owner-pid <runner-pid>
```

자동 중지는 로컬 Docker 연결과 영구 데이터 마운트를 가진 PostgreSQL 이미지에 한정합니다. 지원하지 않는 DB는 유지합니다. 포트를 열지 않은 작업도 확인하므로 작업 폴더에 남은 셸이나 편집기 프로세스가 있으면 보수적으로 유지합니다. macOS 이외의 플랫폼은 이 확인을 지원하기 전까지 DB를 유지합니다. 컨테이너가 모두 중지된 뒤 VM 휴식은 Docker Desktop의 [Resource Saver 설정](https://docs.docker.com/desktop/use-desktop/resource-saver/)을 따릅니다.

## 다운로드

| 앱 | macOS | Windows x64 |
| --- | --- | --- |
| TokenMeter | [![TokenMeter macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/TokenMeter_universal.dmg) | [![TokenMeter Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/TokenMeter_x64-setup.exe) |
| Runtime Atlas | [![Runtime Atlas macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/RuntimeAtlas_universal.dmg) | [![Runtime Atlas Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/RuntimeAtlas_x64-setup.exe) |
| DevConsole | [![DevConsole macOS DMG](https://img.shields.io/badge/macOS-DMG-000000?logo=apple&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/DevConsole_universal.dmg) | [![DevConsole Windows x64](https://img.shields.io/badge/Windows-x64_EXE-0078D4?logo=windows11&logoColor=white)](https://github.com/kmg0308/dev-console/releases/latest/download/DevConsole_x64-setup.exe) |

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

Rust 산출물이 커지면 `npm run clean:rust`로 `target`의 모든 Cargo 산출물을 정리합니다. 배포 파일도 삭제되므로 필요하면 먼저 보관하세요.

서명과 릴리스 설정은 [GitHub 설정](docs/github-setup.md)을 참고하세요.
