# GitHub 설정

DevConsole 저장소의 `main`은 pull request와 GitHub Actions의 `verify` check(UI 표시 `Verify / verify`)를 필수로 하고 직접 push와 관리자 우회를 제한한다. REST 설정에서는 context `verify`를 GitHub Actions 앱에 연결한다. 저장소 설정에서 auto-merge를 켠다.

GitHub `Settings → Developer settings → Personal access tokens → Fine-grained tokens`에서 DevConsole 저장소 하나만 대상으로 token을 만들고 `Contents: Read and write`와 `Pull requests: Read and write`만 부여한다. 세 저장소에 그 값을 `DEV_CONSOLE_AUTOMATION_TOKEN` repository secret으로 설정하고, 만료 전에 같은 범위의 새 token으로 세 secret을 함께 교체한다. 컴포넌트 workflow는 이 token으로 DevConsole의 `repository_dispatch`만 호출한다.

DevConsole workflow는 checkout credential을 보존하지 않고 같은 token으로 bot branch push와 PR 생성·auto-merge만 수행한다. 기본 `GITHUB_TOKEN`이 만든 PR은 후속 workflow 실행이 제한될 수 있어 사용하지 않는다. `component-released` payload는 `runtime-atlas` 또는 `token-meter`, 40자 소문자 SHA, numeric version, 같은 version의 `v` tag여야 하며 tag가 실제 SHA를 가리키는지도 검증한다.
