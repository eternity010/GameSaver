import { computed, ref } from "vue";
import {
  confirmRule,
  getLearningSession,
  getRuntimeStatus,
  launchGame,
  openCandidatePath,
  restartAsAdmin,
  startFinishLearningTask,
  startLearning,
} from "../api";
import type { CandidatePath, TaskState } from "../types";

export type UiStep = "setup" | "running" | "results";
export type LearningBusyStage = "" | "starting" | "analyzing" | "saving";

type TabState = {
  loading: boolean;
  error: string;
};

type WaitForTaskCompletion = <T>(
  taskId: string,
  onProgress?: (message: string, progress: number | null) => void,
) => Promise<TaskState<T>>;

function cleanInferredGameId(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function isGenericExeName(value: string): boolean {
  const normalized = value.trim().toLowerCase();
  return [
    "game",
    "game.exe",
    "main",
    "launcher",
    "start",
    "play",
    "nw",
    "node-webkit",
    "rpg_rt",
    "rpgvxace",
    "rpgmaker",
    "unitycrashhandler64",
    "unitycrashhandler32",
  ].includes(normalized) || normalized.endsWith("-win64-shipping") || normalized.endsWith("-win32-shipping");
}

function isGenericPathSegment(value: string): boolean {
  const normalized = value.trim().toLowerCase();
  return [
    "bin",
    "binaries",
    "win64",
    "win32",
    "x64",
    "x86",
    "windows",
    "release",
    "debug",
    "build",
    "dist",
  ].includes(normalized);
}

function inferGameId(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const fileName = parts[parts.length - 1] ?? "";
  const exeName = fileName.toLowerCase().endsWith(".exe") ? fileName.slice(0, -4) : fileName;
  const commonIndex = parts.findIndex((part) => part.toLowerCase() === "common");
  if (commonIndex >= 0 && commonIndex + 1 < parts.length - 1) {
    return cleanInferredGameId(parts[commonIndex + 1]);
  }
  if (!isGenericExeName(exeName)) {
    return cleanInferredGameId(exeName);
  }
  for (let index = parts.length - 2; index >= 0; index -= 1) {
    const candidate = parts[index] ?? "";
    if (candidate && !isGenericPathSegment(candidate)) {
      return cleanInferredGameId(candidate);
    }
  }
  return cleanInferredGameId(exeName || fileName);
}

export function useLearningPage(options: {
  waitForTaskCompletion: WaitForTaskCompletion;
  showToast: (message: string, level?: "success" | "error" | "info", timeoutMs?: number) => void;
  afterRuleSaved: (gameId: string) => Promise<void>;
}) {
  const step = ref<UiStep>("setup");
  const gameId = ref("");
  const exePath = ref("");
  const extraScanRootsText = ref("");
  const sessionId = ref("");
  const pid = ref<number | null>(null);
  const candidates = ref<CandidatePath[]>([]);
  const selected = ref<string[]>([]);
  const learningState = ref<TabState>({ loading: false, error: "" });
  const learningBusyStage = ref<LearningBusyStage>("");
  const learningTaskMessage = ref("");
  const learningTaskProgress = ref<number | null>(null);
  const eventCaptureMode = ref("unknown");
  const capturedEventCount = ref(0);
  const eventCaptureError = ref("");
  const runtimeIsAdmin = ref(false);
  const runtimeMessage = ref("");

  const hasHighConfidence = computed(() => candidates.value.some((item) => item.score >= 45));

  function toggleSelect(path: string) {
    if (selected.value.includes(path)) {
      selected.value = selected.value.filter((item) => item !== path);
      return;
    }
    selected.value = [...selected.value, path];
  }

  async function chooseExePath() {
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const chosen = await open({
        multiple: false,
        filters: [{ name: "Executable", extensions: ["exe"] }],
      });
      if (!chosen || Array.isArray(chosen)) return;
      exePath.value = chosen;
      if (!gameId.value.trim()) {
        gameId.value = inferGameId(chosen);
      }
    } catch (err) {
      learningState.value.error = `无法打开文件选择器：${String(err)}`;
    }
  }

  async function chooseExtraScanRoot() {
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const chosen = await open({
        multiple: false,
        directory: true,
      });
      if (!chosen || Array.isArray(chosen)) return;
      const current = extraScanRootsText.value
        .split(/\r?\n/)
        .map((item) => item.trim())
        .filter(Boolean);
      if (!current.includes(chosen)) {
        current.push(chosen);
        extraScanRootsText.value = current.join("\n");
      }
    } catch (err) {
      learningState.value.error = `无法打开目录选择器：${String(err)}`;
    }
  }

  async function beginLearning() {
    learningBusyStage.value = "starting";
    learningState.value.loading = true;
    learningState.value.error = "";
    try {
      const trimmedGameId = gameId.value.trim();
      const trimmedExePath = exePath.value.trim();
      const extraScanRoots = extraScanRootsText.value
        .split(/\r?\n/)
        .map((item) => item.trim())
        .filter(Boolean);
      if (!trimmedGameId || !trimmedExePath) {
        throw new Error("请先填写 gameId 并选择 exePath");
      }
      sessionId.value = await startLearning(trimmedGameId, trimmedExePath, extraScanRoots);
      pid.value = await launchGame(sessionId.value);
      step.value = "running";
    } catch (err) {
      learningState.value.error = String(err);
    } finally {
      learningState.value.loading = false;
      learningBusyStage.value = "";
    }
  }

  async function endLearning() {
    learningBusyStage.value = "analyzing";
    learningState.value.loading = true;
    learningState.value.error = "";
    learningTaskMessage.value = "任务已创建，准备分析存档变化...";
    learningTaskProgress.value = null;
    try {
      const taskId = await startFinishLearningTask(sessionId.value);
      const finalTask = await options.waitForTaskCompletion<CandidatePath[]>(
        taskId,
        (message, progress) => {
          learningTaskMessage.value = message || "正在分析存档变化...";
          learningTaskProgress.value = progress;
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "结束学习失败");
      }
      const taskResult = finalTask.result;
      candidates.value = Array.isArray(taskResult) ? taskResult : [];
      const session = await getLearningSession(sessionId.value);
      eventCaptureMode.value = session.eventCaptureMode ?? "unknown";
      capturedEventCount.value = session.capturedEventCount ?? 0;
      eventCaptureError.value = session.eventCaptureError ?? "";
      const autoSelectable = candidates.value.filter(
        (item) => item.recommendation === "strong" || item.recommendation === "recommended",
      );
      selected.value = autoSelectable.slice(0, 2).map((item) => item.path);
      step.value = "results";
      if (!hasHighConfidence.value) {
        options.showToast("未检测到高可信候选，请确认学习阶段已执行存档动作", "info", 3600);
      }
    } catch (err) {
      learningState.value.error = String(err);
    } finally {
      learningState.value.loading = false;
      learningBusyStage.value = "";
      learningTaskMessage.value = "";
      learningTaskProgress.value = null;
    }
  }

  async function saveLearningRule() {
    if (!selected.value.length) {
      learningState.value.error = "请至少选择一个候选路径。";
      return;
    }
    learningBusyStage.value = "saving";
    learningState.value.loading = true;
    learningState.value.error = "";
    try {
      await confirmRule(sessionId.value, selected.value);
      options.showToast("规则保存成功", "success");
      await options.afterRuleSaved(gameId.value.trim());
    } catch (err) {
      learningState.value.error = String(err);
      options.showToast("规则保存失败", "error");
    } finally {
      learningState.value.loading = false;
      learningBusyStage.value = "";
    }
  }

  async function openPath(path: string) {
    learningState.value.error = "";
    try {
      await openCandidatePath(path);
    } catch (err) {
      learningState.value.error = `打开目录失败：${String(err)}`;
    }
  }

  async function loadRuntimeStatus() {
    try {
      const status = await getRuntimeStatus();
      runtimeIsAdmin.value = status.isAdmin;
      runtimeMessage.value = status.message;
    } catch (err) {
      runtimeMessage.value = `运行状态读取失败：${String(err)}`;
    }
  }

  async function relaunchAsAdmin() {
    learningState.value.error = "";
    try {
      await restartAsAdmin();
    } catch (err) {
      learningState.value.error = `管理员重启失败：${String(err)}`;
    }
  }

  return {
    step,
    gameId,
    exePath,
    extraScanRootsText,
    sessionId,
    pid,
    candidates,
    selected,
    learningState,
    learningBusyStage,
    learningTaskMessage,
    learningTaskProgress,
    eventCaptureMode,
    capturedEventCount,
    eventCaptureError,
    runtimeIsAdmin,
    runtimeMessage,
    chooseExePath,
    chooseExtraScanRoot,
    beginLearning,
    endLearning,
    toggleSelect,
    openPath,
    saveLearningRule,
    loadRuntimeStatus,
    relaunchAsAdmin,
  };
}
