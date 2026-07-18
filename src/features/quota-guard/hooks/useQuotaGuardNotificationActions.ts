import { useEffect } from "react";
import {
  onAction,
  registerActionTypes,
  type ActionType,
} from "@tauri-apps/plugin-notification";
import { isMobileRuntime } from "@/services/tauri";

export const QUOTA_GUARD_NOTIFICATION_ACTION_TYPE = "codex-usage-limiter.quota-guard";
export const QUOTA_GUARD_NOTIFICATION_OPEN_ACTION = "open-quota-guard";

const quotaGuardActionTypes: ActionType[] = [
  {
    id: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE,
    actions: [{ id: QUOTA_GUARD_NOTIFICATION_OPEN_ACTION, title: "Open usage limiter" }],
  },
];

type NotificationActionPayload = {
  actionId?: unknown;
  notification?: {
    actionTypeId?: unknown;
  };
};

function isNotificationActionPayload(value: unknown): value is NotificationActionPayload {
  return typeof value === "object" && value !== null;
}

/**
 * Accepts only an activation of a quota-guard notification.
 *
 * Android and iOS report a default notification press as `tap`; named action
 * presses use the registered action identifier. `onAction` is never a delivery
 * callback, so neither notification creation nor delivery can open the panel.
 */
export function isQuotaGuardNotificationAction(payload: unknown): boolean {
  if (!isNotificationActionPayload(payload)) return false;

  const { actionId, notification } = payload;
  return (
    notification?.actionTypeId === QUOTA_GUARD_NOTIFICATION_ACTION_TYPE &&
    (actionId === QUOTA_GUARD_NOTIFICATION_OPEN_ACTION || actionId === "tap")
  );
}

export function useQuotaGuardNotificationActions(onOpenPanel?: () => void) {
  useEffect(() => {
    let active = true;
    let unregister: (() => void) | undefined;

    void onAction((payload) => {
      if (active && isQuotaGuardNotificationAction(payload)) {
        onOpenPanel?.();
      }
    }).then((listener) => {
      if (active) {
        unregister = () => void listener.unregister();
      } else {
        void listener.unregister();
      }
    });

    void isMobileRuntime().then((mobileRuntime) => {
      if (active && mobileRuntime) {
        return registerActionTypes(quotaGuardActionTypes);
      }
      return undefined;
    });

    return () => {
      active = false;
      unregister?.();
    };
  }, [onOpenPanel]);
}
