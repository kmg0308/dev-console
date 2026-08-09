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
    const keyDown = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if (key === "q" && event.ctrlKey) {
        controlQ.current = true;
        event.preventDefault();
        return;
      }
      if (key !== "tab" || !event.ctrlKey) return;
      consumedTab.current = true;
      event.preventDefault();
      if (controlQ.current) {
        if (selected === "runtimeAtlas") {
          window.dispatchEvent(new CustomEvent("runtime-atlas:advance-worktree-navigation", {
            detail: { forward: !event.shiftKey },
          }));
        }
        return;
      }
      if (identity.features.length < 2) return;
      const current = identity.features.indexOf(selected);
      const offset = event.shiftKey ? identity.features.length - 1 : 1;
      setSelected(identity.features[(current + offset) % identity.features.length]);
    };
    const keyUp = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if (key === "q" || key === "control") commitWorktreeNavigation();
      if (key === "tab" && consumedTab.current) {
        consumedTab.current = false;
        event.preventDefault();
      }
    };
    window.addEventListener("keydown", keyDown);
    window.addEventListener("keyup", keyUp);
    window.addEventListener("blur", commitWorktreeNavigation);
    return () => {
      window.removeEventListener("keydown", keyDown);
      window.removeEventListener("keyup", keyUp);
      window.removeEventListener("blur", commitWorktreeNavigation);
    };
  }, [identity, selected]);

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
                  onClick={() => setSelected(feature)}
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
      {selected === "tokenMeter" ? <TokenMeter /> : <RuntimeAtlas />}
    </main>
  );
}
