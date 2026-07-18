import type { AppSettings } from "@/types";
import {
  SettingsSection,
  SettingsToggleRow,
  SettingsToggleSwitch,
} from "@/features/design-system/components/settings/SettingsPrimitives";

type Props = {
  appSettings: AppSettings;
  onUpdateAppSettings: (next: AppSettings) => Promise<void>;
};

export function SettingsQuotaGuardSection({ appSettings, onUpdateAppSettings }: Props) {
  const quotaGuard = appSettings.quotaGuard;
  const update = (patch: Partial<AppSettings["quotaGuard"]>) => {
    void onUpdateAppSettings({
      ...appSettings,
      quotaGuard: { ...quotaGuard, ...patch },
    });
  };
  const remoteIncompatible = appSettings.backendMode === "remote";
  return (
    <SettingsSection
      title="Quota guard"
      subtitle="Pause local Codex turns when an account quota window reaches its limit."
    >
      <SettingsToggleRow
        title="Enable quota guard"
        subtitle="Applies only to local app-server sessions launched by this app."
      >
        <SettingsToggleSwitch
          pressed={quotaGuard.enabled}
          disabled={remoteIncompatible}
          onClick={() => update({ enabled: !quotaGuard.enabled })}
        />
      </SettingsToggleRow>
      {remoteIncompatible ? (
        <div className="settings-help" role="alert">
          Quota guard is unavailable while the remote backend is selected.
        </div>
      ) : null}
      <div className="settings-divider" />
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-primary-threshold">Primary threshold (%)</label>
        <input
          id="quota-primary-threshold"
          className="settings-input"
          type="number"
          min={0}
          max={100}
          value={quotaGuard.primaryThresholdPercent}
          onChange={(event) => update({ primaryThresholdPercent: Number(event.target.value) })}
        />
      </div>
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-secondary-threshold">Secondary threshold (%)</label>
        <input
          id="quota-secondary-threshold"
          className="settings-input"
          type="number"
          min={0}
          max={100}
          value={quotaGuard.secondaryThresholdPercent}
          onChange={(event) => update({ secondaryThresholdPercent: Number(event.target.value) })}
        />
      </div>
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-action">When a threshold is reached</label>
        <select id="quota-action" className="settings-select" value={quotaGuard.action} onChange={(event) => update({ action: event.target.value as AppSettings["quotaGuard"]["action"] })}>
          <option value="notifyOnly">Notify only</option>
          <option value="interruptImmediately">Interrupt immediately</option>
          <option value="finishCurrentTurn">Finish current turn</option>
        </select>
      </div>
      {quotaGuard.action === "finishCurrentTurn" ? (
        <>
          <div className="settings-field">
            <label className="settings-field-label" htmlFor="quota-drain-timeout">Drain timeout (minutes)</label>
            <input id="quota-drain-timeout" className="settings-input" type="number" min={1} max={1440} value={quotaGuard.drainTimeoutMinutes} onChange={(event) => update({ drainTimeoutMinutes: Number(event.target.value) })} />
          </div>
          <div className="settings-field">
            <label className="settings-field-label" htmlFor="quota-drain-action">At drain timeout</label>
            <select id="quota-drain-action" className="settings-select" value={quotaGuard.drainTimeoutAction} onChange={(event) => update({ drainTimeoutAction: event.target.value as AppSettings["quotaGuard"]["drainTimeoutAction"] })}>
              <option value="notifyAndHold">Notify and hold</option>
              <option value="interrupt">Interrupt remaining turns</option>
            </select>
          </div>
        </>
      ) : null}
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-reset-grace">Reset grace (minutes)</label>
        <input id="quota-reset-grace" className="settings-input" type="number" min={0} max={1440} value={quotaGuard.resetGraceMinutes} onChange={(event) => update({ resetGraceMinutes: Number(event.target.value) })} />
      </div>
      <SettingsToggleRow title="Notify when available" subtitle="Send a notification after a verified quota reset.">
        <SettingsToggleSwitch pressed={quotaGuard.notifyWhenAvailable} onClick={() => update({ notifyWhenAvailable: !quotaGuard.notifyWhenAvailable })} />
      </SettingsToggleRow>
    </SettingsSection>
  );
}
