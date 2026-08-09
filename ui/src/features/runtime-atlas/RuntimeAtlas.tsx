import { FormEvent, ReactNode, useCallback, useEffect, useRef, useState } from "react";
import "./RuntimeAtlas.css";
import {
  ActionConfirmationPlan,
  ActionRun,
  CustomAction,
  Language,
  ProcessIdentity,
  ProcessRelation,
  Repository,
  RuntimeAtlasSnapshot,
  RuntimeProcess,
  Worktree,
  runtimeAtlasCommands,
} from "./contract";

const sameProcess = (left: ProcessIdentity, right: ProcessIdentity) =>
  left.pid === right.pid && left.startIdentity === right.startIdentity;

const leafName = (path: string) => path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;

export function RuntimeAtlas() {
  const [snapshot, setSnapshot] = useState<RuntimeAtlasSnapshot>();
  const [selectedPath, setSelectedPath] = useState<string>();
  const [repositoryPath, setRepositoryPath] = useState("");
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [editingAction, setEditingAction] = useState<CustomAction>();
  const selectedPathRef = useRef<string | undefined>(undefined);
  const navigationStartPath = useRef<string | undefined>(undefined);
  const navigationQueue = useRef<Promise<void>>(Promise.resolve());

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      const status = await runtimeAtlasCommands.status();
      setSnapshot(status);
      setSelectedPath((current) =>
        current && status.repositories.some((repository) =>
          repository.worktrees.some((worktree) => worktree.path === current))
          ? current
          : status.repositories.flatMap((repository) => repository.worktrees)[0]?.path,
      );
      setError(undefined);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 60_000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    selectedPathRef.current = selectedPath;
  }, [selectedPath]);

  useEffect(() => {
    let active = true;
    const enqueue = (operation: () => Promise<void>) => {
      navigationQueue.current = navigationQueue.current.then(operation).catch((reason) => {
        if (active) setError(String(reason));
      });
    };
    const advance = (event: Event) => enqueue(async () => {
      const { forward } = (event as CustomEvent<{ forward: boolean }>).detail;
      navigationStartPath.current ??= selectedPathRef.current;
      const path = await runtimeAtlasCommands.advanceWorktreeNavigation(selectedPathRef.current, forward);
      if (active && path) {
        selectedPathRef.current = path;
        setSelectedPath(path);
      }
    });
    const commit = () => enqueue(async () => {
      await runtimeAtlasCommands.commitWorktreeNavigation();
      navigationStartPath.current = undefined;
    });
    const cancel = () => enqueue(async () => {
      await runtimeAtlasCommands.cancelWorktreeNavigation();
      const path = navigationStartPath.current;
      navigationStartPath.current = undefined;
      if (active && path) {
        selectedPathRef.current = path;
        setSelectedPath(path);
      }
    });
    window.addEventListener("runtime-atlas:advance-worktree-navigation", advance);
    window.addEventListener("runtime-atlas:commit-worktree-navigation", commit);
    window.addEventListener("runtime-atlas:cancel-worktree-navigation", cancel);
    return () => {
      active = false;
      window.removeEventListener("runtime-atlas:advance-worktree-navigation", advance);
      window.removeEventListener("runtime-atlas:commit-worktree-navigation", commit);
      window.removeEventListener("runtime-atlas:cancel-worktree-navigation", cancel);
      enqueue(() => runtimeAtlasCommands.cancelWorktreeNavigation());
    };
  }, []);

  const mutate = async (operation: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await operation();
      await refresh();
      return true;
    } catch (reason) {
      setError(String(reason));
      setBusy(false);
      return false;
    }
  };

  if (!snapshot) {
    return (
      <section className="atlas atlas-state" aria-labelledby="runtime-atlas-title">
        <h2 id="runtime-atlas-title">Runtime Atlas</h2>
        {error ? (
          <div className="atlas-notice error" role="alert">
            <strong>Runtime Atlas backend is unavailable.</strong>
            <span>{error}</span>
            <button type="button" onClick={() => void refresh()}>Retry</button>
          </div>
        ) : <p aria-live="polite">Reading verified runtime state…</p>}
      </section>
    );
  }

  const isKorean = snapshot.language === "ko";
  const text = (korean: string, english: string) => isKorean ? korean : english;
  const selected = findWorktree(snapshot.repositories, selectedPath);

  const addRepository = (event: FormEvent) => {
    event.preventDefault();
    const path = repositoryPath;
    if (!path) return;
    void mutate(() => runtimeAtlasCommands.addRepository(path));
    setRepositoryPath("");
  };

  const selectWorktree = (path: string) => {
    navigationStartPath.current = undefined;
    selectedPathRef.current = path;
    setSelectedPath(path);
    void runtimeAtlasCommands.recordWorktreeSelection(path).catch((reason) => setError(String(reason)));
  };

  return (
    <section className="atlas" aria-labelledby="runtime-atlas-title">
      <header className="atlas-header">
        <div>
          <p className="atlas-eyebrow">{text("검증된 실행 관계", "Verified runtime relationships")}</p>
          <h2 id="runtime-atlas-title">Runtime Atlas</h2>
        </div>
        <div className="atlas-header-controls">
          <label>
            <span>{text("언어", "Language")}</span>
            <select
              aria-label={text("Runtime Atlas 언어", "Runtime Atlas language")}
              value={snapshot.language}
              onChange={(event) => void mutate(() =>
                runtimeAtlasCommands.setLanguage(event.target.value as Language))}
            >
              <option value="ko">한국어</option>
              <option value="en">English</option>
            </select>
          </label>
          <button type="button" disabled={busy} onClick={() => void refresh()}>
            {busy ? text("새로고침 중…", "Refreshing…") : text("새로고침", "Refresh")}
          </button>
        </div>
      </header>

      {error && (
        <div className="atlas-notice error atlas-operation-error" role="alert">
          <strong>{text("요청을 완료하지 못했습니다.", "The request could not be completed.")}</strong>
          <span>{error}</span>
          <button type="button" onClick={() => setError(undefined)}>{text("닫기", "Dismiss")}</button>
        </div>
      )}

      <div className="atlas-layout">
        <aside className="atlas-sidebar" aria-label={text("저장소와 워크트리", "Repositories and worktrees") }>
          <div className="atlas-sidebar-title">
            <strong>{text("저장소", "Repositories")}</strong>
            <span>{snapshot.repositories.length}</span>
          </div>
          <form className="atlas-add-repository" onSubmit={addRepository}>
            <label htmlFor="atlas-repository-path">{text("Git 저장소 경로", "Git repository path")}</label>
            <div>
              <input
                id="atlas-repository-path"
                value={repositoryPath}
                onChange={(event) => setRepositoryPath(event.target.value)}
                placeholder={text("절대 경로", "Absolute path")}
              />
              <button type="submit" disabled={busy || !repositoryPath}>{text("추가", "Add")}</button>
            </div>
          </form>
          <div className="atlas-repository-list">
            {snapshot.repositories.length === 0 && (
              <p className="atlas-empty">{text("추적 중인 저장소가 없습니다.", "No repositories are being tracked.")}</p>
            )}
            {snapshot.repositories.map((repository) => (
              <RepositoryGroup
                key={repository.id}
                repository={repository}
                selectedPath={selectedPath}
                select={selectWorktree}
                remove={() => {
                  if (window.confirm(text(
                    `${repository.name} 추적을 중단할까요? 저장소와 워크트리는 삭제되지 않습니다.`,
                    `Stop tracking ${repository.name}? The repository and its worktrees will not be deleted.`,
                  ))) void mutate(() => runtimeAtlasCommands.removeRepository(repository.id));
                }}
                reorder={(keys) => void mutate(() => runtimeAtlasCommands.setWorktreeOrder(repository.id, keys))}
                open={(path) => void runtimeAtlasCommands.openWorktreeInVsCode(path)
                  .catch((reason) => setError(String(reason)))}
                korean={isKorean}
                busy={busy}
              />
            ))}
          </div>
        </aside>

        <main className="atlas-detail">
          {selected ? (
            <WorktreeDetail
              repository={selected.repository}
              worktree={selected.worktree}
              snapshot={snapshot}
              korean={isKorean}
              busy={busy}
              mutate={mutate}
              editAction={setEditingAction}
            />
          ) : (
            <div className="atlas-empty-detail">
              <strong>{text("워크트리를 선택하세요.", "Select a worktree.")}</strong>
              <span>{text("저장소를 추가하면 Git worktree가 여기에 표시됩니다.", "Add a repository to show its Git worktrees here.")}</span>
            </div>
          )}
        </main>
      </div>

      <UnlinkedProcesses
        snapshot={snapshot}
        korean={isKorean}
        busy={busy}
        mutate={mutate}
      />

      {editingAction && (
        <ActionEditor
          action={editingAction}
          korean={isKorean}
          close={() => setEditingAction(undefined)}
          save={(action) => void mutate(() => runtimeAtlasCommands.saveAction(action)).then((saved) => {
            if (saved) setEditingAction(undefined);
          })}
        />
      )}
    </section>
  );
}

function RepositoryGroup({ repository, selectedPath, select, remove, reorder, open, korean, busy }: {
  repository: Repository;
  selectedPath?: string;
  select: (path: string) => void;
  remove: () => void;
  reorder: (keys: string[]) => void;
  open: (path: string) => void;
  korean: boolean;
  busy: boolean;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const move = (index: number, offset: number) => {
    const worktrees = [...repository.worktrees];
    [worktrees[index], worktrees[index + offset]] = [worktrees[index + offset], worktrees[index]];
    reorder(worktrees.map((worktree) => worktree.path));
  };
  return (
    <section className="atlas-repository">
      <header>
        <div>
          <strong>{repository.name}</strong>
          <code title={repository.path}>{repository.path}</code>
        </div>
        <button type="button" className="danger-text" onClick={remove} aria-label={text(`${repository.name} 제거`, `Remove ${repository.name}`)}>
          {text("제거", "Remove")}
        </button>
      </header>
      {repository.availability === "unavailable" && (
        <p className="atlas-inline-warning" role="status">{repository.unavailableReason || text("Git 저장소를 확인할 수 없습니다.", "Git repository is unavailable.")}</p>
      )}
      {repository.worktrees.length === 0 ? (
        <p className="atlas-empty">{text("워크트리를 찾지 못했습니다.", "No worktrees found.")}</p>
      ) : repository.worktrees.map((worktree, index) => (
        <div className="atlas-worktree-row" key={worktree.path}>
          <button
            type="button"
            className="atlas-worktree-button"
            aria-current={selectedPath === worktree.path ? "page" : undefined}
            onClick={() => select(worktree.path)}
          >
            <span><strong>{leafName(worktree.path)}</strong>{worktree.dirty && <i aria-label={text("변경 있음", "Dirty")}>●</i>}</span>
            <small>{worktree.detached ? text("분리된 HEAD", "Detached HEAD") : worktree.branch || text("브랜치 없음", "No branch")} · {worktree.shortSHA || "—"}</small>
          </button>
          <div className="atlas-worktree-order">
            <button type="button" disabled={busy || worktree.availability !== "available"} aria-label={text(`${leafName(worktree.path)}을 VS Code로 열기`, `Open ${leafName(worktree.path)} in VS Code`)} onClick={() => open(worktree.path)}>VS Code</button>
            <button type="button" disabled={busy || index === 0} aria-label={text(`${leafName(worktree.path)} 위로 이동`, `Move ${leafName(worktree.path)} up`)} onClick={() => move(index, -1)}>↑</button>
            <button type="button" disabled={busy || index === repository.worktrees.length - 1} aria-label={text(`${leafName(worktree.path)} 아래로 이동`, `Move ${leafName(worktree.path)} down`)} onClick={() => move(index, 1)}>↓</button>
          </div>
        </div>
      ))}
    </section>
  );
}

function WorktreeDetail({ repository, worktree, snapshot, korean, busy, mutate, editAction }: {
  repository: Repository;
  worktree: Worktree;
  snapshot: RuntimeAtlasSnapshot;
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
  editAction: (action: CustomAction) => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const relations = snapshot.relations.filter(
    (relation): relation is Exclude<ProcessRelation, { kind: "unlinked" }> =>
      relation.kind !== "unlinked" && relation.worktreePath === worktree.path,
  );
  const processes = relations.flatMap((relation) => {
    const process = snapshot.processes.find((item) => sameProcess(item.identity, relation.processIdentity));
    return process ? [{ process, relation }] : [];
  });
  const containers = snapshot.containers.filter((container) =>
    container.worktreeLinks.some((link) => link.worktreePath === worktree.path));
  const actions = snapshot.actions.filter((action) => action.repositoryID === repository.id);

  const newAction = (): CustomAction => ({
    id: crypto.randomUUID(), repositoryID: repository.id, name: "", commandTemplate: "",
    restartCommandTemplate: null, kind: "task", risk: "normal", workingDirectory: "selectedWorktree",
    effects: [], inputs: [], detectsRunningWorktreeListener: false,
  });

  return (
    <>
      <header className="atlas-worktree-header">
        <div>
          <h3>{leafName(worktree.path)}</h3>
          <code>{worktree.path}</code>
        </div>
        <div className="atlas-badges" aria-label={text("Git 상태", "Git status") }>
          <span>{worktree.detached ? text("분리됨", "Detached") : worktree.branch || text("브랜치 없음", "No branch")}</span>
          <span className={worktree.dirty ? "warning" : "success"}>{worktree.dirty ? text("변경 있음", "Dirty") : text("깨끗함", "Clean")}</span>
          <span title={worktree.sha}>{worktree.shortSHA || "—"}</span>
        </div>
      </header>

      {worktree.availability === "unavailable" && (
        <Notice kind="error" title={text("워크트리를 확인할 수 없습니다.", "Worktree unavailable")} message={worktree.unavailableReason || text("Git이 이 워크트리를 검사하지 못했습니다.", "Git could not inspect this worktree.")} />
      )}
      {snapshot.notices.map((notice, index) => (
        <Notice key={`${notice.kind}-${index}`} kind={notice.kind} title={notice.kind === "error" ? text("로컬 데이터 문제", "Local data issue") : text("알림", "Notice")} message={notice.message} />
      ))}
      {snapshot.processDiscovery.state === "unavailable" && (
        <Notice kind="warning" title={text("프로세스 탐색 일부 실패", "Process discovery unavailable")} message={snapshot.processDiscovery.reason || text("수신 포트 프로세스를 읽지 못했습니다.", "Listening processes could not be read.")} />
      )}
      {snapshot.dockerDiscovery.state === "unavailable" && (
        <Notice kind="warning" title={text("Docker 탐색 일부 실패", "Docker discovery unavailable")} message={snapshot.dockerDiscovery.reason || text("Docker 상태를 읽지 못했습니다.", "Docker state could not be read.")} />
      )}

      <section className="atlas-card" aria-labelledby="atlas-actions-title">
        <header>
          <div><h4 id="atlas-actions-title">{text("사용자 작업", "Custom actions")}</h4><p>{text("선택한 워크트리에서 명시적으로 실행합니다.", "Run explicitly in the selected worktree.")}</p></div>
          <button type="button" onClick={() => editAction(newAction())}>{text("작업 추가", "Add action")}</button>
        </header>
        <div className="atlas-card-body">
          {actions.length === 0 ? <p className="atlas-empty">{text("설정된 작업이 없습니다.", "No actions configured.")}</p> : actions.map((action) => (
            <ActionRow
              key={`${action.id}:${worktree.path}:${JSON.stringify(action.inputs)}`}
              action={action}
              run={snapshot.actionRuns.find((item) => item.actionID === action.id && item.worktreePath === worktree.path)}
              worktree={worktree}
              allWorktrees={repository.worktrees}
              korean={korean}
              busy={busy}
              mutate={mutate}
              edit={() => editAction(action)}
            />
          ))}
        </div>
      </section>

      <section className="atlas-card" aria-labelledby="atlas-runtime-title">
        <header><div><h4 id="atlas-runtime-title">{text("실행 상태", "Runtime status")}</h4><p>{text("검증된 워크트리·프로세스·포트·컨테이너 관계입니다.", "Verified worktree, process, port, and container relationships.")}</p></div></header>
        <div className="atlas-card-body atlas-runtime-list">
          <RuntimeRow title={leafName(worktree.path)} detail={`${worktree.detached ? "detached" : worktree.branch || "—"} @ ${worktree.shortSHA || "—"}`} tone="accent" />
          {snapshot.processDiscovery.state === "available" && processes.length === 0 && (
            <RuntimeRow title={text("연결된 수신 프로세스 없음", "No linked listening process")} detail={text("이 워크트리와 검증된 관계가 없습니다.", "No verified relationship exists for this worktree.")} />
          )}
          {processes.map(({ process, relation }) => (
            <ProcessRow key={`${process.identity.pid}-${process.identity.startIdentity}`} process={process} relation={relation} worktree={worktree} korean={korean} busy={busy} mutate={mutate} />
          ))}
          {snapshot.dockerDiscovery.state === "available" && containers.length === 0 && (
            <RuntimeRow title={text("연결된 컨테이너 없음", "No linked container")} detail={text("이 워크트리를 마운트한 실행 중 컨테이너가 없습니다.", "No running container has a verified mount for this worktree.")} />
          )}
          {containers.map((container) => (
            <RuntimeRow
              key={container.id}
              title={container.name}
              detail={`${container.image} · ${container.worktreeLinks.filter((link) => link.worktreePath === worktree.path).map((link) => link.mountSource).join(", ")}`}
              badges={container.ports.map((port) => `${port.hostIP || "*"}:${port.hostPort} → ${port.containerPort}/${port.transport}`)}
              tone="accent"
            />
          ))}
        </div>
      </section>
    </>
  );
}

function ProcessRow({ process, relation, worktree, korean, busy, mutate }: {
  process: RuntimeProcess;
  relation: Exclude<ProcessRelation, { kind: "unlinked" }>;
  worktree: Worktree;
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const stop = () => {
    const ports = process.ports.map((port) => `${port.address}:${port.port}`).join(", ");
    const warning = text(
      `${process.name} (PID ${process.identity.pid}, ${ports}) 프로세스를 중지할까요? 백엔드는 종료 직전에 PID와 프로세스 시작 identity를 다시 검증합니다. identity가 다르면 중지하지 않습니다.`,
      `Stop ${process.name} (PID ${process.identity.pid}, ${ports})? The backend re-validates the PID and process start identity immediately before termination. It will not stop a different process.`,
    );
    if (window.confirm(warning)) void mutate(() => runtimeAtlasCommands.stopProcess(process.identity, worktree.path));
  };
  return (
    <RuntimeRow
      title={process.name}
      detail={`PID ${process.identity.pid} · ${process.cwd || text("실행 위치(cwd) 확인 불가", "Run location (cwd) unavailable")} · ${relation.kind} · ${relation.evidence}`}
      badges={process.ports.map((port) => `${port.address}:${port.port}`)}
      tone="success"
      actions={<>
        {relation.kind === "userLinked" && <button type="button" disabled={busy} onClick={() => void mutate(() => runtimeAtlasCommands.unlinkProcess(process.identity))}>{text("연결 해제", "Unlink")}</button>}
        {process.cwd
          ? <button type="button" className="danger-text" disabled={busy} onClick={stop}>{text("프로세스 중지", "Stop process")}</button>
          : <span title={text("검증된 cwd가 없어 안전하게 중지할 수 없습니다.", "A verified cwd is required for safe termination.")}>{text("중지 불가", "Stop unavailable")}</span>}
      </>}
    />
  );
}

function RuntimeRow({ title, detail, badges = [], tone = "muted", actions }: {
  title: string;
  detail: string;
  badges?: string[];
  tone?: "accent" | "success" | "muted";
  actions?: ReactNode;
}) {
  return (
    <article className={`atlas-runtime-row ${tone}`}>
      <span className="atlas-runtime-node" aria-hidden="true" />
      <div className="atlas-runtime-copy"><strong>{title}</strong><small>{detail}</small></div>
      {badges.length > 0 && <div className="atlas-port-list">{badges.map((badge, index) => <code key={`${badge}-${index}`}>{badge}</code>)}</div>}
      {actions && <div className="atlas-row-actions">{actions}</div>}
    </article>
  );
}

function ActionRow({ action, run, worktree, allWorktrees, korean, busy, mutate, edit }: {
  action: CustomAction;
  run?: ActionRun;
  worktree: Worktree;
  allWorktrees: Worktree[];
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
  edit: () => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const [values, setValues] = useState<Record<string, string | boolean>>(() =>
    Object.fromEntries(action.inputs.map((input) => [input.key, input.kind === "flag" ? input.isEnabledByDefault : input.kind === "worktree" ? worktree.path : ""])));
  const [review, setReview] = useState<{ plan: ActionConfirmationPlan; restart: boolean }>();
  const [planning, setPlanning] = useState(false);
  const [planError, setPlanError] = useState<string>();
  const execute = async (restart: boolean) => {
    setPlanning(true);
    setPlanError(undefined);
    try {
      setReview({ plan: await runtimeAtlasCommands.planAction(action.id, worktree.path, values, restart), restart });
    } catch (reason) {
      setPlanError(String(reason));
    } finally {
      setPlanning(false);
    }
  };
  return (
    <article className="atlas-action-row">
      <div className="atlas-action-description">
        <strong>{action.name}</strong>
        <code>{action.commandTemplate}</code>
        <small>{action.kind === "session" ? text("실행 세션", "Running session") : text("일회성 작업", "One-time task")}{action.risk === "destructive" ? ` · ${text("파괴적", "Destructive")}` : ""}</small>
      </div>
      {action.inputs.length > 0 && <div className="atlas-action-inputs">
        {action.inputs.map((input) => input.kind === "flag" ? (
          <label key={input.id} className="atlas-checkbox"><input type="checkbox" checked={Boolean(values[input.key])} onChange={(event) => setValues({ ...values, [input.key]: event.target.checked })} />{input.label || input.key}</label>
        ) : input.kind === "worktree" ? (
          <label key={input.id}>{input.label || input.key}<select value={String(values[input.key] || "")} onChange={(event) => setValues({ ...values, [input.key]: event.target.value })}><option value="">{text("선택", "Select")}</option>{allWorktrees.filter((item) => item.availability === "available").map((item) => <option key={item.path} value={item.path}>{leafName(item.path)}</option>)}</select></label>
        ) : (
          <label key={input.id}>{input.label || input.key}<input value={String(values[input.key] || "")} onChange={(event) => setValues({ ...values, [input.key]: event.target.value })} /></label>
        ))}
      </div>}
      {run && <p className={`atlas-action-status ${run.phase}`} aria-live="polite">{run.phase}{run.exitCode !== null ? ` (${run.exitCode})` : ""}</p>}
      {planError && <p className="atlas-action-error" role="alert">{planError}</p>}
      {run?.output && <details><summary>{text("출력", "Output")}</summary><pre>{run.output}</pre></details>}
      <div className="atlas-row-actions">
        <button type="button" onClick={edit} disabled={busy || planning || ["pending", "running"].includes(run?.phase || "")}>{text("편집", "Edit")}</button>
        <button type="button" onClick={() => void execute(false)} disabled={busy || planning || worktree.availability !== "available" || ["pending", "running"].includes(run?.phase || "")}>{planning ? text("검토 준비 중…", "Preparing review…") : action.kind === "session" ? text("시작", "Start") : text("실행", "Run")}</button>
        {action.kind === "session" && action.restartCommandTemplate && run?.phase === "running" && <button type="button" onClick={() => void execute(true)} disabled={busy || planning || worktree.availability !== "available"}>{text("재시작", "Restart")}</button>}
        {action.kind === "session" && run?.managed && ["pending", "running", "restarting", "stopping"].includes(run.phase) && <button type="button" className="danger-text" disabled={busy} onClick={() => void mutate(() => runtimeAtlasCommands.stopAction(action.id, worktree.path))}>{text("중지", "Stop")}</button>}
        <button type="button" className="danger-text" disabled={busy || ["pending", "running"].includes(run?.phase || "")} onClick={() => {
          if (window.confirm(text(`${action.name} 작업을 삭제할까요?`, `Delete ${action.name}?`))) void mutate(() => runtimeAtlasCommands.deleteAction(action.id));
        }}>{text("삭제", "Delete")}</button>
      </div>
      {review && <ActionConfirmationDialog action={action} review={review} korean={korean} busy={busy} close={() => setReview(undefined)} confirm={() => void mutate(() => runtimeAtlasCommands.confirmAction(review.plan.confirmationToken)).then((confirmed) => {
        if (confirmed) setReview(undefined);
      })} />}
    </article>
  );
}

function ActionConfirmationDialog({ action, review, korean, busy, close, confirm }: {
  action: CustomAction;
  review: { plan: ActionConfirmationPlan; restart: boolean };
  korean: boolean;
  busy: boolean;
  close: () => void;
  confirm: () => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  return (
    <dialog ref={dialog} className="atlas-dialog" aria-labelledby="atlas-action-confirmation-title" onCancel={close}>
      <form onSubmit={(event) => { event.preventDefault(); confirm(); }}>
        <header><h3 id="atlas-action-confirmation-title">{text("실행 검토", "Review action")}</h3><button type="button" disabled={busy} onClick={close}>{text("취소", "Cancel")}</button></header>
        <div className="atlas-dialog-fields atlas-action-review">
          <strong>{action.name}{action.risk === "destructive" ? ` · ${text("파괴적", "Destructive")}` : ""}</strong>
          <label>{text("실행할 명령", "Command to execute")}<code>{review.plan.displayCommand}</code></label>
          <label>{text("대상 워크트리", "Target worktree")}<code>{review.plan.worktreePath}</code></label>
          <div><span>{text("영향", "Effects")}</span>{review.plan.effects.length > 0 ? <ul>{review.plan.effects.map((effect, index) => <li key={`${effect}-${index}`}>{effect}</li>)}</ul> : <p>{text("명시된 영향 없음", "No declared effects")}</p>}</div>
        </div>
        <footer><button type="submit" disabled={busy}>{review.restart ? text("재시작 확인", "Confirm restart") : text("실행 확인", "Confirm run")}</button></footer>
      </form>
    </dialog>
  );
}

function UnlinkedProcesses({ snapshot, korean, busy, mutate }: {
  snapshot: RuntimeAtlasSnapshot;
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const unlinked = snapshot.relations.flatMap((relation) => {
    if (relation.kind !== "unlinked") return [];
    const process = snapshot.processes.find((item) => sameProcess(item.identity, relation.processIdentity));
    return process ? [{ process, relation }] : [];
  });
  return (
    <section className="atlas-unlinked atlas-card" aria-labelledby="atlas-unlinked-title">
      <header><div><h3 id="atlas-unlinked-title">{text("연결되지 않은 프로세스", "Unlinked processes")}</h3><p>{text("확인할 수 없는 cwd를 숨기거나 워크트리로 추측하지 않습니다. 연결하려면 워크트리를 직접 선택하세요.", "Unavailable cwd evidence is not hidden or guessed. Select a worktree explicitly to link a process.")}</p></div></header>
      <div className="atlas-card-body">
        {unlinked.length === 0 ? <p className="atlas-empty">{text("연결되지 않은 수신 프로세스가 없습니다.", "No unlinked listening processes.")}</p> : unlinked.map(({ process, relation }) => (
          <LinkProcessRow key={`${process.identity.pid}-${process.identity.startIdentity}`} process={process} relation={relation} repositories={snapshot.repositories} korean={korean} busy={busy} mutate={mutate} />
        ))}
      </div>
    </section>
  );
}

function LinkProcessRow({ process, relation, repositories, korean, busy, mutate }: {
  process: RuntimeProcess;
  relation: Extract<ProcessRelation, { kind: "unlinked" }>;
  repositories: Repository[];
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const [worktreePath, setWorktreePath] = useState("");
  return (
    <article className="atlas-link-row">
      <div><strong>{process.name} · PID {process.identity.pid}</strong><code>{process.identity.startIdentity}</code><small>{process.cwd || text("실행 위치(cwd) 확인 불가", "Run location (cwd) unavailable")} · {relation.unlinkedReason} · {relation.evidence}</small></div>
      <label>{text("연결할 워크트리", "Worktree to link")}
        <select value={worktreePath} onChange={(event) => setWorktreePath(event.target.value)}>
          <option value="">{text("직접 선택", "Choose explicitly")}</option>
          {repositories.flatMap((repository) => repository.worktrees).filter((worktree) => worktree.availability === "available").map((worktree) => <option key={worktree.path} value={worktree.path}>{leafName(worktree.path)} — {worktree.path}</option>)}
        </select>
      </label>
      <button type="button" disabled={busy || !worktreePath} onClick={() => void mutate(() => runtimeAtlasCommands.linkProcess(process.identity, worktreePath))}>{text("연결", "Link")}</button>
    </article>
  );
}

function ActionEditor({ action, korean, close, save }: {
  action: CustomAction;
  korean: boolean;
  close: () => void;
  save: (action: CustomAction) => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const [draft, setDraft] = useState(action);
  const [effects, setEffects] = useState(action.effects.join("\n"));
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  const updateInput = (id: string, changes: Partial<CustomAction["inputs"][number]>) => setDraft((current) => ({
    ...current,
    inputs: current.inputs.map((input) => input.id === id ? { ...input, ...changes } : input),
  }));
  return (
    <dialog ref={dialog} className="atlas-dialog" aria-labelledby="atlas-action-editor-title" onCancel={close}>
      <form onSubmit={(event) => { event.preventDefault(); save({ ...draft, effects: effects.split("\n").map((line) => line.trim()).filter(Boolean) }); }}>
        <header><h3 id="atlas-action-editor-title">{action.name ? text("작업 편집", "Edit action") : text("작업 추가", "Add action")}</h3><button type="button" onClick={close}>{text("취소", "Cancel")}</button></header>
        <div className="atlas-dialog-fields">
          <label>{text("이름", "Name")}<input autoFocus required value={draft.name} onChange={(event) => setDraft({ ...draft, name: event.target.value })} /></label>
          <label>{text("명령 템플릿", "Command template")}<input required placeholder="npm run dev" value={draft.commandTemplate} onChange={(event) => setDraft({ ...draft, commandTemplate: event.target.value })} /></label>
          <label>{text("작업 종류", "Action kind")}<select value={draft.kind} onChange={(event) => { const kind = event.target.value as CustomAction["kind"]; setDraft({ ...draft, kind, restartCommandTemplate: kind === "task" ? null : draft.restartCommandTemplate, detectsRunningWorktreeListener: kind === "session" && draft.workingDirectory === "selectedWorktree" && draft.detectsRunningWorktreeListener }); }}><option value="task">{text("일회성 작업", "One-time task")}</option><option value="session">{text("실행 세션", "Running session")}</option></select></label>
          {draft.kind === "session" && <label>{text("재시작 명령(선택)", "Restart command (optional)")}<input value={draft.restartCommandTemplate || ""} onChange={(event) => setDraft({ ...draft, restartCommandTemplate: event.target.value || null })} /></label>}
          <label>{text("실행 위치", "Run from")}<select value={draft.workingDirectory} onChange={(event) => { const workingDirectory = event.target.value as CustomAction["workingDirectory"]; setDraft({ ...draft, workingDirectory, detectsRunningWorktreeListener: draft.kind === "session" && workingDirectory === "selectedWorktree" && draft.detectsRunningWorktreeListener }); }}><option value="selectedWorktree">{text("선택한 워크트리", "Selected worktree")}</option><option value="repositoryRoot">{text("저장소 루트", "Repository root")}</option></select></label>
          <label className="atlas-checkbox"><input type="checkbox" checked={draft.risk === "destructive"} onChange={(event) => setDraft({ ...draft, risk: event.target.checked ? "destructive" : "normal" })} />{text("파괴적 작업", "Destructive action")}</label>
          {draft.kind === "session" && draft.workingDirectory === "selectedWorktree" && <label className="atlas-checkbox"><input type="checkbox" checked={draft.detectsRunningWorktreeListener} onChange={(event) => setDraft({ ...draft, detectsRunningWorktreeListener: event.target.checked })} />{text("열린 포트로 실행 중 상태 감지", "Detect running state from open ports")}</label>}
          <label>{text("영향(한 줄에 하나)", "Effects (one per line)")}<textarea rows={3} value={effects} onChange={(event) => setEffects(event.target.value)} /></label>
          <fieldset className="atlas-input-editor">
            <legend>{text("입력", "Inputs")}</legend>
            {draft.inputs.map((input) => <div className="atlas-input-definition" key={input.id}>
              <label>{text("종류", "Kind")}<select value={input.kind} onChange={(event) => { const kind = event.target.value as typeof input.kind; updateInput(input.id, { kind, flagArgument: kind === "flag" ? input.flagArgument || "" : null, isEnabledByDefault: kind === "flag" && input.isEnabledByDefault }); }}><option value="text">{text("텍스트", "Text")}</option><option value="worktree">{text("워크트리", "Worktree")}</option><option value="flag">{text("체크박스 인자", "Checkbox argument")}</option></select></label>
              <label>{text("키", "Key")}<input required maxLength={32} pattern="[A-Za-z][A-Za-z0-9_]*" value={input.key} onChange={(event) => updateInput(input.id, { key: event.target.value })} /></label>
              <label>{text("레이블", "Label")}<input required maxLength={60} value={input.label} onChange={(event) => updateInput(input.id, { label: event.target.value })} /></label>
              {input.kind === "flag" && <>
                <label>{text("명령 인자", "Command argument")}<input required value={input.flagArgument || ""} onChange={(event) => updateInput(input.id, { flagArgument: event.target.value })} /></label>
                <label className="atlas-checkbox"><input type="checkbox" checked={input.isEnabledByDefault} onChange={(event) => updateInput(input.id, { isEnabledByDefault: event.target.checked })} />{text("기본 선택", "Enabled by default")}</label>
              </>}
              <button type="button" className="danger-text" onClick={() => setDraft((current) => ({ ...current, inputs: current.inputs.filter((candidate) => candidate.id !== input.id) }))}>{text("입력 삭제", "Delete input")}</button>
            </div>)}
            <button type="button" onClick={() => setDraft((current) => ({ ...current, inputs: [...current.inputs, { id: crypto.randomUUID(), key: "", label: "", kind: "text", flagArgument: null, isEnabledByDefault: false }] }))}>{text("입력 추가", "Add input")}</button>
          </fieldset>
        </div>
        <footer><button type="submit">{text("저장", "Save")}</button></footer>
      </form>
    </dialog>
  );
}

function Notice({ kind, title, message }: { kind: "warning" | "error"; title: string; message: string }) {
  return <div className={`atlas-notice ${kind}`} role={kind === "error" ? "alert" : "status"}><strong>{title}</strong><span>{message}</span></div>;
}

function findWorktree(repositories: Repository[], path?: string) {
  for (const repository of repositories) {
    const worktree = repository.worktrees.find((item) => item.path === path);
    if (worktree) return { repository, worktree };
  }
}
