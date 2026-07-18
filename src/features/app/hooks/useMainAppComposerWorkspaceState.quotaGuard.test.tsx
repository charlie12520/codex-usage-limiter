// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { QuotaGuardController } from "@/features/quota-guard/hooks/useQuotaGuardState";
import type { QuotaGuardPublicState } from "@/features/quota-guard/quotaGuardTypes";
import { useMainAppComposerWorkspaceState } from "./useMainAppComposerWorkspaceState";

let quotaBlocked: (() => void) | null = null;
let queueFlushPaused: boolean | null = null;

vi.mock("@app/hooks/useComposerController", () => ({
  useComposerController: (options: {
    onQuotaGuardBlocked: () => void;
    queueFlushPaused: boolean;
  }) => {
    quotaBlocked = options.onQuotaGuardBlocked;
    queueFlushPaused = options.queueFlushPaused;
    return {
      activeDraft: "",
      handleDraftChange: vi.fn(),
      resumeQueuedSends: vi.fn(),
    };
  },
}));
vi.mock("@app/hooks/useWorkspaceFileListing", () => ({
  useWorkspaceFileListing: () => ({ files: [], isLoading: false, setFileAutocompleteActive: vi.fn() }),
}));
vi.mock("@/features/workspaces/hooks/useWorkspaceHome", () => ({
  useWorkspaceHome: () => ({ draft: "", setDraft: vi.fn() }),
}));
vi.mock("@app/hooks/useComposerInsert", () => ({ useComposerInsert: () => vi.fn() }));
vi.mock("@/features/workspaces/hooks/useWorkspaceAgentMd", () => ({ useWorkspaceAgentMd: () => ({}) }));

function quotaState(): QuotaGuardPublicState {
  return {
    accountKey: "account",
    accountLabel: "Account",
    phase: "parked",
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
      workspace: { sessionEpoch: "epoch", open: false, reason: "processClosed" },
    },
  };
}

function Harness({ guard, openPanel }: { guard: QuotaGuardController; openPanel: () => void }) {
  useMainAppComposerWorkspaceState({
    view: {
      centerMode: "chat",
      isCompact: false,
      isTablet: false,
      activeTab: "codex",
      tabletTab: "codex",
      filePanelMode: "files",
      rightPanelCollapsed: false,
    },
    workspace: {
      activeWorkspace: null,
      activeWorkspaceId: "workspace",
      isNewAgentDraftMode: false,
      startingDraftThreadWorkspaceId: null,
      threadsByWorkspace: {},
    },
    thread: {
      activeThreadId: null,
      activeItems: [],
      activeTurnIdByThread: {},
      threadStatusById: {},
      userInputRequests: [],
    },
    settings: {
      steerEnabled: false,
      followUpMessageBehavior: "queue",
      experimentalAppsEnabled: false,
      pauseQueuedMessagesWhenResponseRequired: false,
    },
    quota: { guard, openPanel },
    models: {
      models: [],
      selectedModelId: null,
      resolvedEffort: null,
      selectedServiceTier: null,
      collaborationModePayload: null,
    },
    refs: { composerInputRef: { current: null }, workspaceHomeTextareaRef: { current: null } },
    actions: {
      addWorktreeAgent: vi.fn(),
      connectWorkspace: vi.fn(),
      startThreadForWorkspace: vi.fn(),
      sendUserMessage: vi.fn(),
      sendUserMessageToThread: vi.fn(),
      seedThreadCodexParams: vi.fn(),
      startQueuedFork: vi.fn(),
      startQueuedReview: vi.fn(),
      startResume: vi.fn(),
      startQueuedCompact: vi.fn(),
      startQueuedNew: vi.fn(),
      startApps: vi.fn(),
      startMcp: vi.fn(),
      startFast: vi.fn(),
      startStatus: vi.fn(),
      addDebugEntry: vi.fn(),
    },
  });
  return null;
}

describe("useMainAppComposerWorkspaceState quota guard wiring", () => {
  beforeEach(() => {
    quotaBlocked = null;
    queueFlushPaused = null;
  });

  afterEach(() => vi.clearAllMocks());

  it("uses the orchestration controller and central modal opener for a quota-blocked send", async () => {
    const requireQueueResume = vi.fn();
    const openPanel = vi.fn();
    const guard: QuotaGuardController = {
      state: quotaState(),
      queueResumeRequired: false,
      applyActionNow: vi.fn(),
      keepWaiting: vi.fn(),
      interruptNow: vi.fn(),
      verifyNow: vi.fn(),
      resolveIntervention: vi.fn(),
      resumeQueuedSends: vi.fn(),
      requireQueueResume,
    };
    const root = createRoot(document.createElement("div"));

    await act(async () => {
      root.render(<Harness guard={guard} openPanel={openPanel} />);
    });
    requireQueueResume.mockClear();

    act(() => quotaBlocked?.());
    expect(requireQueueResume).toHaveBeenCalledOnce();
    expect(openPanel).toHaveBeenCalledOnce();

    await act(async () => root.unmount());
  });

  it("keeps queues open for an explicit disabled projection without a workspace entry", async () => {
    const guard: QuotaGuardController = {
      state: { ...quotaState(), phase: "disabled", admissionByWorkspace: {} },
      queueResumeRequired: false,
      applyActionNow: vi.fn(),
      keepWaiting: vi.fn(),
      interruptNow: vi.fn(),
      verifyNow: vi.fn(),
      resolveIntervention: vi.fn(),
      resumeQueuedSends: vi.fn(),
      requireQueueResume: vi.fn(),
    };
    const root = createRoot(document.createElement("div"));

    await act(async () => {
      root.render(<Harness guard={guard} openPanel={vi.fn()} />);
    });

    expect(queueFlushPaused).toBe(false);
    await act(async () => root.unmount());
  });

  it("fails closed for an enabled projection missing the active workspace admission", async () => {
    const guard: QuotaGuardController = {
      state: { ...quotaState(), phase: "monitoring", admissionByWorkspace: {} },
      queueResumeRequired: false,
      applyActionNow: vi.fn(),
      keepWaiting: vi.fn(),
      interruptNow: vi.fn(),
      verifyNow: vi.fn(),
      resolveIntervention: vi.fn(),
      resumeQueuedSends: vi.fn(),
      requireQueueResume: vi.fn(),
    };
    const root = createRoot(document.createElement("div"));

    await act(async () => {
      root.render(<Harness guard={guard} openPanel={vi.fn()} />);
    });

    expect(queueFlushPaused).toBe(true);
    await act(async () => root.unmount());
  });
});
