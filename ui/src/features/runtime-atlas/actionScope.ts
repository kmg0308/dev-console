export type ActionScope = "selectedWorktree" | "repositoryRoot";

type ScopedAction = {
  repositoryID: string;
  workingDirectory: ActionScope;
};

type RepositoryLike<T> = {
  path: string;
  worktrees: T[];
};

export function actionsForScope<T extends ScopedAction>(actions: T[], repositoryID: string, scope: ActionScope) {
  return actions.filter((action) => action.repositoryID === repositoryID && action.workingDirectory === scope);
}

export function repositoryExecutionWorktree<T extends { path: string }>(repository: RepositoryLike<T>) {
  const repositoryPath = comparablePath(repository.path);
  return repository.worktrees
    .filter((worktree) => {
      const worktreePath = comparablePath(worktree.path);
      return repositoryPath === worktreePath || repositoryPath.startsWith(`${worktreePath}/`);
    })
    .sort((left, right) => comparablePath(right.path).length - comparablePath(left.path).length)[0];
}

function comparablePath(path: string) {
  const normalized = path.replaceAll("\\", "/").replace(/\/+$/, "");
  return /^[A-Za-z]:\//.test(normalized) || normalized.startsWith("//") ? normalized.toLowerCase() : normalized;
}

const nodeProcess = (globalThis as { process?: { argv: string[] } }).process;
if (nodeProcess?.argv[1]?.endsWith("/actionScope.ts")) {
  const actions = [
    { repositoryID: "repo", workingDirectory: "repositoryRoot" as const },
    { repositoryID: "repo", workingDirectory: "selectedWorktree" as const },
    { repositoryID: "other", workingDirectory: "repositoryRoot" as const },
  ];
  const repository = { path: "/repo/apps/web", worktrees: [{ path: "/repo-linked" }, { path: "/repo" }] };
  const windowsRepository = { path: "C:\\Repo\\apps", worktrees: [{ path: "c:\\repo" }] };
  if (actionsForScope(actions, "repo", "repositoryRoot").length !== 1
    || repositoryExecutionWorktree(repository)?.path !== "/repo"
    || repositoryExecutionWorktree(windowsRepository)?.path !== "c:\\repo") {
    throw new Error("Runtime Atlas action scope self-check failed");
  }
}
