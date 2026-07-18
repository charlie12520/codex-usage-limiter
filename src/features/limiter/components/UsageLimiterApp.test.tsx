// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QuotaGuardPublicState } from "@/features/quota-guard/quotaGuardTypes";
import { useQuotaGuardState } from "@/features/quota-guard/hooks/useQuotaGuardState";
import {
  getAppSettings,
  listWorkspaces,
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
  listWorkspaces: vi.fn(),
  pickWorkspacePath: vi.fn(),
  updateAppSettings: vi.fn(),
}));

const appSettings = {
  quotaGuard: {
    enabled: true,
    armed: true,
    primaryThresholdPercent: 90,
    secondaryThresholdPercent: 90,
    action: "notifyOnly",
    drainTimeoutMinutes: 15,
    drainTimeoutAction: "interrupt",
    resetGraceMinutes: 10,
    notifyWhenAvailable: true,
  },
} as AppSettings;

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
  drainDeadline: null,
  verifyAt: null,
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
  vi.mocked(getAppSettings).mockResolvedValue(appSettings);
  vi.mocked(listWorkspaces).mockResolvedValue([workspace]);
  vi.mocked(updateAppSettings).mockImplementation(async (settings) => settings);
  vi.mocked(useQuotaGuardState).mockReturnValue({
    state: publicState,
    queueResumeRequired: false,
    applyActionNow: vi.fn(),
    keepWaiting: vi.fn(),
    interruptNow: vi.fn(),
    verifyNow: vi.fn(),
    resolveIntervention: vi.fn(),
    resumeQueuedSends: vi.fn(),
    requireQueueResume: vi.fn(),
  });
});

describe("UsageLimiterApp", () => {
  it("projects monitoring, usage, threshold, response, and workspace in the compact dashboard", async () => {
    render(<UsageLimiterApp />);

    expect(await screen.findByRole("heading", { name: "Current usage" })).toBeTruthy();
    expect(screen.getByText("Monitoring")).toBeTruthy();
    expect(screen.getByRole("progressbar", { name: "Current Codex usage" }).getAttribute("aria-valuenow")).toBe("37");
    expect((screen.getByRole("combobox", { name: "When limit is reached" }) as HTMLSelectElement).value).toBe("notifyOnly");
    expect(screen.getByText("Below 10%")).toBeTruthy();
    expect(screen.getByText("Last checked just now")).toBeTruthy();
  });

  it("stages a window size change and applies it on save", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.click(screen.getByRole("button", { name: /320/ }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.getByText("Codex Usage")).toBeTruthy());
    expect(localStorage.getItem("codex-usage-limiter.windowMode")).toBe("mini");
  });

  it("stages compact settings and saves response, threshold, and appearance together", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    expect(screen.getByText("Settings")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Finish turn" }));
    fireEvent.change(screen.getByRole("spinbutton", { name: "Trigger percentage" }), {
      target: { value: "25" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Dark" }));
    expect(updateAppSettings).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(updateAppSettings).toHaveBeenCalledOnce());
    const updated = vi.mocked(updateAppSettings).mock.calls[0]?.[0].quotaGuard;
    expect(updated.action).toBe("finishCurrentTurn");
    expect(updated.primaryThresholdPercent).toBe(75);
    expect(updated.secondaryThresholdPercent).toBe(75);
    await waitFor(() => expect(document.documentElement.dataset.appearance).toBe("dark"));
    expect(screen.getByRole("heading", { name: "Current usage" })).toBeTruthy();
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
    fireEvent.click(screen.getByRole("button", { name: "Finish turn" }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    expect(screen.getByRole("button", { name: "Notify" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Finish turn" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Interrupt" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Save changes" }).hasAttribute("disabled")).toBe(true);

    resolveUpdate({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, action: "finishCurrentTurn" },
    });
    await waitFor(() => expect(screen.getByRole("heading", { name: "Current usage" })).toBeTruthy());
  });

  it("cancels staged settings without writing them", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.click(screen.getByRole("button", { name: "Finish turn" }));
    fireEvent.change(screen.getByRole("spinbutton", { name: "Trigger percentage" }), {
      target: { value: "82" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    expect(updateAppSettings).not.toHaveBeenCalled();
    expect((screen.getByRole("combobox", { name: "When limit is reached" }) as HTMLSelectElement).value).toBe("notifyOnly");
    expect(screen.getByText("Below 10%")).toBeTruthy();
  });

  it("keeps settings open and restores persisted values when save fails", async () => {
    vi.mocked(updateAppSettings).mockRejectedValueOnce(new Error("save rejected"));
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.click(screen.getByRole("button", { name: "Finish turn" }));
    fireEvent.click(screen.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("save rejected"));
    expect(screen.getByRole("button", { name: "Notify" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "Finish turn" }).getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByText("Settings")).toBeTruthy();
  });

  it("marks the usage reading as stale when the snapshot is no longer fresh", async () => {
    vi.mocked(useQuotaGuardState).mockReturnValue({
      state: { ...publicState, snapshotFresh: false },
      queueResumeRequired: false,
      applyActionNow: vi.fn(),
      keepWaiting: vi.fn(),
      interruptNow: vi.fn(),
      verifyNow: vi.fn(),
      resolveIntervention: vi.fn(),
      resumeQueuedSends: vi.fn(),
      requireQueueResume: vi.fn(),
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    expect(screen.getByText("Stale reading — refresh for current usage")).toBeTruthy();
    expect(screen.getByText("37%").className).toContain("is-stale");
  });

  it("clamps the staged threshold to at least 1 percent when the field is cleared", async () => {
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });
    fireEvent.click(screen.getByRole("button", { name: "Open settings" }));

    fireEvent.change(screen.getByRole("spinbutton", { name: "Trigger percentage" }), {
      target: { value: "" },
    });

    expect((screen.getByRole("spinbutton", { name: "Trigger percentage" }) as HTMLInputElement).value).toBe("1");
  });

  it("restores the persisted armed toggle when its immediate update fails", async () => {
    vi.mocked(updateAppSettings).mockRejectedValueOnce(new Error("save rejected"));
    render(<UsageLimiterApp />);
    const toggle = await screen.findByRole("checkbox", { name: "Limiter armed" });

    fireEvent.click(toggle);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("save rejected"));
    expect((screen.getByRole("checkbox", { name: "Limiter armed" }) as HTMLInputElement).checked).toBe(true);
  });

  it("greys out and locks the threshold grabber while disarmed", async () => {
    vi.mocked(getAppSettings).mockResolvedValue({
      ...appSettings,
      quotaGuard: { ...appSettings.quotaGuard, armed: false },
    });
    render(<UsageLimiterApp />);
    await screen.findByRole("heading", { name: "Current usage" });

    const handle = screen.getByRole("slider", { name: "Trigger threshold" });
    expect(handle.className).toContain("is-disarmed");
    expect(handle.getAttribute("aria-disabled")).toBe("true");
    expect(handle.getAttribute("tabindex")).toBe("-1");
  });
});
