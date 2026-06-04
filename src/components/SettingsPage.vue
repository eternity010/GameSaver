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
}>();

const emit = defineEmits<{
  (e: "update:backupRootDraft", value: string): void;
  (e: "update:backupMaxFileMbDraft", value: string): void;
  (e: "reload"): void;
  (e: "choose-directory", kind: DataPathKind): void;
  (e: "open-directory", path: string): void;
  (e: "save-path", kind: DataPathKind): void;
  (e: "migrate-path", kind: DataPathKind): void;
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

    </div>
  </section>
</template>
