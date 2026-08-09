import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";
import { RuntimeAtlas } from "./features/runtime-atlas/RuntimeAtlas";
import { TokenMeter } from "./features/token-meter/TokenMeter";

type Feature = "tokenMeter" | "runtimeAtlas";

type AppIdentity = {
  kind: "tokenMeter" | "runtimeAtlas" | "devConsole";
  displayName: string;
  features: Feature[];
};

type UpdateCheck = {
  available: boolean;
  version?: string;
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
  const updateBusyRef = useRef(false);
  const controlQ = useRef(false);
  const consumedQ = useRef(false);
  const consumedTab = useRef(false);

  useEffect(() => {
    invoke<AppIdentity>("app_identity")
      .then((value) => {
        setIdentity(value);
        setSelected(value.features[0]);
        document.title = value.displayName;
      })
      .catch((reason: unknown) => setError(String(reason)));
  }, []);

  useEffect(() => {
    if (!identity) return;
    let active = true;
    const checkForUpdates = async () => {
      if (updateBusyRef.current) return;
      try {
        const result = await invoke<UpdateCheck>("updater_check");
        if (!active || updateBusyRef.current) return;
        setUpdateFailed(false);
        if (result.available && result.version) {
          setUpdateVersion(result.version);
          setUpdateStatus(`Version ${result.version} is available`);
        } else {
          setUpdateVersion(undefined);
          setUpdateStatus(undefined);
        }
      } catch {}
    };
    void checkForUpdates();
    const interval = window.setInterval(() => void checkForUpdates(), UPDATE_CHECK_INTERVAL_MS);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [identity]);

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

  const runUpdateAction = async () => {
    updateBusyRef.current = true;
    setUpdateBusy(true);
    setUpdateFailed(false);
    try {
      if (updateVersion) {
        setUpdateStatus(`Installing v${updateVersion}…`);
        await invoke("updater_install", { expectedVersion: updateVersion });
        setUpdateVersion(undefined);
        setUpdateStatus("Update installed");
      } else {
        setUpdateStatus("Checking for updates…");
        const result = await invoke<UpdateCheck>("updater_check");
        if (result.available && result.version) {
          setUpdateVersion(result.version);
          setUpdateStatus(`Version ${result.version} is available`);
        } else {
          setUpdateStatus("You’re up to date");
        }
      }
    } catch (reason: unknown) {
      setUpdateFailed(true);
      setUpdateStatus(String(reason));
    } finally {
      updateBusyRef.current = false;
      setUpdateBusy(false);
    }
  };

  if (error) return <main className="centered" role="alert">{error}</main>;
  if (!identity || !selected) return <main className="centered">Loading…</main>;

  return (
    <main className="app-shell">
      <header className="top-bar">
        <h1>{identity.displayName}</h1>
        <div className="top-bar-actions">
          {identity.features.length > 1 && (
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
          )}
          <div className="update-control">
            {updateStatus && (
              <span
                id="update-status"
                role={updateFailed ? "alert" : "status"}
              >
                {updateStatus}
              </span>
            )}
            <button
              aria-describedby={updateStatus ? "update-status" : undefined}
              disabled={updateBusy}
              onClick={runUpdateAction}
              type="button"
            >
              {updateBusy
                ? updateVersion ? "Installing…" : "Checking…"
                : updateVersion ? `Install v${updateVersion}` : "Check for updates"}
            </button>
          </div>
        </div>
      </header>
      {identity.features.includes("tokenMeter") && (
        <div hidden={selected !== "tokenMeter"} aria-hidden={selected !== "tokenMeter"}>
          <TokenMeter />
        </div>
      )}
      {identity.features.includes("runtimeAtlas") && (
        <div hidden={selected !== "runtimeAtlas"} aria-hidden={selected !== "runtimeAtlas"}>
          <RuntimeAtlas />
        </div>
      )}
    </main>
  );
}
