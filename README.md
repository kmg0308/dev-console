# DevConsole

Runtime Atlas와 TokenMeter를 한 macOS 앱의 탭으로 제공하는 통합 shell입니다. 두 프로젝트의 코드와 독립 앱 릴리스는 각각 원본 저장소에 남습니다.

[DevConsole.pkg 다운로드](https://github.com/kmg0308/dev-console/releases/latest/download/DevConsole.pkg)

`Control+Tab`과 `Control+Shift+Tab`은 두 탭을 전환합니다. Runtime Atlas 탭의 `Control+Q+Tab`과 역방향 조합은 worktree를 전환합니다. DevConsole과 독립 RuntimeAtlas.app은 동시에 Runtime Atlas 세션을 소유하지 못합니다.

두 feature는 원본 저장소의 revision을 `Package.swift`와 `Package.resolved`에 고정해 사용합니다. Runtime Atlas는 기존 Application Support 데이터를, TokenMeter는 기존 `local.tokenmeter.app` preference와 로컬 데이터를 그대로 사용하며 복제나 마이그레이션을 하지 않습니다.

## Build

`./scripts/verify.sh`는 앱, helper, ZIP, PKG, updater와 workflow 계약을 검증합니다. `VERSION=0.1.0 ./scripts/package.sh`는 `dist`에 `DevConsole.app`, 고정·버전 ZIP/PKG와 manifest를 만듭니다.

업데이트는 `kmg0308/dev-console`의 `DevConsole.zip`만 허용합니다. embedded feature는 이 서비스를 사용하지 않습니다.

컴포넌트 릴리스는 검증된 dependency-update PR을 만들고, DevConsole `main`은 전체 검증 후 앱을 릴리스합니다. 필요한 저장소 설정과 최소 권한은 `docs/github-setup.md`에 있습니다.
