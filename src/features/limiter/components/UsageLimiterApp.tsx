import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import {
  ArrowLeft,
  FolderOpen,
  Minus,
  RefreshCw,
  Settings,
  Shield,
  X,
} from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
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
type WindowMode = "compact" | "mini" | "pill";

const APPEARANCE_KEY = "codex-usage-limiter.appearance";
const WINDOW_MODE_KEY = "codex-usage-limiter.windowMode";
const ALWAYS_ON_TOP_KEY = "codex-usage-limiter.alwaysOnTop";

const MODE_WINDOWS: Record<WindowMode, { width: number; height: number; minWidth: number; minHeight: number; resizable: boolean }> = {
  compact: { width: 420, height: 240, minWidth: 380, minHeight: 220, resizable: true },
  mini: { width: 320, height: 168, minWidth: 320, minHeight: 168, resizable: false },
  pill: { width: 280, height: 72, minWidth: 280, minHeight: 72, resizable: false },
};
const SETTINGS_WINDOW = { width: 420, height: 500, minWidth: 420, minHeight: 500, resizable: false };

const windowModeOptions: Array<{ value: WindowMode; label: string; dims: string }> = [
  { value: "compact", label: "Compact", dims: "420 × 240" },
  { value: "mini", label: "Mini", dims: "320 × 168" },
  { value: "pill", label: "Pill", dims: "280 × 72" },
];

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

/** The UI works in "% remaining"; the backend stores "% used". */
function clampRemainingFloor(value: number) {
  return Math.min(99, Math.max(1, Math.round(Number.isFinite(value) ? Number(value) : 0)));
}

function remainingParts(timestamp: number) {
  const remainingMinutes = Math.max(0, Math.ceil((timestamp * 1000 - Date.now()) / 60_000));
  const days = Math.floor(remainingMinutes / 1440);
  const hours = Math.floor((remainingMinutes % 1440) / 60);
  const minutes = remainingMinutes % 60;
  return { remainingMinutes, days, hours, minutes };
}

function formatReset(timestamp: number | null | undefined) {
  if (!timestamp) return "Reset time unavailable";
  const { remainingMinutes, days, hours, minutes } = remainingParts(timestamp);
  if (remainingMinutes === 0) return "Reset pending";
  if (days > 0) return `Resets in ${days}d ${hours}h ${minutes}m`;
  return hours > 0 ? `Resets in ${hours}h ${minutes}m` : `Resets in ${minutes}m`;
}

function formatResetShort(timestamp: number | null | undefined) {
  if (!timestamp) return "no reset data";
  const { remainingMinutes, days, hours, minutes } = remainingParts(timestamp);
  if (remainingMinutes === 0) return "reset pending";
  if (days > 0) return `resets ${days}d ${hours}h`;
  return hours > 0 ? `resets ${hours}h ${minutes}m` : `resets ${minutes}m`;
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

function loadWindowMode(): WindowMode {
  const stored = localStorage.getItem(WINDOW_MODE_KEY);
  return stored === "mini" || stored === "pill" ? stored : "compact";
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
    localStorage.getItem(APPEARANCE_KEY) === "dark" ? "dark" : "light",
  );
  const [windowMode, setWindowMode] = useState<WindowMode>(loadWindowMode);
  const [alwaysOnTop, setAlwaysOnTop] = useState<boolean>(() =>
    localStorage.getItem(ALWAYS_ON_TOP_KEY) === "true",
  );
  const [draftAppearance, setDraftAppearance] = useState<Appearance>(appearance);
  const [draftWindowMode, setDraftWindowMode] = useState<WindowMode>(windowMode);
  const [draftAlwaysOnTop, setDraftAlwaysOnTop] = useState<boolean>(alwaysOnTop);
  const barRef = useRef<HTMLDivElement | null>(null);
  const dragValue = useRef<number | null>(null);

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
    localStorage.setItem(APPEARANCE_KEY, appearance);
  }, [appearance]);

  useEffect(() => {
    localStorage.setItem(WINDOW_MODE_KEY, windowMode);
  }, [windowMode]);

  useEffect(() => {
    localStorage.setItem(ALWAYS_ON_TOP_KEY, String(alwaysOnTop));
  }, [alwaysOnTop]);

  useEffect(() => {
    void (async () => {
      try {
        const appWindow = getCurrentWindow();
        const target = screen === "settings" ? SETTINGS_WINDOW : MODE_WINDOWS[windowMode];
        await appWindow.setAlwaysOnTop(alwaysOnTop);
        await appWindow.setResizable(screen === "monitor" && target.resizable);
        await appWindow.setMinSize(new LogicalSize(target.minWidth, target.minHeight));
        await appWindow.setSize(new LogicalSize(target.width, target.height));
      } catch (windowError) {
        setError(`Window update failed: ${String(windowError)}`);
      }
    })();
  }, [screen, windowMode, alwaysOnTop]);

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

  const setDraftFloor = useCallback((remainingFloor: number) => {
    const usedThreshold = 100 - clampRemainingFloor(remainingFloor);
    setDraft((current) => current ? {
      ...current,
      primaryThresholdPercent: usedThreshold,
      secondaryThresholdPercent: usedThreshold,
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
  const remaining = Math.round(100 - used);
  const usedThreshold = draft
    ? Math.min(draft.primaryThresholdPercent, draft.secondaryThresholdPercent)
    : 90;
  const floor = 100 - usedThreshold;
  const barStyle = { "--progress": `${remaining}%`, "--threshold": `${floor}%` } as CSSProperties;
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
  const snapshotStale = Boolean(activeWindow) && quotaGuard.state?.snapshotFresh === false;

  const floorFromPointer = useCallback((clientX: number) => {
    const rect = barRef.current?.getBoundingClientRect();
    if (!rect || rect.width === 0) return null;
    return clampRemainingFloor(((clientX - rect.left) / rect.width) * 100);
  }, []);

  const persistFloor = useCallback((remainingFloor: number) => {
    if (!settings) return;
    const usedThreshold = 100 - clampRemainingFloor(remainingFloor);
    if (settings.quotaGuard.primaryThresholdPercent === usedThreshold
      && settings.quotaGuard.secondaryThresholdPercent === usedThreshold) {
      setDraftFloor(remainingFloor);
      return;
    }
    persistPatch({ primaryThresholdPercent: usedThreshold, secondaryThresholdPercent: usedThreshold });
  }, [persistPatch, setDraftFloor, settings]);

  const handlePointerDown = useCallback((event: React.PointerEvent<HTMLElement>) => {
    if (busy === "save") return;
    event.currentTarget.setPointerCapture(event.pointerId);
    dragValue.current = floor;
  }, [busy, floor]);

  const handlePointerMove = useCallback((event: React.PointerEvent<HTMLElement>) => {
    if (dragValue.current === null) return;
    const next = floorFromPointer(event.clientX);
    if (next !== null && next !== dragValue.current) {
      dragValue.current = next;
      setDraftFloor(next);
    }
  }, [setDraftFloor, floorFromPointer]);

  const handlePointerUp = useCallback((event: React.PointerEvent<HTMLElement>) => {
    if (dragValue.current === null) return;
    const next = floorFromPointer(event.clientX) ?? dragValue.current;
    dragValue.current = null;
    persistFloor(next);
  }, [persistFloor, floorFromPointer]);

  const handleKeyDown = useCallback((event: React.KeyboardEvent<HTMLElement>) => {
    const delta = event.key === "ArrowRight" || event.key === "ArrowUp"
      ? 1
      : event.key === "ArrowLeft" || event.key === "ArrowDown" ? -1 : 0;
    if (delta === 0) return;
    event.preventDefault();
    if (busy === "save") return;
    persistFloor(clampRemainingFloor(floor + delta));
  }, [busy, persistFloor, floor]);

  if (!draft || !settings) {
    return (
      <main className="limiter-window limiter-window--loading" data-mode={windowMode}>
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
    setDraftWindowMode(windowMode);
    setDraftAlwaysOnTop(alwaysOnTop);
    setError(null);
    setNotice(null);
    setScreen("settings");
  };
  const cancelSettings = () => {
    setDraft(settings.quotaGuard);
    setDraftAppearance(appearance);
    setDraftWindowMode(windowMode);
    setDraftAlwaysOnTop(alwaysOnTop);
    setError(null);
    setScreen("monitor");
  };
  const saveSettings = async () => {
    const saved = await persistDraft(draft);
    if (!saved) return;
    setAppearance(draftAppearance);
    setWindowMode(draftWindowMode);
    setAlwaysOnTop(draftAlwaysOnTop);
    setScreen("monitor");
  };

  const armed = draft.armed !== false;

  const armedSwitch = (
    <label
      className="limiter-enabled-control"
      title={armed ? "Responses armed" : "Responses off — still tracking"}
    >
      <span className="reference-switch">
        <input
          type="checkbox"
          checked={armed}
          disabled={busy === "save"}
          onChange={(event) => persistPatch({ armed: event.target.checked })}
          aria-label={armed ? "Limiter armed" : "Limiter disarmed"}
        />
        <span aria-hidden="true" />
      </span>
    </label>
  );

  const settingsEnabledSwitch = (
    <label className="limiter-enabled-control">
      <span className="sr-only">{draft.enabled ? "Enabled" : "Disabled"}</span>
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
  );

  const usageBar = (
    <div
      className="limiter-progress"
      ref={barRef}
      style={barStyle}
      role="progressbar"
      aria-label="Current Codex usage"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={remaining}
    >
      <span className={`limiter-progress__fill${remaining <= floor ? " is-over" : ""}`} />
      <span
        className={`limiter-progress__handle${armed ? "" : " is-disarmed"}`}
        role="slider"
        tabIndex={armed ? 0 : -1}
        aria-label="Stop threshold"
        aria-disabled={!armed}
        aria-valuemin={1}
        aria-valuemax={99}
        aria-valuenow={floor}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={handlePointerUp}
        onKeyDown={handleKeyDown}
      >
        <span className="limiter-progress__knob" aria-hidden="true" />
        <small aria-hidden="true">{floor}%</small>
      </span>
    </div>
  );

  const usageValueClass = `limiter-usage__value${snapshotStale ? " is-stale" : ""}`;
  const resetText = activeWindow
    ? snapshotStale
      ? "Stale reading — refresh for current usage"
      : formatReset(activeWindow.resetsAt)
    : "No usage reading yet";
  const shortResetText = activeWindow
    ? snapshotStale ? "stale reading" : formatResetShort(activeWindow.resetsAt)
    : "no data yet";

  const showTitlebar = screen === "settings" || windowMode !== "pill";

  return (
    <main className="limiter-window" data-mode={windowMode} data-screen={screen}>
      {showTitlebar ? (
        <header className="limiter-titlebar" data-tauri-drag-region>
          <div className="limiter-titlebar__brand" data-tauri-drag-region>
            {screen === "monitor" ? (
              <>
                <Shield aria-hidden="true" />
                <span data-tauri-drag-region>
                  {windowMode === "mini" ? "Codex Usage" : "Codex Usage Limiter"}
                </span>
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
            {screen === "monitor" ? armedSwitch : null}
            {screen === "monitor" ? (
              <button type="button" aria-label="Open settings" onClick={openSettings}><Settings /></button>
            ) : null}
            <button type="button" aria-label="Minimize window" onClick={() => void appWindow().minimize()}><Minus /></button>
            <button type="button" aria-label="Close window" onClick={() => void appWindow().close()}><X /></button>
          </div>
        </header>
      ) : null}

      {screen === "monitor" ? (
        <div className="limiter-page limiter-monitor-page">
          <h1 className="sr-only">Current usage</h1>

          {windowMode === "compact" ? (
            <>
              <div className="limiter-compact-top">
                <strong className={usageValueClass}>{remaining}%</strong>
                <div className="limiter-compact-meta">
                  <span className={`limiter-status limiter-status--${statusTone}`}>
                    <span aria-hidden="true" />
                    <strong>{phaseLabel}</strong>
                  </span>
                  <p className="limiter-reset-caption">{resetText}</p>
                </div>
              </div>
              {usageBar}
              <div className="limiter-action-row">
                <span>Below {floor}%</span>
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
              </div>
              <footer className="limiter-compact-footer">
                <span className="limiter-last-checked">
                  {quotaGuard.state?.snapshotFresh ? "Last checked just now" : "Waiting for update"}
                </span>
                {workspaces.length === 0 ? (
                  <button type="button" onClick={() => void connectWorkspace()} disabled={busy !== null}>
                    <FolderOpen /> Connect
                  </button>
                ) : (
                  <button type="button" onClick={() => void refreshUsage()} disabled={busy !== null || !draft.enabled}>
                    <RefreshCw className={busy === "refresh" ? "limiter-spin" : ""} /> Refresh
                  </button>
                )}
              </footer>
            </>
          ) : null}

          {windowMode === "mini" ? (
            <>
              <div className="limiter-mini-top">
                <strong className={usageValueClass}>{remaining}%</strong>
                <span className="limiter-reset-caption">{resetText}</span>
              </div>
              {usageBar}
              <div className="limiter-mini-status">
                <span className={`limiter-status limiter-status--${statusTone}`}>
                  <span aria-hidden="true" />
                </span>
                <em>below {floor}%: {currentAction.shortLabel.toLowerCase()}</em>
              </div>
            </>
          ) : null}

          {windowMode === "pill" ? (
            <div
              className="limiter-pill"
              onMouseDown={(event) => {
                if (event.button !== 0) return;
                const target = event.target as HTMLElement;
                if (target.closest("button, input, label, .limiter-progress__handle")) return;
                void appWindow().startDragging();
              }}
            >
              <Shield className="limiter-pill__icon" aria-hidden="true" />
              <div className="limiter-pill__mid">
                <div className="limiter-pill__row">
                  <strong className={usageValueClass}>
                    {remaining}%<small>left</small>
                  </strong>
                  <span className="limiter-reset-caption">{shortResetText}</span>
                </div>
                {usageBar}
              </div>
              {armedSwitch}
              <button
                type="button"
                className="limiter-pill__settings"
                aria-label="Open settings"
                onClick={openSettings}
              >
                <Settings />
              </button>
            </div>
          ) : null}

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
              {settingsEnabledSwitch}
            </section>

            <section className="limiter-settings-row limiter-settings-row--threshold">
              <div className="limiter-setting-heading">
                <h2>Stop new work below</h2>
                <label className="limiter-percent-input">
                  <input
                    type="number"
                    min={1}
                    max={99}
                    disabled={busy === "save"}
                    value={floor}
                    aria-label="Stop new work percentage"
                    onChange={(event) => setDraftFloor(Number(event.target.value))}
                  />
                  <span>% left</span>
                </label>
              </div>
              <input
                className="limiter-threshold-range"
                type="range"
                min={1}
                max={99}
                disabled={busy === "save"}
                value={floor}
                style={{ "--range-progress": `${floor}%` } as CSSProperties}
                aria-label="Stop new work below"
                onChange={(event) => setDraftFloor(Number(event.target.value))}
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

            <section className="limiter-settings-row limiter-settings-row--window">
              <h2>Window size</h2>
              <div className="limiter-size-cards" role="group" aria-label="Window size">
                {windowModeOptions.map((option) => (
                  <button
                    type="button"
                    key={option.value}
                    className={`limiter-size-card${draftWindowMode === option.value ? " is-selected" : ""}`}
                    aria-pressed={draftWindowMode === option.value}
                    onClick={() => setDraftWindowMode(option.value)}
                  >
                    <span className={`limiter-size-card__pict limiter-size-card__pict--${option.value}`} aria-hidden="true"><i /></span>
                    <b>{option.label}</b>
                    <small>{option.dims}</small>
                  </button>
                ))}
              </div>
            </section>

            <section className="limiter-settings-row limiter-settings-row--foreground">
              <div>
                <h2>Keep in foreground</h2>
                <p>Stay above other windows</p>
              </div>
              <label className="limiter-enabled-control">
                <span className="reference-switch">
                  <input
                    type="checkbox"
                    checked={draftAlwaysOnTop}
                    onChange={(event) => setDraftAlwaysOnTop(event.target.checked)}
                    aria-label="Keep in foreground"
                  />
                  <span aria-hidden="true" />
                </span>
              </label>
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

            {workspaces.length === 0 ? (
              <section className="limiter-settings-row limiter-settings-row--workspace">
                <div>
                  <h2>Codex workspace</h2>
                  <p>Connect the folder where Codex runs to read usage</p>
                </div>
                <button
                  type="button"
                  className="limiter-button limiter-button--quiet"
                  onClick={() => void connectWorkspace()}
                  disabled={busy !== null}
                >
                  <FolderOpen /> Connect
                </button>
              </section>
            ) : null}

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
