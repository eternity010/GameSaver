<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useConfirmDialog } from "./composables/useConfirmDialog";
import { useLearningPage } from "./composables/useLearningPage";
import { useLibraryPage } from "./composables/useLibraryPage";
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

type TopTab = "learning" | "rules" | "library" | "settings";

const activeTab = ref<TopTab>("library");
const { toast, showToast, closeToast } = useToast();
const { confirmDialog, askConfirm, resolveConfirm } = useConfirmDialog();
const blockingErrorMessage = ref("");

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

onMounted(() => {
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
      :backup-max-file-mb-draft="backupMaxFileMbDraft"
      :migration-kind="settingsMigrationKind"
      :migration-message="settingsMigrationMessage"
      :migration-progress="settingsMigrationProgress"
      @update:backup-root-draft="backupRootDraft = $event"
      @update:backup-max-file-mb-draft="backupMaxFileMbDraft = $event"
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
