import ShieldAlert from "lucide-react/dist/esm/icons/shield-alert";
import type { QuotaGuardPublicState } from "../quotaGuardTypes";
import { quotaGuardPhaseLabel } from "../quotaGuardViewModel";

type Props = {
  state: QuotaGuardPublicState | null;
  onOpen?: () => void;
};

export function QuotaGuardStatusBadge({ state, onOpen }: Props) {
  const phase = state?.phase;
  const label = phase ? quotaGuardPhaseLabel(phase) : "Unavailable";
  return (
    <button
      type="button"
      className={`ghost sidebar-labeled-button quota-guard-status phase-${phase ?? "unavailable"}`}
      onClick={onOpen}
      aria-label={`Quota guard: ${label}`}
    >
      <ShieldAlert aria-hidden size={15} />
      <span>Quota: {label}</span>
    </button>
  );
}
