// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QuotaGuardController } from "./useQuotaGuardState";
import { useQuotaGuardState } from "./useQuotaGuardState";
import type { QuotaGuardPublicState } from "../quotaGuardTypes";
import {
  subscribeQuotaGuardOpenPanel,
  subscribeQuotaGuardStateChanged,
} from "@/services/events";
import { quotaGuardGetState } from "@/services/tauri";

vi.mock("@/services/events", () => ({
  subscribeQuotaGuardOpenPanel: vi.fn(),
  subscribeQuotaGuardStateChanged: vi.fn(),
}));

vi.mock("@/services/tauri", () => ({
  quotaGuardGetState: vi.fn(),
  quotaGuardApplyActionNow: vi.fn(),
  quotaGuardKeepWaiting: vi.fn(),
  quotaGuardInterruptNow: vi.fn(),
  quotaGuardResolveIntervention: vi.fn(),
  quotaGuardVerifyNow: vi.fn(),
}));
vi.mock("./useQuotaGuardNotificationActions", () => ({
  useQuotaGuardNotificationActions: vi.fn(),
}));

function quotaState(open: boolean): QuotaGuardPublicState {
  return {
    accountKey: "account",
    accountLabel: "Account",
    phase: open ? "ready" : "parked",
    snapshot: null,
    snapshotFresh: true,
    breachedWindows: [],
    affectedTurns: [],
    drainDeadline: null,
    verifyAt: null,
    monitorHealthy: true,
    lastError: null,
    activity: [],
    admissionByWorkspace: {
      workspace: { sessionEpoch: "epoch", open, reason: open ? "open" : "processClosed" },
    },
  };
}

function Harness({ onOpen, onController }: {
  onOpen: () => void;
  onController: (controller: QuotaGuardController) => void;
}) {
  onController(useQuotaGuardState(onOpen));
  return null;
}

let stateListener: ((state: QuotaGuardPublicState) => void) | null = null;
let openPanelListener: (() => void) | null = null;

beforeEach(() => {
  stateListener = null;
  openPanelListener = null;
  vi.mocked(quotaGuardGetState).mockResolvedValue(quotaState(false));
  vi.mocked(subscribeQuotaGuardStateChanged).mockImplementation((listener) => {
    stateListener = listener;
    return vi.fn();
  });
  vi.mocked(subscribeQuotaGuardOpenPanel).mockImplementation((listener) => {
    openPanelListener = listener;
    return vi.fn();
  });
});

afterEach(() => vi.clearAllMocks());

async function mount(onOpen: () => void) {
  let controller: QuotaGuardController | null = null;
  const container = document.createElement("div");
  const root = createRoot(container);
  await act(async () => {
    root.render(<Harness onOpen={onOpen} onController={(next) => { controller = next; }} />);
  });
  if (!controller) throw new Error("quota controller did not mount");
  return { controller: () => controller!, root };
}

describe("useQuotaGuardState", () => {
  it("keeps the shared queue latch through Ready and clears it only after admission opens", async () => {
    const { controller, root } = await mount(vi.fn());

    act(() => controller().requireQueueResume());
    expect(controller().queueResumeRequired).toBe(true);

    act(() => stateListener?.(quotaState(true)));
    expect(controller().queueResumeRequired).toBe(true);
    act(() => {
      expect(controller().resumeQueuedSends("workspace")).toBe(true);
    });
    expect(controller().queueResumeRequired).toBe(false);

    await act(async () => root.unmount());
  });

  it("does not clear the latch while the active workspace remains closed", async () => {
    const { controller, root } = await mount(vi.fn());

    act(() => controller().requireQueueResume());
    expect(controller().resumeQueuedSends("workspace")).toBe(false);
    expect(controller().queueResumeRequired).toBe(true);

    await act(async () => root.unmount());
  });

  it("routes notification panel events through the supplied central opener", async () => {
    const onOpen = vi.fn();
    const { root } = await mount(onOpen);

    act(() => openPanelListener?.());
    expect(onOpen).toHaveBeenCalledOnce();

    await act(async () => root.unmount());
  });
});
