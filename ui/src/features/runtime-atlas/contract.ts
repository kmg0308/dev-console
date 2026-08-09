import { invoke } from "@tauri-apps/api/core";

export type Language = "ko" | "en";
export type Availability = "available" | "unavailable";

export type DiscoveryAvailability = {
  state: Availability;
  reason?: string;
};

export type AtlasNotice = {
  kind: "warning" | "error";
  message: string;
};

export type Worktree = {
  path: string;
  branch?: string;
  detached: boolean;
  sha: string;
  shortSHA: string;
  dirty: boolean;
  availability: Availability;
  unavailableReason?: string;
};

export type Repository = {
  id: string;
  path: string;
  name: string;
  availability: Availability;
  unavailableReason?: string;
  worktrees: Worktree[];
};

export type ProcessIdentity = {
  pid: number;
  startIdentity: string;
};

export type ListeningPort = {
  address: string;
  port: number;
};

export type RuntimeProcess = {
  identity: ProcessIdentity;
  name: string;
  cwd: string | null;
  ports: ListeningPort[];
};

type LinkedProcessRelation = {
  kind: "managedSession" | "observedCwd" | "userLinked";
  processIdentity: ProcessIdentity;
  worktreePath: string;
  evidence: string;
  sessionId?: string;
};

type UnlinkedProcessRelation = {
  kind: "unlinked";
  processIdentity: ProcessIdentity;
  evidence: string;
  unlinkedReason: string;
};

export type ProcessRelation = LinkedProcessRelation | UnlinkedProcessRelation;

export type PublishedPort = {
  hostIP: string;
  hostPort: number;
  containerPort: number;
  transport: string;
};

export type RuntimeContainer = {
  id: string;
  name: string;
  image: string;
  mountSources: string[];
  ports: PublishedPort[];
  worktreeLinks: Array<{ worktreePath: string; mountSource: string }>;
};

export type CustomActionInput = {
  id: string;
  key: string;
  label: string;
  kind: "text" | "worktree" | "flag";
  flagArgument?: string | null;
  isEnabledByDefault: boolean;
};

export type CustomAction = {
  id: string;
  repositoryID: string;
  name: string;
  commandTemplate: string;
  restartCommandTemplate?: string | null;
  kind: "task" | "session";
  risk: "normal" | "destructive";
  workingDirectory: "selectedWorktree" | "repositoryRoot";
  effects: string[];
  inputs: CustomActionInput[];
  detectsRunningWorktreeListener: boolean;
};

export type ActionRun = {
  actionID: string;
  worktreePath: string;
  phase: "pending" | "running" | "restarting" | "stopping" | "succeeded" | "stopped" | "failed";
  output: string;
  exitCode: number | null;
  managed: boolean;
};

export type ActionConfirmationPlan = {
  confirmationToken: string;
  displayCommand: string;
  worktreePath: string;
  effects: string[];
};

export type RuntimeAtlasSnapshot = {
  schemaVersion: number;
  generatedAt: string;
  language: Language;
  processDiscovery: DiscoveryAvailability;
  dockerDiscovery: DiscoveryAvailability;
  notices: AtlasNotice[];
  repositories: Repository[];
  processes: RuntimeProcess[];
  relations: ProcessRelation[];
  containers: RuntimeContainer[];
  actions: CustomAction[];
  actionRuns: ActionRun[];
};

export const runtimeAtlasCommands = {
  status: () => invoke<RuntimeAtlasSnapshot>("runtime_atlas_status"),
  addRepository: (path: string) => invoke<void>("runtime_atlas_add_repository", { path }),
  removeRepository: (repositoryId: string) =>
    invoke<void>("runtime_atlas_remove_repository", { repositoryId }),
  setLanguage: (language: Language) =>
    invoke<void>("runtime_atlas_set_language", { language }),
  saveAction: (action: CustomAction) =>
    invoke<void>("runtime_atlas_save_action", { action }),
  deleteAction: (actionId: string) =>
    invoke<void>("runtime_atlas_delete_action", { actionId }),
  planAction: (actionId: string, worktreePath: string, values: Record<string, string | boolean>, restart: boolean) =>
    invoke<ActionConfirmationPlan>("runtime_atlas_plan_action", { actionId, worktreePath, values, restart }),
  confirmAction: (confirmationToken: string) =>
    invoke<void>("runtime_atlas_confirm_action", { confirmationToken }),
  setWorktreeOrder: (repositoryId: string, keys: string[]) =>
    invoke<void>("runtime_atlas_set_worktree_order", { repositoryId, keys }),
  stopAction: (actionId: string, worktreePath: string) =>
    invoke<void>("runtime_atlas_stop_action", { actionId, worktreePath }),
  stopProcess: (processIdentity: ProcessIdentity, worktreePath: string) =>
    invoke<void>("runtime_atlas_stop_process", { processIdentity, worktreePath }),
  linkProcess: (processIdentity: ProcessIdentity, worktreePath: string) =>
    invoke<void>("runtime_atlas_link_process", { processIdentity, worktreePath }),
  unlinkProcess: (processIdentity: ProcessIdentity) =>
    invoke<void>("runtime_atlas_unlink_process", { processIdentity }),
  advanceWorktreeNavigation: (currentPath: string | undefined, forward: boolean) =>
    invoke<string | null>("runtime_atlas_advance_worktree_navigation", { currentPath, forward }),
  commitWorktreeNavigation: () =>
    invoke<void>("runtime_atlas_commit_worktree_navigation"),
  cancelWorktreeNavigation: () =>
    invoke<void>("runtime_atlas_cancel_worktree_navigation"),
  recordWorktreeSelection: (path: string) =>
    invoke<void>("runtime_atlas_record_worktree_selection", { path }),
  openWorktreeInVsCode: (path: string) =>
    invoke<void>("runtime_atlas_open_worktree_in_vscode", { path }),
};
