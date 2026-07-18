import { describe, expect, it } from "vitest";
import type { QuotaGuardPublicState } from "./quotaGuardTypes";
import {
  admissionForWorkspace,
  isQuotaGuardBlockedError,
} from "./quotaGuardTypes";
import {
  formatQuotaGuardTimestamp,
  quotaGuardControls,
  quotaGuardPhaseSeverity,
} from "./quotaGuardViewModel";

function state(phase: QuotaGuardPublicState["phase"]): QuotaGuardPublicState {
  return {
    accountKey: "hashed",
    accountLabel: "Account",
    phase,
    snapshot: null,
    snapshotFresh: true,
    breachedWindows: ["primary"],
    affectedTurns: [],
    drainDeadline: null,
    verifyAt: null,
    monitorHealthy: true,
    lastError: null,
    activity: [],
    admissionByWorkspace: {},
  };
}

describe("quota guard view model", () => {
  it("exposes only phase-authorized actions", () => {
    expect(quotaGuardControls(state("monitoring")).applyActionNow).toBe(true);
    expect(quotaGuardControls(state("awaitingDrainDecision"))).toMatchObject({
      keepWaiting: true,
      interruptNow: true,
    });
    expect(quotaGuardControls(state("draining")).interruptNow).toBe(true);
    expect(quotaGuardControls(state("parked")).verifyNow).toBe(true);
    expect(quotaGuardControls(state("interventionRequired")).resolve).toBe(true);
  });

  it("keeps deadline and intervention controls constrained to backend phases", () => {
    expect(quotaGuardControls(state("verifyingReset"))).toMatchObject({
      verifyNow: true,
      interruptNow: false,
      resolve: false,
    });
    expect(quotaGuardControls(state("interventionRequired"))).toMatchObject({
      verifyNow: true,
      resolve: true,
      keepWaiting: false,
    });
  });

  it("fails closed for missing enabled-workspace admissions while preserving disabled openness", () => {
    const configuredState = state("parked");
    configuredState.admissionByWorkspace = {
      open: { sessionEpoch: "e1", open: true, reason: "open" },
      disabled: { sessionEpoch: "e2", open: true, reason: "guardDisabled" },
      closed: { sessionEpoch: "e3", open: false, reason: "processClosed" },
      unverified: { sessionEpoch: "e4", open: false, reason: "epochUnverified" },
      unbound: { sessionEpoch: null, open: false, reason: "workspaceUnbound" },
    };

    expect(admissionForWorkspace(configuredState, "open")).toMatchObject({
      open: true,
      reason: "open",
    });
    expect(admissionForWorkspace(configuredState, "disabled")).toMatchObject({
      open: true,
      reason: "guardDisabled",
    });
    expect(admissionForWorkspace(configuredState, "closed")).toMatchObject({
      open: false,
      reason: "processClosed",
    });
    expect(admissionForWorkspace(configuredState, "unverified")).toMatchObject({
      open: false,
      reason: "epochUnverified",
    });
    expect(admissionForWorkspace(configuredState, "unbound")).toMatchObject({
      open: false,
      reason: "workspaceUnbound",
    });
    expect(admissionForWorkspace(configuredState, "missing")).toEqual({
      sessionEpoch: null,
      open: false,
      reason: "workspaceUnbound",
    });
    expect(admissionForWorkspace(state("disabled"), "missing")).toEqual({
      sessionEpoch: null,
      open: true,
      reason: "guardDisabled",
    });
  });

  it("tolerates stale partial display fields without reopening admission", () => {
    const staleProjection = {
      phase: "monitoring",
      snapshotFresh: false,
      admissionByWorkspace: undefined,
    } as unknown as QuotaGuardPublicState;

    expect(quotaGuardControls(staleProjection).applyActionNow).toBe(false);
    expect(admissionForWorkspace(staleProjection, "workspace")).toMatchObject({
      open: false,
      reason: "workspaceUnbound",
    });
    expect(formatQuotaGuardTimestamp(undefined)).toBe("Not scheduled");
  });

  it("keeps intervention required ahead of ready in badge severity", () => {
    expect(quotaGuardPhaseSeverity("interventionRequired")).toBeLessThan(
      quotaGuardPhaseSeverity("ready"),
    );
  });

  it("recognizes only the stable backend blocked prefix", () => {
    expect(isQuotaGuardBlockedError("QUOTA_GUARD_BLOCKED|state=parked|verifyAt=1")).toBe(true);
    expect(isQuotaGuardBlockedError("quota guard blocked")).toBe(false);
  });
});
