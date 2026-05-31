<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useConfirmDialog } from "./composables/useConfirmDialog";
import { useLibraryPage } from "./composables/useLibraryPage";
import { useRulesPage } from "./composables/useRulesPage";
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
  getSettingsPaths,
  getTask,
  getLearningSession,
  getRuntimeStatus,
  launchGame,
  openCandidatePath,
  restartAsAdmin,
  startMigrateDataPathTask,
  startFinishLearningTask,
  startLearning,
  updateSettingsPaths,
} from "./api";
import type {
  CandidatePath,
  DataPathKind,
  DataPathMigrationResult,
  SettingsPaths,
} from "./types";

type UiStep = "setup" | "running" | "results";
type TopTab = "learning" | "rules" | "library" | "settings";
type TabState = {
  loading: boolean;
  error: string;
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
const learningState = ref<TabState>({ loading: false, error: "" });
const settingsState = ref<TabState>({ loading: false, error: "" });
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
const { confirmDialog, askConfirm, resolveConfirm } = useConfirmDialog();
const blockingErrorMessage = ref("");

const hasHighConfidence = computed(() => candidates.value.some((item) => item.score >= 45));

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

const {
  rules,
  ruleConflicts,
  ruleSearch,
  ruleDrafts,
  rulesState,
  migrationExportWaiting,
  migrationExportMessage,
  migrationExportProgress,
  migrationImportWaiting,
  migrationImportMessage,
  migrationImportProgress,
  updateRuleDraft,
  refreshRules,
  reloadRulesWithLoading,
  markPrimaryRule,
  saveManagedRule,
  removeManagedRule,
  exportRulesToFile,
  importRulesFromFile,
  exportMigrationZipToFile,
  importMigrationZipFromFile,
} = useRulesPage({
  waitForTaskCompletion,
  askConfirm,
  showToast,
  refreshLibraryItems: () => refreshLibraryItems(),
});

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
