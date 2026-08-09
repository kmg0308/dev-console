import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
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

const compactTokens = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const exactTokens = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const integer = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const dateTime = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const shortTime = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

function asDate(value: Timestamp) {
  return new Date(typeof value === "number" ? (value + 978_307_200) * 1_000 : value);
}

function formatDate(value?: Timestamp, short = false) {
  if (value === undefined) return "—";
  const date = asDate(value);
  return Number.isNaN(date.valueOf()) ? "Unknown time" : (short ? shortTime : dateTime).format(date);
}

function formatTokens(value: TokenCount | bigint, exact: boolean) {
  value = typeof value === "bigint" ? value : BigInt(value);
  return (exact ? exactTokens : compactTokens).format(value);
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
  const cacheCreation = BigInt(usage.cacheCreation);
  const cacheRead = BigInt(usage.cacheRead);
  const output = BigInt(usage.output);
  const reasoning = BigInt(usage.reasoning);
  return [
    { label: "Input", value: input > cachedInput ? input - cachedInput : 0n, className: "tm-input" },
    { label: "Cache", value: cachedInput + cacheCreation + cacheRead, className: "tm-cache" },
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

function moveChartFocus(event: KeyboardEvent<HTMLButtonElement>, index: number) {
  const bars = event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>(".tm-chart-bar");
  if (!bars) return;
  const next = event.key === "ArrowLeft" ? index - 1 : event.key === "ArrowRight" ? index + 1 : event.key === "Home" ? 0 : event.key === "End" ? bars.length - 1 : index;
  if (next === index) return;
  event.preventDefault();
  bars[Math.max(0, Math.min(bars.length - 1, next))]?.focus();
}

export function TokenMeter() {
  const [source, setSource] = useState<TokenSource>("all");
  const [range, setRange] = useState("8h");
  const [bucket, setBucket] = useState("auto");
  const [project, setProject] = useState("");
  const [model, setModel] = useState("");
  const [device, setDevice] = useState("");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot>();
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [reload, setReload] = useState(0);
  const [exact, setExact] = useState(false);
  const exactInitialized = useRef(false);
  const [activeBucket, setActiveBucket] = useState(0);
  const [cleanupDays, setCleanupDays] = useState(30);
  const [cleanupPlan, setCleanupPlan] = useState<CleanupPlan>();
  const [syncPath, setSyncPath] = useState("");
  const syncPathDirty = useRef(false);
  const [sourcePaths, setSourcePaths] = useState<SourcePaths>({ codexHome: "", claudeProjectsPath: "", hermesDatabasePath: "", codexExecutablePath: "" });
  const sourcePathsDirty = useRef(false);
  const [action, setAction] = useState<ActionState>({ kind: "idle" });

  useEffect(() => {
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
    setLoading(true);
    setLoadError(undefined);
    invoke<DashboardSnapshot>("token_meter_dashboard", { request })
      .then((value) => {
        if (stale) return;
        setSnapshot(value);
        setProject(value.selection.filters.project ?? "");
        setModel(value.selection.filters.model ?? "");
        setDevice(value.selection.filters.device ?? "");
        setActiveBucket(Math.max(0, value.buckets.length - 1));
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
      })
      .catch((reason: unknown) => {
        if (!stale) setLoadError(errorMessage(reason));
      })
      .finally(() => {
        if (!stale) setLoading(false);
      });
    return () => { stale = true; };
  }, [source, range, bucket, project, model, device, reload]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") setReload((value) => value + 1);
    }, 60_000);
    return () => window.clearInterval(timer);
  }, []);

  async function previewCleanup() {
    setAction({ kind: "working", scope: "sources", message: "Checking old sessions…" });
    setCleanupPlan(undefined);
    try {
      const plan = await invoke<CleanupPlan>("token_meter_cleanup_preview", { olderThanDays: cleanupDays });
      setCleanupPlan(plan);
      setAction({ kind: "success", scope: "sources", message: `${integer.format(plan.candidateCount)} sessions can be archived (${compactTokens.format(BigInt(plan.totalBytes))} bytes).` });
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
      setReload((value) => value + 1);
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
      setReload((value) => value + 1);
    } catch (reason) {
      setAction({ kind: "error", scope: "sync", message: errorMessage(reason) });
    }
  }

  async function chooseSyncFolder() {
    try {
      const path = await pickDirectory();
      if (!path) return;
      setSyncPath(path);
      syncPathDirty.current = true;
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
          <button type="button" onClick={() => setReload((value) => value + 1)}>Try Again</button>
        </div>
      </section>
    );
  }

  const previous = snapshot.previousTotal.total;
  const change = snapshot.changePercent;
  const chartMaximum = Math.max(1, ...snapshot.buckets.map((item) => tokenNumber(item.usage.total)));
  const selectedBucket = snapshot.buckets[activeBucket] ?? snapshot.buckets.at(-1);
  const selectedSegments = selectedBucket ? segmentsFor(selectedBucket, source) : [];
  const noUsage = BigInt(snapshot.total.total) === 0n && snapshot.groups.sessions.length === 0;

  return (
    <section className="tm-shell" aria-labelledby="token-meter-title" aria-busy={loading}>
      <header className="tm-header">
        <div><p className="tm-eyebrow">Local usage</p><h2 id="token-meter-title">TokenMeter</h2></div>
        <div className="tm-source-tabs" role="group" aria-label="Token source">
          {SOURCES.map((option) => (
            <button className={source === option.value ? "selected" : ""} type="button" aria-pressed={source === option.value} onClick={() => setSource(option.value)} key={option.value}>{option.label}</button>
          ))}
        </div>
        <button className="tm-button" type="button" disabled={loading} onClick={() => setReload((value) => value + 1)}>{loading ? "Refreshing…" : "Refresh"}</button>
      </header>

      {loadError && <div className="tm-notice tm-error" role="alert"><strong>Refresh failed.</strong> {loadError}</div>}
      {noUsage && <div className="tm-notice" role="status"><strong>No matching usage.</strong> Change the source, time range, or filters, or check the data sources below.</div>}

      <section className="tm-card tm-summary" aria-labelledby="tm-summary-title">
        <div className="tm-section-heading">
          <div><p className="tm-label" id="tm-summary-title">{range} total</p><p className="tm-total">{formatTokens(snapshot.total.total, exact)} <span>tokens</span></p></div>
          <label className="tm-switch"><input type="checkbox" checked={exact} onChange={(event) => void saveExactNumbers(event.target.checked)} /> Show exact numbers</label>
        </div>
        <dl className="tm-metrics">
          {source === "all" && <><div><dt><i className="tm-dot tm-codex" />Codex</dt><dd>{formatTokens(snapshot.buckets.reduce((sum, item) => sum + BigInt(item.sourceUsage.codex?.total ?? "0"), 0n), exact)}</dd></div><div><dt><i className="tm-dot tm-claude" />Claude Code</dt><dd>{formatTokens(snapshot.buckets.reduce((sum, item) => sum + BigInt(item.sourceUsage.claude?.total ?? "0"), 0n), exact)}</dd></div></>}
          <div><dt>Sessions</dt><dd>{integer.format(snapshot.sessionCount)}</dd></div>
          <div><dt>Previous</dt><dd>{formatTokens(previous, exact)}</dd></div>
          <div><dt>Change</dt><dd className={change != null && change > 0 ? "tm-warning" : change != null && change < 0 ? "tm-positive" : ""}>{change == null ? (BigInt(snapshot.total.total) > 0n && previous === "0" ? "New" : "—") : `${change > 0 ? "+" : ""}${Math.round(change)}%`}</dd></div>
        </dl>
      </section>

      <section className="tm-section" aria-labelledby="tm-usage-title">
        <div className="tm-section-heading">
          <h3 id="tm-usage-title">Usage</h3>
          <div className="tm-controls">
            <label>Range<select value={range} onChange={(event) => setRange(event.target.value)}>{RANGES.map((value) => <option key={value}>{value}</option>)}</select></label>
            <label>Bucket<select value={bucket} onChange={(event) => setBucket(event.target.value)}>{BUCKETS.map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></label>
          </div>
        </div>
        <div className="tm-chart-card">
          {snapshot.buckets.length === 0 ? <p className="tm-empty">No usage buckets for this selection.</p> : (
            <>
              <div className="tm-chart" aria-label="Token usage over time">
                {snapshot.buckets.map((item, index) => {
                  const segments = segmentsFor(item, source);
                  const label = `${formatDate(item.start, true)}, ${formatTokens(item.usage.total, exact)} tokens${segments.map((segment) => `, ${segment.label} ${formatTokens(segment.value, exact)}`).join("")}`;
                  return (
                    <button className={`tm-chart-bar${activeBucket === index ? " active" : ""}`} type="button" aria-label={label} tabIndex={activeBucket === index ? 0 : -1} key={`${String(item.start)}-${index}`} onFocus={() => setActiveBucket(index)} onPointerEnter={() => setActiveBucket(index)} onKeyDown={(event) => moveChartFocus(event, index)}>
                      <span className="tm-bar-stack" style={{ height: `${Math.max(2, tokenNumber(item.usage.total) / chartMaximum * 100)}%` }} aria-hidden="true">
                        {segments.map((segment) => <i className={segment.className} style={{ flexGrow: tokenNumber(segment.value) }} key={segment.label} />)}
                      </span>
                    </button>
                  );
                })}
              </div>
              <output className="tm-chart-summary" aria-live="polite">
                {selectedBucket && <><strong>{formatDate(selectedBucket.start, true)}</strong><span>{formatTokens(selectedBucket.usage.total, exact)} total</span>{selectedSegments.map((segment) => <span key={segment.label}><i className={`tm-dot ${segment.className}`} />{segment.label} {formatTokens(segment.value, exact)}</span>)}</>}
              </output>
            </>
          )}
        </div>
      </section>

      <section className="tm-section" aria-labelledby="tm-breakdown-title">
        <h3 id="tm-breakdown-title">Breakdown</h3>
        <div className="tm-breakdown">{usageComponents(snapshot.total).map((item) => <div className="tm-card" key={item.label}><span><i className={`tm-dot ${item.className}`} />{item.label}</span><strong>{formatTokens(item.value, exact)}</strong></div>)}</div>
      </section>

      <details className="tm-details" open>
        <summary>Filters</summary>
        <div className="tm-filter-grid">
          <label>Project<select value={project} onChange={(event) => setProject(event.target.value)}><option value="">All Projects</option>{snapshot.filterOptions.projects.map((value) => <option value={value} key={value}>{shortPath(value)}</option>)}</select></label>
          <label>Model<select value={model} onChange={(event) => setModel(event.target.value)}><option value="">All Models</option>{snapshot.filterOptions.models.map((value) => <option key={value}>{value}</option>)}</select></label>
          <label>Device<select value={device} onChange={(event) => setDevice(event.target.value)}><option value="">All Devices</option>{snapshot.filterOptions.devices.map((value) => <option value={value.id} key={value.id}>{value.name}</option>)}</select></label>
        </div>
      </details>

      <details className="tm-details" open>
        <summary>Details</summary>
        <div className="tm-tables">
          <UsageTable title="Projects" keyLabel="Project" rows={snapshot.groups.projects} exact={exact} formatKey={shortPath} />
          <UsageTable title="Models" keyLabel="Model" rows={snapshot.groups.models} exact={exact} />
          <UsageTable title="Sessions" keyLabel="Session" rows={snapshot.groups.sessions} exact={exact} wide />
        </div>
      </details>

      <CodexLimits account={snapshot.codexAccount} />

      <section className="tm-section" aria-labelledby="tm-source-title">
        <div className="tm-section-heading"><h3 id="tm-source-title">Data Sources</h3><button className="tm-button" type="button" disabled={action.kind === "working"} onClick={rebuildCache}>Rebuild Cache</button></div>
        <div className="tm-card tm-status-list">
          {snapshot.sourceStatuses.length === 0 ? <p className="tm-empty">Source status is not available.</p> : snapshot.sourceStatuses.map((status) => <SourceStatus status={status} key={`${status.source}-${status.path}`} />)}
          <form className="tm-source-settings" onSubmit={(event) => { event.preventDefault(); void saveSourcePaths(); }}>
            <p>Blank paths use verified platform defaults when available. Set an absolute Codex executable path only to override the platform lookup.</p>
            <label>Codex home<input type="text" value={sourcePaths.codexHome} placeholder="Absolute path to the Codex data folder" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, codexHome: event.target.value })); }} /></label>
            <label>Claude projects<input type="text" value={sourcePaths.claudeProjectsPath} placeholder="Absolute path to the Claude projects folder" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, claudeProjectsPath: event.target.value })); }} /></label>
            <label>Hermes database<input type="text" value={sourcePaths.hermesDatabasePath} placeholder="Absolute path to the Hermes state database" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, hermesDatabasePath: event.target.value })); }} /></label>
            <label>Codex executable<input type="text" value={sourcePaths.codexExecutablePath} placeholder="Absolute path to the Codex executable" spellCheck={false} onChange={(event) => { sourcePathsDirty.current = true; setSourcePaths((paths) => ({ ...paths, codexExecutablePath: event.target.value })); }} /></label>
            <div className="tm-actions"><button className="tm-button tm-primary" type="submit" disabled={action.kind === "working"}>Save Paths</button></div>
          </form>
          <div className="tm-cleanup">
            <label>Archive sessions older than <input type="number" min="1" max="3650" value={cleanupDays} onChange={(event) => { setCleanupDays(Math.max(1, Number(event.target.value))); setCleanupPlan(undefined); }} /> days</label>
            <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={previewCleanup}>Preview Cleanup</button>
            {cleanupPlan && cleanupPlan.candidateCount > 0 && <button className="tm-button tm-primary" type="button" disabled={action.kind === "working"} onClick={applyCleanup}>Archive {integer.format(cleanupPlan.candidateCount)} Sessions</button>}
          </div>
        </div>
        {action.scope === "sources" && <div className={`tm-notice${action.kind === "error" ? " tm-error" : ""}`} role={action.kind === "error" ? "alert" : "status"}>{action.message}</div>}
      </section>

      <section className="tm-section" aria-labelledby="tm-sync-title">
        <h3 id="tm-sync-title">Sync Folder</h3>
        <div className="tm-card tm-sync">
          <SyncSummary status={snapshot.syncStatus} />
          <label>Folder path<input type="text" value={syncPath} placeholder="Enter an absolute folder path" onChange={(event) => { setSyncPath(event.target.value); syncPathDirty.current = true; }} /></label>
          <div className="tm-actions">{snapshot.settings.icloudSyncFolderPath && <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => saveSyncFolder(snapshot.settings.icloudSyncFolderPath!, false)}>Use iCloud Drive</button>}<button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => void chooseSyncFolder()}>Choose Folder…</button><button className="tm-button tm-primary" type="button" disabled={!syncPath.trim() || action.kind === "working"} onClick={() => saveSyncFolder(syncPath.trim())}>Use Folder</button>{snapshot.settings.syncFolderPath && <button className="tm-button" type="button" disabled={action.kind === "working"} onClick={() => saveSyncFolder(null)}>Turn Off</button>}</div>
        </div>
        {action.scope === "sync" && <div className={`tm-notice${action.kind === "error" ? " tm-error" : ""}`} role={action.kind === "error" ? "alert" : "status"}>{action.message}</div>}
      </section>

      <footer className="tm-footer">Updated {formatDate(snapshot.generatedAt)} · {snapshot.settings.localDeviceName}</footer>
    </section>
  );
}

function UsageTable({ title, keyLabel, rows, exact, formatKey = (value) => value, wide = false }: { title: string; keyLabel: string; rows: UsageRow[]; exact: boolean; formatKey?: (value: string) => string; wide?: boolean }) {
  return (
    <section className={`tm-card tm-table-card${wide ? " wide" : ""}`} aria-labelledby={`tm-table-${title}`}>
      <h3 id={`tm-table-${title}`}>{title}</h3>
      <div className="tm-table-scroll"><table><thead><tr><th>{keyLabel}</th><th>Total</th><th>Input</th><th>Cache</th><th>Output</th><th>Reasoning</th><th>Events</th></tr></thead><tbody>
        {rows.map((row) => { const components = usageComponents(row.usage); const value = (label: string) => components.find((item) => item.label === label)?.value ?? 0n; return <tr key={row.key}><td title={row.key}>{formatKey(row.key)}</td><td>{formatTokens(row.usage.total, exact)}</td><td>{formatTokens(value("Input"), exact)}</td><td>{formatTokens(value("Cache"), exact)}</td><td>{formatTokens(value("Output"), exact)}</td><td>{formatTokens(value("Reasoning"), exact)}</td><td>{integer.format(row.eventCount)}</td></tr>; })}
        {rows.length === 0 && <tr><td colSpan={7}>No data</td></tr>}
      </tbody></table></div>
    </section>
  );
}

function CodexLimits({ account }: { account?: CodexAccount | null }) {
  const windows: Array<[string, RateLimitWindow | undefined]> = [["5 hour", account?.fiveHour], ["7 day", account?.weekly]];
  return (
    <section className="tm-section" aria-labelledby="tm-limits-title"><h3 id="tm-limits-title">Codex limits</h3><div className="tm-card tm-limits">
      {windows.map(([label, window]) => <div key={label}><p className="tm-label">{label}</p>{window ? <><strong>{Math.max(0, window.remainingPercent ?? 100 - window.usedPercent)}% left</strong><progress max="100" value={Math.max(0, window.remainingPercent ?? 100 - window.usedPercent)} /><small>{window.usedPercent}% used · Resets {formatDate(window.resetsAt, true)}</small></> : <p className="tm-empty">Not available</p>}</div>)}
      <div><p className="tm-label">Reset credits</p>{account?.resetCredits ? <><strong>{integer.format(account.resetCredits.availableCount)} available</strong><small>{account.resetCredits.expirations.length ? account.resetCredits.expirations.map((value) => formatDate(value, true)).join(" · ") : "Expiration details unavailable"}</small></> : <p className="tm-empty">Not available</p>}</div>
      {account?.message && <p className="tm-error" role="status">{account.message}</p>}
    </div></section>
  );
}

function SourceStatus({ status }: { status: DataSourceStatus }) {
  return <div className="tm-status-row"><span className={`tm-status-icon ${status.exists && status.parseErrorCount === 0 ? "ok" : "warn"}`} aria-hidden="true">{status.exists && status.parseErrorCount === 0 ? "●" : "▲"}</span><div><strong><i className={`tm-dot tm-${status.source}`} />{status.label}</strong><small>{status.exists ? "Available" : "Missing"}</small><small title={status.path}>{status.path}</small></div><dl><div><dt>Scanned</dt><dd>{integer.format(status.scannedFileCount)}</dd></div><div><dt>Total</dt><dd>{integer.format(status.totalFileCount)}</dd></div>{status.parseErrorCount > 0 && <div><dt>Errors</dt><dd className="tm-warning">{integer.format(status.parseErrorCount)}</dd></div>}</dl></div>;
}

function SyncSummary({ status }: { status: SyncStatus }) {
  const title = !status.path ? "Off" : !status.exists ? "Missing folder" : status.exportError || status.parseErrorCount ? "Needs attention" : "Active";
  return <div className="tm-sync-summary"><div><strong>{title}</strong><small>{status.path || "No folder selected"}{status.lastSyncedAt ? ` · Synced ${formatDate(status.lastSyncedAt, true)}` : ""}</small></div>{status.path && <dl><div><dt>Files</dt><dd>{integer.format(status.deviceFileCount)}</dd></div><div><dt>Imported</dt><dd>{integer.format(status.importedEventCount)}</dd></div><div><dt>Exported</dt><dd>{integer.format(status.exportedEventCount)}</dd></div><div><dt>Errors</dt><dd>{integer.format(status.parseErrorCount)}</dd></div></dl>}{status.exportError && <p className="tm-error" role="alert">{status.exportError}</p>}</div>;
}
