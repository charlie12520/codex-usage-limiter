import type { QuotaGuardPhase, QuotaGuardPublicState } from "./quotaGuardTypes";

const PHASE_ORDER: readonly QuotaGuardPhase[] = [
  "interventionRequired",
  "closing",
  "interrupting",
  "awaitingDrainDecision",
  "draining",
  "parked",
  "verifyingReset",
  "revalidatingIdentity",
  "ready",
  "monitoring",
  "disabled",
];

const PHASE_LABELS: Record<QuotaGuardPhase, string> = {
  disabled: "Disabled",
  monitoring: "Monitoring",
  revalidatingIdentity: "Checking account",
  closing: "Closing admission",
  draining: "Finishing current turns",
  awaitingDrainDecision: "Drain decision needed",
  interrupting: "Interrupting turns",
  parked: "Parked until reset",
  verifyingReset: "Verifying reset",
  ready: "Ready",
  interventionRequired: "Intervention required",
};

export type QuotaGuardControls = {
  applyActionNow: boolean;
  keepWaiting: boolean;
  interruptNow: boolean;
  verifyNow: boolean;
  resolve: boolean;
};

export function quotaGuardPhaseLabel(phase: QuotaGuardPhase): string {
  return PHASE_LABELS[phase];
}

export function quotaGuardPhaseSeverity(phase: QuotaGuardPhase): number {
  return PHASE_ORDER.indexOf(phase);
}

export function quotaGuardControls(state: QuotaGuardPublicState | null): QuotaGuardControls {
  if (!state) {
    return {
      applyActionNow: false,
      keepWaiting: false,
      interruptNow: false,
      verifyNow: false,
      resolve: false,
    };
  }
  const breachedWindows = state.breachedWindows ?? [];
  return {
    applyActionNow:
      state.phase === "monitoring" && state.snapshotFresh && breachedWindows.length > 0,
    keepWaiting: state.phase === "awaitingDrainDecision",
    interruptNow:
      state.phase === "awaitingDrainDecision" || state.phase === "draining",
    verifyNow:
      state.phase === "parked" ||
      state.phase === "verifyingReset" ||
      (state.phase === "interventionRequired" && state.accountKey !== null),
    resolve: state.phase === "interventionRequired",
  };
}

export function formatQuotaGuardTimestamp(value: number | null | undefined): string {
  return value == null ? "Not scheduled" : new Date(value).toLocaleString();
}
