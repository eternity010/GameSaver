<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useConfirmDialog } from "./composables/useConfirmDialog";
import { useLearningPage } from "./composables/useLearningPage";
import { useLibraryPage, type LibraryGameProductStatus } from "./composables/useLibraryPage";
import { useRulesPage } from "./composables/useRulesPage";
import { useSettingsPage } from "./composables/useSettingsPage";
import { useToast } from "./composables/useToast";
import LearningPage from "./components/learning/LearningPage.vue";
import LibraryPage from "./components/library/LibraryPage.vue";
import RulesPage from "./components/rules/RulesPage.vue";
import SettingsPage from "./components/SettingsPage.vue";
import AppToast from "./components/ui/AppToast.vue";
import BlockingErrorDialog from "./components/ui/BlockingErrorDialog.vue";
import ConfirmDialog from "./components/ui/ConfirmDialog.vue";
import { getTask } from "./api";
import { BookOpenCheck, Gamepad2, Library, Settings, SlidersHorizontal } from "@lucide/vue";

type TopTab = "learning" | "rules" | "library" | "settings";

type PostExitBackupCompletedEvent = {
  gameId: string;
  sessionId: string;
  changedFiles: number;
  skippedLargeFiles: number;
  versionId?: string;
  error?: string;
};

const activeTab = ref<TopTab>("library");
const { toast, showToast, closeToast } = useToast();
const { confirmDialog, askConfirm, resolveConfirm } = useConfirmDialog();
const blockingErrorMessage = ref("");
let unlistenPostExitBackup: UnlistenFn | null = null;
const initializedTabs = new Set<TopTab>(["library"]);

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

function buildPostExitBackupToast(payload: PostExitBackupCompletedEvent): {
  message: string;
  level: "success" | "error" | "info";
  timeoutMs: number;
} {
  if (payload.error) {
    return {
      message: `${payload.gameId} 自动备份失败：${payload.error}`,
      level: "error",
      timeoutMs: 5200,
    };
  }
  const skippedNote = payload.skippedLargeFiles > 0 ? `，跳过 ${payload.skippedLargeFiles} 个大文件` : "";
  if (payload.versionId) {
    return {
      message: `${payload.gameId} 已自动保护本次存档：变更 ${payload.changedFiles} 个文件${skippedNote}`,
      level: "success",
      timeoutMs: 4200,
    };
  }
  return {
    message: `${payload.gameId} 本次没有检测到存档变化${skippedNote}`,
    level: "info",
    timeoutMs: 3600,
  };
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
  toggleManagedRule,
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
  librarySortMode,
  filteredLibraryItems,
  libraryIconFor,
  selectedLibraryItem,
  libraryCardErrorFor,
  isLibraryGameSelected,
  gameDirResolutionIssue,
  syncDecisionFor,
  libraryGameProductStatus,
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
  loadSelectedLibraryGameDetails,
} = useLibraryPage({
  rules,
  waitForTaskCompletion,
  askConfirm,
  showToast,
  showBlockingError,
});

const {
  settings,
  settingsState,
  backupRootDraft,
  backupMaxFileMbDraft,
  settingsMigrationKind,
  settingsMigrationMessage,
  settingsMigrationProgress,
  openDirectory,
  reloadSettings,
  chooseSettingsDirectory,
  saveSettingsPath,
  migrateSettingsPath,
} = useSettingsPage({
  waitForTaskCompletion,
  askConfirm,
  showToast,
});

const {
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
  chooseExePath,
  chooseExtraScanRoot,
  beginLearning,
  endLearning,
  toggleSelect,
  openPath,
  saveLearningRule,
  retryLearningAnalysis,
  abandonLearning,
} = useLearningPage({
  waitForTaskCompletion,
  askConfirm,
  showToast,
  afterRuleSaved: async (savedGameId) => {
    await refreshRules();
    await refreshLibraryItems();
    activeTab.value = "library";
    await selectLibraryGame(savedGameId);
  },
});

async function handleLibraryPrimaryAction(payload: {
  gameId: string;
  action: LibraryGameProductStatus["action"];
}) {
  await selectLibraryGame(payload.gameId);
  switch (payload.action) {
    case "launch":
      await launchLibraryGame(payload.gameId, "backup");
      break;
    case "bind_exe":
      await choosePreferredExeForGame(payload.gameId, true);
      break;
    case "enable_rule":
      ruleSearch.value = payload.gameId;
      activeTab.value = "rules";
      showToast("已定位到对应规则，请启用后返回游戏库", "info", 3600);
      break;
    case "learn":
      gameId.value = payload.gameId;
      exePath.value = selectedLibraryItem.value?.preferredExePath || "";
      activeTab.value = "learning";
      break;
    default:
      break;
  }
}

async function ensureTabInitialized(tab: TopTab) {
  if (initializedTabs.has(tab)) return;
  initializedTabs.add(tab);
  if (tab === "rules") {
    await reloadRulesWithLoading();
  } else if (tab === "settings") {
    await reloadSettings();
  }
}

watch(activeTab, (tab) => {
  void ensureTabInitialized(tab);
});

onMounted(() => {
  void reloadLibraryWithLoading();
  void listen<PostExitBackupCompletedEvent>("post_exit_backup_completed", async (event) => {
    const toastResult = buildPostExitBackupToast(event.payload);
    showToast(toastResult.message, toastResult.level, toastResult.timeoutMs);
    await refreshLibraryItems();
    if (selectedLibraryItem.value?.gameId === event.payload.gameId) {
      await loadSelectedLibraryGameDetails();
    }
  }).then((unlisten) => {
    unlistenPostExitBackup = unlisten;
  });
});

onUnmounted(() => {
  if (unlistenPostExitBackup) {
    unlistenPostExitBackup();
    unlistenPostExitBackup = null;
  }
});

</script>

<template>
  <main class="app-shell">
    <aside class="app-sidebar">
      <div class="app-brand">
        <span class="app-brand-mark"><Gamepad2 :size="22" stroke-width="2" /></span>
        <div>
          <strong>GameSaver</strong>
          <span>存档保护</span>
        </div>
      </div>
      <nav class="app-nav" aria-label="主导航">
      <button
        class="app-nav-item"
        :class="{ active: activeTab === 'library' }"
        type="button"
        @click="activeTab = 'library'"
      >
        <Library :size="19" />
        <span>游戏库</span>
      </button>
      <button
        class="app-nav-item"
        :class="{ active: activeTab === 'learning' }"
        type="button"
        @click="activeTab = 'learning'"
      >
        <BookOpenCheck :size="19" />
        <span>添加游戏</span>
      </button>
      <button
        class="app-nav-item"
        :class="{ active: activeTab === 'rules' }"
        type="button"
        @click="activeTab = 'rules'"
      >
        <SlidersHorizontal :size="19" />
        <span>规则</span>
      </button>
      <button
        class="app-nav-item"
        :class="{ active: activeTab === 'settings' }"
        type="button"
        @click="activeTab = 'settings'"
      >
        <Settings :size="19" />
        <span>设置</span>
      </button>
      </nav>
    </aside>

    <section class="app-content">

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
      @retry-learning-analysis="retryLearningAnalysis"
      @abandon-learning="abandonLearning"
    />

    <RulesPage
      v-else-if="activeTab === 'rules'"
      :rules="rules"
      :rule-conflicts="ruleConflicts"
      :rule-search="ruleSearch"
      :rule-drafts="ruleDrafts"
      :rules-state="rulesState"
      @update:rule-search="ruleSearch = $event"
      @update:rule-draft="updateRuleDraft($event.ruleId, $event.patch)"
      @reload="reloadRulesWithLoading"
      @export-rules="exportRulesToFile"
      @import-rules="importRulesFromFile"
      @open-migration-settings="activeTab = 'settings'"
      @mark-primary="markPrimaryRule"
      @save-rule="saveManagedRule"
      @toggle-rule="toggleManagedRule($event.rule, $event.enabled)"
      @remove-rule="removeManagedRule"
    />

    <SettingsPage
      v-else-if="activeTab === 'settings'"
      :settings="settings"
      :settings-state="settingsState"
      :backup-root-draft="backupRootDraft"
      :backup-max-file-mb-draft="backupMaxFileMbDraft"
      :migration-kind="settingsMigrationKind"
      :migration-message="settingsMigrationMessage"
      :migration-progress="settingsMigrationProgress"
      :migration-export-waiting="migrationExportWaiting"
      :migration-export-message="migrationExportMessage"
      :migration-export-progress="migrationExportProgress"
      :migration-import-waiting="migrationImportWaiting"
      :migration-import-message="migrationImportMessage"
      :migration-import-progress="migrationImportProgress"
      @update:backup-root-draft="backupRootDraft = $event"
      @update:backup-max-file-mb-draft="backupMaxFileMbDraft = $event"
      @reload="reloadSettings"
      @choose-directory="chooseSettingsDirectory"
      @open-directory="openDirectory"
      @save-path="saveSettingsPath"
      @migrate-path="migrateSettingsPath"
      @export-migration="exportMigrationZipToFile"
      @import-migration="importMigrationZipFromFile"
    />

    <LibraryPage
      v-else
      :library-state="libraryState"
      :library-search="librarySearch"
      :library-sort-mode="librarySortMode"
      :filtered-library-items="filteredLibraryItems"
      :library-icon-for="libraryIconFor"
      :selected-library-item="selectedLibraryItem"
      :library-card-error-for="libraryCardErrorFor"
      :is-library-game-selected="isLibraryGameSelected"
      :game-dir-resolution-issue="gameDirResolutionIssue"
      :sync-decision-for="syncDecisionFor"
      :library-game-product-status="libraryGameProductStatus"
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
      @update:library-sort-mode="librarySortMode = $event"
      @reload="reloadLibraryWithLoading"
      @select="selectLibraryGame"
      @launch="launchLibraryGame($event, 'backup')"
      @primary-action="handleLibraryPrimaryAction"
      @choose-exe="choosePreferredExeForGame"
      @update-backup-keep="updateBackupKeepDraft"
      @save-backup-keep="saveBackupKeepPolicy"
      @prune-backups="pruneOldBackupsForGame"
      @rollback-version="rollbackToLibraryBackupVersion"
      @undo-restore="undoLibraryRestore"
    />
    </section>

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
