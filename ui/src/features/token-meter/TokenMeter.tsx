import {
  Archive,
  ArrowsClockwise,
  CalendarBlank,
  CaretDown,
  CaretRight,
  CaretUp,
  ChartBar,
  ChartBarHorizontal,
  CheckCircle,
  Cloud,
  CloudCheck,
  CloudSlash,
  Clock,
  DownloadSimple,
  Folder,
  Gauge,
  NumberCircleZero,
  MinusCircle,
  Monitor,
  SlidersHorizontal,
  Warning,
  Wrench,
  XCircle,
} from "@phosphor-icons/react";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import { pickDirectory } from "../../folder-dialog";
import "./token-meter.css";

type TokenSource = "all" | "codex" | "claude";
type Timestamp = string | number;
type TokenCount = string;

type TokenUsage = {
  input: TokenCount;
  cachedInput: TokenCount;
  cacheCreation: TokenCount;
  cacheRead: TokenCount;
  output: TokenCount;
  reasoning: TokenCount;
  total: TokenCount;
};

type UsageBucket = {
  start: Timestamp;
  end: Timestamp;
  usage: TokenUsage;
  sourceUsage: Partial<Record<Exclude<TokenSource, "all">, TokenUsage>>;
};

type UsageRow = {
  key: string;
  usage: TokenUsage;
  eventCount: number;
  lastActive: Timestamp;
};

type DataSourceStatus = {
  source: TokenSource;
  label: string;
  path: string;
  exists: boolean;
  totalFileCount: number;
  scannedFileCount: number;
  parseErrorCount: number;
};

type SyncStatus = {
  path?: string;
  exists: boolean;
  deviceFileCount: number;
  importedEventCount: number;
  exportedEventCount: number;
  parseErrorCount: number;
  exportError?: string;
  lastSyncedAt?: Timestamp;
};

type RateLimitWindow = {
  usedPercent: number;
  remainingPercent?: number;
  resetsAt: Timestamp;
};

type CodexAccount = {
  status: string;
  message?: string;
  fetchedAt?: Timestamp;
  fiveHour?: RateLimitWindow;
  weekly?: RateLimitWindow;
  resetCredits?: { availableCount: number; expirations: Timestamp[] };
};

type DashboardSelection = {
  source: TokenSource;
  range: string;
  bucket: string;
  filters: { project?: string; model?: string; device?: string };
};

export type DashboardSnapshot = {
  generatedAt: Timestamp;
  selection: DashboardSelection;
  eventCount: number;
  sessionCount: number;
  total: TokenUsage;
  previousTotal: TokenUsage;
  changePercent: number | null;
  buckets: UsageBucket[];
  groups: { projects: UsageRow[]; models: UsageRow[]; sessions: UsageRow[] };
  filterOptions: {
    projects: string[];
    models: string[];
    devices: Array<{ id: string; name: string }>;
  };
  sourceStatuses: DataSourceStatus[];
  syncStatus: SyncStatus;
  codexAccount: CodexAccount | null;
  settings: {
    showFullTokenNumbers: boolean;
    syncFolderPath?: string;
    icloudSyncFolderPath?: string;
    localDeviceId: string;
    localDeviceName: string;
    codexHome?: string;
    claudeProjectsPath?: string;
    hermesDatabasePath?: string;
    codexExecutablePath?: string;
  };
};

export type TokenMeterUpdate = {
  currentVersion?: string;
  availableVersion?: string;
  status?: string;
  failed: boolean;
  busy: boolean;
  onOpen: () => void;
  onInstall: () => void;
};

type DashboardRequest = DashboardSelection;
type ActionState = { kind: "idle" | "working" | "success" | "error"; scope?: "sources" | "sync"; message?: string };
type CleanupPlan = { planId: string; candidateCount: number; totalBytes: string };
type ChartSegment = { label: string; value: bigint; className: string };
type SourcePaths = { codexHome: string; claudeProjectsPath: string; hermesDatabasePath: string; codexExecutablePath: string };

const SOURCES: Array<{ value: TokenSource; label: string }> = [
  { value: "all", label: "All" },
  { value: "codex", label: "Codex" },
  { value: "claude", label: "Claude Code" },
];

const RANGES = ["30m", "1h", "3h", "6h", "8h", "12h", "24h", "Today", "Yesterday", "7d", "30d", "3m", "6m", "12m", "All"];
const BUCKETS = [
  ["auto", "Auto"],
  ["1m", "1 min"],
  ["5m", "5 min"],
  ["10m", "10 min"],
  ["20m", "20 min"],
  ["30m", "30 min"],
  ["1h", "Hourly"],
  ["1d", "Daily"],
  ["1w", "Weekly"],
  ["1mo", "Monthly"],
] as const;

const exactTokens = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const integer = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const dateTime = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const shortTime = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
const timeOnly = new Intl.DateTimeFormat(undefined, { timeStyle: "short" });

function asDate(value: Timestamp) {
  return new Date(typeof value === "number" ? (value + 978_307_200) * 1_000 : value);
}

function formatDate(value?: Timestamp, short = false) {
  if (value === undefined) return "—";
  const date = asDate(value);
  return Number.isNaN(date.valueOf()) ? "Unknown time" : (short ? shortTime : dateTime).format(date);
}

function formatTime(value?: Timestamp) {
  if (value === undefined) return "—";
  const date = asDate(value);
  return Number.isNaN(date.valueOf()) ? "Unknown time" : timeOnly.format(date);
}

function formatTokens(value: TokenCount | bigint | number, exact: boolean) {
  value = typeof value === "bigint" ? value : BigInt(value);
  if (exact) return exactTokens.format(value);
  const absolute = value < 0n ? -value : value;
  const unit = absolute >= 1_000_000_000n ? [1_000_000_000, "B"] as const
    : absolute >= 1_000_000n ? [1_000_000, "M"] as const
      : absolute >= 1_000n ? [1_000, "K"] as const
        : undefined;
  return unit ? `${(Number(value) / unit[0]).toFixed(1)}${unit[1]}` : value.toString();
}

function tokenNumber(value: TokenCount | bigint) {
  return Number(typeof value === "bigint" ? value : BigInt(value));
}

function shortPath(path: string) {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) || path || "Unknown";
}

function errorMessage(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason);
}

function usageComponents(usage: TokenUsage): ChartSegment[] {
  const input = BigInt(usage.input);
  const cachedInput = BigInt(usage.cachedInput);
  const output = BigInt(usage.output);
  const reasoning = BigInt(usage.reasoning);
  return [
    { label: "Input", value: input > cachedInput ? input - cachedInput : 0n, className: "tm-input" },
    { label: "Cache", value: cachedInput + BigInt(usage.cacheCreation) + BigInt(usage.cacheRead), className: "tm-cache" },
    { label: "Output", value: output > reasoning ? output - reasoning : 0n, className: "tm-output" },
    { label: "Reasoning", value: reasoning, className: "tm-reasoning" },
  ].filter((segment) => segment.value > 0n);
}

function segmentsFor(bucket: UsageBucket, source: TokenSource): ChartSegment[] {
  if (source !== "all") return usageComponents(bucket.usage);
  return [
    { label: "Codex", value: BigInt(bucket.sourceUsage.codex?.total ?? "0"), className: "tm-codex" },
    { label: "Claude Code", value: BigInt(bucket.sourceUsage.claude?.total ?? "0"), className: "tm-claude" },
  ].filter((segment) => segment.value > 0n);
}

function chartLegend(source: TokenSource): Array<Pick<ChartSegment, "label" | "className">> {
  return source === "all"
    ? [{ label: "Codex", className: "tm-codex" }, { label: "Claude Code", className: "tm-claude" }]
    : [
        { label: "Input", className: "tm-input" },
        { label: "Cache", className: "tm-cache" },
        { label: "Output", className: "tm-output" },
        { label: "Reasoning", className: "tm-reasoning" },
      ];
}

function niceMaximum(value: number) {
  if (value <= 1) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(value));
  const normalized = value / magnitude;
  return (normalized <= 1 ? 1 : normalized <= 2 ? 2 : normalized <= 5 ? 5 : 10) * magnitude;
}

function chartLabelIndexes(length: number) {
  if (length <= 1) return [0];
  const count = Math.min(5, length);
  return Array.from(new Set(Array.from({ length: count }, (_, index) => Math.round(index * (length - 1) / (count - 1)))));
}

function chartGeometry(buckets: UsageBucket[]) {
  const count = buckets.length;
  const starts = buckets.map((item) => asDate(item.start).getTime());
  const ends = buckets.map((item) => asDate(item.end).getTime());
  const sparse = count > 1 && starts.some((start, index) => index > 0 && start > ends[index - 1] + 1_000);
  const intervalStart = Math.min(...starts);
  const intervalEnd = Math.max(...ends, intervalStart + 1);
  const position = (index: number) => sparse
    ? (starts[index] - intervalStart) / (intervalEnd - intervalStart) * 100
    : (index + .5) / Math.max(1, count) * 100;
  const width = sparse
    ? `clamp(2px, ${100 / Math.max(80, count)}%, 10px)`
    : count <= 12
      ? `clamp(12px, ${58 / Math.max(1, count)}%, 34px)`
      : count <= 35
        ? `clamp(6px, ${62 / count}%, 20px)`
        : count <= 60
          ? `clamp(4px, ${65 / count}%, 14px)`
          : count <= 180
            ? `clamp(2px, ${72 / count}%, 9px)`
            : count <= 800
              ? `clamp(1px, ${78 / count}%, 4px)`
              : `clamp(.6px, ${82 / count}%, 3px)`;
  const style = (index: number): CSSProperties => ({ left: `${position(index)}%`, width });
  return { position, style };
}

function autoBucketLabel(range: string) {
  if (range === "30m" || range === "1h") return "1 min";
  if (range === "3h") return "5 min";
  if (range === "6h" || range === "8h") return "10 min";
  if (range === "12h") return "20 min";
  if (range === "24h" || range === "Today" || range === "Yesterday") return "Hourly";
  if (range === "7d" || range === "30d") return "Daily";
  if (range === "3m" || range === "6m") return "Weekly";
  return "Monthly";
}

function countdown(value: Timestamp, now: number) {
  const seconds = Math.max(0, Math.floor((asDate(value).getTime() - now) / 1_000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h ${Math.floor(seconds % 3_600 / 60)}m`;
  return `${Math.floor(seconds / 86_400)}d ${Math.floor(seconds % 86_400 / 3_600)}h`;
}

function resetText(value: Timestamp) {
  const date = asDate(value);
  const today = new Date();
  const day = new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const currentDay = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  const tomorrow = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  tomorrow.setDate(tomorrow.getDate() + 1);
  if (day === currentDay) return `Resets today at ${formatTime(value)}`;
  if (day === tomorrow.getTime()) return `Resets tomorrow at ${formatTime(value)}`;
  return `Resets ${formatDate(value, true)}`;
}

function moveChartFocus(event: KeyboardEvent<HTMLButtonElement>, index: number) {
  const bars = event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>(".tm-chart-bar");
  if (!bars) return;
  const next = event.key === "ArrowLeft" ? index - 1 : event.key === "ArrowRight" ? index + 1 : event.key === "Home" ? 0 : event.key === "End" ? bars.length - 1 : index;
  if (next === index) return;
  event.preventDefault();
  bars[Math.max(0, Math.min(bars.length - 1, next))]?.focus();
}

function CollapsibleLabel({ children }: { children: ReactNode }) {
  return <><CaretRight className="tm-collapsed-icon" weight="bold" /><CaretDown className="tm-expanded-icon" weight="bold" /><span>{children}</span></>;
}

export function TokenMeter({ update, active }: { update?: TokenMeterUpdate; active: boolean }) {
  const [source, setSource] = useState<TokenSource>("all");
  const [sourceOpen, setSourceOpen] = useState(false);
  const [range, setRange] = useState("8h");
  const [bucket, setBucket] = useState("auto");
  const [project, setProject] = useState("");
  const [model, setModel] = useState("");
  const [device, setDevice] = useState("");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot>();
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [reload, setReload] = useState(0);
  const refreshRequested = useRef(false);
  const refreshAfterLoad = useRef(true);
  const [exact, setExact] = useState(false);
  const exactInitialized = useRef(false);
  const [activeBucket, setActiveBucket] = useState(0);
  const [visibleBucket, setVisibleBucket] = useState<number | null>(null);
  const cleanupDays = 90;
  const [cleanupPlan, setCleanupPlan] = useState<CleanupPlan>();
  const [syncPath, setSyncPath] = useState("");
  const syncPathDirty = useRef(false);
  const [sourcePaths, setSourcePaths] = useState<SourcePaths>({ codexHome: "", claudeProjectsPath: "", hermesDatabasePath: "", codexExecutablePath: "" });
  const sourcePathsDirty = useRef(false);
  const [action, setAction] = useState<ActionState>({ kind: "idle" });
  const [now, setNow] = useState(Date.now());
  const loadInFlight = useRef(false);
  const pendingLoad = useRef(false);
  const wasActive = useRef(false);
  const activeRef = useRef(active);
  activeRef.current = active;

  function refreshDashboard() {
    refreshRequested.current = true;
    setReload((value) => value + 1);
  }

  useEffect(() => {
    if (!active) {
      wasActive.current = false;
      void invoke("token_meter_cancel_dashboard_refresh").catch(() => undefined);
      return;
    }
    if (!wasActive.current) {
      wasActive.current = true;
      if (snapshot) refreshRequested.current = true;
      else refreshAfterLoad.current = true;
    }
    if (loadInFlight.current) {
      pendingLoad.current = true;
      return;
    }
    pendingLoad.current = false;
    let stale = false;
    const request: DashboardRequest = {
      source,
      range,
      bucket,
      filters: {
        ...(project ? { project } : {}),
        ...(model ? { model } : {}),
        ...(device ? { device } : {}),
      },
    };
    const refresh = refreshRequested.current;
    refreshRequested.current = false;
    loadInFlight.current = true;
    if (refresh || !snapshot) setLoading(true);
    setLoadError(undefined);
    invoke<DashboardSnapshot>("token_meter_dashboard", { request, refresh })
      .then((value) => {
        if (stale) return;
        setSnapshot(value);
        setProject(value.selection.filters.project ?? "");
        setModel(value.selection.filters.model ?? "");
        setDevice(value.selection.filters.device ?? "");
        setActiveBucket(Math.max(0, value.buckets.length - 1));
        setVisibleBucket(null);
        if (!exactInitialized.current) {
          setExact(value.settings.showFullTokenNumbers);
          exactInitialized.current = true;
        }
        if (!syncPathDirty.current) setSyncPath(value.settings.syncFolderPath ?? "");
        if (!sourcePathsDirty.current) {
          setSourcePaths({
            codexHome: value.settings.codexHome ?? "",
            claudeProjectsPath: value.settings.claudeProjectsPath ?? "",
            hermesDatabasePath: value.settings.hermesDatabasePath ?? "",
            codexExecutablePath: value.settings.codexExecutablePath ?? "",
          });
        }
        if (!refresh && refreshAfterLoad.current) {
          refreshAfterLoad.current = false;
          refreshRequested.current = true;
          pendingLoad.current = true;
        }
      })
      .catch((reason: unknown) => {
        const message = errorMessage(reason);
        if (!stale && message !== "TokenMeter refresh was cancelled.") setLoadError(message);
      })
      .finally(() => {
        loadInFlight.current = false;
        if (!stale) setLoading(false);
        if (pendingLoad.current && activeRef.current) {
          pendingLoad.current = false;
          setReload((value) => value + 1);
        }
      });
    return () => { stale = true; };
  }, [active, source, range, bucket, project, model, device, reload]);

  useEffect(() => {
    if (!active || snapshot?.codexAccount?.status !== "updating") return;
    const timer = window.setTimeout(() => setReload((value) => value + 1), 1_000);
    return () => window.clearTimeout(timer);
  }, [active, snapshot?.codexAccount?.status, snapshot?.generatedAt]);

  useEffect(() => {
    if (!active) return;
    const refreshTimer = window.setInterval(() => {
      if (document.visibilityState === "visible" && !loadInFlight.current) refreshDashboard();
    }, 5 * 60_000);
    const clockTimer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => {
      window.clearInterval(refreshTimer);
      window.clearInterval(clockTimer);
    };
  }, [active]);

  async function previewCleanup() {
    setAction({ kind: "working", scope: "sources", message: "Checking old sessions…" });
    setCleanupPlan(undefined);
    try {
      const plan = await invoke<CleanupPlan>("token_meter_cleanup_preview", { olderThanDays: cleanupDays });
      setCleanupPlan(plan);
      setAction({ kind: "success", scope: "sources", message: `${integer.format(plan.candidateCount)} sessions can be archived (${formatTokens(plan.totalBytes, false)} bytes).` });
    } catch (reason) {
      setAction({ kind: "error", scope: "sources", message: errorMessage(reason) });
    }
  }

  async function applyCleanup() {
    if (!cleanupPlan || !window.confirm(`Archive ${cleanupPlan.candidateCount} old Codex sessions?`)) return;
    setAction({ kind: "working", scope: "sources", message: "Archiving old sessions…" });
    try {
      const result = await invoke<{ archivedCount: number }>("token_meter_cleanup_apply", { planId: cleanupPlan.planId });
      setCleanupPlan(undefined);
      setAction({ kind: "success", scope: "sources", message: `Archived ${integer.format(result.archivedCount)} sessions.` });
      setReload((value) => value + 1);
    } catch (reason) {
      setAction({ kind: "error", scope: "sources", message: errorMessage(reason) });
    }
  }

  async function rebuildCache() {
    if (!window.confirm("Rebuild TokenMeter's local cache now? Source logs will not be changed.")) return;
    setAction({ kind: "working", scope: "sources", message: "Rebuilding cache…" });
    try {
      const result = await invoke<{ eventCount: number }>("token_meter_rebuild_cache");
      setAction({ kind: "success", scope: "sources", message: `Cache rebuilt with ${integer.format(result.eventCount)} events.` });
      setReload((value) => value + 1);
    } catch (reason) {
      setAction({ kind: "error", scope: "sources", message: errorMessage(reason) });
    }
  }

  async function saveSourcePaths() {
    setAction({ kind: "working", scope: "sources", message: "Updating data source paths…" });
    try {
      await invoke<DashboardSnapshot["settings"]>("token_meter_set_source_paths", {
        paths: {
          codexHome: sourcePaths.codexHome || null,
          claudeProjectsPath: sourcePaths.claudeProjectsPath || null,
          hermesDatabasePath: sourcePaths.hermesDatabasePath || null,
          codexExecutablePath: sourcePaths.codexExecutablePath || null,
        },
      });
      sourcePathsDirty.current = false;
      setAction({ kind: "success", scope: "sources", message: "Data source paths updated." });
      refreshDashboard();
    } catch (reason) {
      setAction({ kind: "error", scope: "sources", message: errorMessage(reason) });
    }
  }

  async function saveSyncFolder(path: string | null, confirmChange = true) {
    const description = path ? `Use this sync folder?\n\n${path}` : "Turn off sync and clear the configured folder?";
    if (confirmChange && !window.confirm(description)) return;
    setAction({ kind: "working", scope: "sync", message: "Updating sync folder…" });
    try {
      const settings = await invoke<DashboardSnapshot["settings"]>("token_meter_set_sync_folder", { path });
      setSyncPath(settings.syncFolderPath ?? "");
      syncPathDirty.current = false;
      setAction({ kind: "success", scope: "sync", message: path ? "Sync folder updated." : "Sync turned off." });
      refreshDashboard();
    } catch (reason) {
      setAction({ kind: "error", scope: "sync", message: errorMessage(reason) });
    }
  }

  async function chooseSyncFolder() {
    try {
      const path = await pickDirectory();
      if (path) await saveSyncFolder(path, false);
    } catch (reason) {
      setAction({ kind: "error", scope: "sync", message: errorMessage(reason) });
    }
  }

  async function saveExactNumbers(value: boolean) {
    const previousValue = exact;
    setExact(value);
    try {
      await invoke("token_meter_set_show_full_numbers", { value });
    } catch (reason) {
      setExact(previousValue);
      setAction({ kind: "error", scope: "sources", message: errorMessage(reason) });
    }
  }

  if (!snapshot && loading) {
    return <section className="tm-shell" aria-labelledby="token-meter-title" aria-busy="true"><p className="tm-state">Loading TokenMeter data…</p></section>;
  }

  if (!snapshot) {
    return (
      <section className="tm-shell" aria-labelledby="token-meter-title">
        <div className="tm-state tm-error" role="alert">
          <h2 id="token-meter-title">TokenMeter data could not be loaded</h2>
          <p>{loadError || "The dashboard command returned no data."}</p>
          <button className="tm-button" type="button" onClick={refreshDashboard}>Try Again</button>
        </div>
      </section>
    );
  }

  const previous = snapshot.previousTotal.total;
  const change = snapshot.changePercent;
  const chartMaximum = niceMaximum(Math.max(1, ...snapshot.buckets.map((item) => tokenNumber(item.usage.total))));
  const selectedBucket = visibleBucket == null ? undefined : snapshot.buckets[visibleBucket];
  const selectedSegments = selectedBucket ? segmentsFor(selectedBucket, source) : [];
  const components = usageComponents(snapshot.total);
  const componentTotal = components.reduce((sum, item) => sum + item.value, 0n);
  const noUsage = BigInt(snapshot.total.total) === 0n && snapshot.groups.sessions.length === 0;
  const selectedSource = SOURCES.find((item) => item.value === source)?.label ?? "All";
  const geometry = chartGeometry(snapshot.buckets);
  const syncPanel = (
    <div className="tm-card tm-sync">
      <SyncSummary status={snapshot.syncStatus} />
      <div className="tm-actions">
        {snapshot.settings.icloudSyncFolderPath && <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => void saveSyncFolder(snapshot.settings.icloudSyncFolderPath!, false)}><Cloud weight="bold" />Use iCloud Drive</button>}
        <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => void chooseSyncFolder()}><Folder weight="bold" />{snapshot.settings.syncFolderPath ? "Change" : "Choose Folder"}</button>
        {snapshot.settings.syncFolderPath && <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => void saveSyncFolder(null, false)}><XCircle weight="bold" />Turn Off</button>}
      </div>
    </div>
  );

  return (
    <section className="tm-shell" aria-labelledby="token-meter-title" aria-busy={loading}>
      <header className="tm-header">
        <div className="tm-header-row">
          <div className="tm-product"><span className="tm-product-icon"><ChartBar size={18} weight="bold" /></span><h2 id="token-meter-title">TokenMeter</h2></div>
          <div className="tm-header-actions">
            <button className="tm-button tm-source-button" type="button" aria-expanded={sourceOpen} aria-controls="tm-source-selector" onClick={() => setSourceOpen((value) => !value)}>{selectedSource}{sourceOpen ? <CaretUp weight="bold" /> : <CaretDown weight="bold" />}</button>
            <button className="tm-icon-button" type="button" aria-label="Refresh" title="Refresh" disabled={loading} onClick={refreshDashboard}><ArrowsClockwise className={loading ? "tm-spin" : ""} weight="bold" /></button>
            {update && <button className={`tm-button${update.availableVersion ? " tm-primary" : ""}`} type="button" aria-label={update.availableVersion ? "Update available" : "Updates"} disabled={update.busy} onClick={update.onOpen}>{update.busy ? <ArrowsClockwise className="tm-spin" weight="bold" /> : <DownloadSimple weight={update.availableVersion ? "fill" : "bold"} />}<span>{update.availableVersion ? "Update" : "Updates"}</span></button>}
          </div>
        </div>
        {sourceOpen && <div className="tm-source-tabs" id="tm-source-selector" role="group" aria-label="Token source">{SOURCES.map((option) => <button className={source === option.value ? "selected" : ""} type="button" aria-pressed={source === option.value} onClick={() => { setSource(option.value); setSourceOpen(false); }} key={option.value}>{option.label}</button>)}</div>}
      </header>

      <CodexLimits account={snapshot.codexAccount} now={now} />

      {update?.availableVersion && <section className="tm-card tm-update-banner" aria-label="Update available"><DownloadSimple size={18} weight="fill" /><p>{update.status || `Version ${update.availableVersion} is available`}</p><button className="tm-button tm-primary" type="button" disabled={update.busy} onClick={update.onInstall}>Update Now</button><button className="tm-button" type="button" disabled={update.busy} onClick={update.onOpen}>Details</button></section>}

      <section className="tm-card tm-summary" aria-labelledby="tm-summary-title">
        <div className="tm-section-heading">
          <div className="tm-summary-context"><span className="tm-compact-pill"><Clock weight="bold" />{range}</span>{snapshot.settings.syncFolderPath && <label className="tm-select-control"><Monitor weight="bold" /><span className="tm-sr-only">Device</span><select value={device} onChange={(event) => setDevice(event.target.value)}><option value="">All Devices</option>{snapshot.filterOptions.devices.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select><CaretDown className="tm-select-chevron" weight="bold" /></label>}</div>
          <div className="tm-summary-actions"><button className={`tm-icon-button tm-compact-icon${exact ? " selected" : ""}`} type="button" aria-label="Show exact token counts with separators" aria-pressed={exact} onClick={() => void saveExactNumbers(!exact)}><NumberCircleZero weight={exact ? "fill" : "bold"} /></button>{loading && <span className="tm-scanning"><ArrowsClockwise className="tm-spin" weight="bold" />Scanning</span>}</div>
        </div>
        <p className={`tm-total${exact ? " exact" : ""}`} id="tm-summary-title">{formatTokens(snapshot.total.total, exact)} <span>tokens</span></p>
        <dl className="tm-metrics">
          {source === "all" && <><div><dt><i className="tm-dot tm-codex" />Codex</dt><dd>{formatTokens(snapshot.buckets.reduce((sum, item) => sum + BigInt(item.sourceUsage.codex?.total ?? "0"), 0n), exact)}</dd></div><div><dt><i className="tm-dot tm-claude" />Claude Code</dt><dd>{formatTokens(snapshot.buckets.reduce((sum, item) => sum + BigInt(item.sourceUsage.claude?.total ?? "0"), 0n), exact)}</dd></div></>}
          <div><dt>Sessions</dt><dd>{integer.format(snapshot.sessionCount)}</dd></div>
          {snapshot.settings.syncFolderPath && <div><dt>Devices</dt><dd>{integer.format(snapshot.filterOptions.devices.length)}</dd></div>}
        </dl>
        <dl className="tm-metrics tm-comparison"><div><dt>Previous</dt><dd>{formatTokens(previous, exact)}</dd></div><div><dt>Change</dt><dd className={change != null && change > 0 ? "tm-warning" : change != null && change < 0 ? "tm-positive" : ""}>{change == null ? (BigInt(snapshot.total.total) > 0n && previous === "0" ? "New" : "0%") : `${change > 0 ? "+" : ""}${Math.round(change)}%`}</dd></div></dl>
      </section>

      {loadError && <div className="tm-notice tm-error" role="alert"><Warning weight="bold" /><div><strong>Refresh failed.</strong> {loadError}</div></div>}
      {noUsage && <div className="tm-notice" role="status"><SlidersHorizontal weight="bold" /><div><strong>No matching usage.</strong> Change the source, time range, or filters, or check the data sources below.</div></div>}

      <section className="tm-section tm-usage-section" aria-labelledby="tm-usage-title">
        <div className="tm-section-heading">
          <h3 id="tm-usage-title">Usage</h3>
          <div className="tm-controls">
            <label className="tm-select-control"><CalendarBlank weight="bold" /><span className="tm-sr-only">Range</span><select value={range} onChange={(event) => { refreshRequested.current = true; setRange(event.target.value); }}>{RANGES.map((value) => <option key={value}>{value}</option>)}</select><CaretDown className="tm-select-chevron" weight="bold" /></label>
            <label className="tm-select-control"><ChartBarHorizontal weight="bold" /><span className="tm-sr-only">Bucket</span><select value={bucket} onChange={(event) => setBucket(event.target.value)}>{BUCKETS.map(([value, label]) => <option value={value} key={value}>{value === "auto" ? `${label}: ${autoBucketLabel(range)}` : label}</option>)}</select><CaretDown className="tm-select-chevron" weight="bold" /></label>
          </div>
        </div>
        <div className="tm-chart-card">
          {snapshot.buckets.length === 0 ? <p className="tm-empty">No data</p> : <>
            <div className={`tm-chart-layout${exact ? " exact" : ""}`}>
              <div className="tm-y-axis" aria-hidden="true">{[1, .75, .5, .25, 0].map((ratio) => <span style={{ top: `${(1 - ratio) * 100}%` }} key={ratio}>{formatTokens(Math.round(chartMaximum * ratio), exact)}</span>)}</div>
              <div className="tm-chart" aria-label="Token usage over time" onPointerLeave={() => setVisibleBucket(null)}>
                {snapshot.buckets.map((item, index) => {
                  const segments = segmentsFor(item, source);
                  const label = `${formatDate(item.start, true)}, ${formatTokens(item.usage.total, exact)} tokens${segments.map((segment) => `, ${segment.label} ${formatTokens(segment.value, exact)}`).join("")}`;
                  return <button className={`tm-chart-bar${visibleBucket === index ? " active" : ""}`} style={geometry.style(index)} type="button" aria-label={label} tabIndex={activeBucket === index ? 0 : -1} key={`${String(item.start)}-${index}`} onBlur={() => setVisibleBucket(null)} onFocus={() => { setActiveBucket(index); setVisibleBucket(index); }} onPointerEnter={() => setVisibleBucket(index)} onKeyDown={(event) => moveChartFocus(event, index)}><span className="tm-bar-stack" style={{ height: `${Math.max(2, tokenNumber(item.usage.total) / chartMaximum * 100)}%` }} aria-hidden="true">{segments.map((segment) => <i className={segment.className} style={{ flexGrow: tokenNumber(segment.value) }} key={segment.label} />)}</span></button>;
                })}
              </div>
              <div className="tm-x-axis" aria-hidden="true">{chartLabelIndexes(snapshot.buckets.length).map((index) => <span style={{ left: `${geometry.position(index)}%` }} key={index}>{formatDate(snapshot.buckets[index]?.start, true)}</span>)}</div>
            </div>
            {selectedBucket && visibleBucket != null && <output className="tm-chart-tooltip" aria-live="polite" style={{ left: `clamp(106px, ${geometry.position(visibleBucket)}%, calc(100% - 106px))` }}><small>{formatDate(selectedBucket.start, true)}</small><strong>{formatTokens(selectedBucket.usage.total, exact)} <em>tokens</em></strong>{selectedSegments.map((segment) => <span key={segment.label}><i className={`tm-dot ${segment.className}`} />{segment.label}<b>{formatTokens(segment.value, exact)}</b></span>)}</output>}
            <div className="tm-chart-legend" aria-label="Chart legend">{chartLegend(source).map((item) => <span key={item.label}><i className={item.className} />{item.label}</span>)}</div>
          </>}
        </div>
      </section>

      <section className="tm-section" aria-labelledby="tm-breakdown-title">
        <h3 id="tm-breakdown-title">Breakdown</h3>
        <div className="tm-card tm-breakdown">
          {components.length ? <><div className="tm-breakdown-bar" aria-hidden="true">{components.map((item) => <i className={item.className} style={{ flexGrow: Number(item.value), flexBasis: componentTotal ? `${Number(item.value * 10000n / componentTotal) / 100}%` : 0 }} key={item.label} />)}</div><div className="tm-breakdown-legend">{components.map((item) => <span key={item.label}><i className={item.className} />{item.label}<strong>{formatTokens(item.value, exact)}</strong></span>)}</div></> : <p className="tm-empty">No token breakdown</p>}
        </div>
      </section>

      <details className="tm-details"><summary><CollapsibleLabel>Filters</CollapsibleLabel></summary><div className="tm-card tm-filter-grid"><label><span aria-hidden="true">PROJECT</span><select aria-label="Project" value={project} onChange={(event) => setProject(event.target.value)}><option value="">All Projects</option>{snapshot.filterOptions.projects.map((value) => <option value={value} key={value}>{shortPath(value)}</option>)}</select><CaretDown className="tm-select-chevron" weight="bold" /></label><label><span aria-hidden="true">MODEL</span><select aria-label="Model" value={model} onChange={(event) => setModel(event.target.value)}><option value="">All Models</option>{snapshot.filterOptions.models.map((value) => <option key={value}>{value}</option>)}</select><CaretDown className="tm-select-chevron" weight="bold" /></label></div></details>

      <details className="tm-details"><summary><CollapsibleLabel>Details</CollapsibleLabel></summary><div className="tm-tables"><UsageTable title="Projects" keyLabel="Project" rows={snapshot.groups.projects} exact={exact} formatKey={shortPath} /><UsageTable title="Models" keyLabel="Model" rows={snapshot.groups.models} exact={exact} /><UsageTable title="Sessions" keyLabel="Session" rows={snapshot.groups.sessions} exact={exact} wide /></div></details>

      {snapshot.settings.syncFolderPath ? <section className="tm-section" aria-labelledby="tm-sync-title"><h3 id="tm-sync-title">Sync Folder</h3>{syncPanel}{action.scope === "sync" && <div className={`tm-notice${action.kind === "error" ? " tm-error" : ""}`} role={action.kind === "error" ? "alert" : "status"}>{action.message}</div>}</section> : <details className="tm-details tm-sync-details"><summary><CollapsibleLabel>Sync Folder</CollapsibleLabel></summary><div className="tm-details-content">{syncPanel}{action.scope === "sync" && <div className={`tm-notice${action.kind === "error" ? " tm-error" : ""}`} role={action.kind === "error" ? "alert" : "status"}>{action.message}</div>}</div></details>}

      <section className="tm-section" aria-labelledby="tm-source-title">
        <h3 id="tm-source-title">Data Sources</h3>
        <div className="tm-card tm-status-list">{snapshot.sourceStatuses.length === 0 ? <p className="tm-empty">Source status is not available.</p> : snapshot.sourceStatuses.map((status) => <SourceStatus status={status} key={`${status.source}-${status.path}`} />)}{snapshot.settings.syncFolderPath && <div className="tm-cleanup"><button className="tm-button" type="button" disabled={action.kind === "working"} onClick={previewCleanup}><SlidersHorizontal weight="bold" />Preview Cleanup</button>{cleanupPlan && cleanupPlan.candidateCount > 0 && <button className="tm-button tm-primary" type="button" disabled={action.kind === "working"} onClick={applyCleanup}><Archive weight="bold" />Archive Old Sessions</button>}</div>}</div>
        <details className="tm-details tm-advanced"><summary><CollapsibleLabel>Advanced</CollapsibleLabel></summary><div className="tm-advanced-content">
          <form className="tm-source-settings" onSubmit={(event) => { event.preventDefault(); void saveSourcePaths(); }}><p>Blank paths use verified platform defaults when available. Set an absolute Codex executable path only to override the platform lookup.</p><label>Codex home<input type="text" value={sourcePaths.codexHome} placeholder="Absolute path to the Codex data folder" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, codexHome: event.target.value })); }} /></label><label>Claude projects<input type="text" value={sourcePaths.claudeProjectsPath} placeholder="Absolute path to the Claude projects folder" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, claudeProjectsPath: event.target.value })); }} /></label><label>Hermes database<input type="text" value={sourcePaths.hermesDatabasePath} placeholder="Absolute path to the Hermes state database" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, hermesDatabasePath: event.target.value })); }} /></label><label>Codex executable<input type="text" value={sourcePaths.codexExecutablePath} placeholder="Absolute path to the Codex executable" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, codexExecutablePath: event.target.value })); }} /></label><div className="tm-actions"><button className="tm-button tm-primary" type="submit" disabled={action.kind === "working"}>Save Paths</button></div></form>
          <form className="tm-manual-sync" onSubmit={(event) => { event.preventDefault(); void saveSyncFolder(syncPath.trim()); }}><label>Manual sync folder path<input type="text" value={syncPath} placeholder="Enter an absolute folder path" onChange={(event) => { setSyncPath(event.target.value); syncPathDirty.current = true; }} /></label><button className="tm-button" type="submit" disabled={!syncPath.trim() || action.kind === "working"}><Folder weight="bold" />Use Folder</button></form>
          <div className="tm-advanced-actions"><button className="tm-button" type="button" disabled={action.kind === "working"} onClick={rebuildCache}><Wrench weight="bold" />Rebuild Cache</button></div>
        </div></details>
        {action.scope === "sources" && <div className={`tm-notice${action.kind === "error" ? " tm-error" : ""}`} role={action.kind === "error" ? "alert" : "status"}>{action.message}</div>}
      </section>

      <footer className="tm-footer"><div>{snapshot.sourceStatuses.map((status) => <span key={status.label}>{status.label} files <b>{integer.format(status.totalFileCount)}</b></span>)}<span>Events <b>{integer.format(snapshot.eventCount)}</b></span>{snapshot.settings.syncFolderPath && <span>Devices <b>{integer.format(snapshot.syncStatus.deviceFileCount)}</b></span>}<span>Errors <b>{integer.format(snapshot.sourceStatuses.reduce((sum, status) => sum + status.parseErrorCount, 0) + snapshot.syncStatus.parseErrorCount)}</b></span></div><span>Scanned {formatDate(snapshot.generatedAt, true)}</span></footer>
    </section>
  );
}

function UsageTable({ title, keyLabel, rows, exact, formatKey = (value) => value, wide = false }: { title: string; keyLabel: string; rows: UsageRow[]; exact: boolean; formatKey?: (value: string) => string; wide?: boolean }) {
  return <section className={`tm-card tm-table-card${wide ? " wide" : ""}`} aria-labelledby={`tm-table-${title}`}><h3 id={`tm-table-${title}`}>{title}</h3><div className="tm-table-scroll"><table><thead><tr><th>{keyLabel}</th><th>Total</th><th>Input</th><th>Cache</th><th>Output</th><th>Reasoning</th><th>Count</th></tr></thead><tbody>{rows.map((row) => { const components = usageComponents(row.usage); const value = (label: string) => components.find((item) => item.label === label)?.value ?? 0n; return <tr key={row.key}><td title={row.key}>{formatKey(row.key)}</td><td>{formatTokens(row.usage.total, exact)}</td><td>{formatTokens(value("Input"), exact)}</td><td>{formatTokens(value("Cache"), exact)}</td><td>{formatTokens(value("Output"), exact)}</td><td>{formatTokens(value("Reasoning"), exact)}</td><td>{integer.format(row.eventCount)}</td></tr>; })}{rows.length === 0 && <tr><td colSpan={7}>No data</td></tr>}</tbody></table></div></section>;
}

function CodexLimits({ account, now }: { account?: CodexAccount | null; now: number }) {
  const windows: Array<[string, RateLimitWindow | undefined]> = [["5 hour", account?.fiveHour], ["7 day", account?.weekly]];
  const status = account?.status === "updating" ? "Updating…" : account?.message ? "Could not refresh" : account?.fetchedAt ? `Updated ${formatTime(account.fetchedAt)}` : account?.status || "Checking account";
  const unavailable = <div className="tm-unavailable"><strong>—</strong><small>Not provided by Codex</small></div>;
  return <section className="tm-limits-section" aria-labelledby="tm-limits-title"><div className="tm-card tm-limits"><header><Gauge size={16} weight="bold" /><h3 id="tm-limits-title">Codex limits</h3><span className={account?.message ? "tm-warning" : ""}>{account?.message && <Warning weight="fill" />}{status}</span></header><div className="tm-limit-grid">{windows.map(([label, window]) => <div key={label}><p className="tm-label">{label}</p>{window ? <><div className="tm-limit-value"><strong>{Math.max(0, window.remainingPercent ?? 100 - window.usedPercent)}% left</strong></div><progress className={(window.remainingPercent ?? 100 - window.usedPercent) <= 10 ? "warning" : (window.remainingPercent ?? 100 - window.usedPercent) <= 30 ? "violet" : ""} max="100" value={Math.max(0, window.remainingPercent ?? 100 - window.usedPercent)} /><small>{resetText(window.resetsAt)}<br />Resets in {countdown(window.resetsAt, now)}</small></> : unavailable}</div>)}<div><p className="tm-label">Reset credits</p>{account?.resetCredits ? <><strong>{integer.format(account.resetCredits.availableCount)} available</strong><small>{account.resetCredits.availableCount === 0 ? "No reset credits available" : account.resetCredits.expirations.length ? account.resetCredits.expirations.slice(0, account.resetCredits.availableCount).map((value, index) => <span key={`${String(value)}-${index}`}>Credit {index + 1} expires {formatDate(value, true)}</span>) : "Expiration details unavailable"}</small></> : unavailable}</div></div>{account?.message && <p className="tm-account-error" role="status">{account.message}</p>}</div></section>;
}

function SourceStatus({ status }: { status: DataSourceStatus }) {
  const StatusIcon = !status.exists ? XCircle : status.parseErrorCount > 0 ? Warning : status.scannedFileCount > 0 ? CheckCircle : MinusCircle;
  const ok = status.exists && status.parseErrorCount === 0 && status.scannedFileCount > 0;
  return <div className="tm-status-row"><StatusIcon className={ok ? "tm-positive" : status.exists && status.parseErrorCount === 0 ? "tm-tertiary" : "tm-warning"} weight="bold" aria-hidden="true" /><div><strong><i className={`tm-dot tm-${status.source}`} />{status.label}</strong><small title={status.path}>{status.path}</small></div><dl><div><dt>Scanned</dt><dd>{integer.format(status.scannedFileCount)}</dd></div><div><dt>Total</dt><dd>{integer.format(status.totalFileCount)}</dd></div>{status.parseErrorCount > 0 && <div><dt>Errors</dt><dd className="tm-warning">{integer.format(status.parseErrorCount)}</dd></div>}</dl></div>;
}

function SyncSummary({ status }: { status: SyncStatus }) {
  const title = !status.path ? "Off" : !status.exists ? "Missing folder" : status.exportError || status.parseErrorCount ? "Needs attention" : "Active";
  const Icon = !status.path ? CloudSlash : !status.exists || status.exportError || status.parseErrorCount ? Warning : CloudCheck;
  return <div className="tm-sync-summary"><Icon className={!status.path ? "tm-tertiary" : title === "Active" ? "tm-positive" : "tm-warning"} weight="bold" aria-hidden="true" /><div><strong>{title}</strong><small>{status.path || "No folder selected"}{status.lastSyncedAt ? ` · Synced ${formatDate(status.lastSyncedAt, true)}` : ""}</small></div>{status.path && <dl><div><dt>Files</dt><dd>{integer.format(status.deviceFileCount)}</dd></div><div><dt>Synced</dt><dd>{integer.format(status.importedEventCount)}</dd></div><div><dt>Exported</dt><dd>{integer.format(status.exportedEventCount)}</dd></div>{status.parseErrorCount > 0 && <div><dt>Errors</dt><dd className="tm-warning">{integer.format(status.parseErrorCount)}</dd></div>}</dl>}{status.exportError && <p className="tm-error" role="alert">{status.exportError}</p>}</div>;
}
