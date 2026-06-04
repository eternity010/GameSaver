<script setup lang="ts">
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

type RestoreUndoState = {
  gameId: string;
  versionId: string;
  restoredVersionId: string;
};

const props = defineProps<{
  selectedItem: GameLibraryItem;
  cardError: string;
  loading: boolean;
  launchBusy: boolean;
  bindExeBusy: boolean;
  backupPolicySaveBusy: boolean;
  backupPruneBusy: boolean;
  backupRollbackBusy: boolean;
  preferredExePath: string;
  precheck: GameLaunchPrecheck | null;
  syncDecision: LaunchSyncDecision | null;
  anchorTokens: string[];
  gameDirIssue: string;
  visiblePrecheckChecks: LaunchPrecheckCheck[];
  backupStats: BackupStatsResult | null;
  backupKeepDraft: string;
  backupVersions: BackupVersion[];
  restoreUndo: RestoreUndoState | null;
  restoreTaskMessage: string;
  restoreTaskProgress: number | null;
  sessionDetails: LauncherSession | null;
  productStatus: LibraryGameProductStatus;
}>();

const emit = defineEmits<{
  (e: "launch", gameId: string): void;
  (e: "choose-exe", gameId: string): void;
  (e: "update-backup-keep", gameId: string, value: string): void;
  (e: "save-backup-keep", gameId: string): void;
  (e: "prune-backups", gameId: string): void;
  (e: "rollback-version", gameId: string, versionId: string): void;
  (e: "undo-restore", gameId: string): void;
}>();

const PATH_ANCHOR_DESCRIPTIONS: Record<string, string> = {
  "%GAME_DIR%": "跟随当前绑定 EXE 所在目录动态解析",
  "%SAVED_GAMES%": "Windows 的 Saved Games 目录",
  "%DOCUMENTS%": "当前用户 Documents 目录",
  "%LOCALLOW%": "当前用户 AppData\\LocalLow 目录",
  "%LOCALAPPDATA%": "当前用户 AppData\\Local 目录",
  "%APPDATA%": "当前用户 AppData\\Roaming 目录",
  "%USERPROFILE%": "当前用户目录兼容锚点",
};

function pathAnchorLabel(token: string): string {
  switch (token.toUpperCase()) {
    case "%GAME_DIR%":
      return "游戏目录";
    case "%SAVED_GAMES%":
      return "Saved Games";
    case "%DOCUMENTS%":
      return "文档";
    case "%LOCALLOW%":
      return "LocalLow";
    case "%LOCALAPPDATA%":
      return "Local";
    case "%APPDATA%":
      return "Roaming";
    case "%USERPROFILE%":
      return "用户目录";
    default:
      return token;
  }
}

function pathAnchorDescription(token: string): string {
  return PATH_ANCHOR_DESCRIPTIONS[token.toUpperCase()] || "路径锚点";
}

function syncStatusLabel(status: string): string {
  switch (status) {
    case "no_backup":
      return "暂无备份";
    case "backup_only":
      return "仅备份存在";
    case "local_only":
      return "仅本地存在";
    case "in_sync":
      return "看起来一致";
    case "local_newer":
      return "本地较新";
    case "backup_newer":
      return "备份较新";
    default:
      return "需人工判断";
  }
}

function syncStatusClass(status: string): string {
  switch (status) {
    case "in_sync":
    case "local_only":
    case "no_backup":
      return "ok";
    case "backup_only":
    case "local_newer":
    case "backup_newer":
      return "warn";
    default:
      return "fail";
  }
}

function syncRecommendedActionLabel(action: string): string {
  switch (action) {
    case "launch_direct":
      return "可以直接启动";
    case "restore_then_launch":
      return "建议先恢复最近备份";
    default:
      return "建议先确认存档状态";
  }
}

function formatUnixTs(value: string): string {
  const timestamp = Number(value.startsWith("pre_restore_") ? value.slice("pre_restore_".length) : value);
  if (!Number.isFinite(timestamp) || timestamp <= 0) {
    return value || "未知";
  }
  const date = new Date(timestamp * 1000);
  if (Number.isNaN(date.getTime())) {
    return value || "未知";
  }
  return date.toLocaleString();
}

function formatOptionalUnixTs(value?: string): string {
  if (!value) return "未知";
  return formatUnixTs(value);
}

function formatBytes(totalBytes: number): string {
  if (!Number.isFinite(totalBytes) || totalBytes <= 0) {
    return "0 B";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = totalBytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  const precision = value >= 100 || unitIndex === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(precision)} ${units[unitIndex]}`;
}

function syncSummaryMeta(summary?: LaunchSyncDecision["localSummary"] | null): string {
  if (!summary) return "无摘要";
  if (!summary.exists) return "未检测到文件";
  return `${summary.fileCount} 个文件 / ${formatBytes(summary.totalBytes)}`;
}

function backupSummaryText(): string {
  if (!props.backupStats) return "备份读取中";
  return `${props.backupStats.versionCount ?? 0} 版 / ${formatBytes(props.backupStats.totalBytes ?? 0)}`;
}

function launchStatusLabel(): string {
  return props.productStatus.label;
}

function launchActionHint(): string {
  return props.productStatus.description;
}

function shortExePath(path?: string | null): string {
  const value = (path || "").trim();
  if (!value) return "";
  if (value.length <= 72) return value;
  return `${value.slice(0, 28)}...${value.slice(-36)}`;
}

function onBackupKeepInput(event: Event) {
  const target = event.target as HTMLInputElement | null;
  emit("update-backup-keep", props.selectedItem.gameId, target?.value ?? "");
}
</script>

<template>
  <aside class="panel library-detail-panel">
    <p v-if="cardError" class="error inline-error card-error">
      {{ cardError }}
    </p>
    <div class="detail-head">
      <div>
        <span class="eyebrow">当前选中</span>
        <h3>{{ selectedItem.gameId }}</h3>
      </div>
      <button
        type="button"
        class="primary"
        :disabled="loading || launchBusy"
        @click="emit('launch', selectedItem.gameId)"
      >
        {{ launchBusy ? "正在启动..." : "启动游戏" }}
      </button>
    </div>
    <p class="detail-launch-hint">{{ launchActionHint() }}</p>

    <section class="detail-summary-strip">
      <div class="detail-summary-item">
        <span>当前状态</span>
        <strong>{{ launchStatusLabel() }}</strong>
      </div>
      <div class="detail-summary-item">
        <span>建议操作</span>
        <strong>{{ productStatus.actionHint }}</strong>
      </div>
      <div class="detail-summary-item">
        <span>备份</span>
        <strong>{{ backupSummaryText() }}</strong>
      </div>
    </section>

    <label class="field compact-detail-field">
      <span>启动 EXE</span>
      <div class="row">
        <input
          :value="shortExePath(preferredExePath)"
          :title="preferredExePath || ''"
          readonly
          placeholder="尚未绑定 EXE，先点击右侧按钮"
        />
        <button
          type="button"
          :disabled="loading || bindExeBusy"
          @click="emit('choose-exe', selectedItem.gameId)"
        >
          选择/更换 EXE
        </button>
      </div>
    </label>

    <section class="precheck-box compact-precheck-box">
      <div class="row precheck-head">
        <strong>启动前确认</strong>
        <div class="row precheck-head-actions">
          <span
            v-if="precheck"
            class="precheck-state-pill"
            :class="precheck.backupReady ? 'ok' : 'fail'"
          >
            {{ precheck.backupReady ? "存档保护就绪" : "需要处理" }}
          </span>
          <span v-else class="precheck-state-pill idle">未检查</span>
        </div>
      </div>
      <section v-if="syncDecision" class="sync-decision-box compact-sync-box">
        <div class="sync-decision-head">
          <span class="precheck-anchor-label">同步状态</span>
          <span class="precheck-state-pill" :class="syncStatusClass(syncDecision.status)">
            {{ syncStatusLabel(syncDecision.status) }}
          </span>
        </div>
        <p class="sync-decision-action">
          {{ syncRecommendedActionLabel(syncDecision.recommendedAction || "") }}
        </p>
        <details class="sync-details">
          <summary>查看同步依据</summary>
          <p class="sync-decision-message">
            {{ syncDecision.message }}
          </p>
          <p class="sync-inline-summary">
            本地：{{ syncSummaryMeta(syncDecision.localSummary) }} ·
            备份：{{ syncSummaryMeta(syncDecision.backupSummary) }}
          </p>
          <div class="sync-decision-grid">
            <div class="sync-side-card">
              <strong>本地存档</strong>
              <p>{{ syncSummaryMeta(syncDecision.localSummary) }}</p>
              <p>最近修改：{{ formatOptionalUnixTs(syncDecision.localSummary?.latestModifiedAt) }}</p>
            </div>
            <div class="sync-side-card">
              <strong>最近备份</strong>
              <p>{{ syncSummaryMeta(syncDecision.backupSummary) }}</p>
              <p>
                最近版本：
                {{
                  syncDecision.backupSummary?.latestVersionId
                    ? formatUnixTs(syncDecision.backupSummary.latestVersionId)
                    : "无"
                }}
              </p>
            </div>
          </div>
        </details>
      </section>
      <details v-if="anchorTokens.length" class="precheck-anchor-summary">
        <summary>规则路径锚点</summary>
        <div class="anchor-chip-row compact detail-anchor-row">
          <span
            v-for="token in anchorTokens"
            :key="`${selectedItem.gameId}-${token}`"
            class="anchor-chip"
            :class="{ warning: token === '%GAME_DIR%' }"
            :title="pathAnchorDescription(token)"
          >
            {{ pathAnchorLabel(token) }}
          </span>
        </div>
      </details>
      <div v-if="gameDirIssue" class="warning-banner">
        <strong>需要绑定本地 EXE</strong>
        <p>{{ gameDirIssue }}</p>
      </div>
      <template v-if="precheck">
        <details v-if="visiblePrecheckChecks.length" class="precheck-details">
          <summary>查看检查明细（{{ visiblePrecheckChecks.length }} 项）</summary>
          <ul class="precheck-list">
            <li
              v-for="check in visiblePrecheckChecks"
              :key="`${selectedItem.gameId}-${check.key}`"
            >
              <span class="precheck-badge" :class="check.ok ? 'ok' : 'fail'">{{ check.ok ? "OK" : "FAIL" }}</span>
              <span class="precheck-label">{{ check.label }}</span>
              <span class="precheck-detail">{{ check.detail }}</span>
            </li>
          </ul>
        </details>
      </template>
      <p v-else class="empty-hint">尚未检查，点击“刷新”查看启动条件。</p>
    </section>

    <section class="backup-detail-stack">
      <details class="library-collapsible">
        <summary>备份占用与保留数量</summary>
        <section class="backup-policy-box">
          <div class="row backup-policy-head">
            <h4>备份占用</h4>
          </div>
          <div class="backup-policy-stats">
            <span>当前占用：{{ formatBytes(backupStats?.totalBytes ?? 0) }}</span>
            <span>版本数：{{ backupStats?.versionCount ?? 0 }}</span>
            <span>当前保留策略：最近 {{ backupStats?.keepVersions ?? 10 }} 版</span>
            <span v-if="backupStats?.latestVersionId">
              最新版本：{{ backupStats.latestVersionId }}
            </span>
          </div>
          <details class="policy-controls-details">
            <summary>展开策略与清理操作</summary>
            <div class="row backup-policy-controls">
              <label class="backup-keep-input">
                <span>保留最近 N 版</span>
                <input
                  :value="backupKeepDraft"
                  type="number"
                  min="1"
                  max="10"
                  step="1"
                  @input="onBackupKeepInput"
                />
              </label>
              <button
                type="button"
                :disabled="loading || backupPolicySaveBusy"
                @click="emit('save-backup-keep', selectedItem.gameId)"
              >
                保存策略
              </button>
              <button
                type="button"
                class="danger"
                :disabled="loading || backupPruneBusy"
                @click="emit('prune-backups', selectedItem.gameId)"
              >
                一键清理旧备份
              </button>
            </div>
          </details>
        </section>
      </details>

      <details class="library-collapsible">
        <summary>历史备份与恢复</summary>
        <div class="row">
          <h4>历史备份</h4>
        </div>
        <p class="field-note restore-safety-note">
          恢复任一版本前，GameSaver 会先尝试为当前本地存档创建一份“恢复前备份”，方便你随时撤销。
        </p>
        <div v-if="restoreTaskMessage" class="migration-progress restore-progress">
          <p>{{ restoreTaskMessage }}</p>
          <div class="progress-track" role="progressbar" aria-label="恢复进度">
            <span v-if="restoreTaskProgress === null" class="progress-indeterminate"></span>
            <span
              v-else
              class="progress-determinate"
              :style="{ width: `${restoreTaskProgress}%` }"
            ></span>
          </div>
          <p v-if="restoreTaskProgress !== null">
            当前进度：{{ restoreTaskProgress }}%
          </p>
        </div>
        <ul v-if="backupVersions.length" class="backup-timeline">
          <li
            v-for="version in backupVersions"
            :key="version.versionId"
            :class="{ 'pre-restore': version.label === '回滚前备份' }"
          >
            <div class="backup-version-main">
              <div class="backup-version-title">
                <span class="backup-version-label">{{ version.label }}</span>
                <strong>{{ formatUnixTs(version.createdAt) }}</strong>
              </div>
              <p>{{ version.fileCount }} 个文件</p>
              <details class="backup-version-id">
                <summary>查看版本 ID</summary>
                <code>{{ version.versionId }}</code>
              </details>
            </div>
            <div class="backup-version-actions">
              <button
                type="button"
                class="primary"
                :disabled="loading || backupRollbackBusy || !version.restorable"
                @click="emit('rollback-version', selectedItem.gameId, version.versionId)"
              >
                回滚到此版本
              </button>
            </div>
          </li>
        </ul>
        <p v-else>暂无备份版本。</p>
        <section v-if="restoreUndo" class="restore-undo-box">
          <div>
            <strong>可撤销本次恢复</strong>
            <p>
              已从 {{ restoreUndo.restoredVersionId }} 恢复。
              回滚前备份：{{ restoreUndo.versionId }}
            </p>
          </div>
          <button
            type="button"
            class="danger"
            :disabled="loading || backupRollbackBusy"
            @click="emit('undo-restore', selectedItem.gameId)"
          >
            撤销本次恢复
          </button>
        </section>
      </details>

      <details class="library-collapsible diagnostics-collapsible">
        <summary>诊断日志</summary>
        <div class="row">
          <h4>最近一次启动记录</h4>
        </div>
        <template v-if="sessionDetails">
          <p class="diagnostic-meta">
            状态：{{ sessionDetails.status }} · 模式：{{ sessionDetails.launchMode ?? "backup" }} ·
            PID：{{ sessionDetails.pid ?? "无" }}
          </p>
          <ul class="rule-list">
            <li
              v-for="(log, idx) in (sessionDetails.logs || []).slice(-10)"
              :key="`${selectedItem.gameId}-${idx}`"
            >
              {{ log }}
            </li>
          </ul>
        </template>
        <p v-else>暂无诊断日志。</p>
      </details>
    </section>
  </aside>
</template>
