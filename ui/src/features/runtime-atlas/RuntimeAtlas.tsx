import {
  ArrowClockwise,
  ArrowDown,
  ArrowUp,
  Broadcast,
  CaretRight,
  CheckCircle,
  Circle,
  Code,
  Cube,
  FileText,
  FolderPlus,
  GitBranch,
  HardDrive,
  Hash,
  Info,
  Play,
  ShareNetwork,
  Stop,
  Terminal,
  Trash,
  Warning,
  XCircle,
} from "@phosphor-icons/react";
import { ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { pickDirectory } from "../../folder-dialog";
import "./RuntimeAtlas.css";
import { actionsForScope, repositoryExecutionWorktree, type ActionScope } from "./actionScope";
import { movePath } from "./reorder";
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

type RuntimeAtlasUpdate = {
  currentVersion?: string;
  availableVersion?: string;
  status?: string;
  failed: boolean;
  busy: boolean;
  onOpen: () => void;
  onInstall: () => void;
};

export function RuntimeAtlas({ update, active }: { update?: RuntimeAtlasUpdate; active: boolean }) {
  const [snapshot, setSnapshot] = useState<RuntimeAtlasSnapshot>();
  const [selectedPath, setSelectedPath] = useState<string>();
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [editingAction, setEditingAction] = useState<CustomAction>();
  const [commandRepository, setCommandRepository] = useState<Repository>();
  const [showingSettings, setShowingSettings] = useState(false);
  const [navigationPreviewPath, setNavigationPreviewPath] = useState<string>();
  const selectedPathRef = useRef<string | undefined>(undefined);
  const navigationStartPath = useRef<string | undefined>(undefined);
  const navigationQueue = useRef<Promise<void>>(Promise.resolve());
  const busyGate = useRef({ tail: Promise.resolve(), pending: 0 });
  const sidebar = useRef<HTMLElement>(null);
  const sidebarDrag = useRef<{ x: number; width: number } | undefined>(undefined);
  const [sidebarWidth, setSidebarWidth] = useState(360);

  const enqueue = useCallback(<T,>(operation: () => Promise<T>) => {
    const gate = busyGate.current;
    gate.pending += 1;
    setBusy(true);
    const result = gate.tail.then(operation, operation);
    gate.tail = result.then(() => undefined, () => undefined);
    return result.finally(() => {
      gate.pending -= 1;
      if (gate.pending === 0) setBusy(false);
    });
  }, []);

  const loadStatus = useCallback(async () => {
    const status = await runtimeAtlasCommands.status();
    setSnapshot(status);
    setSelectedPath((current) =>
      current && status.repositories.some((repository) =>
        repository.worktrees.some((worktree) => worktree.path === current))
        ? current
        : status.repositories.flatMap((repository) => repository.worktrees)[0]?.path,
    );
  }, []);

  const refresh = useCallback(() => {
    if (busyGate.current.pending > 0) return Promise.resolve(false);
    return enqueue(async () => {
      try {
        await loadStatus();
        setError(undefined);
        return true;
      } catch (reason) {
        setError(String(reason));
        return false;
      }
    });
  }, [enqueue, loadStatus]);

  useEffect(() => {
    if (!active) return;
    void refresh();
    const timer = window.setInterval(() => void refresh(), 60_000);
    return () => window.clearInterval(timer);
  }, [active, refresh]);

  useEffect(() => {
    selectedPathRef.current = selectedPath;
  }, [selectedPath]);

  useEffect(() => {
    const openSettings = () => setShowingSettings(true);
    window.addEventListener("runtime-atlas:open-settings", openSettings);
    return () => window.removeEventListener("runtime-atlas:open-settings", openSettings);
  }, []);

  useEffect(() => {
    const element = sidebar.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setSidebarWidth(Math.round(entry.contentRect.width)));
    observer.observe(element);
    return () => observer.disconnect();
  }, [snapshot]);

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
        setNavigationPreviewPath(path);
      }
    });
    const commit = () => enqueue(async () => {
      await runtimeAtlasCommands.commitWorktreeNavigation();
      navigationStartPath.current = undefined;
      setNavigationPreviewPath(undefined);
    });
    const cancel = () => enqueue(async () => {
      await runtimeAtlasCommands.cancelWorktreeNavigation();
      const path = navigationStartPath.current;
      navigationStartPath.current = undefined;
      setNavigationPreviewPath(undefined);
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

  const mutate = (operation: () => Promise<unknown>) => enqueue(async () => {
    let failure: unknown;
    try { await operation(); } catch (reason) { failure = reason; }
    try { await loadStatus(); } catch (reason) { failure ??= reason; }
    setError(failure === undefined ? undefined : String(failure));
    return failure === undefined;
  });

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

  const chooseRepository = async () => {
    try {
      const path = await pickDirectory();
      if (path) void mutate(() => runtimeAtlasCommands.addRepository(path));
    } catch (reason) {
      setError(String(reason));
    }
  };

  const selectWorktree = (path: string) => {
    navigationStartPath.current = undefined;
    selectedPathRef.current = path;
    setSelectedPath(path);
    void runtimeAtlasCommands.recordWorktreeSelection(path).catch((reason) => setError(String(reason)));
  };

  const resizeSidebar = (width: number) => {
    const boundedWidth = Math.max(200, Math.min(360, width));
    if (sidebar.current) sidebar.current.style.width = `${boundedWidth}px`;
    setSidebarWidth(boundedWidth);
  };

  return (
    <section className="atlas" aria-label="Runtime Atlas">
      <div className="atlas-layout">
        <aside id="atlas-sidebar" ref={sidebar} className="atlas-sidebar" aria-label={text("저장소와 워크트리", "Repositories and worktrees") }>
          <div className="atlas-sidebar-title">
            <div><strong>{text("저장소", "Repositories")}</strong><span>{snapshot.repositories.length}</span></div>
            <div className="atlas-sidebar-actions">
              <IconButton label={text("저장소 추가", "Add repository")} disabled={busy} onClick={() => void chooseRepository()}><FolderPlus /></IconButton>
            </div>
          </div>
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
                configure={() => setCommandRepository(repository)}
                snapshot={snapshot}
                korean={isKorean}
                busy={busy}
              />
            ))}
          </div>
        </aside>
        <div
          className="atlas-sidebar-separator"
          role="separator"
          aria-controls="atlas-sidebar"
          aria-label={text("사이드바 너비 조절", "Resize sidebar")}
          aria-orientation="vertical"
          aria-valuemin={200}
          aria-valuemax={360}
          aria-valuenow={sidebarWidth}
          tabIndex={0}
          onPointerDown={(event) => {
            sidebarDrag.current = { x: event.clientX, width: sidebarWidth };
            event.currentTarget.setPointerCapture(event.pointerId);
          }}
          onPointerMove={(event) => {
            if (!event.currentTarget.hasPointerCapture(event.pointerId) || !sidebarDrag.current) return;
            resizeSidebar(sidebarDrag.current.width + event.clientX - sidebarDrag.current.x);
          }}
          onPointerUp={(event) => {
            sidebarDrag.current = undefined;
            event.currentTarget.releasePointerCapture(event.pointerId);
          }}
          onLostPointerCapture={() => { sidebarDrag.current = undefined; }}
          onKeyDown={(event) => {
            if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
            event.preventDefault();
            resizeSidebar(sidebarWidth + (event.key === "ArrowLeft" ? -10 : 10));
          }}
        />

        <main className="atlas-detail">
          {update?.availableVersion && (
            <UpdateBanner update={update} korean={isKorean} />
          )}
          {!selected && error && <div className="atlas-notice error atlas-operation-error" role="alert"><XCircle weight="fill" /><div><strong>{text("요청을 완료하지 못했습니다.", "The request could not be completed.")}</strong><span>{error}</span></div><button type="button" onClick={() => setError(undefined)}>{text("닫기", "Dismiss")}</button></div>}
          {selected ? (
            <WorktreeDetail
              repository={selected.repository}
              worktree={selected.worktree}
              snapshot={snapshot}
              korean={isKorean}
              busy={busy}
              error={error}
              clearError={() => setError(undefined)}
              notices={snapshot.notices}
              refresh={refresh}
              mutate={mutate}
            />
          ) : snapshot.repositories.length === 0 ? (
            <div className="atlas-empty-detail">
              <ShareNetwork size={44} weight="light" />
              <strong>{text("무엇이 실행 중인지 확인하세요", "See what is running")}</strong>
              <span>{text("Git 저장소를 추가해 코드 버전과 프로세스, 포트, 컨테이너를 연결하세요.", "Add a Git repository to connect code versions with processes, ports, and containers.")}</span>
              <button type="button" className="prominent" disabled={busy} onClick={() => void chooseRepository()}><FolderPlus />{text("저장소 추가", "Add Repository")}</button>
            </div>
          ) : (
            <div className="atlas-empty-detail">
              <Warning size={44} weight="light" />
              <strong>{text("사용 가능한 워크트리가 없습니다", "No available worktree")}</strong>
              <span>{text("왼쪽에서 사용할 수 없는 워크트리와 저장소 상태를 확인하세요.", "Review the unavailable worktrees and repository status in the sidebar.")}</span>
            </div>
          )}
          <UnlinkedProcesses snapshot={snapshot} korean={isKorean} busy={busy} mutate={mutate} />
        </main>
      </div>

      {navigationPreviewPath && <WorktreeSwitcher repositories={snapshot.repositories} selectedPath={navigationPreviewPath} korean={isKorean} />}

      {showingSettings && (
        <SettingsDialog language={snapshot.language} korean={isKorean} busy={busy} close={() => setShowingSettings(false)} save={(language) => void mutate(() => runtimeAtlasCommands.setLanguage(language))} />
      )}

      {commandRepository && (
        <ActionManager repository={commandRepository} snapshot={snapshot} korean={isKorean} busy={busy} close={() => setCommandRepository(undefined)} edit={setEditingAction} mutate={mutate} />
      )}

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

function RepositoryGroup({ repository, snapshot, selectedPath, select, remove, reorder, open, configure, korean, busy }: {
  repository: Repository;
  snapshot: RuntimeAtlasSnapshot;
  selectedPath?: string;
  select: (path: string) => void;
  remove: () => void;
  reorder: (keys: string[]) => void;
  open: (path: string) => void;
  configure: () => void;
  korean: boolean;
  busy: boolean;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const sessionActions = snapshot.actions.filter((action) => action.repositoryID === repository.id && action.kind === "session");
  const [draggedPath, setDraggedPath] = useState<string>();
  const [previewOrder, setPreviewOrder] = useState<string[]>();
  const pointerDrag = useRef<{ pointerId: number; path: string; startY: number; moved: boolean; order: string[] } | undefined>(undefined);
  const suppressClick = useRef(false);
  const orderedWorktrees = previewOrder
    ? previewOrder.flatMap((path) => repository.worktrees.find((worktree) => worktree.path === path) ?? [])
    : repository.worktrees;

  useEffect(() => {
    if (!pointerDrag.current) setPreviewOrder(undefined);
  }, [repository.worktrees]);

  const finishDrag = (pointerId: number, canceled = false) => {
    const drag = pointerDrag.current;
    if (!drag || drag.pointerId !== pointerId) return;
    pointerDrag.current = undefined;
    setDraggedPath(undefined);
    if (!drag.moved || canceled) {
      setPreviewOrder(undefined);
      return;
    }
    suppressClick.current = true;
    window.setTimeout(() => { suppressClick.current = false; });
    if (drag.order.some((path, index) => path !== repository.worktrees[index]?.path)) reorder(drag.order);
    else setPreviewOrder(undefined);
  };
  const move = (index: number, offset: number) => {
    const worktrees = [...orderedWorktrees];
    [worktrees[index], worktrees[index + offset]] = [worktrees[index + offset], worktrees[index]];
    reorder(worktrees.map((worktree) => worktree.path));
  };
  return (
    <section className="atlas-repository">
      <header>
        <HardDrive className={repository.availability === "available" ? "accent-text" : "warning-text"} />
        <div>
          <strong>{repository.name}</strong>
          <code title={repository.path}>{repository.path}</code>
        </div>
        <IconButton label={text(`${repository.name} 제거`, `Remove ${repository.name}`)} onClick={remove}><Trash /></IconButton>
      </header>
      <button type="button" className="atlas-configure-actions" onClick={configure}>
        <Terminal /><span>{text("명령어 설정", "Configure Commands")}</span><small>{snapshot.actions.filter((action) => action.repositoryID === repository.id).length}</small>
      </button>
      {repository.availability === "unavailable" && (
        <p className="atlas-inline-warning" role="status">{repository.unavailableReason || text("Git 저장소를 확인할 수 없습니다.", "Git repository is unavailable.")}</p>
      )}
      {repository.worktrees.length === 0 ? (
        <p className="atlas-empty">{text("워크트리를 찾지 못했습니다.", "No worktrees found.")}</p>
      ) : orderedWorktrees.map((worktree, index) => {
        const managedRun = snapshot.actionRuns.some((run) =>
          sessionActions.some((action) => action.id === run.actionID)
          && run.worktreePath === worktree.path
          && ["pending", "running", "restarting", "stopping"].includes(run.phase));
        const linkedProcesses = snapshot.relations.flatMap((relation) => {
          if (relation.kind === "unlinked" || relation.worktreePath !== worktree.path) return [];
          const process = snapshot.processes.find((item) => sameProcess(item.identity, relation.processIdentity));
          return process ? [process] : [];
        });
        const running = managedRun || (sessionActions.some((action) => action.detectsRunningWorktreeListener) && linkedProcesses.length > 0);
        const ports = [...new Set(linkedProcesses.flatMap((process) => process.ports.map((port) => port.port)))];
        return <div
          className={`atlas-worktree-row${selectedPath === worktree.path ? " selected" : ""}${draggedPath === worktree.path ? " dragging" : ""}`}
          data-worktree-path={worktree.path}
          key={worktree.path}
        >
          <button
            type="button"
            className="atlas-worktree-button"
            aria-current={selectedPath === worktree.path ? "page" : undefined}
            onClick={() => {
              if (!suppressClick.current) select(worktree.path);
            }}
            onPointerCancel={(event) => finishDrag(event.pointerId, true)}
            onPointerDown={(event) => {
              if (busy || event.button !== 0) return;
              event.currentTarget.setPointerCapture(event.pointerId);
              pointerDrag.current = {
                pointerId: event.pointerId,
                path: worktree.path,
                startY: event.clientY,
                moved: false,
                order: repository.worktrees.map((item) => item.path),
              };
            }}
            onPointerMove={(event) => {
              const drag = pointerDrag.current;
              if (!drag || drag.pointerId !== event.pointerId) return;
              if (!drag.moved && Math.abs(event.clientY - drag.startY) < 4) return;
              event.preventDefault();
              drag.moved = true;
              setDraggedPath(drag.path);
              const targetPath = document.elementFromPoint(event.clientX, event.clientY)
                ?.closest<HTMLElement>("[data-worktree-path]")?.dataset.worktreePath;
              if (!targetPath || !movePath(drag.order, drag.path, targetPath)) return;
              setPreviewOrder([...drag.order]);
            }}
            onPointerUp={(event) => finishDrag(event.pointerId)}
          >
            <i className={`atlas-worktree-rail ${selectedPath === worktree.path ? "selected" : ""} ${worktree.availability}`} aria-hidden="true" />
            <span><strong>{leafName(worktree.path)}</strong>{worktree.dirty && <Circle className="atlas-dirty-dot" weight="fill" aria-label={text("변경 있음", "Dirty")} />}</span>
            <small>{worktree.detached ? text("분리된 HEAD", "Detached HEAD") : worktree.branch || text("브랜치 없음", "No branch")} {worktree.shortSHA || "—"}</small>
            {sessionActions.length > 0 && <span className={`atlas-server-badge ${running ? "running" : ""}`}>{running ? `ON${ports.length ? ` · ${ports.join(", ")}` : ""}` : "OFF"}</span>}
          </button>
          <div className="atlas-worktree-order">
            <IconButton label={text(`${leafName(worktree.path)}을 VS Code로 열기`, `Open ${leafName(worktree.path)} in VS Code`)} disabled={busy || worktree.availability !== "available"} onClick={() => open(worktree.path)}><Code /></IconButton>
            <IconButton label={text(`${leafName(worktree.path)} 위로 이동`, `Move ${leafName(worktree.path)} up`)} disabled={busy || index === 0} onClick={() => move(index, -1)}><ArrowUp /></IconButton>
            <IconButton label={text(`${leafName(worktree.path)} 아래로 이동`, `Move ${leafName(worktree.path)} down`)} disabled={busy || index === orderedWorktrees.length - 1} onClick={() => move(index, 1)}><ArrowDown /></IconButton>
          </div>
        </div>
      })}
    </section>
  );
}

function WorktreeDetail({ repository, worktree, snapshot, korean, busy, error, clearError, notices, refresh, mutate }: {
  repository: Repository;
  worktree: Worktree;
  snapshot: RuntimeAtlasSnapshot;
  korean: boolean;
  busy: boolean;
  error?: string;
  clearError: () => void;
  notices: RuntimeAtlasSnapshot["notices"];
  refresh: () => Promise<boolean>;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const [message, setMessage] = useState<string>();
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
  const repositoryActions = actionsForScope(snapshot.actions, repository.id, "repositoryRoot");
  const worktreeActions = actionsForScope(snapshot.actions, repository.id, "selectedWorktree");
  const repositoryWorktree = repositoryExecutionWorktree(repository);

  return (
    <div className="atlas-worktree-stack">
      {repositoryActions.length > 0 && <section className="atlas-card atlas-repository-actions" aria-labelledby="atlas-repository-actions-title">
        <header><div><h4 id="atlas-repository-actions-title">{text("저장소 공통 작업", "Repository Actions")}</h4><p>{text("워크트리와 관계없이 등록된 저장소 폴더에서 실행합니다.", "Run from the registered repository folder, independent of the selected worktree.")}</p></div><code title={repository.path}>{repository.path}</code></header>
        <div className="atlas-card-body">
          {repositoryWorktree
            ? <section className="atlas-command-grid" aria-label={text("저장소 공통 명령어", "Repository commands")}>{repositoryActions.map((action) => <ActionRow key={`${action.id}:${repositoryWorktree.path}:${JSON.stringify(action.inputs)}`} action={action} run={snapshot.actionRuns.find((item) => item.actionID === action.id && item.worktreePath === repositoryWorktree.path)} executionWorktree={repositoryWorktree} executionPath={repository.path} allWorktrees={repository.worktrees} korean={korean} busy={busy} mutate={mutate} message={setMessage} />)}</section>
            : <p className="atlas-action-error" role="status">{text("등록한 저장소 폴더를 워크트리 목록에서 찾을 수 없어 실행할 수 없습니다.", "The registered repository folder is not in the worktree list, so these actions cannot run.")}</p>}
        </div>
      </section>}

      <header className="atlas-worktree-header">
        <div className="atlas-worktree-identity">
          <span className="atlas-connection-icon"><ShareNetwork weight="bold" /></span>
          <div>
            <div className="atlas-worktree-title"><h2>{leafName(worktree.path)}</h2><IconButton label={busy ? text("새로고침 중", "Refreshing") : text("새로고침", "Refresh")} disabled={busy} onClick={() => void refresh()}><ArrowClockwise className={busy ? "spinning" : undefined} /></IconButton></div>
            <code>{worktree.path}</code>
          </div>
        </div>
        <div className="atlas-badges" aria-label={text("Git 상태", "Git status") }>
          <span className="accent"><GitBranch />{worktree.detached ? text("분리됨", "Detached") : worktree.branch || text("브랜치 없음", "No branch")}</span>
          <span className={worktree.dirty ? "warning" : "success"}>{worktree.dirty ? <Warning /> : <CheckCircle weight="fill" />}{worktree.dirty ? text("변경 있음", "Dirty") : text("깨끗함", "Clean")}</span>
          <span className="muted" title={worktree.sha}><Hash />{worktree.shortSHA || "—"}</span>
        </div>
      </header>

      {error && <div className="atlas-notice error atlas-operation-error" role="alert"><XCircle weight="fill" /><div><strong>{text("요청을 완료하지 못했습니다.", "The request could not be completed.")}</strong><span>{error}</span></div><button type="button" onClick={clearError}>{text("닫기", "Dismiss")}</button></div>}
      {message && <Notice kind="info" title="Runtime Atlas" message={message} />}
      {notices.map((notice, index) => <Notice key={`${notice.kind}-${index}`} kind={notice.kind} title={notice.kind === "error" ? text("로컬 데이터 문제", "Local data issue") : text("알림", "Notice")} message={notice.message} />)}
      {worktree.availability === "unavailable" && (
        <Notice kind="error" title={text("워크트리를 확인할 수 없습니다.", "Worktree unavailable")} message={worktree.unavailableReason || text("Git이 이 워크트리를 검사하지 못했습니다.", "Git could not inspect this worktree.")} />
      )}
      {worktreeActions.length > 0 && <section className="atlas-command-grid" aria-label={text("워크트리 명령어", "Worktree commands")}>{worktreeActions.map((action) => <ActionRow key={`${action.id}:${worktree.path}:${JSON.stringify(action.inputs)}`} action={action} run={snapshot.actionRuns.find((item) => item.actionID === action.id && item.worktreePath === worktree.path)} executionWorktree={worktree} allWorktrees={repository.worktrees} korean={korean} busy={busy} mutate={mutate} message={setMessage} />)}</section>}

      <section className="atlas-card" aria-labelledby="atlas-runtime-title">
        <header><div><h4 id="atlas-runtime-title">{text("실행 상태", "Runtime Status")}</h4><p>{text("이 작업 폴더와 연결된 프로세스, 컨테이너와 열린 포트를 보여줍니다.", "Processes, containers, and open ports linked to this working folder.")}</p></div></header>
        <div className="atlas-card-body atlas-runtime-list">
          <RuntimeRow icon={<GitBranch />} title={leafName(worktree.path)} detail={`${worktree.detached ? "detached" : worktree.branch || "—"} @ ${worktree.shortSHA || "—"}`} tone="accent" />
          {snapshot.processDiscovery.state === "unavailable" && <RuntimeRow icon={<Warning weight="fill" />} title={text("프로세스 사용 불가", "Processes unavailable")} detail={snapshot.processDiscovery.reason || text("열린 포트(LISTEN)를 읽지 못했습니다.", "Open ports could not be read (LISTEN).") } tone="warning" />}
          {snapshot.processDiscovery.state === "available" && processes.length === 0 && (
            <RuntimeRow icon={<Terminal />} title={text("포트를 열고 기다리는 프로세스 없음", "No process opening a port")} detail={text("이 작업 폴더에서 실행되고(cwd) 포트를 열어 둔(LISTEN) 프로세스가 없습니다.", "No process with an open LISTEN port is running from this working folder (cwd).") } />
          )}
          {processes.map(({ process, relation }) => (
            <ProcessRow key={`${process.identity.pid}-${process.identity.startIdentity}`} process={process} relation={relation} worktree={worktree} korean={korean} busy={busy} mutate={mutate} />
          ))}
          {snapshot.dockerDiscovery.state === "unavailable" && <RuntimeRow icon={<Warning weight="fill" />} title={text("Docker 사용 불가", "Docker unavailable")} detail={snapshot.dockerDiscovery.reason || text("Docker 정보를 읽지 못했습니다.", "Docker could not be read.")} tone="warning" />}
          {snapshot.dockerDiscovery.state === "available" && containers.length === 0 && (
            <RuntimeRow icon={<Cube />} title={text("연결된 Docker 컨테이너 없음", "No linked Docker container")} detail={text("이 폴더를 마운트했거나 저장소가 명시적으로 등록한 실행 컨테이너가 없습니다.", "No running container mounts this folder or was explicitly registered by the repository.")} />
          )}
          {containers.map((container) => (
            <RuntimeRow
              key={container.id}
              icon={<Cube weight="fill" />}
              title={container.name}
              detail={`${container.image} · ${container.worktreeLinks.filter((link) => link.worktreePath === worktree.path).map((link) => link.mountSource).join(", ")}`}
              badges={container.ports.map((port) => `${port.hostIP || "*"}:${port.hostPort} → ${port.containerPort}/${port.transport}`)}
              tone="accent"
            />
          ))}
        </div>
      </section>
    </div>
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
      icon={<Terminal weight="fill" />}
      title={process.name}
      detail={`PID ${process.identity.pid} · ${process.cwd || text("실행 위치(cwd) 확인 불가", "Run location (cwd) unavailable")} · ${relation.kind} · ${relation.evidence}`}
      badges={process.ports.map((port) => `${port.address}:${port.port}`)}
      tone="success"
      actions={<>
        {relation.kind === "userLinked" && <button type="button" disabled={busy} onClick={() => void mutate(() => runtimeAtlasCommands.unlinkProcess(process.identity))}>{text("연결 해제", "Unlink")}</button>}
        {process.cwd
          ? <button type="button" className="danger-text" disabled={busy} onClick={stop}>{text("포트 닫기", "Close ports")}</button>
          : <span title={text("검증된 cwd가 없어 안전하게 중지할 수 없습니다.", "A verified cwd is required for safe termination.")}>{text("중지 불가", "Stop unavailable")}</span>}
      </>}
    />
  );
}

function RuntimeRow({ icon, title, detail, badges = [], tone = "muted", actions }: {
  icon: ReactNode;
  title: string;
  detail: string;
  badges?: string[];
  tone?: "accent" | "success" | "warning" | "muted";
  actions?: ReactNode;
}) {
  return (
    <article className={`atlas-runtime-row ${tone}`}>
      <span className="atlas-runtime-node" aria-hidden="true">{icon}</span>
      <div className="atlas-runtime-copy"><strong>{title}</strong><small>{detail}</small></div>
      {badges.length > 0 && <div className="atlas-port-list">{badges.map((badge, index) => <code key={`${badge}-${index}`}>{badge}</code>)}</div>}
      {actions && <div className="atlas-row-actions">{actions}</div>}
    </article>
  );
}

function ActionRow({ action, run, executionWorktree, executionPath = executionWorktree.path, allWorktrees, korean, busy, mutate, message }: {
  action: CustomAction;
  run?: ActionRun;
  executionWorktree: Worktree;
  executionPath?: string;
  allWorktrees: Worktree[];
  korean: boolean;
  busy: boolean;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
  message: (value?: string) => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const [values, setValues] = useState<Record<string, string | boolean>>(() =>
    Object.fromEntries(action.inputs.map((input) => [input.key, input.kind === "flag" ? input.isEnabledByDefault : input.kind === "worktree" ? executionWorktree.path : ""])));
  const [review, setReview] = useState<{ plan: ActionConfirmationPlan; restart: boolean }>();
  const [preparing, setPreparing] = useState(false);
  const [restart, setRestart] = useState(false);
  const [showingOutput, setShowingOutput] = useState(false);
  const [planning, setPlanning] = useState(false);
  const [planError, setPlanError] = useState<string>();
  const execute = async (restart: boolean) => {
    setPlanning(true);
    setPlanError(undefined);
    try {
      setReview({ plan: await runtimeAtlasCommands.planAction(action.id, executionWorktree.path, values, restart), restart });
    } catch (reason) {
      setPlanError(String(reason));
    } finally {
      setPlanning(false);
    }
  };
  const openExecution = (nextRestart: boolean) => {
    setRestart(nextRestart);
    setReview(undefined);
    setPlanError(undefined);
    setPreparing(true);
    if (action.inputs.length === 0) void execute(nextRestart);
  };
  const active = run?.managed && ["pending", "running", "restarting", "stopping"].includes(run.phase);
  const externalRunning = Boolean(run && !run.managed);
  const status = run ? (run.managed ? run.phase : text("외부 실행 중", "External · Running")) : undefined;
  return (
    <article className="atlas-action-row">
      <button type="button" className={`atlas-command-button${externalRunning ? " external-running" : ""}`} disabled={busy || planning || executionWorktree.availability !== "available"} onClick={() => externalRunning ? message(text("이 작업 폴더에서 이미 포트를 연 프로세스가 있습니다. 새로 실행하려면 실행 상태에서 해당 포트를 먼저 닫으세요.", "A listener is already running from this working folder. Close it in Runtime Status before starting another one.")) : active ? void mutate(() => runtimeAtlasCommands.stopAction(action.id, executionWorktree.path)) : openExecution(false)}>
        {externalRunning ? <Broadcast weight="bold" /> : active ? <Stop weight="fill" /> : action.kind === "session" ? <Play weight="fill" /> : <Terminal weight="fill" />}
        <strong>{action.name}</strong>
        {status && run && <span className={run.phase}>{status}{run.exitCode !== null ? ` (${run.exitCode})` : ""}</span>}
      </button>
      {action.kind === "session" && action.restartCommandTemplate && run?.managed && run.phase === "running" && <IconButton label={text(`${action.name} 재시작`, `Restart ${action.name}`)} disabled={busy || planning} onClick={() => openExecution(true)}><ArrowClockwise /></IconButton>}
      {run?.output && <IconButton label={text(`${action.name} 출력`, `${action.name} output`)} onClick={() => setShowingOutput(true)}><FileText /></IconButton>}
      {preparing && <ActionConfirmationDialog action={action} review={review} restart={restart} executionPath={executionPath} values={values} setValues={setValues} allWorktrees={allWorktrees} korean={korean} busy={busy || planning} planError={planError} close={() => setPreparing(false)} prepare={() => void execute(restart)} confirm={() => review && void mutate(() => runtimeAtlasCommands.confirmAction(review.plan.confirmationToken)).then((confirmed) => { if (confirmed) setPreparing(false); })} />}
      {showingOutput && run?.output && <ActionOutputDialog action={action} run={run} korean={korean} close={() => setShowingOutput(false)} />}
    </article>
  );
}

function ActionConfirmationDialog({ action, review, restart, executionPath, values, setValues, allWorktrees, korean, busy, planError, close, prepare, confirm }: {
  action: CustomAction;
  review?: { plan: ActionConfirmationPlan; restart: boolean };
  restart: boolean;
  executionPath: string;
  values: Record<string, string | boolean>;
  setValues: (values: Record<string, string | boolean>) => void;
  allWorktrees: Worktree[];
  korean: boolean;
  busy: boolean;
  planError?: string;
  close: () => void;
  prepare: () => void;
  confirm: () => void;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  return (
    <dialog ref={dialog} className="atlas-dialog" aria-labelledby="atlas-action-confirmation-title" onCancel={close}>
      <form onSubmit={(event) => { event.preventDefault(); review ? confirm() : prepare(); }}>
        <header><h3 id="atlas-action-confirmation-title">{text("실행 검토", "Review action")}</h3><button type="button" disabled={busy} onClick={close}>{text("취소", "Cancel")}</button></header>
        <div className="atlas-dialog-fields atlas-action-review">
          <strong>{action.name}{action.risk === "destructive" ? ` · ${text("파괴적", "Destructive")}` : ""}</strong>
          {!review && action.inputs.map((input) => input.kind === "flag" ? <label key={input.id} className="atlas-checkbox"><input type="checkbox" checked={Boolean(values[input.key])} onChange={(event) => setValues({ ...values, [input.key]: event.target.checked })} />{input.label || input.key}</label> : input.kind === "worktree" ? <label key={input.id}>{input.label || input.key}<select value={String(values[input.key] || "")} onChange={(event) => setValues({ ...values, [input.key]: event.target.value })}>{allWorktrees.filter((item) => item.availability === "available").map((item) => <option key={item.path} value={item.path}>{leafName(item.path)}</option>)}</select></label> : <label key={input.id}>{input.label || input.key}<input autoFocus value={String(values[input.key] || "")} onChange={(event) => setValues({ ...values, [input.key]: event.target.value })} /></label>)}
          {review && <><label>{text("실행할 명령", "Command to execute")}<code>{review.plan.displayCommand}</code></label><label>{action.workingDirectory === "repositoryRoot" ? text("실행할 저장소", "Repository to run from") : text("대상 워크트리", "Target worktree")}<code>{executionPath}</code></label><div><span>{text("영향", "Effects")}</span>{review.plan.effects.length > 0 ? <ul>{review.plan.effects.map((effect, index) => <li key={`${effect}-${index}`}>{effect}</li>)}</ul> : <p>{text("명시된 영향 없음", "No declared effects")}</p>}</div></>}
          {planError && <p className="atlas-action-error" role="alert">{planError}</p>}
        </div>
        <footer><button type="submit" className="prominent" disabled={busy}>{review ? restart ? text("재시작 확인", "Confirm restart") : text("실행 확인", "Confirm run") : text("검토", "Review")}</button></footer>
      </form>
    </dialog>
  );
}

function ActionOutputDialog({ action, run, korean, close }: { action: CustomAction; run: ActionRun; korean: boolean; close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  return <dialog ref={dialog} className="atlas-dialog atlas-output-dialog" aria-labelledby="atlas-output-title" onCancel={close}><header><h3 id="atlas-output-title">{action.name}</h3><button type="button" onClick={close}>{korean ? "닫기" : "Close"}</button></header><code>{action.commandTemplate}</code><pre>{run.output}</pre></dialog>;
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
  if (unlinked.length === 0) return null;
  return (
    <details className="atlas-unlinked atlas-card" aria-labelledby="atlas-unlinked-title">
      <summary><CaretRight aria-hidden="true" /><div><h3 id="atlas-unlinked-title">{text("연결되지 않은 프로세스", "Unlinked processes")}</h3><p>{text("확인할 수 없는 cwd를 숨기거나 워크트리로 추측하지 않습니다. 연결하려면 워크트리를 직접 선택하세요.", "Unavailable cwd evidence is not hidden or guessed. Select a worktree explicitly to link a process.")}</p></div><span>{unlinked.length}</span></summary>
      <div className="atlas-card-body">
        {unlinked.map(({ process, relation }) => (
          <LinkProcessRow key={`${process.identity.pid}-${process.identity.startIdentity}`} process={process} relation={relation} repositories={snapshot.repositories} korean={korean} busy={busy} mutate={mutate} />
        ))}
      </div>
    </details>
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
          <label>{text("작업 종류", "Action kind")}<select value={draft.kind} onChange={(event) => { const kind = event.target.value as CustomAction["kind"]; setDraft({ ...draft, kind, restartCommandTemplate: kind === "task" ? null : draft.restartCommandTemplate, detectsRunningWorktreeListener: kind === "session" && draft.workingDirectory === "selectedWorktree" }); }}><option value="task">{text("일회성 작업", "One-time task")}</option><option value="session">{text("실행 세션", "Running session")}</option></select></label>
          {draft.kind === "session" && <label>{text("재시작 명령(선택)", "Restart command (optional)")}<input value={draft.restartCommandTemplate || ""} onChange={(event) => setDraft({ ...draft, restartCommandTemplate: event.target.value || null })} /></label>}
          <label>{text("작업 범위", "Action scope")}<select value={draft.workingDirectory} onChange={(event) => { const workingDirectory = event.target.value as CustomAction["workingDirectory"]; setDraft({ ...draft, workingDirectory, detectsRunningWorktreeListener: draft.kind === "session" && workingDirectory === "selectedWorktree" && draft.detectsRunningWorktreeListener }); }}><option value="selectedWorktree">{text("워크트리 작업", "Worktree action")}</option><option value="repositoryRoot">{text("저장소 공통 작업", "Repository action")}</option></select></label>
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

function ActionManager({ repository, snapshot, korean, busy, close, edit, mutate }: {
  repository: Repository;
  snapshot: RuntimeAtlasSnapshot;
  korean: boolean;
  busy: boolean;
  close: () => void;
  edit: (action: CustomAction) => void;
  mutate: (operation: () => Promise<unknown>) => Promise<boolean>;
}) {
  const text = (ko: string, en: string) => korean ? ko : en;
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  const openEditor = (action: CustomAction) => { close(); edit(action); };
  const newAction = (workingDirectory: ActionScope): CustomAction => ({ id: crypto.randomUUID(), repositoryID: repository.id, name: "", commandTemplate: "", restartCommandTemplate: null, kind: "task", risk: "normal", workingDirectory, effects: [], inputs: [], detectsRunningWorktreeListener: false });
  const scopes = [
    { value: "repositoryRoot", title: text("저장소 공통 작업", "Repository Actions"), description: text("등록된 저장소 폴더에서 한 번만 표시하고 실행합니다.", "Shown once and run from the registered repository folder.") },
    { value: "selectedWorktree", title: text("워크트리 작업", "Worktree Actions"), description: text("선택한 각 워크트리에서 표시하고 실행합니다.", "Shown and run inside each selected worktree.") },
  ] satisfies Array<{ value: ActionScope; title: string; description: string }>;
  return <dialog ref={dialog} className="atlas-dialog atlas-manager-dialog" aria-labelledby="atlas-manager-title" onCancel={close}>
    <header><h3 id="atlas-manager-title">{repository.name} {text("명령어", "Commands")}</h3><button type="button" onClick={close}>{text("닫기", "Close")}</button></header>
    <div className="atlas-dialog-fields">
      {scopes.map((scope) => <section className="atlas-manager-section" key={scope.value} aria-labelledby={`atlas-manager-${scope.value}`}>
        <header><div><h4 id={`atlas-manager-${scope.value}`}>{scope.title}</h4><p>{scope.description}</p></div><button type="button" className="atlas-add-command" onClick={() => openEditor(newAction(scope.value))}><Terminal />{text("추가", "Add")}</button></header>
        {actionsForScope(snapshot.actions, repository.id, scope.value).map((action) => {
          const running = snapshot.actionRuns.some((run) => run.actionID === action.id && ["pending", "running", "restarting", "stopping"].includes(run.phase));
          return <article className="atlas-manager-row" key={action.id}><div><strong>{action.name}</strong><code>{action.commandTemplate}</code>{action.restartCommandTemplate && <code>{text("재시작", "Restart")}: {action.restartCommandTemplate}</code>}</div><div><button type="button" disabled={busy || running} onClick={() => openEditor(action)}>{text("편집", "Edit")}</button><IconButton label={text(`${action.name} 삭제`, `Delete ${action.name}`)} disabled={busy || running} onClick={() => { if (window.confirm(text(`${action.name} 명령어를 삭제할까요?`, `Delete ${action.name}?`))) void mutate(() => runtimeAtlasCommands.deleteAction(action.id)); }}><Trash /></IconButton></div></article>;
        })}
      </section>)}
    </div>
  </dialog>;
}

function SettingsDialog({ language, korean, busy, close, save }: { language: Language; korean: boolean; busy: boolean; close: () => void; save: (language: Language) => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => dialog.current?.showModal(), []);
  return <dialog ref={dialog} className="atlas-dialog atlas-settings-dialog" aria-labelledby="atlas-settings-title" onCancel={close}><header><h3 id="atlas-settings-title">{korean ? "설정" : "Settings"}</h3><button type="button" onClick={close}>{korean ? "닫기" : "Close"}</button></header><div className="atlas-dialog-fields"><p>{korean ? "Runtime Atlas에서 사용할 언어를 선택하세요." : "Choose the language used by Runtime Atlas."}</p><label className="atlas-radio"><input type="radio" name="atlas-language" checked={language === "ko"} disabled={busy} onChange={() => save("ko")} />한국어</label><label className="atlas-radio"><input type="radio" name="atlas-language" checked={language === "en"} disabled={busy} onChange={() => save("en")} />English</label><small>{korean ? "이 설정은 다음 실행에도 유지됩니다." : "This setting is kept for future launches."}</small></div></dialog>;
}

function UpdateBanner({ update, korean }: { update: RuntimeAtlasUpdate; korean: boolean }) {
  const text = (ko: string, en: string) => korean ? ko : en;
  return <section className={`atlas-update-banner ${update.failed ? "error" : ""}`} aria-label={text("업데이트", "Update")}><div><Info weight="fill" /><strong>{update.status || text(`Runtime Atlas ${update.availableVersion} 버전을 사용할 수 있습니다.`, `Runtime Atlas ${update.availableVersion} is available.`)}</strong></div><div>{update.busy && <span>{text("업데이트 중…", "Updating…")}</span>}<button type="button" className="prominent" disabled={update.busy} onClick={update.onInstall}>{text("지금 업데이트", "Update Now")}</button><button type="button" disabled={update.busy} onClick={update.onOpen}>{text("세부 정보", "Details")}</button></div></section>;
}

function WorktreeSwitcher({ repositories, selectedPath, korean }: { repositories: Repository[]; selectedPath: string; korean: boolean }) {
  return <div className="atlas-switcher" role="status" aria-label={korean ? "최근 본 워크트리" : "Recently viewed worktrees"}><strong>{korean ? "최근 본 워크트리" : "Recently viewed worktrees"}</strong><div>{repositories.flatMap((repository) => repository.worktrees).map((worktree) => <article className={worktree.path === selectedPath ? "selected" : undefined} key={worktree.path}><GitBranch /><div><strong>{leafName(worktree.path)}</strong><small>{worktree.detached ? korean ? "브랜치에 연결되지 않음" : "Detached HEAD" : worktree.branch}</small></div></article>)}</div></div>;
}

function IconButton({ label, disabled, onClick, children }: { label: string; disabled?: boolean; onClick: () => void; children: ReactNode }) {
  return <button type="button" className="atlas-icon-button" aria-label={label} title={label} disabled={disabled} onClick={onClick}>{children}</button>;
}

function Notice({ kind, title, message }: { kind: "info" | "warning" | "error"; title: string; message: string }) {
  return <div className={`atlas-notice ${kind}`} role={kind === "error" ? "alert" : "status"}>{kind === "error" ? <XCircle weight="fill" /> : kind === "info" ? <Info weight="fill" /> : <Warning weight="fill" />}<div><strong>{title}</strong><span>{message}</span></div></div>;
}

function findWorktree(repositories: Repository[], path?: string) {
  for (const repository of repositories) {
    const worktree = repository.worktrees.find((item) => item.path === path);
    if (worktree) return { repository, worktree };
  }
}
