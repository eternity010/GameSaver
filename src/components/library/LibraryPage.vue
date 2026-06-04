<script setup lang="ts">
import LibraryDetailPanel from "./LibraryDetailPanel.vue";
import LibraryGameCard from "./LibraryGameCard.vue";
import type { LibraryGameProductStatus } from "../../composables/useLibraryPage";
import type {
  BackupStatsResult,
  BackupVersion,
  GameLaunchPrecheck,
  GameLibraryItem,
  LaunchPrecheckCheck,
  LaunchSyncDecision,
  LauncherSession,
} from "../../types";

type TabState = {
  loading: boolean;
  error: string;
};

type RestoreUndoState = {
  gameId: string;
  versionId: string;
  restoredVersionId: string;
};

type LibraryCardAction =
  | "bind_exe"
  | "precheck"
  | "launch"
  | "backup_stats"
  | "backup_policy_save"
  | "backup_prune"
  | "backup_versions"
  | "backup_rollback"
  | "session_logs";

defineProps<{
  libraryState: TabState;
  librarySearch: string;
  filteredLibraryItems: GameLibraryItem[];
  selectedLibraryItem: GameLibraryItem | null;
  libraryCardErrorFor: (gameId: string) => string;
  isLibraryGameSelected: (gameId: string) => boolean;
  gameDirResolutionIssue: (gameId: string) => string;
  syncDecisionFor: (gameId: string) => LaunchSyncDecision | null;
  libraryGameProductStatus: (item: GameLibraryItem) => LibraryGameProductStatus;
  backupStatsFor: (gameId: string) => BackupStatsResult | null;
  isCardBusy: (gameId: string, action?: LibraryCardAction) => boolean;
  launchPrecheckFor: (gameId: string) => GameLaunchPrecheck | null;
  selectedRuleAnchorTokens: (gameId: string) => string[];
  visiblePrecheckChecks: (gameId: string) => LaunchPrecheckCheck[];
  backupKeepDraftFor: (gameId: string) => string;
  backupVersionsFor: (gameId: string) => BackupVersion[];
  restoreUndoFor: (gameId: string) => RestoreUndoState | null;
  restoreTaskMessageFor: (gameId: string) => string;
  restoreTaskProgressFor: (gameId: string) => number | null;
  sessionDetailsFor: (gameId: string) => LauncherSession | null;
}>();

const emit = defineEmits<{
  (e: "update:librarySearch", value: string): void;
  (e: "reload"): void;
  (e: "select", gameId: string): void;
  (e: "launch", gameId: string): void;
  (e: "choose-exe", gameId: string): void;
  (e: "update-backup-keep", gameId: string, value: string): void;
  (e: "save-backup-keep", gameId: string): void;
  (e: "prune-backups", gameId: string): void;
  (e: "rollback-version", gameId: string, versionId: string): void;
  (e: "undo-restore", gameId: string): void;
}>();
</script>

<template>
  <section class="panel library-shell">
    <header class="library-header">
      <div class="library-title-row">
        <h2>游戏库（自动备份优先）</h2>
        <button :disabled="libraryState.loading" type="button" @click="emit('reload')">刷新</button>
      </div>
      <label class="library-search">
        <span>搜索游戏</span>
        <input
          :value="librarySearch"
          placeholder="按 gameId 搜索"
          @input="emit('update:librarySearch', ($event.target as HTMLInputElement).value)"
        />
      </label>
      <p v-if="libraryState.error" class="error inline-error">{{ libraryState.error }}</p>
    </header>

    <div v-if="filteredLibraryItems.length" class="library-layout">
      <div class="library-grid" :class="{ single: filteredLibraryItems.length === 1 }">
        <LibraryGameCard
          v-for="item in filteredLibraryItems"
          :key="item.gameId"
          :item="item"
          :selected="isLibraryGameSelected(item.gameId)"
          :warning="!!gameDirResolutionIssue(item.gameId)"
          :card-error="libraryCardErrorFor(item.gameId)"
          :product-status="libraryGameProductStatus(item)"
          @select="emit('select', $event)"
        />
      </div>

      <LibraryDetailPanel
        v-if="selectedLibraryItem"
        :selected-item="selectedLibraryItem"
        :card-error="libraryCardErrorFor(selectedLibraryItem.gameId)"
        :loading="libraryState.loading"
        :launch-busy="isCardBusy(selectedLibraryItem.gameId, 'launch')"
        :bind-exe-busy="isCardBusy(selectedLibraryItem.gameId, 'bind_exe')"
        :backup-policy-save-busy="isCardBusy(selectedLibraryItem.gameId, 'backup_policy_save')"
        :backup-prune-busy="isCardBusy(selectedLibraryItem.gameId, 'backup_prune')"
        :backup-rollback-busy="isCardBusy(selectedLibraryItem.gameId, 'backup_rollback')"
        :preferred-exe-path="selectedLibraryItem.preferredExePath || ''"
        :precheck="launchPrecheckFor(selectedLibraryItem.gameId)"
        :sync-decision="syncDecisionFor(selectedLibraryItem.gameId)"
        :anchor-tokens="selectedRuleAnchorTokens(selectedLibraryItem.gameId)"
        :game-dir-issue="gameDirResolutionIssue(selectedLibraryItem.gameId)"
        :visible-precheck-checks="visiblePrecheckChecks(selectedLibraryItem.gameId)"
        :backup-stats="backupStatsFor(selectedLibraryItem.gameId)"
        :backup-keep-draft="backupKeepDraftFor(selectedLibraryItem.gameId)"
        :backup-versions="backupVersionsFor(selectedLibraryItem.gameId)"
        :restore-undo="restoreUndoFor(selectedLibraryItem.gameId)"
        :restore-task-message="restoreTaskMessageFor(selectedLibraryItem.gameId)"
        :restore-task-progress="restoreTaskProgressFor(selectedLibraryItem.gameId)"
        :session-details="sessionDetailsFor(selectedLibraryItem.gameId)"
        :product-status="libraryGameProductStatus(selectedLibraryItem)"
        @launch="(gameId) => emit('launch', gameId)"
        @choose-exe="(gameId) => emit('choose-exe', gameId)"
        @update-backup-keep="(gameId, value) => emit('update-backup-keep', gameId, value)"
        @save-backup-keep="(gameId) => emit('save-backup-keep', gameId)"
        @prune-backups="(gameId) => emit('prune-backups', gameId)"
        @rollback-version="(gameId, versionId) => emit('rollback-version', gameId, versionId)"
        @undo-restore="(gameId) => emit('undo-restore', gameId)"
      />
    </div>
    <p v-else>暂无游戏卡片，请先在“学习存档”中生成规则。</p>
  </section>
</template>
