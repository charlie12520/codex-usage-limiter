// @vitest-environment jsdom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  isQuotaGuardNotificationAction,
  QUOTA_GUARD_NOTIFICATION_ACTION_TYPE,
  QUOTA_GUARD_NOTIFICATION_OPEN_ACTION,
  useQuotaGuardNotificationActions,
} from "./useQuotaGuardNotificationActions";
import { isMobileRuntime } from "@/services/tauri";
import { onAction, registerActionTypes } from "@tauri-apps/plugin-notification";
import type { PluginListener } from "@tauri-apps/api/core";

vi.mock("@/services/tauri", () => ({
  isMobileRuntime: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-notification", () => ({
  onAction: vi.fn(),
  registerActionTypes: vi.fn(),
}));

function Harness({ onOpen }: { onOpen: () => void }) {
  useQuotaGuardNotificationActions(onOpen);
  return null;
}

let actionListener: Parameters<typeof onAction>[0] | null = null;
let unregister: () => Promise<void>;

beforeEach(() => {
  actionListener = null;
  unregister = vi.fn(() => Promise.resolve());
  vi.mocked(isMobileRuntime).mockResolvedValue(true);
  vi.mocked(onAction).mockImplementation(async (listener) => {
    actionListener = listener;
    return {
      plugin: "notification",
      event: "actionPerformed",
      channelId: 1,
      unregister,
    } satisfies PluginListener;
  });
  vi.mocked(registerActionTypes).mockResolvedValue();
});

afterEach(() => vi.clearAllMocks());

async function mount(onOpen: () => void) {
  const container = document.createElement("div");
  const root = createRoot(container);
  await act(async () => {
    root.render(<Harness onOpen={onOpen} />);
    await Promise.resolve();
    await Promise.resolve();
  });
  return root;
}

describe("useQuotaGuardNotificationActions", () => {
  it("registers the named quota action on mobile before handling activations", async () => {
    const root = await mount(vi.fn());

    expect(registerActionTypes).toHaveBeenCalledWith([
      {
        id: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE,
        actions: [{ id: QUOTA_GUARD_NOTIFICATION_OPEN_ACTION, title: "Open usage limiter" }],
      },
    ]);

    await act(async () => root.unmount());
  });

  it("opens the quota panel only for a named action or default tap on a tagged notification", async () => {
    const onOpen = vi.fn();
    const root = await mount(onOpen);

    act(() => {
      actionListener?.({
        actionId: QUOTA_GUARD_NOTIFICATION_OPEN_ACTION,
        notification: { actionTypeId: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE },
      } as never);
    });
    expect(onOpen).toHaveBeenCalledOnce();

    act(() => {
      actionListener?.({
        actionId: "tap",
        notification: { actionTypeId: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE },
      } as never);
    });
    expect(onOpen).toHaveBeenCalledTimes(2);

    act(() => {
      actionListener?.({
        actionId: "tap",
        notification: { actionTypeId: "other-notification" },
      } as never);
      actionListener?.({
        actionId: "dismiss",
        notification: { actionTypeId: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE },
      } as never);
    });
    expect(onOpen).toHaveBeenCalledTimes(2);

    await act(async () => root.unmount());
  });

  it("requires the plugin activation payload rather than treating notification data as an open request", () => {
    expect(isQuotaGuardNotificationAction({
      actionId: "tap",
      notification: { actionTypeId: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE },
    })).toBe(true);
    expect(isQuotaGuardNotificationAction({
      notification: { actionTypeId: QUOTA_GUARD_NOTIFICATION_ACTION_TYPE },
    })).toBe(false);
    expect(isQuotaGuardNotificationAction({
      actionId: "tap",
      notification: { actionTypeId: "other-notification" },
    })).toBe(false);
  });
});
