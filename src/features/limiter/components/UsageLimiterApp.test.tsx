// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QuotaGuardPublicState } from "@/features/quota-guard/quotaGuardTypes";
import { useQuotaGuardState } from "@/features/quota-guard/hooks/useQuotaGuardState";
import {
  getAppSettings,
  getAutostart,
  getLimiterBootScreen,
  listWorkspaces,
  setAutostart,
  setTrayUsageTooltip,
  updateAppSettings,
} from "@/services/tauri";
import type { AppSettings, WorkspaceInfo } from "@/types";
import { UsageLimiterApp } from "./UsageLimiterApp";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    close: vi.fn(),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    setSize: vi.fn(),
    setMinSize: vi.fn(),
    setResizable: vi.fn(),
    setAlwaysOnTop: vi.fn(),
  }),
}));

vi.mock("@tauri-apps/api/dpi", () => ({
  LogicalSize: class MockLogicalSize {
    width: number;
    height: number;
    constructor(width: number, height: number) {
      this.width = width;
      this.height = height;
    }
  },
}));

vi.mock("@/features/quota-guard/hooks/useQuotaGuardState", () => ({
  useQuotaGuardState: vi.fn(),
}));

vi.mock("@/services/tauri", () => ({
  addWorkspace: vi.fn(),
  getAppSettings: vi.fn(),
  getAutostart: vi.fn(),
  getLimiterBootScreen: vi.fn(),
  listWorkspaces: vi.fn(),
  pickWorkspacePath: vi.fn(),
  setAutostart: vi.fn(),
  setTrayUsageTooltip: vi.fn(),
  updateAppSettings: vi.fn(),
}));

const appSettings = {
  quotaGuard: {
    enabled: true,
    armed: true,
    primaryThresholdPercent: 90,
    secondaryThresholdPercent: 90,
    action: "notifyOnly",
  },
} as unknown as AppSettings;

const workspace = {
  id: "workspace-1",
  name: "Limiter project",
  path: "C:/work/limiter",
  connected: true,
  settings: { sidebarCollapsed: false },
} satisfies WorkspaceInfo;

const publicState: QuotaGuardPublicState = {
  accountKey: "account",
  accountLabel: "user@example.com",
  phase: "monitoring",
  snapshot: {
    primary: { usedPercent: 63, windowDurationMins: 300, resetsAt: 1_900_000_000 },
    secondary: { usedPercent: 28, windowDurationMins: 10_080, resetsAt: 1_900_100_000 },
    credits: null,
    planType: "pro",
    observedAt: 1_800_000_000,
  },
  snapshotFresh: true,
  breachedWindows: [],
  affectedTurns: [],
  monitorHealthy: true,
  lastError: null,
  activity: [],
  admissionByWorkspace: {
    "workspace-1": { sessionEpoch: "epoch-1", open: true, reason: "open" },
  },
};

afterEach(() => {
  cleanup();
  localStorage.clear();
  delete document.documentElement.dataset.appearance;
});

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.setItem("codex-usage-limiter.windowMode", "compact");
  vi.mocked(getAppSettings).mockResolvedValue(appSettings);
  vi.mocked(getAutostart).mockResolvedValue(false);
  vi.mocked(getLimiterBootScreen).mockResolvedValue(null);
  vi.mocked(setAutostart).mockResolvedValue(undefined);
  vi.mocked(setTrayUsageTooltip).mockResolvedValue(undefined);
  vi.mocked(listWorkspaces).mockResolvedValue([workspace]);
  vi.mocked(updateAppSettings).mockImplementation(async (settings) => settings);
  vi.mocked(useQuotaGuardState).mockReturnValue({
    state: publicState,
    queueResumeRequired: false,
    applyActionNow: vi.fn(),
    rearm: vi.fn(),
    resume: vi.fn(),
    resolveIntervention: vi.fn(),
    resumeQueuedSends: vi.fn(),
    requireQueueResume: vi.fn(),
  });
});

describe("UsageLimiterApp", () => {
  it("opens the requested valid boot screen once without persisting it", async () => {
    vi.mocked(getLimiterBootScreen).mockResolvedValue("settings");
    render(<UsageLimiterApp />);

    await screen.findByRole("heading", { name: "When reached" });
    expect(document.querySelector("main")?.dataset.screen).toBe("settings");
    expect(getLimiterBootScreen).toHaveBeenCalledOnce();
    expect(updateAppSettings).not.toHaveBeenCalled();
  });

  it("ignores an invalid boot-screen response", async () => {
    vi.mocked(getLimiterBootScreen).mockResolvedValue("invalid" as never);
    render(<UsageLimiterApp />);

    await screen.findByRole("heading", { name: "Current usage" });
    expect(document.querySelector("main")?.dataset.screen).toBe("monitor");
  });

  it("defaults fresh installs to the pill window", async () => {
    localStorage.removeItem("codex-usage-limiter.windowMode");
    render(<UsageLimiterApp />);

    await screen.findByRole("button", { name: "Open settings" });
    expect(document.querySelector("main")?.dataset.mode).toBe("pill");
    expect(localStorage.getItem("codex-usage-limiter.windowMode")).toBe("pill");
  });

  it("projects monitoring, usage, threshold, response, and workspace in the compact dashboard", async () => {
    render(<UsageLimiterApp />);

    expect(await screen.findByRole("heading", { name: "Current usage" })).toBeTruthy();
    expect(screen.getByText("Monitoring")).toBeTruthy();
    expect(screen.getByRole("progressbar", { name: "Current Codex usage" }).getAttribute("aria-valuenow")).toBe("37");
    expect((screen.getByRole("combobox", { name: "When limit is reached" }) as HTMLSelectElement).value).toBe("notifyOnly");
    expect(screen.getByText("At 10%")).toBeTruthy();
    expect(document.querySelector(".limiter-compact-footer")).toBeNull();
  });

  it("keeps the compact Connect action when no workspace is attached", async () => {
    vi.mocked(listWorkspaces).mockResolvedValue([]);
    render(<UsageLimiterApp />);

    expect(await screen.findByRole("button", { name: "Connect" })).toBeTruthy();
  });

  it("stages the autostart toggle and applies it on save", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.click(screen.getByRole("checkbox", { name: "Start at login" }));
    expect(setAutostart).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(setAutostart).toHaveBeenCalledWith(true));
  });

  it("pushes the usage summary into the tray tooltip", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    await waitFor(() => expect(setTrayUsageTooltip).toHaveBeenCalled());
    const calls = vi.mocked(setTrayUsageTooltip).mock.calls;
    expect(calls[calls.length - 1]?.[0]).toContain("37% left");
  });

  it("stages a window size change and applies it on save", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.click(screen.getByRole("button", { name: /320/ }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.getByText("Codex Usage")).toBeTruthy());
    await waitFor(() => expect(localStorage.getItem("codex-usage-limiter.windowMode")).toBe("mini"));
  });

  it("stages response and appearance in the rebuilt settings sheet", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    expect(screen.getByText("Limit", { selector: ".limiter-settings-group" })).toBeTruthy();
    expect(screen.queryByText("Type a number to turn this on; clear it to turn it off.")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Interrupt" }));
    fireEvent.click(screen.getByRole("button", { name: "Dark" }));
    expect(updateAppSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(updateAppSettings).toHaveBeenCalledOnce());
    const updated = vi.mocked(updateAppSettings).mock.calls[0]?.[0].quotaGuard;
    expect(updated.action).toBe("interrupt");
    expect(updated.primaryThresholdPercent).toBe(90);
    expect(updated.secondaryThresholdPercent).toBe(90);
    await waitFor(() => expect(document.documentElement.dataset.appearance).toBe("dark"));
    expect(screen.getByRole("heading", { name: "Current usage" })).toBeTruthy();
  });

  it("keeps group labels outside their cards and settings controls inside rows", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    const limitGroup = screen.getByText("Limit", { selector: ".limiter-settings-group" });
    expect(limitGroup.closest("section")).toBeNull();
    const limitCard = screen.getByRole("heading", { name: "When reached" }).closest(".limiter-settings-card");
    expect(limitCard?.tagName).toBe("SECTION");
    expect(screen.getByRole("heading", { name: "When reached" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Notify" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "Interrupt" }).getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByRole("button", { name: "Block" }).getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByText("Send a notification and keep everything running.")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Block" }));
    expect(screen.getByText("Freeze everything and block new sessions until you switch off.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Block" }).className).toContain("is-selected");
    expect(screen.getByText(/420/)).toBeTruthy();
    expect(screen.getByText(/320/)).toBeTruthy();
    expect(screen.getByText(/280/)).toBeTruthy();
    expect(screen.getByText(/Interrupt and Block freeze every Codex app instantly/)).toBeTruthy();
  });

  it("disables settings controls while one settings write is pending", async () => {
    let resolveUpdate: (value: AppSettings) => void = () => undefined;
    const pendingUpdate = new Promise<AppSettings>((resolve) => {
      resolveUpdate = resolve;
    });
    vi.mocked(updateAppSettings).mockReturnValueOnce(pendingUpdate);

    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.click(screen.getByRole("button", { name: "Interrupt" }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    expect(screen.getByRole("button", { name: "Notify" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Interrupt" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Save changes" }).hasAttribute("disabled")).toBe(true);

    resolveUpdate({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, action: "interrupt" },
    });
    await waitFor(() => expect(screen.getByRole("heading", { name: "Current usage" })).toBeTruthy());
  });

  it("cancels staged settings without writing them", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.click(screen.getByRole("button", { name: "Interrupt" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Rearm after reset at % left" }), {
      target: { value: "82" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    expect(updateAppSettings).not.toHaveBeenCalled();
    expect((screen.getByRole("combobox", { name: "When limit is reached" }) as HTMLSelectElement).value).toBe("notifyOnly");
    expect(screen.getByText("At 10%")).toBeTruthy();
  });

  it("keeps settings open and restores persisted values when save fails", async () => {
    vi.mocked(updateAppSettings).mockRejectedValueOnce(new Error("save rejected"));
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.click(screen.getByRole("button", { name: "Interrupt" }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("save rejected"));
    expect(screen.getByRole("button", { name: "Notify" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "Interrupt" }).getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByText("Limit", { selector: ".limiter-settings-group" })).toBeTruthy();
  });

  it("marks the usage reading as stale when the snapshot is no longer fresh", async () => {
    vi.mocked(useQuotaGuardState).mockReturnValue({
      state: { ...publicState, snapshotFresh: false },
      queueResumeRequired: false,
      applyActionNow: vi.fn(),
      rearm: vi.fn(),
      resume: vi.fn(),
      resolveIntervention: vi.fn(),
      resumeQueuedSends: vi.fn(),
      requireQueueResume: vi.fn(),
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    expect(screen.getByText("Stale reading — waiting for the next update")).toBeTruthy();
    expect(screen.getByText("37%").className).toContain("is-stale");
  });

  it("clears the rearm setting immediately", async () => {
    vi.mocked(getAppSettings).mockResolvedValue({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, rearmAfterResetPercentLeft: 40 },
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.change(screen.getByRole("textbox", { name: "Rearm after reset at % left" }), {
      target: { value: "" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    await waitFor(() => expect(updateAppSettings).toHaveBeenCalledWith(expect.objectContaining({
      quotaGuard: expect.objectContaining({ rearmAfterResetPercentLeft: null }),
    })));
  });

  it("restores the persisted armed toggle when its immediate update fails", async () => {
    vi.mocked(updateAppSettings).mockRejectedValueOnce(new Error("save rejected"));
    render(<UsageLimiterApp />);
    const toggle = await screen.findByRole("checkbox", { name: "Limiter armed" });

    fireEvent.click(toggle);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("save rejected"));
    expect((screen.getByRole("checkbox", { name: "Limiter armed" }) as HTMLInputElement).checked).toBe(true);
  });

  it("greys out the full usage control while keeping the threshold grabber operable when disarmed", async () => {
    vi.mocked(getAppSettings).mockResolvedValue({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, armed: false },
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    const progress = screen.getByRole("progressbar", { name: "Current Codex usage" });
    const fill = progress.querySelector(".limiter-progress__fill");
    const handle = screen.getByRole("slider", { name: "Trigger threshold" });

    expect(progress.className).toContain("is-disarmed");
    expect(fill?.className).toContain("limiter-progress__fill");
    expect(handle.className).toContain("is-disarmed");
    expect(handle.getAttribute("aria-disabled")).toBe("false");
    expect(handle.getAttribute("tabindex")).toBe("0");
  });

  it("persists a threshold drag while disarmed", async () => {
    vi.mocked(getAppSettings).mockResolvedValue({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, armed: false },
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    const progress = screen.getByRole("progressbar", { name: "Current Codex usage" });
    Object.defineProperty(progress, "clientWidth", { configurable: true, value: 199 });
    vi.spyOn(progress, "getBoundingClientRect").mockReturnValue({
      x: 0, y: 0, top: 0, left: 0, bottom: 12, right: 200, width: 200, height: 12, toJSON: () => ({}),
    });
    fireEvent(window, new Event("resize"));
    await waitFor(() => expect(progress.style.getPropertyValue("--threshold-position")).toBe("20px"));
    const handle = screen.getByRole("slider", { name: "Trigger threshold" });
    Object.defineProperty(handle, "setPointerCapture", { value: vi.fn() });

    fireEvent.pointerDown(handle, { pointerId: 1, clientX: 20 });
    fireEvent.pointerMove(handle, { pointerId: 1, clientX: 50 });
    fireEvent.pointerUp(handle, { pointerId: 1, clientX: 50 });

    await waitFor(() => expect(updateAppSettings).toHaveBeenCalledWith(expect.objectContaining({
      quotaGuard: expect.objectContaining({
        armed: false,
        primaryThresholdPercent: 75,
        secondaryThresholdPercent: 75,
      }),
    })));
  });
});
