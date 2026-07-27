import type { QuotaGuardPhase, QuotaGuardPublicState } from "./quotaGuardTypes";

const PHASE_ORDER: readonly QuotaGuardPhase[] = [
  "interventionRequired",
  "tripped",
  "monitoring",
  "disabled",
];

const PHASE_LABELS: Record<QuotaGuardPhase, string> = {
  disabled: "Disabled",
  monitoring: "Monitoring",
  tripped: "Frozen",
  interventionRequired: "Intervention required",
};

export type QuotaGuardControls = {
  applyActionNow: boolean;
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
      resolve: false,
    };
  }
  const breachedWindows = state.breachedWindows ?? [];
  return {
    applyActionNow:
      state.phase === "monitoring" && state.snapshotFresh && breachedWindows.length > 0,
    resolve: state.phase === "interventionRequired",
  };
}

export function formatQuotaGuardTimestamp(value: number | null | undefined): string {
  return value == null ? "Not scheduled" : new Date(value).toLocaleString();
}
