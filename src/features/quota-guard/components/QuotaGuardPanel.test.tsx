// @vitest-environment jsdom
import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { QuotaGuardPublicState } from "../quotaGuardTypes";
import { QuotaGuardPanel } from "./QuotaGuardPanel";

describe("QuotaGuardPanel", () => {
  it("renders and closes for a stale partial Tauri projection", () => {
    const onClose = vi.fn();
    const staleProjection = {
      phase: "disabled",
      accountKey: null,
      accountLabel: null,
      snapshot: null,
      snapshotFresh: false,
      monitorHealthy: false,
      lastError: null,
    } as unknown as QuotaGuardPublicState;

    render(
      <QuotaGuardPanel
        activeWorkspaceId="workspace"
        state={staleProjection}
        queueResumeRequired={false}
        onClose={onClose}
        onApplyActionNow={async () => staleProjection}
        onResolve={async () => staleProjection}
        onResumeQueuedSends={vi.fn()}
      />,
    );

    expect(screen.getByRole("dialog", { name: "Quota guard" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Close quota guard" }));
    expect(onClose).toHaveBeenCalledOnce();
    expect(screen.getByText("workspace · Open · guardDisabled")).toBeTruthy();
  });

  it("renders configured workspace admission statuses from the backend projection", () => {
    const state = {
      accountKey: "account",
      accountLabel: "Account",
      phase: "tripped",
      snapshot: null,
      snapshotFresh: true,
      breachedWindows: ["primary"],
      affectedTurns: [],
      monitorHealthy: true,
      lastError: null,
      activity: [],
      admissionByWorkspace: {
        open: { sessionEpoch: "e1", open: true, reason: "open" },
        disabled: { sessionEpoch: "e2", open: true, reason: "guardDisabled" },
        closed: { sessionEpoch: "e3", open: false, reason: "processClosed" },
        unverified: { sessionEpoch: "e4", open: false, reason: "epochUnverified" },
        unbound: { sessionEpoch: null, open: false, reason: "workspaceUnbound" },
      },
    } satisfies QuotaGuardPublicState;

    render(
      <QuotaGuardPanel
        activeWorkspaceId="missing"
        state={state}
        queueResumeRequired
        onClose={vi.fn()}
        onApplyActionNow={async () => state}
        onResolve={async () => state}
        onResumeQueuedSends={vi.fn()}
      />,
    );

    expect(screen.getByText("missing · Closed · workspaceUnbound")).toBeTruthy();
    expect(screen.getByText("open · Open · open")).toBeTruthy();
    expect(screen.getByText("disabled · Open · guardDisabled")).toBeTruthy();
    expect(screen.getByText("closed · Closed · processClosed")).toBeTruthy();
    expect(screen.getByText("unverified · Closed · epochUnverified")).toBeTruthy();
    expect(screen.getByText("unbound · Closed · workspaceUnbound")).toBeTruthy();
  });

  it("requests durable disable without opening optimistically and reports Disabled from the backend state", () => {
    const intervention = {
      accountKey: "account",
      accountLabel: "Account",
      phase: "interventionRequired",
      snapshot: null,
      snapshotFresh: false,
      breachedWindows: [],
      affectedTurns: [],
      monitorHealthy: false,
      lastError: "Needs reconciliation",
      activity: [],
      admissionByWorkspace: {},
    } satisfies QuotaGuardPublicState;
    const disabled = {
      ...intervention,
      phase: "disabled",
      monitorHealthy: true,
      lastError: null,
    } satisfies QuotaGuardPublicState;
    const onResolve = vi.fn(async () => disabled);
    const { container, rerender } = render(
      <QuotaGuardPanel
        activeWorkspaceId="workspace"
        state={intervention}
        queueResumeRequired={false}
        onClose={vi.fn()}
        onApplyActionNow={async () => intervention}
        onResolve={onResolve}
        onResumeQueuedSends={vi.fn()}
      />,
    );
    const panel = within(container);

    expect(panel.queryByRole("button", { name: "Rearm" })).toBeNull();

    fireEvent.click(panel.getByRole("button", { name: "Disable guard and open" }));
    expect(onResolve).toHaveBeenCalledWith("disableGuard");
    expect(panel.getByText("Intervention required")).toBeTruthy();

    rerender(
      <QuotaGuardPanel
        activeWorkspaceId="workspace"
        state={disabled}
        queueResumeRequired={false}
        onClose={vi.fn()}
        onApplyActionNow={async () => intervention}
        onResolve={onResolve}
        onResumeQueuedSends={vi.fn()}
      />,
    );

    expect(panel.getByText("Disabled")).toBeTruthy();
    expect(panel.queryByRole("button", { name: "Disable guard and open" })).toBeNull();

    rerender(
      <QuotaGuardPanel
        activeWorkspaceId="workspace"
        state={{ ...intervention, phase: "tripped" }}
        queueResumeRequired={false}
        onClose={vi.fn()}
        onApplyActionNow={async () => intervention}
        onResolve={onResolve}
        onResumeQueuedSends={vi.fn()}
      />,
    );

    expect(panel.queryByRole("button", { name: "Rearm" })).toBeNull();
    expect(panel.queryByRole("button", { name: "Verify now" })).toBeNull();
    expect(panel.queryByRole("button", { name: "Disable guard and open" })).toBeNull();
  });
});
