import { useCallback, useEffect, useMemo, useState } from "react";
import type {
  AppMention,
  ComposerSendIntent,
  FollowUpMessageBehavior,
  QueuedMessage,
  SendMessageResult,
  WorkspaceInfo,
} from "@/types";
import {
  isQuotaGuardBlockedError,
  type QueueDispatchOutcome,
} from "@/features/quota-guard/quotaGuardTypes";

type UseQueuedSendOptions = {
  activeThreadId: string | null;
  activeTurnId: string | null;
  isProcessing: boolean;
  isReviewing: boolean;
  queueFlushPaused?: boolean;
  steerEnabled: boolean;
  followUpMessageBehavior: FollowUpMessageBehavior;
  appsEnabled: boolean;
  activeWorkspace: WorkspaceInfo | null;
  connectWorkspace: (workspace: WorkspaceInfo) => Promise<void>;
  sendUserMessage: (
    text: string,
    images?: string[],
    appMentions?: AppMention[],
    options?: { sendIntent?: ComposerSendIntent },
  ) => Promise<SendMessageResult>;
  startQueuedFork: (text: string) => Promise<QueueDispatchOutcome>;
  startQueuedReview: (text: string) => Promise<QueueDispatchOutcome>;
  startResume: (text: string) => Promise<void>;
  startQueuedCompact: (text: string) => Promise<QueueDispatchOutcome>;
  startQueuedNew: (text: string) => Promise<QueueDispatchOutcome>;
  startApps: (text: string) => Promise<void>;
  startMcp: (text: string) => Promise<void>;
  startFast: (text: string) => Promise<void>;
  startStatus: (text: string) => Promise<void>;
  onQuotaGuardBlocked?: () => void;
  clearActiveImages: () => void;
};

type UseQueuedSendResult = {
  queuedByThread: Record<string, QueuedMessage[]>;
  activeQueue: QueuedMessage[];
  handleSend: (
    text: string,
    images?: string[],
    appMentions?: AppMention[],
    submitIntent?: ComposerSendIntent,
  ) => Promise<void>;
  queueMessage: (
    text: string,
    images?: string[],
    appMentions?: AppMention[],
  ) => Promise<void>;
  removeQueuedMessage: (threadId: string, messageId: string) => void;
  resumeQueuedSends: () => void;
};

type SlashCommandKind =
  | "apps"
  | "compact"
  | "fast"
  | "fork"
  | "mcp"
  | "new"
  | "resume"
  | "review"
  | "status";

function parseSlashCommand(text: string, appsEnabled: boolean): SlashCommandKind | null {
  if (appsEnabled && /^\/apps\b/i.test(text)) {
    return "apps";
  }
  if (/^\/fork\b/i.test(text)) {
    return "fork";
  }
  if (/^\/fast\b/i.test(text)) {
    return "fast";
  }
  if (/^\/mcp\b/i.test(text)) {
    return "mcp";
  }
  if (/^\/review\b/i.test(text)) {
    return "review";
  }
  if (/^\/compact\b/i.test(text)) {
    return "compact";
  }
  if (/^\/new\b/i.test(text)) {
    return "new";
  }
  if (/^\/resume\b/i.test(text)) {
    return "resume";
  }
  if (/^\/status\b/i.test(text)) {
    return "status";
  }
  return null;
}

export function useQueuedSend({
  activeThreadId,
  activeTurnId,
  isProcessing,
  isReviewing,
  queueFlushPaused = false,
  steerEnabled,
  followUpMessageBehavior,
  appsEnabled,
  activeWorkspace,
  connectWorkspace,
  sendUserMessage,
  startQueuedFork,
  startQueuedReview,
  startResume,
  startQueuedCompact,
  startQueuedNew,
  startApps,
  startMcp,
  startFast,
  startStatus,
  onQuotaGuardBlocked,
  clearActiveImages,
}: UseQueuedSendOptions): UseQueuedSendResult {
  const [queuedByThread, setQueuedByThread] = useState<
    Record<string, QueuedMessage[]>
  >({});
  const [inFlightByThread, setInFlightByThread] = useState<
    Record<string, QueuedMessage | null>
  >({});
  const [quotaBlockedPause, setQuotaBlockedPause] = useState(false);
  const [hasStartedByThread, setHasStartedByThread] = useState<
    Record<string, boolean>
  >({});

  const activeQueue = useMemo(
    () => (activeThreadId ? queuedByThread[activeThreadId] ?? [] : []),
    [activeThreadId, queuedByThread],
  );

  const enqueueMessage = useCallback((threadId: string, item: QueuedMessage) => {
    setQueuedByThread((prev) => ({
      ...prev,
      [threadId]: [...(prev[threadId] ?? []), item],
    }));
  }, []);

  const removeQueuedMessage = useCallback(
    (threadId: string, messageId: string) => {
      setQueuedByThread((prev) => ({
        ...prev,
        [threadId]: (prev[threadId] ?? []).filter(
          (entry) => entry.id !== messageId,
        ),
      }));
    },
    [],
  );

  const prependQueuedMessage = useCallback((threadId: string, item: QueuedMessage) => {
    setQueuedByThread((prev) => ({
      ...prev,
      [threadId]: [item, ...(prev[threadId] ?? [])],
    }));
  }, []);

  const createQueuedItem = useCallback(
    (text: string, images: string[], appMentions: AppMention[]): QueuedMessage => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      text,
      createdAt: Date.now(),
      images,
      ...(appMentions.length > 0 ? { appMentions } : {}),
    }),
    [],
  );

  const runSlashCommand = useCallback(
    async (command: SlashCommandKind, trimmed: string): Promise<QueueDispatchOutcome> => {
      if (command === "fork") {
        return startQueuedFork(trimmed);
      }
      if (command === "review") {
        return startQueuedReview(trimmed);
      }
      if (command === "resume") {
        await startResume(trimmed);
      } else if (command === "compact") {
        return startQueuedCompact(trimmed);
      } else if (command === "apps") {
        await startApps(trimmed);
      } else if (command === "mcp") {
        await startMcp(trimmed);
      } else if (command === "fast") {
        await startFast(trimmed);
      } else if (command === "status") {
        await startStatus(trimmed);
      } else if (command === "new") {
        return startQueuedNew(trimmed);
      }
      return "accepted";
    },
    [
      startQueuedFork,
      startQueuedReview,
      startResume,
      startQueuedCompact,
      startQueuedNew,
      startApps,
      startMcp,
      startFast,
      startStatus,
    ],
  );

  const handleSend = useCallback(
    async (
      text: string,
      images: string[] = [],
      appMentions: AppMention[] = [],
      submitIntent: ComposerSendIntent = "default",
    ) => {
      const trimmed = text.trim();
      const command = parseSlashCommand(trimmed, appsEnabled);
      const nextImages = command ? [] : images;
      const nextMentions = command ? [] : appMentions;
      const canSteerCurrentTurn =
        isProcessing && steerEnabled && Boolean(activeTurnId);
      const effectiveIntent: ComposerSendIntent = !isProcessing
        ? "default"
        : submitIntent === "queue"
          ? "queue"
          : submitIntent === "steer"
            ? canSteerCurrentTurn
              ? "steer"
              : "queue"
            : followUpMessageBehavior === "steer" && canSteerCurrentTurn
              ? "steer"
              : "queue";
      if (!trimmed && nextImages.length === 0) {
        return;
      }
      if (activeThreadId && isReviewing) {
        return;
      }
      if (isProcessing && activeThreadId && effectiveIntent === "queue") {
        const item = createQueuedItem(trimmed, nextImages, nextMentions);
        enqueueMessage(activeThreadId, item);
        clearActiveImages();
        return;
      }
      if (activeWorkspace && !activeWorkspace.connected) {
        await connectWorkspace(activeWorkspace);
      }
      if (command) {
        const outcome = await runSlashCommand(command, trimmed);
        if (outcome === "quotaBlocked" && activeThreadId) {
          enqueueMessage(activeThreadId, createQueuedItem(trimmed, nextImages, nextMentions));
          setQuotaBlockedPause(true);
          onQuotaGuardBlocked?.();
        }
        clearActiveImages();
        return;
      }
      const sendResult =
        nextMentions.length > 0
          ? await sendUserMessage(trimmed, nextImages, nextMentions, {
            sendIntent: effectiveIntent,
          })
          : await sendUserMessage(trimmed, nextImages, undefined, {
          sendIntent: effectiveIntent,
          });
      if (sendResult.status === "blocked" && sendResult.reason === "quotaGuard") {
        if (activeThreadId) {
          enqueueMessage(activeThreadId, createQueuedItem(trimmed, nextImages, nextMentions));
        }
        setQuotaBlockedPause(true);
        onQuotaGuardBlocked?.();
      } else if (
        sendResult.status === "steer_failed" &&
        activeThreadId &&
        isProcessing
      ) {
        enqueueMessage(activeThreadId, createQueuedItem(trimmed, nextImages, nextMentions));
      }
      clearActiveImages();
    },
    [
      activeThreadId,
      appsEnabled,
      activeWorkspace,
      clearActiveImages,
      connectWorkspace,
      createQueuedItem,
      enqueueMessage,
      activeTurnId,
      followUpMessageBehavior,
      isProcessing,
      isReviewing,
      steerEnabled,
      runSlashCommand,
      sendUserMessage,
      onQuotaGuardBlocked,
    ],
  );

  const queueMessage = useCallback(
    async (
      text: string,
      images: string[] = [],
      appMentions: AppMention[] = [],
    ) => {
      const trimmed = text.trim();
      const command = parseSlashCommand(trimmed, appsEnabled);
      const nextImages = command ? [] : images;
      const nextMentions = command ? [] : appMentions;
      if (!trimmed && nextImages.length === 0) {
        return;
      }
      if (activeThreadId && isReviewing) {
        return;
      }
      if (!activeThreadId) {
        return;
      }
      const item = createQueuedItem(trimmed, nextImages, nextMentions);
      enqueueMessage(activeThreadId, item);
      clearActiveImages();
    },
    [
      activeThreadId,
      appsEnabled,
      clearActiveImages,
      createQueuedItem,
      enqueueMessage,
      isReviewing,
    ],
  );

  useEffect(() => {
    if (!activeThreadId) {
      return;
    }
    const inFlight = inFlightByThread[activeThreadId];
    if (!inFlight) {
      return;
    }
    if (isProcessing || isReviewing) {
      if (!hasStartedByThread[activeThreadId]) {
        setHasStartedByThread((prev) => ({
          ...prev,
          [activeThreadId]: true,
        }));
      }
      return;
    }
    if (hasStartedByThread[activeThreadId]) {
      setInFlightByThread((prev) => ({ ...prev, [activeThreadId]: null }));
      setHasStartedByThread((prev) => ({ ...prev, [activeThreadId]: false }));
    }
  }, [
    activeThreadId,
    hasStartedByThread,
    inFlightByThread,
    isProcessing,
    isReviewing,
  ]);
  useEffect(() => {
    if (
      !activeThreadId ||
      isProcessing ||
      isReviewing ||
      queueFlushPaused ||
      quotaBlockedPause
    ) {
      return;
    }
    if (inFlightByThread[activeThreadId]) {
      return;
    }
    const queue = queuedByThread[activeThreadId] ?? [];
    if (queue.length === 0) {
      return;
    }
    const threadId = activeThreadId;
    const nextItem = queue[0];
    setInFlightByThread((prev) => ({ ...prev, [threadId]: nextItem }));
    setHasStartedByThread((prev) => ({ ...prev, [threadId]: false }));
    setQueuedByThread((prev) => ({
      ...prev,
      [threadId]: (prev[threadId] ?? []).slice(1),
    }));
    (async () => {
      try {
        const trimmed = nextItem.text.trim();
        const command = parseSlashCommand(trimmed, appsEnabled);
        const outcome = command
          ? await runSlashCommand(command, trimmed)
          : await (async (): Promise<QueueDispatchOutcome> => {
              const queuedMentions = nextItem.appMentions ?? [];
              const result =
                queuedMentions.length > 0
                  ? await sendUserMessage(nextItem.text, nextItem.images ?? [], queuedMentions)
                  : await sendUserMessage(nextItem.text, nextItem.images ?? []);
              return result.status === "blocked" && result.reason === "quotaGuard"
                ? "quotaBlocked"
                : "accepted";
            })();
        if (outcome === "quotaBlocked") {
          setInFlightByThread((prev) => ({ ...prev, [threadId]: null }));
          setHasStartedByThread((prev) => ({ ...prev, [threadId]: false }));
          setQuotaBlockedPause(true);
          prependQueuedMessage(threadId, nextItem);
          onQuotaGuardBlocked?.();
        }
      } catch (error) {
        setInFlightByThread((prev) => ({ ...prev, [threadId]: null }));
        setHasStartedByThread((prev) => ({ ...prev, [threadId]: false }));
        prependQueuedMessage(threadId, nextItem);
        if (isQuotaGuardBlockedError(error)) {
          setQuotaBlockedPause(true);
          onQuotaGuardBlocked?.();
        }
      }
    })();
  }, [
    activeThreadId,
    appsEnabled,
    inFlightByThread,
    isProcessing,
    isReviewing,
    queueFlushPaused,
    prependQueuedMessage,
    queuedByThread,
    runSlashCommand,
    sendUserMessage,
    quotaBlockedPause,
    onQuotaGuardBlocked,
  ]);

  return {
    queuedByThread,
    activeQueue,
    handleSend,
    queueMessage,
    removeQueuedMessage,
    resumeQueuedSends: () => setQuotaBlockedPause(false),
  };
}
