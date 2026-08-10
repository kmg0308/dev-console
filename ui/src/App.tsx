import { ArrowCircleDown, X } from "@phosphor-icons/react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import { RuntimeAtlas } from "./features/runtime-atlas/RuntimeAtlas";
import { TokenMeter } from "./features/token-meter/TokenMeter";

type Feature = "tokenMeter" | "runtimeAtlas";

type AppIdentity = {
  kind: "tokenMeter" | "tokenMeterUpdaterQa" | "runtimeAtlas" | "devConsole";
  displayName: string;
  features: Feature[];
  platform: string;
};

type UpdateCheck = {
  available: boolean;
  version?: string;
};

type UpdatePresentation = {
  currentVersion?: string;
  availableVersion?: string;
  status?: string;
  failed: boolean;
  busy: boolean;
  onOpen: () => void;
  onInstall: () => void;
};

const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1_000;

export function App() {
  const [identity, setIdentity] = useState<AppIdentity>();
  const [selected, setSelected] = useState<Feature>();
  const [error, setError] = useState<string>();
  const [updateVersion, setUpdateVersion] = useState<string>();
  const [updateStatus, setUpdateStatus] = useState<string>();
  const [updateFailed, setUpdateFailed] = useState(false);
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const [currentVersion, setCurrentVersion] = useState<string>();
  const [showingUpdates, setShowingUpdates] = useState(false);
  const updateDialog = useRef<HTMLDialogElement>(null);
  const updateBusyRef = useRef(false);
  const pendingManualUpdateCheck = useRef(false);
  const controlQ = useRef(false);
  const consumedQ = useRef(false);
  const consumedTab = useRef(false);

  useEffect(() => {
    void getVersion().then(setCurrentVersion).catch(() => {});
    invoke<AppIdentity>("app_identity")
      .then((value) => {
        setIdentity(value);
        setSelected(value.features[0]);
        document.title = value.displayName;
      })
      .catch((reason: unknown) => setError(String(reason)));
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("app:check-for-updates", () => {
      setShowingUpdates(true);
      void checkForUpdates();
    }).then((stop) => {
      unlisten = stop;
    });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("app:open-runtime-atlas-settings", () => {
      setSelected("runtimeAtlas");
      window.requestAnimationFrame(() => window.dispatchEvent(new Event("runtime-atlas:open-settings")));
    }).then((stop) => {
      unlisten = stop;
    });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    if (!identity) return;
    void checkForUpdates(true);
    const interval = window.setInterval(() => void checkForUpdates(true), UPDATE_CHECK_INTERVAL_MS);
    return () => window.clearInterval(interval);
  }, [identity]);

  useEffect(() => {
    const dialog = updateDialog.current;
    if (!dialog) return;
    if (showingUpdates && !dialog.open) dialog.showModal();
    if (!showingUpdates && dialog.open) dialog.close();
  }, [showingUpdates, identity]);

  useEffect(() => {
    if (!identity || !selected) return;
    const commitWorktreeNavigation = () => {
      if (!controlQ.current) return;
      controlQ.current = false;
      window.dispatchEvent(new Event("runtime-atlas:commit-worktree-navigation"));
    };
    const cancelWorktreeNavigation = () => {
      controlQ.current = false;
      consumedQ.current = false;
      consumedTab.current = false;
      window.dispatchEvent(new Event("runtime-atlas:cancel-worktree-navigation"));
    };
    const keyDown = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if (key === "q" && consumedQ.current) {
        event.preventDefault();
        return;
      }
      if (key === "q" && event.ctrlKey && selected === "runtimeAtlas") {
        controlQ.current = true;
        consumedQ.current = true;
        event.preventDefault();
        return;
      }
      if (key !== "tab" || !event.ctrlKey) return;
      if (controlQ.current) {
        consumedTab.current = true;
        event.preventDefault();
        if (selected === "runtimeAtlas") {
          window.dispatchEvent(new CustomEvent("runtime-atlas:advance-worktree-navigation", {
            detail: { forward: !event.shiftKey },
          }));
        }
        return;
      }
      if (identity.features.length < 2) return;
      consumedTab.current = true;
      event.preventDefault();
      const current = identity.features.indexOf(selected);
      const offset = event.shiftKey ? identity.features.length - 1 : 1;
      const next = identity.features[(current + offset) % identity.features.length];
      if (selected === "runtimeAtlas" && next !== selected) {
        window.dispatchEvent(new Event("runtime-atlas:cancel-worktree-navigation"));
      }
      setSelected(next);
    };
    const keyUp = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if (key === "q" && consumedQ.current) {
        commitWorktreeNavigation();
        consumedQ.current = false;
        event.preventDefault();
        return;
      }
      if (key === "control" && controlQ.current) {
        commitWorktreeNavigation();
        event.preventDefault();
      }
      if (key === "tab" && consumedTab.current) {
        consumedTab.current = false;
        event.preventDefault();
      }
    };
    window.addEventListener("keydown", keyDown);
    window.addEventListener("keyup", keyUp);
    window.addEventListener("blur", cancelWorktreeNavigation);
    return () => {
      window.removeEventListener("keydown", keyDown);
      window.removeEventListener("keyup", keyUp);
      window.removeEventListener("blur", cancelWorktreeNavigation);
      cancelWorktreeNavigation();
    };
  }, [identity, selected]);

  const selectFeature = (feature: Feature) => {
    if (selected === "runtimeAtlas" && feature !== selected) {
      controlQ.current = false;
      consumedQ.current = false;
      consumedTab.current = false;
      window.dispatchEvent(new Event("runtime-atlas:cancel-worktree-navigation"));
    }
    setSelected(feature);
  };

  const checkForUpdates = async (silent = false) => {
    if (updateBusyRef.current) {
      if (!silent) pendingManualUpdateCheck.current = true;
      return;
    }
    updateBusyRef.current = true;
    setUpdateBusy(true);
    if (!silent) setUpdateFailed(false);
    if (!silent) setUpdateStatus("Checking for updates…");
    try {
      const result = await invoke<UpdateCheck>("updater_check");
      const reportResult = !silent || pendingManualUpdateCheck.current;
      setUpdateFailed(false);
      if (result.available && result.version) {
        setUpdateVersion(result.version);
        setUpdateStatus(`Version ${result.version} is available`);
      } else {
        setUpdateVersion(undefined);
        setUpdateStatus(reportResult ? "You’re up to date" : undefined);
      }
    } catch (reason: unknown) {
      if (!silent || pendingManualUpdateCheck.current) {
        setUpdateFailed(true);
        setUpdateStatus(String(reason));
      }
    } finally {
      pendingManualUpdateCheck.current = false;
      updateBusyRef.current = false;
      setUpdateBusy(false);
    }
  };

  const installUpdate = async () => {
    if (!updateVersion || updateBusyRef.current) return;
    updateBusyRef.current = true;
    setUpdateBusy(true);
    setUpdateInstalling(true);
    setUpdateFailed(false);
    setUpdateStatus(`Installing v${updateVersion}…`);
    try {
      await invoke("updater_install", { expectedVersion: updateVersion });
      setUpdateVersion(undefined);
      setUpdateStatus("Update installed");
    } catch (reason: unknown) {
      setUpdateFailed(true);
      setUpdateStatus(String(reason));
    } finally {
      updateBusyRef.current = false;
      setUpdateBusy(false);
      setUpdateInstalling(false);
    }
  };

  if (error) return <main className="centered" role="alert">{error}</main>;
  if (!identity || !selected) return <main className="centered">Loading…</main>;

  const standaloneUpdate: UpdatePresentation | undefined = identity.kind === "devConsole" ? undefined : {
    currentVersion,
    availableVersion: updateVersion,
    status: updateStatus,
    failed: updateFailed,
    busy: updateBusy,
    onOpen: () => setShowingUpdates(true),
    onInstall: () => { void installUpdate(); },
  };

  return (
    <main className={`app-shell app-${identity.kind} ${identity.kind === "devConsole" ? "dev-console-shell" : "standalone-shell"}`}>
      {identity.kind !== "devConsole" && identity.platform === "macos" && <div className="standalone-titlebar" data-tauri-drag-region />}
      {identity.kind === "devConsole" && (
        <header className="top-bar" data-tauri-drag-region>
          <nav aria-label="Features">
            {identity.features.map((feature) => (
              <button
                aria-current={selected === feature ? "page" : undefined}
                className={selected === feature ? "selected" : undefined}
                key={feature}
                onClick={() => selectFeature(feature)}
                type="button"
              >
                {feature === "tokenMeter" ? "TokenMeter" : "Runtime Atlas"}
              </button>
            ))}
          </nav>
          {updateVersion && (
            <button
              aria-label={`Update ${updateVersion} available`}
              className="update-chip available"
              disabled={updateBusy}
              onClick={() => setShowingUpdates(true)}
              title={`Update ${updateVersion} available`}
              type="button"
            >
              <ArrowCircleDown aria-hidden="true" size={16} weight="fill" />
              <span>Update {updateVersion}</span>
            </button>
          )}
        </header>
      )}
      {identity.features.includes("tokenMeter") && (
        <div hidden={selected !== "tokenMeter"} aria-hidden={selected !== "tokenMeter"}>
          <TokenMeter active={selected === "tokenMeter"} update={standaloneUpdate} />
        </div>
      )}
      {identity.features.includes("runtimeAtlas") && (
        <div hidden={selected !== "runtimeAtlas"} aria-hidden={selected !== "runtimeAtlas"}>
          <RuntimeAtlas active={selected === "runtimeAtlas"} update={standaloneUpdate} />
        </div>
      )}
      <dialog
        aria-labelledby="update-title"
        className={`update-dialog update-dialog-${identity.kind}`}
        onCancel={(event) => {
          if (updateBusy) event.preventDefault();
          else setShowingUpdates(false);
        }}
        onClose={() => setShowingUpdates(false)}
        ref={updateDialog}
      >
        <header>
          <div>
            <h2 id="update-title">{identity.kind === "devConsole" ? "DevConsole Update" : "Updates"}</h2>
            <span className={updateFailed ? "update-error" : updateVersion ? "update-available" : undefined}>
              {updateInstalling ? "Installing" : updateVersion ? "Update available" : updateStatus === "You’re up to date" ? "Up to date" : updateFailed ? "Update check failed" : "Not checked"}
            </span>
          </div>
          <button aria-label="Close" disabled={updateBusy} onClick={() => setShowingUpdates(false)} type="button"><X weight="bold" /></button>
        </header>
        <section className="update-panel">
          <dl className="update-versions">
            <div><dt>Current</dt><dd>{currentVersion ?? "—"}</dd></div>
            <div><dt>Available</dt><dd>{updateVersion ?? (updateStatus === "You’re up to date" ? "No update" : "Not checked")}</dd></div>
          </dl>
          <p className={updateFailed ? "update-error" : undefined} role={updateFailed ? "alert" : "status"}>
            {updateStatus ?? `Check for the latest ${identity.displayName} release.`}
          </p>
          {identity.kind === "runtimeAtlas" && <small>Updates are verified before installation and keep the bundled command-line tools in sync.</small>}
        </section>
        <div className="update-actions">
          <button disabled={updateBusy} onClick={() => void checkForUpdates()} type="button">
            {updateBusy && !updateInstalling ? "Checking…" : "Check for Updates"}
          </button>
          <span />
          {updateVersion && (
            <button className="primary" disabled={updateBusy} onClick={() => void installUpdate()} type="button">
              {updateInstalling ? "Installing…" : "Install and Relaunch"}
            </button>
          )}
        </div>
      </dialog>
    </main>
  );
}
