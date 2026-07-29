# GitHub 설정

DevConsole 저장소의 `main`은 pull request와 정확한 `Verify / verify` 상태 검사를 필수로 하고 직접 push와 관리자 우회를 제한한다. 저장소 설정에서 auto-merge를 켠다.

세 저장소에 `DEV_CONSOLE_AUTOMATION_TOKEN` repository secret을 설정한다. 값은 DevConsole 저장소 하나만 대상으로 하는 fine-grained token이며 `Contents: Read and write`와 `Pull requests: Read and write`만 부여한다. 컴포넌트 workflow는 이 token으로 DevConsole의 `repository_dispatch`만 호출한다.

DevConsole workflow는 checkout credential을 보존하지 않고 같은 token으로 bot branch push와 PR 생성·auto-merge만 수행한다. 기본 `GITHUB_TOKEN`이 만든 PR은 후속 workflow 실행이 제한될 수 있어 사용하지 않는다. `component-released` payload는 `runtime-atlas` 또는 `token-meter`, 40자 소문자 SHA, numeric version, 같은 version의 `v` tag여야 하며 tag가 실제 SHA를 가리키는지도 검증한다.
