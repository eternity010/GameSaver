<script setup lang="ts">
import type { DataPathKind, SettingsPaths } from "../types";

type TabState = {
  loading: boolean;
  error: string;
};

const props = defineProps<{
  settings: SettingsPaths | null;
  settingsState: TabState;
  backupRootDraft: string;
  backupMaxFileMbDraft: string;
  migrationKind: DataPathKind | "";
  migrationMessage: string;
  migrationProgress: number | null;
  migrationExportWaiting: boolean;
  migrationExportMessage: string;
  migrationExportProgress: number | null;
  migrationImportWaiting: boolean;
  migrationImportMessage: string;
  migrationImportProgress: number | null;
}>();

const emit = defineEmits<{
  (e: "update:backupRootDraft", value: string): void;
  (e: "update:backupMaxFileMbDraft", value: string): void;
  (e: "reload"): void;
  (e: "choose-directory", kind: DataPathKind): void;
  (e: "open-directory", path: string): void;
  (e: "save-path", kind: DataPathKind): void;
  (e: "migrate-path", kind: DataPathKind): void;
  (e: "export-migration"): void;
  (e: "import-migration"): void;
}>();

function isChanged(): boolean {
  if (!props.settings) return false;
  const currentMb = Math.round(props.settings.backupMaxFileBytes / 1024 / 1024);
  return (
    props.backupRootDraft.trim() !== props.settings.backupRoot.trim()
    || props.backupMaxFileMbDraft.trim() !== String(currentMb)
  );
}

function currentPath(): string {
  if (!props.settings) return "";
  return props.settings.backupRoot;
}
</script>

<template>
  <section class="panel settings-shell">
    <header class="settings-header">
      <div class="settings-title-row">
        <div>
          <span class="eyebrow">设置</span>
          <h2>备份目录</h2>
        </div>
        <button :disabled="settingsState.loading" type="button" @click="emit('reload')">刷新</button>
      </div>
      <p v-if="settingsState.error" class="error inline-error">{{ settingsState.error }}</p>
      <div v-if="migrationKind" class="migration-progress">
        <p>{{ migrationMessage || "正在迁移数据目录..." }}</p>
        <div class="progress-track" role="progressbar" aria-label="数据目录迁移进行中">
          <span v-if="migrationProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${migrationProgress}%` }"
          ></span>
        </div>
        <p v-if="migrationProgress !== null">当前进度：{{ migrationProgress }}%</p>
      </div>
      <div v-if="migrationExportWaiting" class="migration-progress">
        <p>{{ migrationExportMessage || "正在导出迁移包，文件较多时可能需要一点时间，请稍候..." }}</p>
        <div class="progress-track" role="progressbar" aria-label="迁移包导出进行中">
          <span v-if="migrationExportProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${migrationExportProgress}%` }"
          ></span>
        </div>
        <p v-if="migrationExportProgress !== null">当前进度：{{ migrationExportProgress }}%</p>
      </div>
      <div v-if="migrationImportWaiting" class="migration-progress">
        <p>{{ migrationImportMessage || "正在导入迁移包，文件较多时可能需要一点时间，请稍候..." }}</p>
        <div class="progress-track" role="progressbar" aria-label="迁移包导入进行中">
          <span v-if="migrationImportProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${migrationImportProgress}%` }"
          ></span>
        </div>
        <p v-if="migrationImportProgress !== null">当前进度：{{ migrationImportProgress }}%</p>
      </div>
    </header>

    <div v-if="settings" class="settings-grid">
      <section class="settings-card">
        <div class="settings-card-head">
          <div>
            <h3>{{ settings.backupRoot }}</h3>
            <p>自动备份、恢复和迁移包会使用这个目录。</p>
          </div>
          <span class="settings-kind-chip">backupRoot</span>
        </div>
        <details class="settings-details">
          <summary>查看默认路径</summary>
          <p>{{ settings.defaultBackupRoot }}</p>
          <p v-if="settings.backupRoot !== settings.defaultBackupRoot" class="field-note settings-note">
            当前使用的是你保存过的自定义路径。
          </p>
        </details>
        <label class="field">
          <span>新路径</span>
          <div class="row">
            <input
              :value="backupRootDraft"
              placeholder="选择新的备份目录"
              @input="emit('update:backupRootDraft', ($event.target as HTMLInputElement).value)"
            />
            <button :disabled="settingsState.loading" type="button" @click="emit('choose-directory', 'backupRoot')">
              浏览
            </button>
          </div>
        </label>
        <div class="row settings-actions-row">
          <button :disabled="!currentPath()" type="button" @click="emit('open-directory', currentPath())">
            打开当前目录
          </button>
          <button :disabled="settingsState.loading || !isChanged()" type="button" @click="emit('save-path', 'backupRoot')">
            仅保存路径
          </button>
          <button
            :disabled="settingsState.loading || !isChanged() || migrationKind !== ''"
            type="button"
            class="primary"
            @click="emit('migrate-path', 'backupRoot')"
          >
            迁移到新路径
          </button>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-head">
          <div>
            <h3>大文件过滤</h3>
            <p>自动备份会跳过超过阈值的单个文件，减少无关缓存或录像进入存档备份。</p>
          </div>
          <span class="settings-kind-chip">backupLimit</span>
        </div>
        <label class="field">
          <span>跳过大于 N MB 的文件</span>
          <input
            :value="backupMaxFileMbDraft"
            type="number"
            min="0"
            step="1"
            placeholder="100"
            @input="emit('update:backupMaxFileMbDraft', ($event.target as HTMLInputElement).value)"
          />
        </label>
        <p class="field-note settings-note">
          默认 {{ Math.round(settings.defaultBackupMaxFileBytes / 1024 / 1024) }} MB；填 0 表示不限制。
          当前生效：{{ settings.backupMaxFileBytes === 0 ? "不限制" : `${Math.round(settings.backupMaxFileBytes / 1024 / 1024)} MB` }}。
        </p>
        <div class="row settings-actions-row">
          <button :disabled="settingsState.loading || !isChanged()" type="button" class="primary" @click="emit('save-path', 'backupRoot')">
            保存备份设置
          </button>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-head">
          <div>
            <h3>换电脑 / 搬家</h3>
            <p>迁移包会包含规则和当前备份目录下的历史备份，用于在另一台电脑恢复 GameSaver 数据。</p>
          </div>
          <span class="settings-kind-chip">migration</span>
        </div>
        <p class="field-note settings-note">
          导入迁移包前会先预览将新增、覆盖的规则和备份数量，确认后才会写入本机数据。
        </p>
        <div class="row settings-actions-row">
          <button
            :disabled="settingsState.loading || migrationExportWaiting || migrationImportWaiting"
            type="button"
            @click="emit('export-migration')"
          >
            导出迁移包
          </button>
          <button
            :disabled="settingsState.loading || migrationExportWaiting || migrationImportWaiting"
            type="button"
            class="primary"
            @click="emit('import-migration')"
          >
            导入迁移包
          </button>
        </div>
      </section>

    </div>
  </section>
</template>
