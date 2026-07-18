import { useCallback, useEffect, useMemo, useState, type CSSProperties } from "react";
import {
  ArrowLeft,
  FolderOpen,
  Minus,
  RefreshCw,
  Settings,
  Shield,
  Square,
  X,
} from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useQuotaGuardState } from "@/features/quota-guard/hooks/useQuotaGuardState";
import { quotaGuardPhaseLabel } from "@/features/quota-guard/quotaGuardViewModel";
import {
  addWorkspace,
  getAppSettings,
  listWorkspaces,
  pickWorkspacePath,
  updateAppSettings,
} from "@/services/tauri";
import type {
  AppSettings,
  QuotaAction,
  QuotaGuardSettings,
  RateLimitWindow,
  WorkspaceInfo,
} from "@/types";

type AsyncAction = "load" | "save" | "refresh" | "workspace" | null;
type Screen = "monitor" | "settings";
type Appearance = "light" | "dark";

const responseOptions: Array<{
  value: QuotaAction;
  shortLabel: string;
  title: string;
  description: string;
}> = [
  {
    value: "notifyOnly",
    shortLabel: "Notify",
    title: "Notify only",
    description: "Show an alert and keep working.",
  },
  {
    value: "finishCurrentTurn",
    shortLabel: "Finish turn",
    title: "Finish current turn",
    description: "Let active turns finish, then pause.",
  },
  {
    value: "interruptImmediately",
    shortLabel: "Interrupt",
    title: "Interrupt immediately",
    description: "Stop active turns now.",
  },
];

function clampPercent(value: number | undefined | null) {
  return Math.min(100, Math.max(0, Number.isFinite(value) ? Number(value) : 0));
}

function formatReset(timestamp: number | null | undefined) {
  if (!timestamp) return "Reset time unavailable";
  const remainingMinutes = Math.max(0, Math.ceil((timestamp * 1000 - Date.now()) / 60_000));
  if (remainingMinutes === 0) return "Reset pending";
  const hours = Math.floor(remainingMinutes / 60);
  const minutes = remainingMinutes % 60;
  return hours > 0 ? `Resets in ${hours}h ${minutes}m` : `Resets in ${minutes}m`;
}

function moreUsedWindow(
  primary: RateLimitWindow | null | undefined,
  secondary: RateLimitWindow | null | undefined,
) {
  if (!primary) return secondary ?? null;
  if (!secondary) return primary;
  return clampPercent(primary.usedPercent) >= clampPercent(secondary.usedPercent)
    ? primary
    : secondary;
}

export function UsageLimiterApp() {
  const quotaGuard = useQuotaGuardState();
  const [screen, setScreen] = useState<Screen>("monitor");
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [draft, setDraft] = useState<QuotaGuardSettings | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceInfo[]>([]);
  const [busy, setBusy] = useState<AsyncAction>("load");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [appearance, setAppearance] = useState<Appearance>(() =>
    localStorage.getItem("codex-usage-limiter.appearance") === "dark" ? "dark" : "light",
  );
  const [draftAppearance, setDraftAppearance] = useState<Appearance>(appearance);

  const load = useCallback(async () => {
    setBusy("load");
    setError(null);
    try {
      const [nextSettings, nextWorkspaces] = await Promise.all([getAppSettings(), listWorkspaces()]);
      setSettings(nextSettings);
      setDraft(nextSettings.quotaGuard);
      setWorkspaces(nextWorkspaces);
    } catch (loadError) {
      setError(String(loadError));
    } finally {
      setBusy(null);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    document.documentElement.dataset.appearance = appearance;
    localStorage.setItem("codex-usage-limiter.appearance", appearance);
  }, [appearance]);

  const persistDraft = useCallback(async (nextDraft: QuotaGuardSettings) => {
    if (!settings) return false;
    setDraft(nextDraft);
    setBusy("save");
    setError(null);
    setNotice(null);
    try {
      const updated = await updateAppSettings({ ...settings, quotaGuard: nextDraft });
      setSettings(updated);
      setDraft(updated.quotaGuard);
      return true;
    } catch (saveError) {
      setDraft(settings.quotaGuard);
      setError(String(saveError));
      return false;
    } finally {
      setBusy(null);
    }
  }, [settings]);

  const persistPatch = useCallback((patch: Partial<QuotaGuardSettings>) => {
    if (!settings) return;
    void persistDraft({ ...settings.quotaGuard, ...patch });
  }, [persistDraft, settings]);

  const setDraftThreshold = useCallback((value: number) => {
    const threshold = Math.max(1, Math.round(clampPercent(value)));
    setDraft((current) => current ? {
      ...current,
      primaryThresholdPercent: threshold,
      secondaryThresholdPercent: threshold,
    } : current);
  }, []);

  const connectWorkspace = useCallback(async () => {
    setError(null);
    const path = await pickWorkspacePath();
    if (!path) return;
    setBusy("workspace");
    try {
      await addWorkspace(path);
      const nextWorkspaces = await listWorkspaces();
      setWorkspaces(nextWorkspaces);
      if (settings) {
        const updated = await updateAppSettings(settings);
        setSettings(updated);
        setDraft(updated.quotaGuard);
      }
      setNotice("Codex workspace connected.");
    } catch (workspaceError) {
      setError(String(workspaceError));
    } finally {
      setBusy(null);
    }
  }, [settings]);

  const refreshUsage = useCallback(async () => {
    if (!settings?.quotaGuard.enabled) {
      setError("Turn on the limiter before refreshing usage.");
      return;
    }
    if (workspaces.length === 0) {
      setError("Connect a Codex workspace before refreshing usage.");
      return;
    }
    setBusy("refresh");
    setError(null);
    setNotice(null);
    try {
      await quotaGuard.verifyNow();
      setNotice("Usage refreshed from Codex.");
    } catch (refreshError) {
      setError(String(refreshError));
    } finally {
      setBusy(null);
    }
  }, [quotaGuard, settings?.quotaGuard.enabled, workspaces.length]);

  const activeWindow = useMemo(
    () => moreUsedWindow(quotaGuard.state?.snapshot?.primary, quotaGuard.state?.snapshot?.secondary),
    [quotaGuard.state?.snapshot?.primary, quotaGuard.state?.snapshot?.secondary],
  );
  const used = clampPercent(activeWindow?.usedPercent);
  const progressStyle = { "--progress": `${used}%` } as CSSProperties;
  const threshold = draft
    ? Math.min(draft.primaryThresholdPercent, draft.secondaryThresholdPercent)
    : 90;
  const markerStyle = { "--threshold": `${threshold}%` } as CSSProperties;
  const currentAction = responseOptions.find((option) => option.value === draft?.action)
    ?? responseOptions[0];
  const phaseLabel = quotaGuard.state
    ? quotaGuardPhaseLabel(quotaGuard.state.phase)
    : settings?.quotaGuard.enabled ? "Connecting" : "Disabled";
  const statusTone = quotaGuard.state?.monitorHealthy === false || error
    ? "danger"
    : quotaGuard.state?.phase === "monitoring" || quotaGuard.state?.phase === "ready"
      ? "healthy"
      : "neutral";
  const workspaceLabel = workspaces.length > 0
    ? `${workspaces[0]?.name || "Codex workspace"} connected`
    : "No Codex workspace";

  if (!draft || !settings) {
    return (
      <main className="limiter-window limiter-window--loading">
        <RefreshCw className="limiter-spin" size={20} />
        <span>{error ?? "Loading Codex usage…"}</span>
        {error ? <button onClick={() => void load()}>Try again</button> : null}
      </main>
    );
  }

  const appWindow = () => getCurrentWindow();
  const openSettings = () => {
    setDraft(settings.quotaGuard);
    setDraftAppearance(appearance);
    setError(null);
    setNotice(null);
    setScreen("settings");
  };
  const cancelSettings = () => {
    setDraft(settings.quotaGuard);
    setDraftAppearance(appearance);
    setError(null);
    setScreen("monitor");
  };
  const saveSettings = async () => {
    const saved = await persistDraft(draft);
    if (!saved) return;
    setAppearance(draftAppearance);
    setNotice("Settings saved.");
    setScreen("monitor");
  };

  return (
    <main className="limiter-window">
      <header className="limiter-titlebar" data-tauri-drag-region onDoubleClick={() => void appWindow().toggleMaximize()}>
        <div className="limiter-titlebar__brand" data-tauri-drag-region>
          {screen === "monitor" ? (
            <>
              <Shield aria-hidden="true" />
              <span data-tauri-drag-region>Codex Usage Limiter</span>
            </>
          ) : (
            <>
              <button type="button" className="limiter-back" aria-label="Back to usage" onClick={cancelSettings}>
                <ArrowLeft />
              </button>
              <span data-tauri-drag-region>Settings</span>
            </>
          )}
        </div>
        <div className="limiter-titlebar__actions">
          {screen === "monitor" ? (
            <button type="button" aria-label="Open settings" onClick={openSettings}><Settings /></button>
          ) : null}
          <button type="button" aria-label="Minimize window" onClick={() => void appWindow().minimize()}><Minus /></button>
          {screen === "monitor" ? (
            <button type="button" aria-label="Maximize window" onClick={() => void appWindow().toggleMaximize()}><Square /></button>
          ) : null}
          <button type="button" aria-label="Close window" onClick={() => void appWindow().close()}><X /></button>
        </div>
      </header>

      {screen === "monitor" ? (
        <div className="limiter-page limiter-monitor-page">
          <section className="limiter-status-row" aria-label="Limiter status">
            <div className={`limiter-status limiter-status--${statusTone}`}>
              <span aria-hidden="true" />
              <strong>{phaseLabel}</strong>
            </div>
            <label className="limiter-enabled-control">
              <span>{draft.enabled ? "Enabled" : "Disabled"}</span>
              <span className="reference-switch">
                <input
                  type="checkbox"
                  checked={draft.enabled}
                  disabled={busy === "save"}
                  onChange={(event) => persistPatch({ enabled: event.target.checked })}
                  aria-label={draft.enabled ? "Limiter enabled" : "Limiter disabled"}
                />
                <span aria-hidden="true" />
              </span>
            </label>
          </section>

          <section className="limiter-usage" aria-label="Current usage">
            <h1>Current usage</h1>
            <strong className={`limiter-usage__value${activeWindow && quotaGuard.state?.snapshotFresh === false ? " is-stale" : ""}`}>
              {Math.round(used)}%
            </strong>
            <p>
              {activeWindow
                ? quotaGuard.state?.snapshotFresh === false
                  ? "Stale reading — refresh for current usage"
                  : formatReset(activeWindow.resetsAt)
                : "No usage reading yet"}
            </p>
            <div
              className="limiter-progress"
              style={{ ...progressStyle, ...markerStyle }}
              role="progressbar"
              aria-label="Current Codex usage"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(used)}
            >
              <span className="limiter-progress__fill" />
              <span className="limiter-progress__marker" aria-hidden="true"><small>{threshold}%</small></span>
            </div>
          </section>

          <section className="limiter-response-row" aria-label="Usage response">
            <span>At {threshold}%</span>
            <label>
              <span className="sr-only">When limit is reached</span>
              <select
                aria-label="When limit is reached"
                value={draft.action}
                disabled={busy === "save"}
                onChange={(event) => persistPatch({ action: event.target.value as QuotaAction })}
              >
                {responseOptions.map((option) => (
                  <option value={option.value} key={option.value}>{option.title}</option>
                ))}
              </select>
            </label>
          </section>

          <footer className="limiter-monitor-footer">
            <div className="limiter-workspace">
              <span aria-hidden="true" />
              <strong>{workspaceLabel}</strong>
            </div>
            {workspaces.length === 0 ? (
              <button type="button" onClick={() => void connectWorkspace()} disabled={busy !== null}>
                <FolderOpen /> Connect
              </button>
            ) : (
              <button type="button" onClick={() => void refreshUsage()} disabled={busy !== null || !draft.enabled}>
                <RefreshCw className={busy === "refresh" ? "limiter-spin" : ""} /> Refresh
              </button>
            )}
            <span className="limiter-last-checked">
              {quotaGuard.state?.snapshotFresh ? "Last checked now" : "Waiting for update"}
            </span>
          </footer>

          {error || notice ? (
            <div className={`limiter-feedback${error ? " is-error" : ""}`} role={error ? "alert" : "status"}>
              {error ?? notice}
              <button type="button" onClick={() => { setError(null); setNotice(null); }} aria-label="Dismiss message"><X /></button>
            </div>
          ) : null}
        </div>
      ) : (
        <div className="limiter-page limiter-settings-page">
          <div className="limiter-settings-content">
            <section className="limiter-settings-row limiter-settings-row--toggle">
              <div>
                <h1>Usage limiter</h1>
                <p>Watch Codex quota and act at your limit</p>
              </div>
              <label className="limiter-enabled-control">
                <span>{draft.enabled ? "Enabled" : "Disabled"}</span>
                <span className="reference-switch">
                  <input
                    type="checkbox"
                    checked={draft.enabled}
                    disabled={busy === "save"}
                    onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}
                    aria-label={draft.enabled ? "Limiter enabled" : "Limiter disabled"}
                  />
                  <span aria-hidden="true" />
                </span>
              </label>
            </section>

            <section className="limiter-settings-row limiter-settings-row--threshold">
              <div className="limiter-setting-heading">
                <h2>Stop new work at</h2>
                <label className="limiter-percent-input">
                  <input
                    type="number"
                    min={1}
                    max={100}
                    disabled={busy === "save"}
                    value={threshold}
                    aria-label="Stop new work percentage"
                    onChange={(event) => setDraftThreshold(Number(event.target.value))}
                  />
                  <span>%</span>
                </label>
              </div>
              <input
                className="limiter-threshold-range"
                type="range"
                min={1}
                max={100}
                disabled={busy === "save"}
                value={threshold}
                style={{ "--range-progress": `${threshold}%` } as CSSProperties}
                aria-label="Stop new work at"
                onChange={(event) => setDraftThreshold(Number(event.target.value))}
              />
            </section>

            <section className="limiter-settings-row limiter-settings-row--response">
              <h2>When reached</h2>
              <div className="limiter-segmented" aria-label="Automatic response">
                {responseOptions.map((option) => (
                  <button
                    type="button"
                    key={option.value}
                    className={draft.action === option.value ? "is-selected" : ""}
                    aria-pressed={draft.action === option.value}
                    disabled={busy === "save"}
                    onClick={() => setDraft({ ...draft, action: option.value })}
                  >
                    {option.shortLabel}
                  </button>
                ))}
              </div>
              <p>{currentAction.description}</p>
            </section>

            <section className="limiter-settings-row limiter-settings-row--appearance">
              <h2>Appearance</h2>
              <div className="limiter-appearance-options">
                <button
                  type="button"
                  className={draftAppearance === "light" ? "is-selected" : ""}
                  aria-pressed={draftAppearance === "light"}
                  onClick={() => setDraftAppearance("light")}
                >Light</button>
                <button
                  type="button"
                  className={draftAppearance === "dark" ? "is-selected" : ""}
                  aria-pressed={draftAppearance === "dark"}
                  onClick={() => setDraftAppearance("dark")}
                >Dark</button>
              </div>
            </section>

            {error ? <div className="limiter-feedback limiter-feedback--inline is-error" role="alert">{error}</div> : null}
          </div>

          <footer className="limiter-settings-footer">
            <button type="button" className="limiter-button limiter-button--quiet" onClick={cancelSettings} disabled={busy === "save"}>Cancel</button>
            <button type="button" className="limiter-button limiter-button--primary" aria-label="Save changes" onClick={() => void saveSettings()} disabled={busy === "save"}>
              {busy === "save" ? "Saving…" : "Save changes"}
            </button>
          </footer>
        </div>
      )}
    </main>
  );
}
