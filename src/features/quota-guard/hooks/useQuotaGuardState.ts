import { useCallback, useEffect, useMemo, useState } from "react";
import {
  quotaGuardApplyActionNow,
  quotaGuardGetState,
  quotaGuardInterruptNow,
  quotaGuardKeepWaiting,
  quotaGuardResolveIntervention,
  quotaGuardVerifyNow,
} from "@/services/tauri";
import { subscribeQuotaGuardOpenPanel, subscribeQuotaGuardStateChanged } from "@/services/events";
import { admissionForWorkspace, type QuotaGuardPublicState, type QuotaGuardResolution } from "../quotaGuardTypes";
import { useQuotaGuardNotificationActions } from "./useQuotaGuardNotificationActions";

export type QuotaGuardController = {
  state: QuotaGuardPublicState | null;
  queueResumeRequired: boolean;
  applyActionNow: () => Promise<QuotaGuardPublicState>;
  keepWaiting: () => Promise<QuotaGuardPublicState>;
  interruptNow: () => Promise<QuotaGuardPublicState>;
  verifyNow: () => Promise<QuotaGuardPublicState>;
  resolveIntervention: (resolution: QuotaGuardResolution) => Promise<QuotaGuardPublicState>;
  resumeQueuedSends: (workspaceId: string | null) => boolean;
  requireQueueResume: () => void;
};
export function useQuotaGuardState(onOpenPanel?: () => void) {
  useQuotaGuardNotificationActions(onOpenPanel);

  const [state, setState] = useState<QuotaGuardPublicState | null>(null);
  const [queueResumeRequired, setQueueResumeRequired] = useState(false);

  useEffect(() => {
    let active = true;
    void quotaGuardGetState().then(
      (next) => {
        if (active) setState(next);
      },
      () => {
        if (active) setState(null);
      },
    );
    const unsubscribeState = subscribeQuotaGuardStateChanged(setState);
    const openPanel = () => onOpenPanel?.();
    const unsubscribeOpen = subscribeQuotaGuardOpenPanel(openPanel);
    window.addEventListener("quota-guard-open-panel", openPanel);
    return () => {
      active = false;
      unsubscribeState();
      unsubscribeOpen();
      window.removeEventListener("quota-guard-open-panel", openPanel);
    };
  }, [onOpenPanel]);
  const update = useCallback(async (action: () => Promise<QuotaGuardPublicState>) => {
    const next = await action();
    setState(next);
    return next;
  }, []);

  const resumeQueuedSends = useCallback(
    (workspaceId: string | null) => {
      if (!admissionForWorkspace(state, workspaceId).open) {
        return false;
      }
      setQueueResumeRequired(false);
      return true;
    },
    [state],
  );
  const requireQueueResume = useCallback(() => setQueueResumeRequired(true), []);

  const actions = useMemo(
    () => ({
      applyActionNow: () => update(quotaGuardApplyActionNow),
      keepWaiting: () => update(quotaGuardKeepWaiting),
      interruptNow: () => update(quotaGuardInterruptNow),
      verifyNow: () => update(quotaGuardVerifyNow),
      resolveIntervention: (resolution: QuotaGuardResolution) =>
        update(() => quotaGuardResolveIntervention(resolution)),
      resumeQueuedSends,
      requireQueueResume,
    }),
    [requireQueueResume, resumeQueuedSends, update],
  );

  return { state, queueResumeRequired, ...actions } satisfies QuotaGuardController;
}
