<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import { AlertTriangle, ArrowLeft, Check, CheckCircle2, FolderOpen, FolderSearch, Gamepad2, LoaderCircle, Plus, Trash2, X } from "@lucide/vue";
import { open } from "@tauri-apps/plugin-dialog";
import {
  cancelSaveLearning,
  cancelTask,
  confirmSaveProfile,
  discardPendingGame,
  getGame,
  getTask,
  startAddGameTask,
  startSaveCandidateVerificationTask,
  startFinishSaveLearningTask,
  startSaveLearningTask,
  openPathInExplorer,
  previewSaveScopes,
  type AppTask,
} from "../api";
import { createDefaultSaveScope, type Game, type SaveCandidateEvidenceLevel, type SaveLearningResult, type SaveLearningSession, type SaveRootType, type SaveScope } from "../domain/game";

type WizardPhase = "form" | "copying" | "ready" | "capturing" | "analyzing" | "review" | "done";

const emit = defineEmits<{ (event: "back"): void; (event: "completed", game: Game): void }>();

const phase = ref<WizardPhase>("form");
const displayName = ref("");
const gameKey = ref("");
const sourcePath = ref("");
const executablePath = ref("");
const taskId = ref("");
const session = ref<SaveLearningSession | null>(null);
const learningResult = ref<SaveLearningResult | null>(null);
const reviewScopes = ref<SaveScope[]>([]);
const scopeEvidence = ref<Record<string, { level: SaveCandidateEvidenceLevel; reason: string }>>({});
const validatingCandidates = ref(false);
const confidence = ref(0);
// `confidence` 是 30–95 之间的**评分**，不是概率（后端 calculate_learning_confidence
// 末尾 clamp(30, 95)，只要有候选就至少 30）。以前直接渲染成 `xx%` 会被当成准确率读，
// 所以只对外暴露档位，原始分数降为副文本。
const confidenceBand = computed(() => (confidence.value >= 85 ? "高" : confidence.value >= 65 ? "中" : "低"));
const progress = ref(0);
const message = ref("");
const error = ref("");
const completedGame = ref<Game | null>(null);
const confirming = ref(false);
const previewing = ref(false);
// 目录推断前置（评审 A1 第 2 项）：选定本体后**自动**跑一次只读推断，把候选目录先摆出来，
// 让用户在启动游戏之前就能看到「准备保护哪些目录」，而不是跑完一整套学习才发现目录不对。
// 与 `previewing` 分开：前者是「后台预取」，不该禁用主动作按钮。
const inferringDrafts = ref(false);
const inferredDrafts = ref<SaveLearningResult | null>(null);
// 同一次添加里只自动推一次：`phase` 在「学习失败/取消」后会退回 `ready`，不该重新遍历一遍目录。
// 换游戏（gameUid 变化）时重置。
let inferredForGameUid = "";
const cancelling = ref(false);
const showLargeConfirmModal = ref(false);
const largeConfirmMessage = ref("");
const newFileByScope = ref<Record<number, string>>({});
const newPatternByScope = ref<Record<number, string>>({});
// 「目录里看起来也是存档、但本次学习没有变化」的文件，按范围根路径索引。
// 它们不会被自动纳入保护范围——毕竟只是启发式判断，误收别人的存档比漏收更难收拾。
const proposedByScope = ref<Record<string, string[]>>({});
let pollTimer: ReturnType<typeof setTimeout> | undefined;

const isBusy = computed(() => phase.value === "copying" || phase.value === "analyzing");
const canStart = computed(() => Boolean(displayName.value.trim() && sourcePath.value.trim() && executablePath.value.trim()) && phase.value === "form");
const stepNumber = computed(() => phase.value === "form" || phase.value === "copying" ? 1 : phase.value === "ready" || phase.value === "capturing" ? 2 : phase.value === "analyzing" || phase.value === "review" ? 3 : 4);
const stepTitle = computed(() => phase.value === "form" || phase.value === "copying" ? "选择并复制游戏本体" : phase.value === "ready" || phase.value === "capturing" ? "完成一次游戏内保存" : phase.value === "analyzing" ? "分析存档变化" : phase.value === "review" ? "确认存档保护范围" : "添加完成");
const canConfirm = computed(() => reviewScopes.value.length > 0 && reviewScopes.value.every((scope) => scope.confirmedFiles.length > 0 || scope.includeDirectories.length > 0));
const hasReviewCandidates = computed(() => reviewScopes.value.some((scope) => scopeEvidence.value[scopeEvidenceKey(scope)]?.level === "review"));
// 只读初稿（评审 A1）：没有启动游戏、没有任何写入证据。文案必须与「真的跑过一次识别」
// 明确区分 —— 否则用户会把一份猜出来的范围当成已经被证据确认过的。
const isPreviewDraft = computed(() => learningResult.value?.eventCaptureMode === "preview");

const rootTypeLabel: Record<SaveRootType, string> = {
  managed_game: "游戏目录",
  app_data: "AppData",
  local_app_data: "LocalAppData",
  local_low: "LocalLow",
  documents: "文档",
  saved_games: "Saved Games",
  user_profile: "用户目录",
  custom: "自定义目录",
  program_data: "ProgramData",
};

function cleanDisplayPath(rawPath: string): string {
  return rawPath.replace(/^\\\\\?\\UNC\\/i, "\\\\").replace(/^\\\\\?\\/i, "");
}

function formatScopeDisplay(scope: SaveScope): string {
  const clean = cleanDisplayPath(scope.rootPath);
  if (scope.rootType === "managed_game" && completedGame.value?.managedPath) {
    const normManaged = cleanDisplayPath(completedGame.value.managedPath)
      .replace(/[/\\]+/g, "\\")
      .replace(/\\+$/, "")
      .toLowerCase();
    const normClean = clean.replace(/[/\\]+/g, "\\").replace(/\\+$/, "").toLowerCase();
    if (normClean === normManaged) {
      return "<游戏根目录>";
    }
    if (normClean.startsWith(normManaged + "\\")) {
      const sub = clean.slice(cleanDisplayPath(completedGame.value.managedPath).length).replace(/^[/\\]+/, "");
      return `<游戏根目录>\\${sub}`;
    }
  }
  return clean;
}

function scopeEvidenceKey(scope: SaveScope): string {
  return scope.rootPath.replace(/[\\/]+/g, "\\").replace(/\\+$/, "").toLocaleLowerCase();
}

function evidenceForScope(scope: SaveScope) {
  return scopeEvidence.value[scopeEvidenceKey(scope)] || { level: "review" as const, reason: "手动添加的范围，请在确认前检查内容。" };
}

function evidenceLabel(scope: SaveScope): string {
  return evidenceForScope(scope).level === "strong" ? "高可信" : "待确认";
}

function applyInitialLearningResult(result: SaveLearningResult) {
  learningResult.value = result;
  reviewScopes.value = result.scopeDrafts.map((draft) => ({ ...draft.scope, confirmedFiles: [...draft.scope.confirmedFiles], includeDirectories: [...draft.scope.includeDirectories], excludeExact: [...draft.scope.excludeExact], excludePatterns: [...draft.scope.excludePatterns], excludeDirectories: [...draft.scope.excludeDirectories] }));
  scopeEvidence.value = Object.fromEntries(result.scopeDrafts.map((draft) => [scopeEvidenceKey(draft.scope), { level: draft.evidenceLevel, reason: draft.evidenceReason }]));
  proposedByScope.value = Object.fromEntries(result.scopeDrafts.map((draft) => [scopeEvidenceKey(draft.scope), [...(draft.proposedFiles ?? [])]]));
  confidence.value = result.confidence;
}

function mergeCandidateVerification(result: SaveLearningResult) {
  const verified = new Map(result.scopeDrafts.map((draft) => [scopeEvidenceKey(draft.scope), draft]));
  let promoted = 0;
  for (const scope of reviewScopes.value) {
    const key = scopeEvidenceKey(scope);
    const prior = scopeEvidence.value[key];
    const matched = verified.get(key);
    if (!matched) continue;
    for (const file of matched.scope.confirmedFiles) {
      if (!scope.confirmedFiles.includes(file)) scope.confirmedFiles.push(file);
    }
    const extraProposals = matched.proposedFiles ?? [];
    if (extraProposals.length) {
      proposedByScope.value[key] = [...new Set([...(proposedByScope.value[key] ?? []), ...extraProposals])];
    }
    if (prior?.level === "review") {
      scopeEvidence.value[key] = { level: "strong", reason: "两次独立保存均命中此范围，已提升为高可信。" };
      promoted += 1;
    }
  }
  const summary = promoted > 0
    ? `再次保存验证完成：${promoted} 个候选范围已提升为高可信。`
    : "再次保存验证未命中待确认范围；现有范围仍保留，可继续手动调整。";
  learningResult.value = {
    ...result,
    notes: [...(learningResult.value?.notes || []), summary, ...result.notes],
  };
  confidence.value = Math.max(confidence.value, result.confidence, promoted > 0 ? 90 : 0);
}

async function chooseSource() {
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  sourcePath.value = selected;
  if (!displayName.value) {
    displayName.value = selected.split(/[\\/]/).filter(Boolean).pop() || "";
    gameKey.value = normalizeGameKey(displayName.value);
  }
}

function normalizeGameKey(value: string): string {
  return value.trim().split(/\s+/).join(" ").toLocaleLowerCase();
}

async function chooseExecutable() {
  const selected = await open({ directory: false, multiple: false, filters: [{ name: "Windows 程序", extensions: ["exe"] }] });
  if (typeof selected === "string") executablePath.value = selected;
}

function stopPolling() {
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = undefined;
}

function handleTaskFailure(task: AppTask, cancelledMessage: string, failedMessage: string) {
  stopPolling();
  taskId.value = "";
  cancelling.value = false;
  const taskError = task.error || "";
  const cleanError = taskError.replace(/^LARGE_SOURCE_REQUIRED:\s*/, "");
  error.value = cleanError || (task.status === "cancelled" ? cancelledMessage : failedMessage);
  phase.value = completedGame.value ? "ready" : "form";
}

async function watchTask(onSuccess: (task: AppTask) => Promise<void>, cancelledMessage: string, failedMessage: string, onFailure?: (task: AppTask) => Promise<void>) {
  const watchedTaskId = taskId.value;
  if (!watchedTaskId) return;
  try {
    const task = await getTask(watchedTaskId);
    progress.value = task.progress;
    message.value = task.message;
    if (task.status === "success") {
      stopPolling();
      taskId.value = "";
      cancelling.value = false;
      await onSuccess(task);
      return;
    }
    if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
      handleTaskFailure(task, cancelledMessage, task.status === "interrupted" ? "任务异常中断，请重试" : failedMessage);
      if (onFailure) await onFailure(task);
      return;
    }
    pollTimer = setTimeout(() => void watchTask(onSuccess, cancelledMessage, failedMessage, onFailure), 350);
  } catch (reason) {
    stopPolling();
    taskId.value = "";
    cancelling.value = false;
    error.value = String(reason);
    phase.value = completedGame.value ? "ready" : "form";
  }
}

async function submitAdd(allowLargeSource: boolean) {
  if (!canStart.value) return;
  error.value = "";
  progress.value = 0;
  message.value = "准备复制游戏本体";
  phase.value = "copying";
  try {
    taskId.value = await startAddGameTask({ displayName: displayName.value, gameKey: normalizeGameKey(gameKey.value || displayName.value), sourcePath: sourcePath.value, executablePath: executablePath.value, allowLargeSource });
    await watchTask(async (task) => {
      completedGame.value = task.gameUid ? await getGame(task.gameUid) : null;
      if (!completedGame.value) throw new Error("游戏已复制，但没有找到待设置的游戏记录");
      phase.value = "ready";
      message.value = "游戏本体已准备好";
    }, "已取消复制", "添加游戏失败", async (task) => {
      const warning = task.error || "";
      const isLargeSource = warning.startsWith("LARGE_SOURCE_REQUIRED:") || warning.includes("超过 3 GB");
      if (!allowLargeSource && isLargeSource) {
        error.value = "";
        largeConfirmMessage.value = warning.replace(/^LARGE_SOURCE_REQUIRED:\s*/, "");
        showLargeConfirmModal.value = true;
      }
    });
  } catch (reason) {
    phase.value = "form";
    error.value = String(reason);
  }
}

function handleConfirmLargeSource() {
  showLargeConfirmModal.value = false;
  largeConfirmMessage.value = "";
  void submitAdd(true);
}

function handleCancelLargeSource() {
  showLargeConfirmModal.value = false;
  error.value = largeConfirmMessage.value;
  largeConfirmMessage.value = "";
}

async function start() {
  await submitAdd(false);
}

async function beginLearning() {
  if (!completedGame.value || phase.value !== "ready") return;
  error.value = "";
  message.value = "准备启动游戏并记录存档变化";
  phase.value = "capturing";
  try {
    taskId.value = await startSaveLearningTask(completedGame.value.gameUid);
    await watchTask(async (task) => {
      const learnedSession = task.result as SaveLearningSession | undefined;
      if (!learnedSession?.sessionId) throw new Error("学习会话没有正确建立");
      session.value = learnedSession;
      message.value = "游戏已启动，请完成一次保存";
    }, "已取消存档识别", "启动存档识别失败");
  } catch (reason) {
    phase.value = "ready";
    error.value = String(reason);
  }
}

async function analyze() {
  if (!session.value || phase.value !== "capturing") return;
  error.value = "";
  message.value = "正在分析保存前后的变化";
  progress.value = 0;
  phase.value = "analyzing";
  try {
    taskId.value = await startFinishSaveLearningTask(session.value.sessionId);
    await watchTask(async (task) => {
      const result = task.result as SaveLearningResult | undefined;
      if (!result) throw new Error("分析完成，但没有返回学习结果");
      if (validatingCandidates.value) {
        mergeCandidateVerification(result);
        validatingCandidates.value = false;
      } else {
        applyInitialLearningResult(result);
      }
      phase.value = "review";
    }, "已取消分析", "存档分析失败", async () => {
      if (validatingCandidates.value) {
        validatingCandidates.value = false;
        phase.value = "review";
      } else {
        session.value = null;
        phase.value = "ready";
      }
    });
  } catch (reason) {
    const wasValidating = validatingCandidates.value;
    validatingCandidates.value = false;
    phase.value = wasValidating ? "review" : "capturing";
    error.value = String(reason);
  }
}

/**
 * 只读推断一份初稿，直接进审阅界面 —— 不必先跑「启动游戏 → 手动存一次档 → 点分析」。
 *
 * 后端返回的结构与「完成学习」一致，所以直接复用 `applyInitialLearningResult`；区别在于
 * 证据等级一律是「待确认」，界面上也会换成只读初稿的文案。
 */
async function previewDraft() {
  if (!completedGame.value || phase.value !== "ready" || previewing.value) return;
  error.value = "";
  previewing.value = true;
  message.value = "正在按目录推断候选范围";
  try {
    applyInitialLearningResult(await previewSaveScopes(completedGame.value.gameUid));
    phase.value = "review";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    previewing.value = false;
  }
}

/**
 * 目录推断前置（评审 A1 第 2 项）：进入 `ready` 后**自动**推断一次，把候选目录先展示出来。
 *
 * 为什么自动而不只留按钮：审阅界面才是用户真正要做判断的地方，原先前置推断挂在按钮上，
 * 用户不知道点了会发生什么，多数人不会点 —— 于是就变成「跑完一整套学习 + 手动保存一次，
 * 才发现目录根本不对」。自动跑一次把这份信息提前。
 *
 * **不阻塞主动作**：用独立的 `inferringDrafts`，不并入 `previewing` —— 推断期间「启动并开始识别」
 * 必须仍然可点。推断失败也只是少一块提示（`inferredDrafts` 保持 `null`），不写 `error`、
 * 不影响继续走完整识别。
 */
async function inferDraftsAhead(gameUid: string) {
  if (!gameUid || inferringDrafts.value || inferredForGameUid === gameUid) return;
  inferredForGameUid = gameUid;
  inferringDrafts.value = true;
  try {
    inferredDrafts.value = await previewSaveScopes(gameUid);
  } catch {
    // 刻意吞掉：这是一次「锦上添花」的后台预取，失败不该在用户还没提要求时就报错。
    inferredDrafts.value = null;
  } finally {
    inferringDrafts.value = false;
  }
}

// 进入 `ready` 就自动推断；换游戏时允许重新推断一次。
watch(
  () => [phase.value, completedGame.value?.gameUid ?? ""] as const,
  ([currentPhase, gameUid]) => {
    if (currentPhase !== "ready" || !gameUid) return;
    if (inferredForGameUid === gameUid) return;
    void inferDraftsAhead(gameUid);
  },
  { immediate: true },
);

/**
 * 用**已推断好**的那份初稿进审阅界面。
 *
 * 复用而不是重新请求：面板上已经把结果显示给用户了，点「查看并编辑」却再等一次遍历、
 * 还可能拿到与刚才展示不一致的清单，属于无谓的不确定。想拿新快照就重新走完整识别。
 */
function openInferredDraft() {
  if (phase.value !== "ready" || !inferredDrafts.value) return;
  error.value = "";
  applyInitialLearningResult(inferredDrafts.value);
  phase.value = "review";
}

/**
 * 从只读初稿退回完整识别。
 *
 * 没有这个出口的话，用户一旦选了「跳过识别」就只能「放弃添加」重来一遍 —— 想反悔的代价
 * 比不跳过大得多。清掉初稿状态再回 `ready`，重新点「启动并开始识别」即可。
 */
function backToFullLearning() {
  if (phase.value !== "review" || confirming.value) return;
  error.value = "";
  learningResult.value = null;
  reviewScopes.value = [];
  scopeEvidence.value = {};
  proposedByScope.value = {};
  confidence.value = 0;
  session.value = null;
  message.value = "";
  phase.value = "ready";
}

async function beginCandidateVerification() {
  if (!completedGame.value || phase.value !== "review" || !hasReviewCandidates.value) return;
  const candidates = reviewScopes.value.filter((scope) => evidenceForScope(scope).level === "review");
  error.value = "";
  message.value = "准备启动游戏并再次验证待确认范围";
  validatingCandidates.value = true;
  phase.value = "capturing";
  try {
    taskId.value = await startSaveCandidateVerificationTask(completedGame.value.gameUid, candidates);
    await watchTask(async (task) => {
      const learnedSession = task.result as SaveLearningSession | undefined;
      if (!learnedSession?.sessionId) throw new Error("再次验证会话没有正确建立");
      session.value = learnedSession;
      message.value = "请在游戏内再次完成一次保存，然后回来分析";
    }, "已取消再次验证", "启动再次验证失败", async () => {
      validatingCandidates.value = false;
      phase.value = "review";
    });
  } catch (reason) {
    validatingCandidates.value = false;
    phase.value = "review";
    error.value = String(reason);
  }
}

function addFile(scopeIndex: number) {
  const value = (newFileByScope.value[scopeIndex] || "").trim().replace(/\\/g, "/");
  if (!value) return;
  const scope = reviewScopes.value[scopeIndex];
  if (scope && !scope.confirmedFiles.includes(value)) scope.confirmedFiles.push(value);
  newFileByScope.value[scopeIndex] = "";
}

function addPattern(scopeIndex: number) {
  const value = (newPatternByScope.value[scopeIndex] || "").trim();
  if (!value) return;
  const scope = reviewScopes.value[scopeIndex];
  if (scope && !scope.excludePatterns.includes(value)) scope.excludePatterns.push(value);
  newPatternByScope.value[scopeIndex] = "";
}

function adoptProposedFiles(scopeIndex: number) {
  const scope = reviewScopes.value[scopeIndex];
  if (!scope) return;
  const key = scopeEvidenceKey(scope);
  for (const file of proposedByScope.value[key] ?? []) {
    if (!scope.confirmedFiles.includes(file)) scope.confirmedFiles.push(file);
  }
  proposedByScope.value[key] = [];
}

function toggleScopePolicy(scopeIndex: number) {
  const scope = reviewScopes.value[scopeIndex];
  if (!scope) return;
  scope.unknownFilePolicy = scope.unknownFilePolicy === "protect" ? "ignore" : "protect";
}

function removeFile(scopeIndex: number, fileIndex: number) {
  reviewScopes.value[scopeIndex]?.confirmedFiles.splice(fileIndex, 1);
}

function removePattern(scopeIndex: number, patternIndex: number) {
  reviewScopes.value[scopeIndex]?.excludePatterns.splice(patternIndex, 1);
}

function removeExcludeExact(scopeIndex: number, exactIndex: number) {
  reviewScopes.value[scopeIndex]?.excludeExact.splice(exactIndex, 1);
}

function removeExcludeDirectory(scopeIndex: number, dirIndex: number) {
  reviewScopes.value[scopeIndex]?.excludeDirectories.splice(dirIndex, 1);
}

function removeScope(scopeIndex: number) {
  const [removed] = reviewScopes.value.splice(scopeIndex, 1);
  if (removed) {
    const key = scopeEvidenceKey(removed);
    delete scopeEvidence.value[key];
    delete proposedByScope.value[key];
  }
}

async function openFolder(path: string) {
  try {
    await openPathInExplorer(cleanDisplayPath(path));
  } catch (reason) {
    error.value = `打开目录失败：${String(reason)}`;
  }
}

async function addDirectoryScope() {
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  const scope = createDefaultSaveScope(selected, "custom");
  reviewScopes.value.push(scope);
  scopeEvidence.value[scopeEvidenceKey(scope)] = { level: "review", reason: "手动添加的范围，请在确认前检查内容。" };
}

async function confirm() {
  if (!completedGame.value || !canConfirm.value || confirming.value) return;
  error.value = "";
  confirming.value = true;
  try {
    await confirmSaveProfile(completedGame.value.gameUid, reviewScopes.value, confidence.value, learningResult.value?.eventCaptureMode ?? null);
    completedGame.value = await getGame(completedGame.value.gameUid);
    if (!completedGame.value) throw new Error("存档保护已保存，但游戏记录读取失败");
    phase.value = "done";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    confirming.value = false;
  }
}

async function cancelTaskOrLearning() {
  if (cancelling.value) return;
  cancelling.value = true;
  error.value = "";
  if (taskId.value) {
    try {
      await cancelTask(taskId.value);
      message.value = "正在停止识别并清理临时数据...";
    } catch (reason) {
      error.value = String(reason);
      cancelling.value = false;
    }
    return;
  }
  try {
    const sessionId = session.value?.sessionId;
    if (sessionId) await cancelSaveLearning(sessionId);
    stopPolling();
    taskId.value = "";
    session.value = null;
    if (validatingCandidates.value) {
      validatingCandidates.value = false;
      phase.value = "review";
      message.value = "已停止候选验证，可继续调整范围。";
    } else {
      phase.value = "ready";
      message.value = "已停止存档识别，可重新开始。";
    }
  } catch (reason) {
    error.value = String(reason);
  } finally {
    cancelling.value = false;
  }
}

async function abandonPendingGame() {
  if (!completedGame.value) {
    emit("back");
    return;
  }
  if (phase.value === "analyzing") {
    error.value = "请先等待或取消当前分析任务，再放弃这次添加";
    return;
  }
  try {
    if (session.value?.sessionId) {
      await cancelSaveLearning(session.value.sessionId);
      session.value = null;
    }
    await discardPendingGame(completedGame.value.gameUid);
    emit("back");
  } catch (reason) {
    error.value = String(reason);
  }
}

onUnmounted(stopPolling);
</script>

<template>
  <section class="wizard-page page-enter">
    <header class="wizard-header">
      <button class="icon-button" type="button" title="返回游戏库" aria-label="返回游戏库" :disabled="isBusy" @click="abandonPendingGame"><ArrowLeft :size="18" /></button>
      <div><p class="eyebrow">添加游戏</p><h1>把游戏加入 GameSaver</h1><p>先复制游戏本体，再确认它的存档保护范围。</p></div>
    </header>

    <div class="step-indicator">
      <span class="step" :class="{ active: stepNumber >= 1 }">1</span><i></i><span class="step" :class="{ active: stepNumber >= 2 }">2</span><i></i><span class="step" :class="{ active: stepNumber >= 3 }">3</span><i></i><span class="step" :class="{ active: stepNumber >= 4 }">4</span>
      <div><strong>{{ stepTitle }}</strong><small>第 {{ stepNumber }} 步，共 4 步</small></div>
    </div>

    <form v-if="phase === 'form' || phase === 'copying'" class="wizard-form" @submit.prevent="start">
      <section class="wizard-section"><h2>游戏信息</h2><label class="field"><span>游戏名称</span><input v-model="displayName" :disabled="phase === 'copying'" type="text" placeholder="例如：Black Market" @input="!gameKey && (gameKey = normalizeGameKey(displayName))" /></label><label class="field"><span>游戏标识</span><input v-model="gameKey" :disabled="phase === 'copying'" type="text" placeholder="用于关联云端游戏" /><small class="field-note">默认由游戏名称生成，确认后不随显示名称变化。</small></label></section>
      <section class="wizard-section"><h2>游戏本体目录</h2><p class="field-note">GameSaver 会复制一份本体到自己的游戏库，原始目录不会被修改。</p><div class="path-row"><input v-model="sourcePath" :disabled="phase === 'copying'" type="text" placeholder="选择游戏所在文件夹" /><button type="button" :disabled="phase === 'copying'" title="选择游戏目录" @click="chooseSource"><FolderOpen :size="17" />选择</button></div></section>
      <section class="wizard-section"><h2>启动程序</h2><p class="field-note">启动程序必须位于游戏本体目录内。</p><div class="path-row"><input v-model="executablePath" :disabled="phase === 'copying'" type="text" placeholder="选择游戏 EXE" /><button type="button" :disabled="phase === 'copying'" title="选择启动程序" @click="chooseExecutable"><Gamepad2 :size="17" />选择</button></div></section>
      <div v-if="phase === 'copying'" class="task-progress"><div class="task-progress-heading"><span>{{ message || "正在处理" }}</span><strong>{{ progress }}%</strong></div><div class="progress-track"><span :style="{ width: `${progress}%` }"></span></div><button class="secondary-button" type="button" :disabled="cancelling" @click="cancelTaskOrLearning"><LoaderCircle v-if="cancelling" :size="16" class="spin" /><X v-else :size="16" />{{ cancelling ? "正在取消" : "取消复制" }}</button></div>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="phase === 'copying'" @click="emit('back')">取消</button><button class="primary-button" type="submit" :disabled="!canStart"><LoaderCircle v-if="phase === 'copying'" :size="17" class="spin" />{{ phase === 'copying' ? "正在复制" : "开始添加" }}</button></footer>
    </form>

    <section v-else-if="phase === 'ready' || phase === 'capturing'" class="wizard-form">
      <section class="wizard-section learning-intro"><div class="section-icon"><Gamepad2 :size="22" /></div><div><h2>{{ completedGame?.displayName }} 的存档保护</h2><p>{{ validatingCandidates ? "只验证待确认的候选目录。请在游戏内再次完成一次保存。" : "启动受管游戏，在游戏内完成一次保存。建议保存后退出游戏再点击分析，确保数据完整落盘。" }}</p></div></section>
      <section class="wizard-section"><div class="task-progress-heading"><span>{{ validatingCandidates ? "再次验证会话" : "学习会话" }}</span><strong v-if="session">PID {{ session.rootPid }}</strong><strong v-else>尚未启动</strong></div><p v-if="phase === 'ready'" class="field-note">只会记录本次学习期间的文件变化，不会立即创建正式存档版本。</p><p v-else class="field-note">在游戏内完成一次保存后，建议先退出游戏，再点击分析；也可以直接点击分析。</p><div v-if="phase === 'capturing'" class="capture-state"><span class="loader"></span><strong>{{ validatingCandidates ? "正在验证候选范围" : "正在记录文件变化" }}</strong><span>{{ message }}</span></div></section>
      <section v-if="phase === 'ready'" class="wizard-section inferred-scopes">
        <div class="editor-label">
          <strong><FolderSearch :size="15" /> 已推断的候选目录</strong>
          <span v-if="inferringDrafts">正在推断</span>
          <span v-else-if="inferredDrafts">{{ inferredDrafts.scopeDrafts.length }} 个</span>
        </div>
        <p v-if="inferringDrafts" class="scope-note">正在按目录名与文件特征推断，不影响你继续启动识别。</p>
        <template v-else-if="inferredDrafts && inferredDrafts.scopeDrafts.length">
          <ul class="inferred-scope-list">
            <li v-for="(draft, index) in inferredDrafts.scopeDrafts.slice(0, 5)" :key="`${draft.scope.rootPath}-${index}`">
              <span class="inferred-root-label">{{ rootTypeLabel[draft.scope.rootType] ?? draft.scope.rootType }}</span>
              <code :title="draft.scope.rootPath">{{ draft.scope.rootPath }}</code>
            </li>
          </ul>
          <p v-if="inferredDrafts.scopeDrafts.length > 5" class="scope-note">另有 {{ inferredDrafts.scopeDrafts.length - 5 }} 个候选目录，可在初稿审阅里逐个查看。</p>
          <p class="scope-note">这是<strong>只读推断</strong>：没有启动游戏、没有任何写入证据，仅按目录名与文件特征判断。可以先看一眼，再启动识别做正式确认。</p>
          <div class="inline-editor"><button class="secondary-button" type="button" :disabled="previewing" @click="openInferredDraft"><ArrowLeft :size="15" />查看并编辑这份初稿</button></div>
        </template>
        <p v-else-if="inferredDrafts" class="scope-note">没有按目录名推断出存档目录：候选目录名里没有出现游戏名时，普通权限发现不了它。启动识别后可以手动添加存档目录。</p>
      </section>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="cancelling" @click="abandonPendingGame">放弃添加</button><button v-if="phase === 'capturing'" class="secondary-button" type="button" :disabled="cancelling" @click="cancelTaskOrLearning"><LoaderCircle v-if="cancelling" :size="16" class="spin" /><X v-else :size="16" />{{ cancelling ? "正在停止" : `停止${validatingCandidates ? "验证" : "识别"}` }}</button><button v-if="phase === 'ready'" class="secondary-button" type="button" :disabled="previewing || inferringDrafts" title="不启动游戏，直接按存档目录名与文件特征推断一份待确认的初稿" @click="previewDraft"><LoaderCircle v-if="previewing" :size="16" class="spin" /><FolderOpen v-else :size="16" />{{ previewing ? "正在推断" : "跳过识别，先看初稿" }}</button><button v-if="phase === 'ready'" class="primary-button" type="button" :disabled="previewing" @click="beginLearning"><Gamepad2 :size="17" />启动并开始识别</button><button v-else class="primary-button" type="button" @click="analyze"><Check :size="17" />完成保存，开始{{ validatingCandidates ? "验证" : "分析" }}</button></footer>
    </section>

    <section v-else-if="phase === 'analyzing'" class="wizard-form">
      <section class="wizard-section analysis-state"><span class="loader"></span><h2>正在分析存档变化</h2><p>{{ message || "正在整理候选文件夹" }}</p><div class="progress-track"><span :style="{ width: `${progress}%` }"></span></div><strong>{{ progress }}%</strong></section>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="cancelling" @click="cancelTaskOrLearning"><LoaderCircle v-if="cancelling" :size="16" class="spin" /><X v-else :size="16" />{{ cancelling ? "正在取消" : "取消分析" }}</button></footer>
    </section>

    <section v-else-if="phase === 'review'" class="wizard-form">
      <section class="wizard-section result-summary"><div><p class="eyebrow">{{ isPreviewDraft ? "只读初稿" : "识别结果" }}</p><h2>确认存档保护范围</h2><p v-if="isPreviewDraft">没有启动游戏，也没有记录任何写入证据；以下 {{ reviewScopes.length }} 个候选范围只按目录名与文件特征推断，请逐项确认，或改回完整识别。</p><p v-else>{{ learningResult?.changedFiles.length || 0 }} 个文件发生变化，已按目录整理为 {{ reviewScopes.length }} 个候选范围。</p><div class="evidence-summary"><span>{{ isPreviewDraft ? "只读推断 · 无写入证据" : learningResult?.eventCaptureMode === "etw" ? "ETW + 快照证据" : "快照差异证据" }}</span><span v-if="learningResult?.transactionSummary">事务 {{ learningResult.transactionSummary.transactionCount }} 个 · {{ learningResult.transactionSummary.operationCount }} 条操作 · {{ learningResult.transactionSummary.status === "completed" ? "已确认" : learningResult.transactionSummary.status === "candidate" ? "候选" : "证据不足" }}</span></div></div><div v-if="isPreviewDraft" class="confidence-score"><strong>初稿</strong><span>只读推断 · 未经写入证据校验</span></div><div v-else class="confidence-score"><strong>{{ confidenceBand }}</strong><span>识别置信度 · 评分 {{ confidence }}/100</span></div></section>
      <section v-for="(scope, scopeIndex) in reviewScopes" :key="`${scope.rootPath}-${scopeIndex}`" class="wizard-section scope-editor">
        <header class="scope-heading">
          <div>
            <span class="scope-type">{{ rootTypeLabel[scope.rootType] }}</span>
            <button class="policy-badge policy-toggle" :class="scope.unknownFilePolicy === 'protect' ? 'policy-protect' : 'policy-ignore'" type="button" :title="scope.unknownFilePolicy === 'protect' ? '当前：目录里像存档的新文件会自动纳入保护。点一下改为「仅保护已确认文件」' : '当前：只备份已确认的文件。点一下改为「自动保护新存档」'" @click="toggleScopePolicy(scopeIndex)">{{ scope.unknownFilePolicy === 'protect' ? '自动保护新存档' : '仅保护已确认文件' }}</button>
            <span class="policy-badge" :class="evidenceForScope(scope).level === 'strong' ? 'policy-protect' : 'policy-ignore'" :title="evidenceForScope(scope).reason">{{ evidenceLabel(scope) }}</span>
            <h2>{{ formatScopeDisplay(scope) }}</h2>
            <small v-if="scope.rootType === 'managed_game' && formatScopeDisplay(scope) !== cleanDisplayPath(scope.rootPath)" class="scope-subtitle" :title="cleanDisplayPath(scope.rootPath)">实际物理路径：{{ cleanDisplayPath(scope.rootPath) }}</small>
          </div>
          <div class="scope-heading-actions">
            <button class="secondary-button compact-button" type="button" title="在文件资源管理器中打开这个存档目录" @click="openFolder(cleanDisplayPath(scope.rootPath))">
              <FolderOpen :size="15" />打开目录
            </button>
            <button class="icon-button danger-icon" type="button" title="删除这个保护范围" :aria-label="`删除 ${scope.rootPath}`" @click="removeScope(scopeIndex)">
              <Trash2 :size="16" />
            </button>
          </div>
        </header>
        <div class="editor-block"><div class="editor-label"><strong>保护文件</strong><span>{{ scope.confirmedFiles.length }} 项</span></div><div class="chip-list"><span v-for="(file, fileIndex) in scope.confirmedFiles" :key="file" class="file-chip">{{ file }}<button type="button" :aria-label="`删除 ${file}`" title="删除文件" @click="removeFile(scopeIndex, fileIndex)"><X :size="13" /></button></span><span v-if="!scope.confirmedFiles.length && !scope.includeDirectories.length" class="muted-text">暂无确认文件</span></div><div class="inline-editor"><input v-model="newFileByScope[scopeIndex]" type="text" placeholder="输入相对文件名，例如 save.dat" @keyup.enter="addFile(scopeIndex)" /><button class="secondary-button" type="button" @click="addFile(scopeIndex)"><Plus :size="15" />添加文件</button></div></div>
        <div v-if="scope.unknownFilePolicy !== 'protect' && proposedByScope[scopeEvidenceKey(scope)]?.length" class="editor-block proposed-block"><div class="editor-label"><strong>疑似存档（本次未变化）</strong><span>{{ proposedByScope[scopeEvidenceKey(scope)].length }} 项</span></div><div class="chip-list"><span v-for="file in proposedByScope[scopeEvidenceKey(scope)].slice(0, 12)" :key="file" class="file-chip proposed-chip">{{ file }}</span><span v-if="proposedByScope[scopeEvidenceKey(scope)].length > 12" class="muted-text">另有 {{ proposedByScope[scopeEvidenceKey(scope)].length - 12 }} 项</span></div><p class="scope-note">这些文件看起来也是存档，但本次学习没有发生变化。纳入后会一起备份并受保护；如果不属于这个游戏，忽略即可。把上方徽章切回「自动保护新存档」就不必逐个确认。</p><div class="inline-editor"><button class="secondary-button" type="button" @click="adoptProposedFiles(scopeIndex)"><Plus :size="15" />全部纳入保护</button></div></div>
        <div v-if="scope.includeDirectories.length" class="editor-block"><div class="editor-label"><strong>保护目录</strong><span>{{ scope.includeDirectories.length }} 项</span></div><div class="chip-list"><span v-for="directory in scope.includeDirectories" :key="directory" class="file-chip directory-chip">{{ directory }}</span></div></div>
        <div v-if="scope.excludeDirectories.length" class="editor-block"><div class="editor-label"><strong>排除目录</strong><span>{{ scope.excludeDirectories.length }} 项</span></div><div class="chip-list"><span v-for="(dir, dirIndex) in scope.excludeDirectories" :key="dir" class="file-chip exclude-dir-chip">{{ dir }}<button type="button" :aria-label="`删除排除目录 ${dir}`" title="删除排除目录" @click="removeExcludeDirectory(scopeIndex, dirIndex)"><X :size="13" /></button></span></div></div>
        <div v-if="scope.excludeExact.length" class="editor-block"><div class="editor-label"><strong>排除特定文件</strong><span>{{ scope.excludeExact.length }} 项</span></div><div class="chip-list"><span v-for="(exact, exactIndex) in scope.excludeExact" :key="exact" class="file-chip exclude-chip">{{ exact }}<button type="button" :aria-label="`删除排除文件 ${exact}`" title="删除排除文件" @click="removeExcludeExact(scopeIndex, exactIndex)"><X :size="13" /></button></span></div></div>
        <div class="editor-block"><div class="editor-label"><strong>排除模式</strong><span>{{ scope.excludePatterns.length }} 项</span></div><div class="chip-list"><span v-for="(pattern, patternIndex) in scope.excludePatterns" :key="pattern" class="file-chip exclude-chip">{{ pattern }}<button type="button" :aria-label="`删除排除模式 ${pattern}`" title="删除排除模式" @click="removePattern(scopeIndex, patternIndex)"><X :size="13" /></button></span><span v-if="!scope.excludePatterns.length" class="muted-text">暂未添加排除模式</span></div><div class="inline-editor"><input v-model="newPatternByScope[scopeIndex]" type="text" placeholder="输入排除模式，例如 *.log" @keyup.enter="addPattern(scopeIndex)" /><button class="secondary-button" type="button" @click="addPattern(scopeIndex)"><Plus :size="15" />添加排除</button></div></div>
        <p class="scope-note">{{ evidenceForScope(scope).reason }}</p>
      </section>
      <div v-if="!reviewScopes.length" class="empty-review"><strong>没有自动识别到存档范围</strong><p>可以手动添加一个存档目录，或放弃本次设置稍后重新学习。</p></div>
      <button class="secondary-button add-scope-button" type="button" @click="addDirectoryScope"><Plus :size="16" />手动添加存档目录</button>
      <div v-if="learningResult?.notes.length" class="notes-panel"><strong>识别说明</strong><p v-for="note in learningResult.notes" :key="note">{{ note }}</p></div>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="confirming" @click="abandonPendingGame">放弃添加</button><button v-if="isPreviewDraft" class="secondary-button" type="button" :disabled="confirming" title="放弃这份只读初稿，回到上一步启动游戏做完整识别" @click="backToFullLearning"><ArrowLeft :size="16" />改回完整识别</button><button v-if="hasReviewCandidates" class="secondary-button" type="button" :disabled="confirming" @click="beginCandidateVerification"><Gamepad2 :size="17" />再次保存验证</button><button class="primary-button" type="button" :disabled="!canConfirm || confirming" @click="confirm"><LoaderCircle v-if="confirming" :size="17" class="spin" /><Check v-else :size="17" />{{ confirming ? "正在保存" : "确认并加入游戏库" }}</button></footer>
    </section>

    <div v-else class="wizard-success"><CheckCircle2 :size="34" /><div><h2>{{ completedGame?.displayName }} 已加入游戏库</h2><p>存档保护范围已确认，现在可以从游戏库启动它。</p></div><button class="primary-button" type="button" @click="emit('completed', completedGame!)">返回游戏库</button></div>
  </section>

  <Teleport to="body">
    <div v-if="showLargeConfirmModal" class="cover-editor-overlay" @click.self="handleCancelLargeSource">
      <section class="confirm-dialog" role="dialog" aria-modal="true" aria-label="游戏大小确认">
        <header class="confirm-dialog-header">
          <div class="confirm-dialog-title">
            <AlertTriangle :size="20" class="warning-icon" />
            <h2>游戏本体大小确认</h2>
          </div>
          <button class="icon-button" type="button" title="关闭" aria-label="关闭" @click="handleCancelLargeSource"><X :size="18" /></button>
        </header>
        <div class="confirm-dialog-content">
          <p>{{ largeConfirmMessage }}</p>
          <p class="field-note">如果此目录确实是游戏本体且大小正常，请点击「确认继续」；若误选了上级文件夹（如整个盘符或多游戏合集），请点击「取消」重新选择。</p>
        </div>
        <footer class="confirm-dialog-footer">
          <button class="secondary-button" type="button" @click="handleCancelLargeSource">取消并重新选择</button>
          <button class="primary-button" type="button" @click="handleConfirmLargeSource">确认继续复制</button>
        </footer>
      </section>
    </div>
  </Teleport>
</template>
