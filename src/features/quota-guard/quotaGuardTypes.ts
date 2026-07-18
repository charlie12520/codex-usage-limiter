import type { RateLimitSnapshot } from "@/types";

export type QuotaGuardPhase =
  | "disabled"
  | "monitoring"
  | "revalidatingIdentity"
  | "closing"
  | "draining"
  | "awaitingDrainDecision"
  | "interrupting"
  | "parked"
  | "verifyingReset"
  | "ready"
  | "interventionRequired";

export type QuotaGuardWindowKind = "primary" | "secondary" | "hardLimit";

export type QuotaGuardActivityKind =
  | "stateChanged"
  | "notificationSent"
  | "notificationFailed"
  | "interruptRequested"
  | "interruptAcknowledged"
  | "interruptCompleted"
  | "monitorError";

export type QuotaGuardActivityEntry = {
  id: string | null;
  kind: QuotaGuardActivityKind;
  timestamp: number;
  operationId: string | null;
  workspaceId: string | null;
  threadId: string | null;
  turnId: string | null;
  attempt: number | null;
  message: string | null;
};

export type QuotaGuardTurn = {
  workspaceId: string;
  threadId: string;
  turnId: string;
};

export type AdmissionReason =
  | "open"
  | "guardDisabled"
  | "processClosed"
  | "epochUnverified"
  | "workspaceUnbound";

export type QuotaGuardAdmission = {
  sessionEpoch: string | null;
  open: boolean;
  reason: AdmissionReason;
};

export type QuotaGuardPublicState = {
  accountKey: string | null;
  accountLabel: string | null;
  phase: QuotaGuardPhase;
  snapshot: RateLimitSnapshot | null;
  snapshotFresh: boolean;
  breachedWindows: QuotaGuardWindowKind[];
  affectedTurns: QuotaGuardTurn[];
  drainDeadline: number | null;
  verifyAt: number | null;
  monitorHealthy: boolean;
  lastError: string | null;
  activity: QuotaGuardActivityEntry[];
  admissionByWorkspace: Record<string, QuotaGuardAdmission>;
};

export type QuotaGuardResolution = "disableGuard" | "retryClosed";

export type QueueDispatchOutcome = "accepted" | "quotaBlocked";

export const QUOTA_GUARD_BLOCKED_PREFIX = "QUOTA_GUARD_BLOCKED|";

export function isQuotaGuardBlockedError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error ?? "");
  return message.startsWith(QUOTA_GUARD_BLOCKED_PREFIX);
}

export function admissionForWorkspace(
  state: QuotaGuardPublicState | null,
  workspaceId: string | null,
): QuotaGuardAdmission {
  if (!workspaceId) {
    return { sessionEpoch: null, open: false, reason: "workspaceUnbound" };
  }
  const admission = state?.admissionByWorkspace?.[workspaceId];
  if (admission) {
    return admission;
  }
  if (state?.phase === "disabled") {
    return { sessionEpoch: null, open: true, reason: "guardDisabled" };
  }
  return { sessionEpoch: null, open: false, reason: "workspaceUnbound" };
}
