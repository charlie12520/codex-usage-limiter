import X from "lucide-react/dist/esm/icons/x";
import { ModalShell } from "@/features/design-system/components/modal/ModalShell";
import {
  admissionForWorkspace,
  type QuotaGuardPublicState,
  type QuotaGuardResolution,
} from "../quotaGuardTypes";
import { formatQuotaGuardTimestamp, quotaGuardControls, quotaGuardPhaseLabel } from "../quotaGuardViewModel";
import { QuotaGuardDrainDecision } from "./QuotaGuardDrainDecision";

type Props = {
  activeWorkspaceId: string | null;
  state: QuotaGuardPublicState | null;
  queueResumeRequired: boolean;
  onClose: () => void;
  onApplyActionNow: () => Promise<unknown>;
  onKeepWaiting: () => Promise<unknown>;
  onInterruptNow: () => Promise<unknown>;
  onVerifyNow: () => Promise<unknown>;
  onResolve: (resolution: QuotaGuardResolution) => Promise<unknown>;
  onResumeQueuedSends: () => void;
};

export function QuotaGuardPanel({
  activeWorkspaceId,
  state,
  queueResumeRequired,
  onClose,
  onApplyActionNow,
  onKeepWaiting,
  onInterruptNow,
  onVerifyNow,
  onResolve,
  onResumeQueuedSends,
}: Props) {
  const breachedWindows = state?.breachedWindows ?? [];
  const affectedTurns = state?.affectedTurns ?? [];
  const activity = state?.activity ?? [];
  const activeAdmission = admissionForWorkspace(state, activeWorkspaceId);
  const workspaceAdmissions = activeWorkspaceId
    ? [
        { workspaceId: activeWorkspaceId, admission: activeAdmission },
        ...Object.entries(state?.admissionByWorkspace ?? {})
          .filter(([workspaceId]) => workspaceId !== activeWorkspaceId)
          .map(([workspaceId, admission]) => ({ workspaceId, admission })),
      ]
    : Object.entries(state?.admissionByWorkspace ?? {}).map(
        ([workspaceId, admission]) => ({ workspaceId, admission }),
      );
  const controls = quotaGuardControls(
    state ? { ...state, breachedWindows } : null,
  );
  const canResumeQueue = queueResumeRequired && activeAdmission.open;
  const requestDurableDisable = () => {
    void onResolve("disableGuard");
  };

  return (
    <ModalShell ariaLabel="Quota guard" cardClassName="settings-modal-card">
      <header className="settings-modal-header">
        <div>
          <div className="settings-modal-title">Quota guard</div>
          <div className="settings-modal-subtitle">
            {state ? quotaGuardPhaseLabel(state.phase) : "Loading quota guard status"}
          </div>
        </div>
        <button type="button" className="ghost icon-button" onClick={onClose} aria-label="Close quota guard">
          <X aria-hidden />
        </button>
      </header>
      <div className="settings-modal-content quota-guard-panel">
        {state ? (
          <>
            <dl className="quota-guard-details">
              <div><dt>Account</dt><dd>{state.accountLabel ?? "Not verified"}</dd></div>
              <div><dt>Observed</dt><dd>{state.snapshotFresh ? "Fresh" : "Stale or unavailable"}</dd></div>
              <div><dt>Breaches</dt><dd>{breachedWindows.join(", ") || "None"}</dd></div>
              <div><dt>Drain deadline</dt><dd>{formatQuotaGuardTimestamp(state.drainDeadline)}</dd></div>
              <div><dt>Verification</dt><dd>{formatQuotaGuardTimestamp(state.verifyAt)}</dd></div>
              <div><dt>Monitor</dt><dd>{state.monitorHealthy ? "Healthy" : state.lastError ?? "Needs attention"}</dd></div>
            </dl>
            <div className="settings-field">
              <div className="settings-field-label">Workspace admission</div>
              {workspaceAdmissions.length === 0 ? (
                <div className="settings-help">No configured workspaces.</div>
              ) : (
                workspaceAdmissions.map(({ workspaceId, admission }) => (
                  <div key={workspaceId} className="settings-help">
                    {workspaceId} · {admission.open ? "Open" : "Closed"} · {admission.reason}
                  </div>
                ))
              )}
            </div>
            {state.snapshot ? (
              <div className="settings-help">
                Primary {state.snapshot.primary?.usedPercent ?? "--"}% · Secondary {state.snapshot.secondary?.usedPercent ?? "--"}%
              </div>
            ) : null}
            {affectedTurns.length > 0 ? (
              <div className="settings-field">
                <div className="settings-field-label">Affected turns</div>
                {affectedTurns.map((turn) => <div key={`${turn.workspaceId}:${turn.threadId}:${turn.turnId}`} className="settings-help">{turn.workspaceId} · {turn.threadId} · {turn.turnId}</div>)}
              </div>
            ) : null}
            {controls.applyActionNow ? <button type="button" className="primary" onClick={() => void onApplyActionNow()}>Apply action now</button> : null}
            {controls.keepWaiting ? <QuotaGuardDrainDecision onKeepWaiting={() => void onKeepWaiting()} onInterruptNow={() => void onInterruptNow()} /> : null}
            {controls.interruptNow && !controls.keepWaiting ? <button type="button" className="danger" onClick={() => void onInterruptNow()}>Interrupt now</button> : null}
            {controls.verifyNow ? <button type="button" className="ghost" onClick={() => void onVerifyNow()}>Verify now</button> : null}
            {controls.resolve ? <div className="modal-actions"><button type="button" className="danger" onClick={requestDurableDisable}>Disable guard and open</button><button type="button" className="ghost" onClick={() => void onResolve("retryClosed")}>Keep closed and retry</button></div> : null}
            {canResumeQueue ? <button type="button" className="primary" onClick={onResumeQueuedSends}>Resume queued sends</button> : null}
            <div className="settings-field">
              <div className="settings-field-label">Activity</div>
              {activity.length === 0 ? <div className="settings-help">No quota guard activity yet.</div> : activity.map((entry) => <div key={entry.id ?? `${entry.timestamp}-${entry.kind}`} className="settings-help">{new Date(entry.timestamp).toLocaleString()} · {entry.kind}{entry.message ? ` · ${entry.message}` : ""}</div>)}
            </div>
          </>
        ) : <div className="settings-help">Quota guard state is unavailable.</div>}
      </div>
    </ModalShell>
  );
}
