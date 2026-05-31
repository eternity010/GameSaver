<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useLibraryPage } from "./composables/useLibraryPage";
import { useToast } from "./composables/useToast";
import LearningPage from "./components/learning/LearningPage.vue";
import LibraryPage from "./components/library/LibraryPage.vue";
import RulesPage from "./components/rules/RulesPage.vue";
import SettingsPage from "./components/SettingsPage.vue";
import AppToast from "./components/ui/AppToast.vue";
import BlockingErrorDialog from "./components/ui/BlockingErrorDialog.vue";
import ConfirmDialog from "./components/ui/ConfirmDialog.vue";
import {
  confirmRule,
  deleteRule,
  exportRules,
  getSettingsPaths,
  getTask,
  getLearningSession,
  getRuntimeStatus,
  importRules,
  launchGame,
  listRuleConflicts,
  listRules,
  openCandidatePath,
  restartAsAdmin,
  setPrimaryRule,
  startMigrateDataPathTask,
  startExportMigrationZipTask,
  startFinishLearningTask,
  startImportMigrationZipTask,
  startLearning,
  updateSettingsPaths,
  updateRule,
} from "./api";
import type {
  CandidatePath,
  DataPathKind,
  DataPathMigrationResult,
  ExportMigrationZipResult,
  GameSaveRule,
  ImportMigrationZipResult,
  RuleConflictItem,
  SettingsPaths,
} from "./types";

type UiStep = "setup" | "running" | "results";
type TopTab = "learning" | "rules" | "library" | "settings";
type RuleDraft = {
  gameIdText: string;
  confirmedPathsText: string;
  enabled: boolean;
};
type TabState = {
  loading: boolean;
  error: string;
};
type ConfirmDialogState = {
  open: boolean;
  title: string;
  message: string;
  confirmText: string;
  cancelText: string;
  danger: boolean;
};
type LearningBusyStage = "" | "starting" | "analyzing" | "saving";

const step = ref<UiStep>("setup");
const activeTab = ref<TopTab>("library");
const gameId = ref("");
const exePath = ref("");
const extraScanRootsText = ref("");
const sessionId = ref("");
const pid = ref<number | null>(null);
const candidates = ref<CandidatePath[]>([]);
const selected = ref<string[]>([]);
const rules = ref<GameSaveRule[]>([]);
const ruleConflicts = ref<RuleConflictItem[]>([]);
const ruleSearch = ref("");
const ruleDrafts = ref<Record<string, RuleDraft>>({});
const learningState = ref<TabState>({ loading: false, error: "" });
const rulesState = ref<TabState>({ loading: false, error: "" });
const settingsState = ref<TabState>({ loading: false, error: "" });
const migrationExportWaiting = ref(false);
const migrationExportMessage = ref("");
const migrationExportProgress = ref<number | null>(null);
const migrationImportWaiting = ref(false);
const migrationImportMessage = ref("");
const migrationImportProgress = ref<number | null>(null);
const settings = ref<SettingsPaths | null>(null);
const backupRootDraft = ref("");
const settingsMigrationKind = ref<DataPathKind | "">("");
const settingsMigrationMessage = ref("");
const settingsMigrationProgress = ref<number | null>(null);
const learningBusyStage = ref<LearningBusyStage>("");
const learningTaskMessage = ref("");
const learningTaskProgress = ref<number | null>(null);
const eventCaptureMode = ref("unknown");
const capturedEventCount = ref(0);
const eventCaptureError = ref("");
const runtimeIsAdmin = ref(false);
const runtimeMessage = ref("");
const { toast, showToast, closeToast } = useToast();
const confirmDialog = ref<ConfirmDialogState>({
  open: false,
  title: "",
  message: "",
  confirmText: "确认",
  cancelText: "取消",
  danger: false,
});
const blockingErrorMessage = ref("");

let confirmResolver: ((value: boolean) => void) | null = null;

const hasHighConfidence = computed(() => candidates.value.some((item) => item.score >= 45));
const ruleConflictByRuleId = computed<Record<string, RuleConflictItem>>(() => {
  const map: Record<string, RuleConflictItem> = {};
  for (const conflict of ruleConflicts.value) {
    for (const ruleId of conflict.ruleIds) {
      map[ruleId] = conflict;
    }
  }
  return map;
});

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

function toggleSelect(path: string) {
  if (selected.value.includes(path)) {
    selected.value = selected.value.filter((item) => item !== path);
    return;
  }
  selected.value = [...selected.value, path];
}

function normalizePaths(rawText: string): string[] {
  const dedup = new Set<string>();
  const output: string[] = [];
  for (const line of rawText.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (!dedup.has(trimmed)) {
      dedup.add(trimmed);
      output.push(trimmed);
    }
  }
  return output;
}

function updateRuleDraft(ruleId: string, patch: Partial<RuleDraft>) {
  const current = ruleDrafts.value[ruleId];
  if (!current) return;
  ruleDrafts.value = {
    ...ruleDrafts.value,
    [ruleId]: {
      ...current,
      ...patch,
    },
  };
}

function ruleConflictFor(ruleId: string): RuleConflictItem | null {
  return ruleConflictByRuleId.value[ruleId] ?? null;
}

function hydrateRuleDrafts() {
  const next: Record<string, RuleDraft> = {};
  for (const rule of rules.value) {
    const previous = ruleDrafts.value[rule.ruleId];
    next[rule.ruleId] = {
      gameIdText: previous?.gameIdText ?? rule.gameId,
      confirmedPathsText: previous?.confirmedPathsText ?? rule.confirmedPaths.join("\n"),
      enabled: previous?.enabled ?? rule.enabled,
    };
  }
  ruleDrafts.value = next;
}

function sortRulesByUpdatedTime(items: GameSaveRule[]): GameSaveRule[] {
  return [...items].sort((a, b) => {
    const aTime = Number(a.updatedAt || a.createdAt || "0");
    const bTime = Number(b.updatedAt || b.createdAt || "0");
    return bTime - aTime;
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitForTaskCompletion<T>(
  taskId: string,
  onProgress?: (message: string, progress: number | null) => void,
) {
  const startedAt = Date.now();
  const timeoutMs = 3 * 60 * 1000;
  let lastPollError = "";
  while (true) {
    try {
      const task = await getTask<T>(taskId);
      lastPollError = "";
      const progressValue =
        typeof task.progress === "number" && Number.isFinite(task.progress) ? Math.max(0, Math.min(100, task.progress)) : null;
      onProgress?.(task.message ?? "", progressValue);
      if (task.status === "success" || task.status === "failed") {
        return task;
      }
    } catch (err) {
      lastPollError = String(err);
    }
    if (Date.now() - startedAt > timeoutMs) {
      if (lastPollError) {
        throw new Error(`任务状态轮询失败：${lastPollError}`);
      }
      throw new Error("任务执行超时，请重试");
    }
    await sleep(350);
  }
}

function showBlockingError(message: string) {
  blockingErrorMessage.value = message;
  showToast("操作失败，请查看错误详情", "error", 3200);
}

function closeBlockingError() {
  blockingErrorMessage.value = "";
}

function askConfirm(options: {
  title: string;
  message: string;
  confirmText?: string;
  cancelText?: string;
  danger?: boolean;
}) {
  return new Promise<boolean>((resolve) => {
    confirmResolver = resolve;
    confirmDialog.value = {
      open: true,
      title: options.title,
      message: options.message,
      confirmText: options.confirmText ?? "确认",
      cancelText: options.cancelText ?? "取消",
      danger: options.danger ?? false,
    };
  });
}

function resolveConfirm(result: boolean) {
  const resolver = confirmResolver;
  confirmResolver = null;
  confirmDialog.value.open = false;
  if (resolver) {
    resolver(result);
  }
}

const {
  libraryState,
  librarySearch,
  filteredLibraryItems,
  selectedLibraryItem,
  libraryCardErrorFor,
  isLibraryGameSelected,
  gameDirResolutionIssue,
  cardSyncStatusLabel,
  syncStatusClass,
  syncDecisionFor,
  gameDirStatusLabel,
  backupStatsFor,
  isCardBusy,
  launchPrecheckFor,
  selectedRuleAnchorTokens,
  visiblePrecheckChecks,
  backupKeepDraftFor,
  backupVersionsFor,
  restoreUndoFor,
  restoreTaskMessageFor,
  restoreTaskProgressFor,
  sessionDetailsFor,
  refreshLibraryItems,
  reloadLibraryWithLoading,
  selectLibraryGame,
  choosePreferredExeForGame,
  launchLibraryGame,
  updateBackupKeepDraft,
  saveBackupKeepPolicy,
  pruneOldBackupsForGame,
  rollbackToLibraryBackupVersion,
  undoLibraryRestore,
} = useLibraryPage({
  rules,
  waitForTaskCompletion,
  askConfirm,
  showToast,
  showBlockingError,
});

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
    const finalTask = await waitForTaskCompletion<CandidatePath[]>(
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
      showToast("未检测到高可信候选，请确认学习阶段已执行存档动作", "info", 3600);
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
    const ruleId = await confirmRule(sessionId.value, selected.value);
    void ruleId;
    showToast("规则保存成功", "success");
    await refreshRules();
    await refreshLibraryItems();
    activeTab.value = "library";
    await selectLibraryGame(gameId.value.trim());
  } catch (err) {
    learningState.value.error = String(err);
    showToast("规则保存失败", "error");
  } finally {
    learningState.value.loading = false;
    learningBusyStage.value = "";
  }
}

async function refreshRules() {
  const [data, conflicts] = await Promise.all([listRules(), listRuleConflicts()]);
  rules.value = sortRulesByUpdatedTime(data);
  ruleConflicts.value = conflicts;
  hydrateRuleDrafts();
}
async function reloadRulesWithLoading() {
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await refreshRules();
  } catch (err) {
    rulesState.value.error = String(err);
  } finally {
    rulesState.value.loading = false;
  }
}

async function markPrimaryRule(rule: GameSaveRule) {
  const conflict = ruleConflictFor(rule.ruleId);
  if (!conflict) {
    rulesState.value.error = "当前规则不存在 exeHash 冲突，无需设置主规则。";
    return;
  }
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await setPrimaryRule(rule.ruleId);
    await refreshRules();
    await refreshLibraryItems();
    showToast("主规则设置成功", "success");
  } catch (err) {
    rulesState.value.error = `设置主规则失败：${String(err)}`;
    showToast("设置主规则失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function saveManagedRule(rule: GameSaveRule) {
  const draft = ruleDrafts.value[rule.ruleId];
  if (!draft) return;
  const normalizedGameId = draft.gameIdText.trim();
  if (!normalizedGameId) {
    rulesState.value.error = "游戏名不能为空。";
    return;
  }
  const normalizedPaths = normalizePaths(draft.confirmedPathsText);
  if (!normalizedPaths.length) {
    rulesState.value.error = "路径不能为空，至少保留一条路径。";
    return;
  }

  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    const updated = await updateRule(rule.ruleId, normalizedGameId, normalizedPaths, draft.enabled);
    await refreshRules();
    showToast(`规则 ${updated.gameId} 已保存`, "success");
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `保存规则失败：${String(err)}`;
    showToast("保存规则失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function removeManagedRule(rule: GameSaveRule) {
  const confirmed = await askConfirm({
    title: "确认删除规则",
    message: `确定删除规则 ${rule.gameId} 吗？此操作不可恢复。`,
    confirmText: "删除",
    cancelText: "取消",
    danger: true,
  });
  if (!confirmed) {
    return;
  }
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await deleteRule(rule.ruleId);
    await refreshRules();
    showToast(`规则 ${rule.gameId} 已删除`, "success");
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `删除规则失败：${String(err)}`;
    showToast("删除规则失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function exportRulesToFile() {
  try {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const chosen = await save({
      defaultPath: "gamesaver-rules.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!chosen) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    const result = await exportRules(chosen);
    void result;
    showToast("规则导出成功", "success");
  } catch (err) {
    rulesState.value.error = `导出失败：${String(err)}`;
    showToast("规则导出失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function importRulesFromFile() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    const result = await importRules(chosen);
    await refreshRules();
    await refreshLibraryItems();
    void result;
    showToast("规则导入完成", "success");
  } catch (err) {
    rulesState.value.error = `导入失败：${String(err)}`;
    showToast("规则导入失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function exportMigrationZipToFile() {
  try {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const chosen = await save({
      defaultPath: "gamesaver-migration.zip",
      filters: [{ name: "ZIP", extensions: ["zip"] }],
    });
    if (!chosen) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    migrationExportWaiting.value = true;
    migrationExportMessage.value = "任务已创建，准备导出迁移包...";
    migrationExportProgress.value = 0;
    const taskId = await startExportMigrationZipTask(chosen);
    const finalTask = await waitForTaskCompletion<ExportMigrationZipResult>(
      taskId,
      (message, progress) => {
        migrationExportMessage.value = message || "正在导出迁移包...";
        migrationExportProgress.value = progress;
      },
    );
    if (finalTask.status === "failed") {
      throw new Error(finalTask.error || "导出迁移包失败");
    }
    const result = finalTask.result;
    if (result) {
      showToast(
        `迁移包导出成功（规则 ${result.ruleCount} 条，备份游戏 ${result.backupGames} 个，文件 ${result.exportedFiles} 个）`,
        "success",
        4200,
      );
    } else {
      showToast("迁移包导出成功", "success");
    }
  } catch (err) {
    rulesState.value.error = `导出迁移包失败：${String(err)}`;
    showToast("迁移包导出失败", "error");
  } finally {
    migrationExportWaiting.value = false;
    migrationExportMessage.value = "";
    migrationExportProgress.value = null;
    rulesState.value.loading = false;
  }
}

async function importMigrationZipFromFile() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "ZIP", extensions: ["zip"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    migrationImportWaiting.value = true;
    migrationImportMessage.value = "任务已创建，准备导入迁移包...";
    migrationImportProgress.value = 0;
    const taskId = await startImportMigrationZipTask(chosen);
    const finalTask = await waitForTaskCompletion<ImportMigrationZipResult>(
      taskId,
      (message, progress) => {
        migrationImportMessage.value = message || "正在导入迁移包...";
        migrationImportProgress.value = progress;
      },
    );
    if (finalTask.status === "failed") {
      throw new Error(finalTask.error || "导入迁移包失败");
    }
    const result = finalTask.result;
    await refreshRules();
    await refreshLibraryItems();
    if (result) {
      showToast(
        `迁移包导入完成（新增规则 ${result.importedRules}，覆盖 ${result.overwrittenRules}，导入备份游戏 ${result.importedBackupGames}）`,
        "success",
        4200,
      );
    } else {
      showToast("迁移包导入完成", "success");
    }
  } catch (err) {
    rulesState.value.error = `导入迁移包失败：${String(err)}`;
    showToast("迁移包导入失败", "error");
  } finally {
    migrationImportWaiting.value = false;
    migrationImportMessage.value = "";
    migrationImportProgress.value = null;
    rulesState.value.loading = false;
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

async function openDirectory(path: string) {
  if (!path.trim()) return;
  try {
    await openCandidatePath(path);
  } catch (err) {
    settingsState.value.error = `打开目录失败：${String(err)}`;
  }
}

async function reloadSettings() {
  settingsState.value.loading = true;
  settingsState.value.error = "";
  try {
    const data = await getSettingsPaths();
    settings.value = data;
    backupRootDraft.value = data.backupRoot;
  } catch (err) {
    settingsState.value.error = `读取设置失败：${String(err)}`;
  } finally {
    settingsState.value.loading = false;
  }
}

async function chooseSettingsDirectory(_kind: DataPathKind) {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      directory: true,
    });
    if (!chosen || Array.isArray(chosen)) return;
    backupRootDraft.value = chosen;
  } catch (err) {
    settingsState.value.error = `无法打开目录选择器：${String(err)}`;
  }
}

async function saveSettingsPath(_kind: DataPathKind) {
  settingsState.value.loading = true;
  settingsState.value.error = "";
  try {
    const input = { backupRoot: backupRootDraft.value.trim() };
    const updated = await updateSettingsPaths(input);
    settings.value = updated;
    backupRootDraft.value = updated.backupRoot;
    showToast("备份路径已保存", "success");
  } catch (err) {
    settingsState.value.error = `保存设置失败：${String(err)}`;
    showToast("保存设置失败", "error");
  } finally {
    settingsState.value.loading = false;
  }
}

async function migrateSettingsPath(kind: DataPathKind) {
  const targetPath = backupRootDraft.value.trim();
  const currentPath = settings.value ? settings.value.backupRoot : "";
  if (!targetPath || targetPath === currentPath) {
    return;
  }
  const confirmed = await askConfirm({
    title: "确认迁移数据目录",
    message:
      `将把当前目录内容复制到新位置：\n\n旧路径：${currentPath}\n新路径：${targetPath}\n\n迁移成功后才会切换配置，默认保留旧目录，不会自动删除。`,
    confirmText: "复制并切换",
    cancelText: "取消",
    danger: false,
  });
  if (!confirmed) return;

  settingsState.value.loading = true;
  settingsState.value.error = "";
  settingsMigrationKind.value = kind;
  settingsMigrationMessage.value = "任务已创建，准备迁移数据目录...";
  settingsMigrationProgress.value = 0;
  try {
    const taskId = await startMigrateDataPathTask(kind, targetPath);
    const finalTask = await waitForTaskCompletion<DataPathMigrationResult>(
      taskId,
      (message, progress) => {
        settingsMigrationMessage.value = message || "正在迁移数据目录...";
        settingsMigrationProgress.value = progress;
      },
    );
    if (finalTask.status === "failed") {
      throw new Error(finalTask.error || "迁移数据目录失败");
    }
    await reloadSettings();
    const result = finalTask.result;
    if (result) {
      showToast(
        `迁移完成：复制 ${result.copiedFiles} 个文件到 ${result.targetPath}，旧目录已保留`,
        "success",
        4200,
      );
    } else {
      showToast("数据目录迁移完成", "success");
    }
  } catch (err) {
    settingsState.value.error = `迁移数据目录失败：${String(err)}`;
    showToast("数据目录迁移失败", "error");
  } finally {
    settingsMigrationKind.value = "";
    settingsMigrationMessage.value = "";
    settingsMigrationProgress.value = null;
    settingsState.value.loading = false;
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

onMounted(() => {
  void loadRuntimeStatus();
  void reloadRulesWithLoading();
  void reloadLibraryWithLoading();
  void reloadSettings();
});

onUnmounted(() => {
  confirmResolver = null;
});
</script>

<template>
  <main class="layout">
    <nav class="tabs panel">
      <button
        class="tab"
        :class="{ active: activeTab === 'library' }"
        type="button"
        @click="activeTab = 'library'"
      >
        游戏库
      </button>
      <button
        class="tab"
        :class="{ active: activeTab === 'learning' }"
        type="button"
        @click="activeTab = 'learning'"
      >
        学习存档
      </button>
      <button
        class="tab"
        :class="{ active: activeTab === 'rules' }"
        type="button"
        @click="activeTab = 'rules'"
      >
        规则管理
      </button>
      <button
        class="tab"
        :class="{ active: activeTab === 'settings' }"
        type="button"
        @click="activeTab = 'settings'"
      >
        设置
      </button>
    </nav>

    <LearningPage
      v-if="activeTab === 'learning'"
      :step="step"
      :game-id="gameId"
      :exe-path="exePath"
      :extra-scan-roots-text="extraScanRootsText"
      :session-id="sessionId"
      :pid="pid"
      :candidates="candidates"
      :selected-paths="selected"
      :learning-state="learningState"
      :learning-busy-stage="learningBusyStage"
      :learning-task-message="learningTaskMessage"
      :learning-task-progress="learningTaskProgress"
      :event-capture-mode="eventCaptureMode"
      :captured-event-count="capturedEventCount"
      :event-capture-error="eventCaptureError"
      :runtime-is-admin="runtimeIsAdmin"
      :runtime-message="runtimeMessage"
      @update:game-id="gameId = $event"
      @update:exe-path="exePath = $event"
      @update:extra-scan-roots-text="extraScanRootsText = $event"
      @update:step="step = $event"
      @choose-exe="chooseExePath"
      @choose-extra-scan-root="chooseExtraScanRoot"
      @begin-learning="beginLearning"
      @end-learning="endLearning"
      @toggle-select="toggleSelect"
      @open-path="openPath"
      @save-learning-rule="saveLearningRule"
      @relaunch-as-admin="relaunchAsAdmin"
    />

    <RulesPage
      v-else-if="activeTab === 'rules'"
      :rules="rules"
      :rule-conflicts="ruleConflicts"
      :rule-search="ruleSearch"
      :rule-drafts="ruleDrafts"
      :rules-state="rulesState"
      :migration-export-waiting="migrationExportWaiting"
      :migration-export-message="migrationExportMessage"
      :migration-export-progress="migrationExportProgress"
      :migration-import-waiting="migrationImportWaiting"
      :migration-import-message="migrationImportMessage"
      :migration-import-progress="migrationImportProgress"
      @update:rule-search="ruleSearch = $event"
      @update:rule-draft="updateRuleDraft($event.ruleId, $event.patch)"
      @reload="reloadRulesWithLoading"
      @export-rules="exportRulesToFile"
      @import-rules="importRulesFromFile"
      @export-migration="exportMigrationZipToFile"
      @import-migration="importMigrationZipFromFile"
      @mark-primary="markPrimaryRule"
      @save-rule="saveManagedRule"
      @remove-rule="removeManagedRule"
    />

    <SettingsPage
      v-else-if="activeTab === 'settings'"
      :settings="settings"
      :settings-state="settingsState"
      :backup-root-draft="backupRootDraft"
      :migration-kind="settingsMigrationKind"
      :migration-message="settingsMigrationMessage"
      :migration-progress="settingsMigrationProgress"
      @update:backup-root-draft="backupRootDraft = $event"
      @reload="reloadSettings"
      @choose-directory="chooseSettingsDirectory"
      @open-directory="openDirectory"
      @save-path="saveSettingsPath"
      @migrate-path="migrateSettingsPath"
    />

    <LibraryPage
      v-else
      :library-state="libraryState"
      :library-search="librarySearch"
      :filtered-library-items="filteredLibraryItems"
      :selected-library-item="selectedLibraryItem"
      :library-card-error-for="libraryCardErrorFor"
      :is-library-game-selected="isLibraryGameSelected"
      :game-dir-resolution-issue="gameDirResolutionIssue"
      :card-sync-status-label="cardSyncStatusLabel"
      :sync-status-class="syncStatusClass"
      :sync-decision-for="syncDecisionFor"
      :game-dir-status-label="gameDirStatusLabel"
      :backup-stats-for="backupStatsFor"
      :is-card-busy="isCardBusy"
      :launch-precheck-for="launchPrecheckFor"
      :selected-rule-anchor-tokens="selectedRuleAnchorTokens"
      :visible-precheck-checks="visiblePrecheckChecks"
      :backup-keep-draft-for="backupKeepDraftFor"
      :backup-versions-for="backupVersionsFor"
      :restore-undo-for="restoreUndoFor"
      :restore-task-message-for="restoreTaskMessageFor"
      :restore-task-progress-for="restoreTaskProgressFor"
      :session-details-for="sessionDetailsFor"
      @update:library-search="librarySearch = $event"
      @reload="reloadLibraryWithLoading"
      @select="selectLibraryGame"
      @launch="launchLibraryGame($event, 'backup')"
      @choose-exe="choosePreferredExeForGame"
      @update-backup-keep="updateBackupKeepDraft"
      @save-backup-keep="saveBackupKeepPolicy"
      @prune-backups="pruneOldBackupsForGame"
      @rollback-version="rollbackToLibraryBackupVersion"
      @undo-restore="undoLibraryRestore"
    />

    <AppToast
      :visible="toast.visible"
      :message="toast.message"
      :level="toast.level"
      @close="closeToast"
    />

    <ConfirmDialog
      :open="confirmDialog.open"
      :title="confirmDialog.title"
      :message="confirmDialog.message"
      :confirm-text="confirmDialog.confirmText"
      :cancel-text="confirmDialog.cancelText"
      :danger="confirmDialog.danger"
      @resolve="resolveConfirm"
    />

    <BlockingErrorDialog
      :message="blockingErrorMessage"
      @close="closeBlockingError"
    />
  </main>
</template>
