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

const actionCopy = {
  notifyOnly: "Send a notification and keep everything running.",
  interrupt: "Freeze all Codex activity once, then switch off.",
  block: "Freeze everything and block new sessions until you switch off.",
} as const;

export function SettingsQuotaGuardSection({ appSettings, onUpdateAppSettings }: Props) {
  const quotaGuard = appSettings.quotaGuard;
  const update = (patch: Partial<AppSettings["quotaGuard"]>) => {
    void onUpdateAppSettings({ ...appSettings, quotaGuard: { ...quotaGuard, ...patch } });
  };
  const remoteIncompatible = appSettings.backendMode === "remote";
  return (
    <SettingsSection title="Quota guard" subtitle="Pause local Codex turns when an account quota window reaches its limit.">
      <SettingsToggleRow title="Enable quota guard" subtitle="Applies only to local app-server sessions launched by this app.">
        <SettingsToggleSwitch pressed={quotaGuard.enabled} disabled={remoteIncompatible} onClick={() => update({ enabled: !quotaGuard.enabled })} />
      </SettingsToggleRow>
      {remoteIncompatible ? <div className="settings-help" role="alert">Quota guard is unavailable while the remote backend is selected.</div> : null}
      <div className="settings-divider" />
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-primary-threshold">Primary threshold (%)</label>
        <input id="quota-primary-threshold" className="settings-input" type="number" min={0} max={100} value={quotaGuard.primaryThresholdPercent} onChange={(event) => update({ primaryThresholdPercent: Number(event.target.value) })} />
      </div>
      <div className="settings-field">
        <label className="settings-field-label" htmlFor="quota-secondary-threshold">Secondary threshold (%)</label>
        <input id="quota-secondary-threshold" className="settings-input" type="number" min={0} max={100} value={quotaGuard.secondaryThresholdPercent} onChange={(event) => update({ secondaryThresholdPercent: Number(event.target.value) })} />
      </div>
      <div className="settings-field">
        <span className="settings-field-label">When reached</span>
        <div className="limiter-segmented" aria-label="When reached">
          {(["notifyOnly", "interrupt", "block"] as const).map((action) => (
            <button key={action} type="button" className={quotaGuard.action === action ? "is-selected" : ""} onClick={() => update({ action })}>
              {{ notifyOnly: "Notify", interrupt: "Interrupt", block: "Block" }[action]}
            </button>
          ))}
        </div>
        <div className="settings-help">{actionCopy[quotaGuard.action]}</div>
      </div>
      <div className="settings-help">Interrupt and Block freeze every Codex app instantly, but a reply already generating on the server still finishes and counts toward usage.</div>
    </SettingsSection>
  );
}
