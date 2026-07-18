import { useCallback } from "react";
import type { Dispatch, MutableRefObject } from "react";
import * as Sentry from "@sentry/react";
import type {
  AppMention,
  ComposerSendIntent,
  RateLimitSnapshot,
  CustomPromptOption,
  DebugEntry,
  ReviewTarget,
  SendMessageResult,
  ServiceTier,
  WorkspaceInfo,
} from "@/types";
import {
  compactThread as compactThreadService,
  sendUserMessage as sendUserMessageService,
  steerTurn as steerTurnService,
  startReview as startReviewService,
  interruptTurn as interruptTurnService,
  getAppsList as getAppsListService,
  listMcpServerStatus as listMcpServerStatusService,
} from "@services/tauri";
import { expandCustomPromptText } from "@utils/customPrompts";
import {
  isQuotaGuardBlockedError,
  type QueueDispatchOutcome,
} from "@/features/quota-guard/quotaGuardTypes";
import {
  asString,
  extractReviewThreadId,
  extractRpcErrorMessage,
  parseReviewTarget,
} from "@threads/utils/threadNormalize";
import type { ThreadAction, ThreadState } from "./useThreadsReducer";
import { useReviewPrompt } from "./useReviewPrompt";
import {
  buildAppsLines,
  buildMcpStatusLines,
  buildReviewThreadTitle,
  buildStatusLines,
  buildTurnStartPayload,
  isStaleSteerTurnError,
  parseFastCommand,
  resolveSendMessageOptions,
  type SendMessageOptions,
} from "./threadMessagingHelpers";

type UseThreadMessagingOptions = {
  activeWorkspace: WorkspaceInfo | null;
  activeThreadId: string | null;
  accessMode?: "read-only" | "current" | "full-access";
  model?: string | null;
  effort?: string | null;
  serviceTier?: ServiceTier | null | undefined;
  collaborationMode?: Record<string, unknown> | null;
  onSelectServiceTier?: (tier: ServiceTier | null | undefined) => void;
  reviewDeliveryMode?: "inline" | "detached";
  steerEnabled: boolean;
  customPrompts: CustomPromptOption[];
  ensureWorkspaceRuntimeCodexArgs?: (
    workspaceId: string,
    threadId: string | null,
  ) => Promise<void>;
  shouldPreflightRuntimeCodexArgsForSend?: (
    workspaceId: string,
    threadId: string,
  ) => boolean;
  threadStatusById: ThreadState["threadStatusById"];
  activeTurnIdByThread: ThreadState["activeTurnIdByThread"];
  rateLimitsByWorkspace: Record<string, RateLimitSnapshot | null>;
  pendingInterruptsRef: MutableRefObject<Set<string>>;
  dispatch: Dispatch<ThreadAction>;
  getCustomName: (workspaceId: string, threadId: string) => string | undefined;
  markProcessing: (threadId: string, isProcessing: boolean) => void;
  markReviewing: (threadId: string, isReviewing: boolean) => void;
  setActiveTurnId: (threadId: string, turnId: string | null) => void;
  recordThreadActivity: (
    workspaceId: string,
    threadId: string,
    timestamp?: number,
  ) => void;
  safeMessageActivity: () => void;
  onDebug?: (entry: DebugEntry) => void;
  pushThreadErrorMessage: (threadId: string, message: string) => void;
  ensureThreadForActiveWorkspace: () => Promise<string | null>;
  startThreadForWorkspace?: (
    workspaceId: string,
    options?: { activate?: boolean },
  ) => Promise<string | null>;
  ensureThreadForWorkspace: (workspaceId: string) => Promise<string | null>;
  refreshThread: (workspaceId: string, threadId: string) => Promise<string | null>;
  forkThreadForWorkspace: (
    workspaceId: string,
    threadId: string,
    options?: { activate?: boolean },
  ) => Promise<string | null>;
  updateThreadParent: (parentId: string, childIds: string[]) => void;
  registerDetachedReviewChild?: (
    workspaceId: string,
    parentId: string,
    childId: string,
  ) => void;
  renameThread?: (workspaceId: string, threadId: string, name: string) => void;
};

export function useThreadMessaging({
  activeWorkspace,
  activeThreadId,
  accessMode,
  model,
  effort,
  serviceTier,
  collaborationMode,
  onSelectServiceTier,
  reviewDeliveryMode = "inline",
  steerEnabled,
  customPrompts,
  ensureWorkspaceRuntimeCodexArgs,
  shouldPreflightRuntimeCodexArgsForSend,
  threadStatusById,
  activeTurnIdByThread,
  rateLimitsByWorkspace,
  pendingInterruptsRef,
  dispatch,
  getCustomName,
  markProcessing,
  markReviewing,
  setActiveTurnId,
  recordThreadActivity,
  safeMessageActivity,
  onDebug,
  startThreadForWorkspace,
  pushThreadErrorMessage,
  ensureThreadForActiveWorkspace,
  ensureThreadForWorkspace,
  refreshThread,
  forkThreadForWorkspace,
  updateThreadParent,
  registerDetachedReviewChild,
  renameThread,
}: UseThreadMessagingOptions) {
  const sendMessageToThread = useCallback(
    async (
      workspace: WorkspaceInfo,
      threadId: string,
      text: string,
      images: string[] = [],
      options?: SendMessageOptions,
    ): Promise<SendMessageResult> => {
      const messageText = text.trim();
      if (!messageText && images.length === 0) {
        return { status: "blocked", reason: "other" };
      }
      let finalText = messageText;
      if (!options?.skipPromptExpansion) {
        const promptExpansion = expandCustomPromptText(messageText, customPrompts);
        if (promptExpansion && "error" in promptExpansion) {
          pushThreadErrorMessage(threadId, promptExpansion.error);
          safeMessageActivity();
          return { status: "blocked", reason: "other" };
        }
        finalText = promptExpansion?.expanded ?? messageText;
      }
      const isProcessing = threadStatusById[threadId]?.isProcessing ?? false;
      const activeTurnId = activeTurnIdByThread[threadId] ?? null;
      const {
        resolvedModel,
        resolvedEffort,
        resolvedServiceTier,
        sanitizedCollaborationMode,
        resolvedAccessMode,
        appMentions,
        sendIntent,
        shouldSteer,
        requestMode,
      } = resolveSendMessageOptions({
        options,
        defaults: {
          accessMode,
          model,
          effort,
          serviceTier,
          collaborationMode,
          steerEnabled,
          isProcessing,
          activeTurnId,
        },
      });
      Sentry.metrics.count("prompt_sent", 1, {
        attributes: {
          workspace_id: workspace.id,
          thread_id: threadId,
          has_images: images.length > 0 ? "true" : "false",
          text_length: String(finalText.length),
          model: resolvedModel ?? "unknown",
          effort: resolvedEffort ?? "unknown",
          service_tier: resolvedServiceTier ?? "default",
          collaboration_mode: sanitizedCollaborationMode ?? "unknown",
          send_intent: sendIntent,
        },
      });
      const timestamp = Date.now();
      const customThreadName = getCustomName(workspace.id, threadId) ?? null;
      recordThreadActivity(workspace.id, threadId, timestamp);
      dispatch({
        type: "setThreadTimestamp",
        workspaceId: workspace.id,
        threadId,
        timestamp,
      });
      markProcessing(threadId, true);
      safeMessageActivity();
      onDebug?.({
        id: `${Date.now()}-${shouldSteer ? "client-turn-steer" : "client-turn-start"}`,
        timestamp: Date.now(),
        source: "client",
        label: shouldSteer ? "turn/steer" : "turn/start",
        payload: {
          workspaceId: workspace.id,
          threadId,
          turnId: activeTurnId,
          text: finalText,
          images,
          model: resolvedModel,
          effort: resolvedEffort,
          serviceTier: resolvedServiceTier,
          collaborationMode: sanitizedCollaborationMode,
          sendIntent,
          threadCustomName: customThreadName,
        },
      });
      try {
        const shouldPreflightRuntimeCodexArgs =
          shouldPreflightRuntimeCodexArgsForSend?.(workspace.id, threadId) ?? true;
        if (
          !shouldSteer &&
          shouldPreflightRuntimeCodexArgs &&
          ensureWorkspaceRuntimeCodexArgs
        ) {
          await ensureWorkspaceRuntimeCodexArgs(workspace.id, threadId);
        }
        const response: Record<string, unknown> = shouldSteer
          ? (await (appMentions.length > 0
            ? steerTurnService(
              workspace.id,
              threadId,
              activeTurnId ?? "",
              finalText,
              images,
              appMentions,
            )
            : steerTurnService(
              workspace.id,
              threadId,
              activeTurnId ?? "",
              finalText,
              images,
            ))) as Record<string, unknown>
          : (await sendUserMessageService(
            workspace.id,
            threadId,
            finalText,
            buildTurnStartPayload({
              model: resolvedModel,
              effort: resolvedEffort,
              serviceTier: resolvedServiceTier,
              collaborationMode: sanitizedCollaborationMode,
              accessMode: resolvedAccessMode,
              images,
              appMentions,
            }),
          )) as Record<string, unknown>;

        const rpcError = extractRpcErrorMessage(response);

        onDebug?.({
          id: `${Date.now()}-${requestMode === "steer" ? "server-turn-steer" : "server-turn-start"}`,
          timestamp: Date.now(),
          source: "server",
          label: requestMode === "steer" ? "turn/steer response" : "turn/start response",
          payload: response,
        });
        if (rpcError) {
          const quotaGuardBlocked = isQuotaGuardBlockedError(rpcError);
          if (requestMode !== "steer") {
            markProcessing(threadId, false);
            setActiveTurnId(threadId, null);
            if (!quotaGuardBlocked) {
              pushThreadErrorMessage(threadId, `Turn failed to start: ${rpcError}`);
            }
            safeMessageActivity();
            return {
              status: "blocked",
              reason: quotaGuardBlocked ? "quotaGuard" : "other",
            };
          }
          if (isStaleSteerTurnError(rpcError)) {
            markProcessing(threadId, false);
            setActiveTurnId(threadId, null);
          }
          pushThreadErrorMessage(
            threadId,
            `Turn steer failed: ${rpcError}`,
          );
          safeMessageActivity();
          return { status: "steer_failed" };
        }
        if (requestMode === "steer") {
          const result = (response?.result ?? response) as Record<string, unknown>;
          const steeredTurnId = asString(result?.turnId ?? result?.turn_id ?? "");
          if (steeredTurnId) {
            setActiveTurnId(threadId, steeredTurnId);
          }
          return { status: "sent" };
        }
        const result = (response?.result ?? response) as Record<string, unknown>;
        const turn = (result?.turn ?? response?.turn ?? null) as
          | Record<string, unknown>
          | null;
        const turnId = asString(turn?.id ?? "");
        if (!turnId) {
          markProcessing(threadId, false);
          setActiveTurnId(threadId, null);
          pushThreadErrorMessage(threadId, "Turn failed to start.");
          safeMessageActivity();
          return { status: "blocked", reason: "other" };
        }
        setActiveTurnId(threadId, turnId);
        return { status: "sent" };
      } catch (error) {
        const errorMessage = error instanceof Error ? error.message : String(error);
        const quotaGuardBlocked = isQuotaGuardBlockedError(errorMessage);
        if (requestMode !== "steer") {
          markProcessing(threadId, false);
          setActiveTurnId(threadId, null);
        } else if (isStaleSteerTurnError(errorMessage)) {
          markProcessing(threadId, false);
          setActiveTurnId(threadId, null);
        }
        onDebug?.({
          id: `${Date.now()}-${requestMode === "steer" ? "client-turn-steer-error" : "client-turn-start-error"}`,
          timestamp: Date.now(),
          source: "error",
          label: requestMode === "steer" ? "turn/steer error" : "turn/start error",
          payload: errorMessage,
        });
        if (!quotaGuardBlocked) {
          pushThreadErrorMessage(
            threadId,
            requestMode === "steer"
              ? `Turn steer failed: ${errorMessage}`
              : errorMessage,
          );
        }
        safeMessageActivity();
        return requestMode === "steer"
          ? { status: "steer_failed" }
          : {
              status: "blocked",
              reason: quotaGuardBlocked ? "quotaGuard" : "other",
            };
      }
    },
    [
      accessMode,
      collaborationMode,
      customPrompts,
      dispatch,
      effort,
      serviceTier,
      ensureWorkspaceRuntimeCodexArgs,
      shouldPreflightRuntimeCodexArgsForSend,
      activeTurnIdByThread,
      getCustomName,
      markProcessing,
      model,
      onDebug,
      pushThreadErrorMessage,
      recordThreadActivity,
      safeMessageActivity,
      setActiveTurnId,
      steerEnabled,
      threadStatusById,
    ],
  );

  const sendUserMessage = useCallback(
    async (
      text: string,
      images: string[] = [],
      appMentions: AppMention[] = [],
      options?: { sendIntent?: ComposerSendIntent },
    ): Promise<SendMessageResult> => {
      if (!activeWorkspace) {
        return { status: "blocked", reason: "other" };
      }
      const messageText = text.trim();
      if (!messageText && images.length === 0) {
        return { status: "blocked", reason: "other" };
      }
      const promptExpansion = expandCustomPromptText(messageText, customPrompts);
      if (promptExpansion && "error" in promptExpansion) {
        if (activeThreadId) {
          pushThreadErrorMessage(activeThreadId, promptExpansion.error);
          safeMessageActivity();
        } else {
          onDebug?.({
            id: `${Date.now()}-client-prompt-expand-error`,
            timestamp: Date.now(),
            source: "error",
            label: "prompt/expand error",
            payload: promptExpansion.error,
          });
        }
        return { status: "blocked", reason: "other" };
      }
      const finalText = promptExpansion?.expanded ?? messageText;
      const threadId = await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return { status: "blocked", reason: "other" };
      }
      return sendMessageToThread(activeWorkspace, threadId, finalText, images, {
        skipPromptExpansion: true,
        appMentions,
        sendIntent: options?.sendIntent,
      });
    },
    [
      activeThreadId,
      activeWorkspace,
      customPrompts,
      ensureThreadForActiveWorkspace,
      onDebug,
      pushThreadErrorMessage,
      safeMessageActivity,
      sendMessageToThread,
    ],
  );

  const sendUserMessageToThread = useCallback(
    async (
      workspace: WorkspaceInfo,
      threadId: string,
      text: string,
      images: string[] = [],
      options?: SendMessageOptions,
    ): Promise<SendMessageResult> => {
      return sendMessageToThread(workspace, threadId, text, images, options);
    },
    [sendMessageToThread],
  );

  const interruptTurn = useCallback(async () => {
    if (!activeWorkspace || !activeThreadId) {
      return;
    }
    const activeTurnId = activeTurnIdByThread[activeThreadId] ?? null;
    const turnId = activeTurnId ?? "pending";
    markProcessing(activeThreadId, false);
    setActiveTurnId(activeThreadId, null);
    dispatch({
      type: "addAssistantMessage",
      threadId: activeThreadId,
      text: "Session stopped.",
    });
    if (!activeTurnId) {
      pendingInterruptsRef.current.add(activeThreadId);
    }
    onDebug?.({
      id: `${Date.now()}-client-turn-interrupt`,
      timestamp: Date.now(),
      source: "client",
      label: "turn/interrupt",
      payload: {
        workspaceId: activeWorkspace.id,
        threadId: activeThreadId,
        turnId,
        queued: !activeTurnId,
      },
    });
    try {
      const response = await interruptTurnService(
        activeWorkspace.id,
        activeThreadId,
        turnId,
      );
      onDebug?.({
        id: `${Date.now()}-server-turn-interrupt`,
        timestamp: Date.now(),
        source: "server",
        label: "turn/interrupt response",
        payload: response,
      });
    } catch (error) {
      onDebug?.({
        id: `${Date.now()}-client-turn-interrupt-error`,
        timestamp: Date.now(),
        source: "error",
        label: "turn/interrupt error",
        payload: error instanceof Error ? error.message : String(error),
      });
    }
  }, [
    activeThreadId,
    activeTurnIdByThread,
    activeWorkspace,
    dispatch,
    markProcessing,
    onDebug,
    pendingInterruptsRef,
    setActiveTurnId,
  ]);

  const startReviewTarget = useCallback(
    async (
      target: ReviewTarget,
      workspaceIdOverride?: string,
      options?: { propagateQuotaGuardBlocked?: boolean },
    ): Promise<boolean> => {
      const workspaceId = workspaceIdOverride ?? activeWorkspace?.id ?? null;
      if (!workspaceId) {
        return false;
      }
      const threadId = workspaceIdOverride
        ? await ensureThreadForWorkspace(workspaceId)
        : await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return false;
      }

      const lockParentThread = reviewDeliveryMode !== "detached";
      if (lockParentThread) {
        markProcessing(threadId, true);
        markReviewing(threadId, true);
        safeMessageActivity();
      }
      onDebug?.({
        id: `${Date.now()}-client-review-start`,
        timestamp: Date.now(),
        source: "client",
        label: "review/start",
        payload: {
          workspaceId,
          threadId,
          target,
        },
      });
      try {
        const response = await startReviewService(
          workspaceId,
          threadId,
          target,
          reviewDeliveryMode,
        );
        onDebug?.({
          id: `${Date.now()}-server-review-start`,
          timestamp: Date.now(),
          source: "server",
          label: "review/start response",
          payload: response,
        });
        const rpcError = extractRpcErrorMessage(response);
        if (rpcError) {
          if (lockParentThread) {
            markProcessing(threadId, false);
            markReviewing(threadId, false);
            setActiveTurnId(threadId, null);
          }
          if (isQuotaGuardBlockedError(rpcError)) {
            if (options?.propagateQuotaGuardBlocked) {
              throw new Error(rpcError);
            }
          } else {
            pushThreadErrorMessage(threadId, `Review failed to start: ${rpcError}`);
          }
          safeMessageActivity();
          return false;
        }
        const reviewThreadId = extractReviewThreadId(response);
        if (reviewThreadId && reviewThreadId !== threadId) {
          updateThreadParent(threadId, [reviewThreadId]);
          if (reviewDeliveryMode === "detached") {
            registerDetachedReviewChild?.(workspaceId, threadId, reviewThreadId);
            const reviewTitle = buildReviewThreadTitle(target);
            if (reviewTitle && !getCustomName(workspaceId, reviewThreadId)) {
              renameThread?.(workspaceId, reviewThreadId, reviewTitle);
            }
          }
        }
        return true;
      } catch (error) {
        if (lockParentThread) {
          markProcessing(threadId, false);
          markReviewing(threadId, false);
        }
        onDebug?.({
          id: `${Date.now()}-client-review-start-error`,
          timestamp: Date.now(),
          source: "error",
          label: "review/start error",
          payload: error instanceof Error ? error.message : String(error),
        });
        const errorMessage = error instanceof Error ? error.message : String(error);
        if (options?.propagateQuotaGuardBlocked && isQuotaGuardBlockedError(errorMessage)) {
          throw error;
        }
        if (!isQuotaGuardBlockedError(errorMessage)) {
          pushThreadErrorMessage(threadId, errorMessage);
        }
        safeMessageActivity();
        return false;
      }
    },
    [
      activeWorkspace,
      ensureThreadForActiveWorkspace,
      ensureThreadForWorkspace,
      getCustomName,
      markProcessing,
      markReviewing,
      onDebug,
      pushThreadErrorMessage,
      safeMessageActivity,
      setActiveTurnId,
      reviewDeliveryMode,
      registerDetachedReviewChild,
      renameThread,
      updateThreadParent,
    ],
  );

  const {
    reviewPrompt,
    openReviewPrompt,
    closeReviewPrompt,
    showPresetStep,
    choosePreset,
    highlightedPresetIndex,
    setHighlightedPresetIndex,
    highlightedBranchIndex,
    setHighlightedBranchIndex,
    highlightedCommitIndex,
    setHighlightedCommitIndex,
    handleReviewPromptKeyDown,
    confirmBranch,
    selectBranch,
    selectBranchAtIndex,
    selectCommit,
    selectCommitAtIndex,
    confirmCommit,
    updateCustomInstructions,
    confirmCustom,
  } = useReviewPrompt({
    activeWorkspace,
    activeThreadId,
    onDebug,
    startReviewTarget,
  });

  const startReview = useCallback(
    async (text: string) => {
      if (!activeWorkspace || !text.trim()) {
        return;
      }
      const trimmed = text.trim();
      const rest = trimmed.replace(/^\/review\b/i, "").trim();
      if (!rest) {
        openReviewPrompt();
        return;
      }

      const target = parseReviewTarget(trimmed);
      await startReviewTarget(target);
    },
    [
      activeWorkspace,
      openReviewPrompt,
      startReviewTarget,
    ],
  );
  const startQueuedReview = useCallback(
    async (text: string): Promise<QueueDispatchOutcome> => {
      if (!activeWorkspace || !text.trim()) {
        return "accepted";
      }
      const trimmed = text.trim();
      const rest = trimmed.replace(/^\/review\b/i, "").trim();
      if (!rest) {
        openReviewPrompt();
        return "accepted";
      }
      try {
        await startReviewTarget(parseReviewTarget(trimmed), undefined, {
          propagateQuotaGuardBlocked: true,
        });
        return "accepted";
      } catch (error) {
        return isQuotaGuardBlockedError(error) ? "quotaBlocked" : "accepted";
      }
    },
    [activeWorkspace, openReviewPrompt, startReviewTarget],
  );

  const startUncommittedReview = useCallback(
    async (workspaceId?: string | null) => {
      const workspaceOverride = workspaceId ?? undefined;
      await startReviewTarget({ type: "uncommittedChanges" }, workspaceOverride);
    },
    [startReviewTarget],
  );

  const startStatus = useCallback(
    async (_text: string) => {
      if (!activeWorkspace) {
        return;
      }
      const threadId = await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return;
      }

      const lines = buildStatusLines({
        model,
        serviceTier,
        effort,
        accessMode,
        collaborationMode,
        rateLimits: rateLimitsByWorkspace[activeWorkspace.id] ?? null,
      });
      const timestamp = Date.now();
      recordThreadActivity(activeWorkspace.id, threadId, timestamp);
      dispatch({
        type: "addAssistantMessage",
        threadId,
        text: lines.join("\n"),
      });
      safeMessageActivity();
    },
    [
      accessMode,
      activeWorkspace,
      collaborationMode,
      dispatch,
      effort,
      ensureThreadForActiveWorkspace,
      model,
      serviceTier,
      rateLimitsByWorkspace,
      recordThreadActivity,
      safeMessageActivity,
    ],
  );

  const startFast = useCallback(
    async (text: string) => {
      if (!activeWorkspace) {
        return;
      }
      const threadId = await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return;
      }

      const action = parseFastCommand(text);
      const isEnabled = serviceTier === "fast";
      let nextTier = serviceTier ?? null;
      let message = "";

      if (action === "invalid") {
        message = "Usage: /fast, /fast on, /fast off, or /fast status.";
      } else if (action === "status") {
        message = `Fast mode is ${isEnabled ? "on" : "off"}.`;
      } else {
        nextTier =
          action === "on"
            ? "fast"
            : action === "off"
              ? null
              : isEnabled
                ? null
                : "fast";
        onSelectServiceTier?.(nextTier);
        message = `Fast mode ${nextTier === "fast" ? "enabled" : "disabled"}.`;
      }

      const timestamp = Date.now();
      recordThreadActivity(activeWorkspace.id, threadId, timestamp);
      dispatch({
        type: "addAssistantMessage",
        threadId,
        text: message,
      });
      safeMessageActivity();
    },
    [
      activeWorkspace,
      dispatch,
      ensureThreadForActiveWorkspace,
      onSelectServiceTier,
      recordThreadActivity,
      safeMessageActivity,
      serviceTier,
    ],
  );

  const startMcp = useCallback(
    async (_text: string) => {
      if (!activeWorkspace) {
        return;
      }
      const threadId = await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return;
      }

      try {
        const response = (await listMcpServerStatusService(
          activeWorkspace.id,
          null,
          null,
        )) as Record<string, unknown> | null;
        const result = (response?.result ?? response) as
          | Record<string, unknown>
          | null;
        const data = Array.isArray(result?.data)
          ? (result?.data as Array<Record<string, unknown>>)
          : [];
        const lines = buildMcpStatusLines(data);

        const timestamp = Date.now();
        recordThreadActivity(activeWorkspace.id, threadId, timestamp);
        dispatch({
          type: "addAssistantMessage",
          threadId,
          text: lines.join("\n"),
        });
      } catch (error) {
        const message =
          error instanceof Error ? error.message : "Failed to load MCP status.";
        dispatch({
          type: "addAssistantMessage",
          threadId,
          text: `MCP tools:\n- ${message}`,
        });
      } finally {
        safeMessageActivity();
      }
    },
    [
      activeWorkspace,
      dispatch,
      ensureThreadForActiveWorkspace,
      recordThreadActivity,
      safeMessageActivity,
    ],
  );

  const startApps = useCallback(
    async (_text: string) => {
      if (!activeWorkspace) {
        return;
      }
      const threadId = await ensureThreadForActiveWorkspace();
      if (!threadId) {
        return;
      }

      try {
        const response = (await getAppsListService(
          activeWorkspace.id,
          null,
          100,
          threadId,
        )) as Record<string, unknown> | null;
        const result = (response?.result ?? response) as
          | Record<string, unknown>
          | null;
        const data = Array.isArray(result?.data)
          ? (result?.data as Array<Record<string, unknown>>)
          : [];
        const lines = buildAppsLines(data);

        const timestamp = Date.now();
        recordThreadActivity(activeWorkspace.id, threadId, timestamp);
        dispatch({
          type: "addAssistantMessage",
          threadId,
          text: lines.join("\n"),
        });
      } catch (error) {
        const message =
          error instanceof Error ? error.message : "Failed to load apps.";
        dispatch({
          type: "addAssistantMessage",
          threadId,
          text: `Apps:\n- ${message}`,
        });
      } finally {
        safeMessageActivity();
      }
    },
    [
      activeWorkspace,
      dispatch,
      ensureThreadForActiveWorkspace,
      recordThreadActivity,
      safeMessageActivity,
    ],
  );

  const startForkWithOutcome = useCallback(
    async (text: string): Promise<QueueDispatchOutcome> => {
      if (!activeWorkspace || !activeThreadId) {
        return "accepted";
      }
      const trimmed = text.trim();
      const rest = trimmed.replace(/^\/fork\b/i, "").trim();
      const threadId = await forkThreadForWorkspace(activeWorkspace.id, activeThreadId);
      if (!threadId) {
        return "accepted";
      }
      updateThreadParent(activeThreadId, [threadId]);
      if (!rest) {
        return "accepted";
      }
      const result = await sendMessageToThread(activeWorkspace, threadId, rest, []);
      return result.status === "blocked" && result.reason === "quotaGuard"
        ? "quotaBlocked"
        : "accepted";
    },
    [
      activeThreadId,
      activeWorkspace,
      forkThreadForWorkspace,
      sendMessageToThread,
      updateThreadParent,
    ],
  );

  const startFork = useCallback(
    async (text: string): Promise<void> => {
      await startForkWithOutcome(text);
    },
    [startForkWithOutcome],
  );

  const startQueuedFork = useCallback(
    async (text: string): Promise<QueueDispatchOutcome> => startForkWithOutcome(text),
    [startForkWithOutcome],
  );

  const startResume = useCallback(
    async (_text: string) => {
      if (!activeWorkspace) {
        return;
      }
      if (activeThreadId && threadStatusById[activeThreadId]?.isProcessing) {
        return;
      }
      const threadId = activeThreadId ?? (await ensureThreadForActiveWorkspace());
      if (!threadId) {
        return;
      }
      await refreshThread(activeWorkspace.id, threadId);
      safeMessageActivity();
    },
    [
      activeThreadId,
      activeWorkspace,
      ensureThreadForActiveWorkspace,
      refreshThread,
      safeMessageActivity,
      threadStatusById,
    ],
  );

  const startCompactWithOutcome = useCallback(
    async (_text: string): Promise<QueueDispatchOutcome> => {
      if (!activeWorkspace) {
        return "accepted";
      }
      const threadId = activeThreadId ?? (await ensureThreadForActiveWorkspace());
      if (!threadId) {
        return "accepted";
      }
      try {
        await compactThreadService(activeWorkspace.id, threadId);
        return "accepted";
      } catch (error) {
        if (isQuotaGuardBlockedError(error)) {
          return "quotaBlocked";
        }
        pushThreadErrorMessage(
          threadId,
          error instanceof Error
            ? error.message
            : "Failed to start context compaction.",
        );
        return "accepted";
      } finally {
        safeMessageActivity();
      }
    },
    [
      activeThreadId,
      activeWorkspace,
      ensureThreadForActiveWorkspace,
      pushThreadErrorMessage,
      safeMessageActivity,
    ],
  );

  const startCompact = useCallback(
    async (text: string): Promise<void> => {
      await startCompactWithOutcome(text);
    },
    [startCompactWithOutcome],
  );

  const startQueuedCompact = useCallback(
    async (text: string): Promise<QueueDispatchOutcome> => startCompactWithOutcome(text),
    [startCompactWithOutcome],
  );
  const startQueuedNew = useCallback(
    async (text: string): Promise<QueueDispatchOutcome> => {
      if (!activeWorkspace || !startThreadForWorkspace) {
        return "accepted";
      }
      const threadId = await startThreadForWorkspace(activeWorkspace.id);
      const rest = text.trim().replace(/^\/new\b/i, "").trim();
      if (!threadId || !rest) {
        return "accepted";
      }
      const result = await sendMessageToThread(activeWorkspace, threadId, rest, []);
      return result.status === "blocked" && result.reason === "quotaGuard"
        ? "quotaBlocked"
        : "accepted";
    },
    [activeWorkspace, sendMessageToThread, startThreadForWorkspace],
  );

  return {
    interruptTurn,
    sendUserMessage,
    sendUserMessageToThread,
    startFork,
    startReview,
    startUncommittedReview,
    startResume,
    startCompact,
    startQueuedFork,
    startQueuedReview,
    startQueuedCompact,
    startQueuedNew,
    startApps,
    startMcp,
    startFast,
    startStatus,
    reviewPrompt,
    openReviewPrompt,
    closeReviewPrompt,
    showPresetStep,
    choosePreset,
    highlightedPresetIndex,
    setHighlightedPresetIndex,
    highlightedBranchIndex,
    setHighlightedBranchIndex,
    highlightedCommitIndex,
    setHighlightedCommitIndex,
    handleReviewPromptKeyDown,
    confirmBranch,
    selectBranch,
    selectBranchAtIndex,
    selectCommit,
    selectCommitAtIndex,
    confirmCommit,
    updateCustomInstructions,
    confirmCustom,
  };
}
