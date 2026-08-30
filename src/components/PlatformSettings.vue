<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { CheckCircle2, CloudDownload, CloudUpload, ExternalLink, FolderOpen, HardDrive, KeyRound, RefreshCw, Save, ShieldCheck, XCircle } from "@lucide/vue";
import { buildBaiduAuthorizeUrl, exchangeBaiduCode, getBaiduConfig, getBaiduQuota, getBaiduStatus, getCloudAccountStatus, getLibrarySettings, getTask, saveBaiduConfig, setBaiduAutoUpload, startDownloadCloudAccountTask, startSetLibraryRootTask, startUploadCloudAccountTask } from "../api";
import type { BaiduConfigView, BaiduQuota, BaiduStatus, CloudAccountStatus, LibrarySettings } from "../api";
import { open } from "@tauri-apps/plugin-dialog";

const appKey = ref("");
const secretKey = ref("");
const config = ref<BaiduConfigView | null>(null);
const status = ref<BaiduStatus | null>(null);
const quota = ref<BaiduQuota | null>(null);
const authorizeUrl = ref("");
const code = ref("");
const loading = ref(true);
const saving = ref(false);
const authorizing = ref(false);
const error = ref("");
const message = ref("");
const library = ref<LibrarySettings | null>(null);
const libraryTaskId = ref("");
const libraryTaskMessage = ref("");
const libraryTaskProgress = ref(0);
let libraryTimer: ReturnType<typeof setTimeout> | undefined;
const cloudAccount = ref<CloudAccountStatus | null>(null);
const cloudAccountTaskId = ref("");
const cloudAccountTaskMessage = ref("");
const cloudAccountTaskProgress = ref(0);
let cloudAccountTimer: ReturnType<typeof setTimeout> | undefined;

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [nextConfig, nextStatus, nextLibrary] = await Promise.all([getBaiduConfig(), getBaiduStatus(), getLibrarySettings()]);
    config.value = nextConfig;
    status.value = nextStatus;
    library.value = nextLibrary;
    appKey.value = nextConfig.appKey || "";
    if (nextStatus.authorized && !nextStatus.expired && !nextStatus.refreshError) {
      [quota.value, cloudAccount.value] = await Promise.all([getBaiduQuota(), getCloudAccountStatus()]);
    } else {
      quota.value = null;
      cloudAccount.value = null;
    }
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

async function syncCloudAccount(direction: "upload" | "download") {
  if (cloudAccountTaskId.value || !status.value?.authorized) return;
  if (direction === "download" && !window.confirm("将云端游戏设置合并到本机。现有游戏本体和本机路径不会被删除，但同 UID 的名称、启动配置和存档规则可能更新。继续吗？")) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    cloudAccountTaskId.value = direction === "upload" ? await startUploadCloudAccountTask() : await startDownloadCloudAccountTask();
    cloudAccountTaskProgress.value = 0;
    cloudAccountTaskMessage.value = direction === "upload" ? "准备上传 GameSaver 云端档案" : "准备恢复 GameSaver 云端档案";
    await watchCloudAccountTask(cloudAccountTaskId.value);
  } catch (reason) {
    cloudAccountTaskId.value = "";
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

async function watchCloudAccountTask(taskId: string) {
  const task = await getTask(taskId);
  cloudAccountTaskProgress.value = task.progress;
  cloudAccountTaskMessage.value = task.message;
  if (["success", "failed", "cancelled", "interrupted"].includes(task.status)) {
    cloudAccountTaskId.value = "";
    if (task.status === "success") {
      message.value = task.message;
      cloudAccount.value = await getCloudAccountStatus();
    } else {
      error.value = task.error || task.message;
    }
    return;
  }
  if (cloudAccountTimer) clearTimeout(cloudAccountTimer);
  cloudAccountTimer = setTimeout(() => void watchCloudAccountTask(taskId), 700);
}

function formatLibraryBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

async function chooseLibraryRoot() {
  if (libraryTaskId.value) return;
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string" || !selected.trim()) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    libraryTaskId.value = await startSetLibraryRootTask(selected);
    libraryTaskProgress.value = 0;
    libraryTaskMessage.value = "准备迁移游戏库";
    await watchLibraryTask(libraryTaskId.value);
  } catch (reason) {
    libraryTaskId.value = "";
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

async function watchLibraryTask(taskId: string) {
  const task = await getTask(taskId);
  libraryTaskProgress.value = task.progress;
  libraryTaskMessage.value = task.message;
  if (task.status === "success") {
    libraryTaskId.value = "";
    message.value = "游戏库已迁移到新位置，原位置的大文件已清理。";
    library.value = await getLibrarySettings();
    return;
  }
  if (["failed", "cancelled", "interrupted"].includes(task.status)) {
    libraryTaskId.value = "";
    error.value = task.error || task.message;
    return;
  }
  if (libraryTimer) clearTimeout(libraryTimer);
  libraryTimer = setTimeout(() => void watchLibraryTask(taskId), 700);
}

async function save() {
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    config.value = await saveBaiduConfig(appKey.value, secretKey.value);
    secretKey.value = "";
    message.value = "百度应用凭证已安全保存";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

async function toggleAutoUpload() {
  if (!config.value?.configured) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    config.value = await setBaiduAutoUpload(!config.value.autoUploadBody);
    message.value = config.value.autoUploadBody ? "自动上传已开启" : "自动上传已关闭";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

async function prepareAuthorization() {
  authorizing.value = true;
  error.value = "";
  message.value = "";
  try {
    authorizeUrl.value = await buildBaiduAuthorizeUrl();
    message.value = "请打开授权页面，完成授权后复制页面中的 Code。";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    authorizing.value = false;
  }
}

async function exchangeCode() {
  if (!code.value.trim()) return;
  authorizing.value = true;
  error.value = "";
  message.value = "正在验证授权 Code";
  try {
    await exchangeBaiduCode(code.value);
    code.value = "";
    authorizeUrl.value = "";
    message.value = "百度网盘授权成功";
    await refresh();
  } catch (reason) {
    error.value = String(reason);
  } finally {
    authorizing.value = false;
  }
}

function statusLabel(): string {
  if (!config.value?.configured) return "未配置应用凭证";
  if (!status.value?.authorized) return "已配置，未授权";
  if (status.value.refreshError) return "授权需要确认";
  if (status.value.expired) return "授权已过期";
  return "已连接";
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function formatCloudAccountTime(timestamp?: number): string {
  if (!timestamp) return "未知时间";
  return new Date(timestamp * 1000).toLocaleString();
}

onMounted(() => void refresh());
onUnmounted(() => {
  if (libraryTimer) clearTimeout(libraryTimer);
  if (cloudAccountTimer) clearTimeout(cloudAccountTimer);
});
</script>

<template>
  <section class="settings-page page-enter">
    <header class="settings-header">
      <div><p class="eyebrow">平台设置</p><h1>GameSaver 设置</h1><p>管理平台级服务，游戏本身的启动和存档设置仍在游戏详情页中。</p></div>
      <button class="icon-button" type="button" title="刷新设置状态" aria-label="刷新设置状态" :disabled="loading || saving || authorizing" @click="refresh"><RefreshCw :size="17" /></button>
    </header>

    <div v-if="loading" class="state-panel settings-state"><span class="loader"></span><strong>正在读取平台设置</strong></div>
    <template v-else>
      <div v-if="error" class="settings-alert settings-alert-error"><XCircle :size="18" /><span>{{ error }}</span></div>
      <div v-if="message" class="settings-alert settings-alert-success"><CheckCircle2 :size="18" /><span>{{ message }}</span></div>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><HardDrive :size="20" /></div><div><p class="eyebrow">本地存储</p><h2>游戏库位置</h2><p>游戏本体、ZIP 包和存档版本统一保存在这里；应用配置和日志仍保留在系统盘。</p></div></div>
        <div v-if="library" class="library-location">
          <div class="library-root-row"><div><span class="settings-label">当前游戏库</span><strong class="library-path">{{ library.libraryRoot }}</strong></div><button class="secondary-button" type="button" :disabled="saving || !!libraryTaskId" @click="chooseLibraryRoot"><FolderOpen :size="16" />{{ libraryTaskId ? "迁移中" : "更换位置" }}</button></div>
          <div class="quota-grid library-usage-grid"><div><span>游戏本体</span><strong>{{ formatLibraryBytes(library.gamesBytes) }}</strong><small>{{ library.gamesPath }}</small></div><div><span>本体 ZIP</span><strong>{{ formatLibraryBytes(library.bodyPackagesBytes) }}</strong><small>{{ library.bodyPackagesPath }}</small></div><div><span>存档版本</span><strong>{{ formatLibraryBytes(library.savesBytes) }}</strong><small>{{ library.savesPath }}</small></div></div>
          <p class="settings-hint"><HardDrive :size="15" />已占用 {{ formatLibraryBytes(library.totalBytes) }}，包含 {{ library.fileCount.toLocaleString() }} 个文件；当前磁盘可用 {{ formatLibraryBytes(library.freeBytes) }}。迁移完成并校验后，旧位置的大文件会被清理。</p>
          <div v-if="libraryTaskId" class="library-migration-progress"><div class="task-progress-heading"><span>{{ libraryTaskMessage }}</span><strong>{{ libraryTaskProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${libraryTaskProgress}%` }"></span></div></div>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><KeyRound :size="20" /></div><div><p class="eyebrow">云端服务</p><h2>百度网盘</h2><p>用于上传和下载游戏本体 ZIP。GameSaver 直连百度网盘，不使用代理。</p></div><span class="settings-status" :class="{ connected: status?.authorized && !status.expired && !status.refreshError }">{{ statusLabel() }}</span></div>
        <div class="settings-form">
          <label><span>AppKey</span><input v-model="appKey" type="text" autocomplete="off" placeholder="输入百度开放平台 AppKey" /></label>
          <label><span>SecretKey</span><input v-model="secretKey" type="password" autocomplete="new-password" :placeholder="config?.secretKeyConfigured ? '已保存，如需更换请重新输入' : '输入百度开放平台 SecretKey'" /></label>
          <p class="settings-hint"><ShieldCheck :size="15" />SecretKey 使用 Windows 本机加密保存，不会显示在日志、迁移包或云端。</p>
          <button class="primary-button" type="button" :disabled="saving || !appKey.trim() || !secretKey.trim()" @click="save"><Save :size="16" />{{ saving ? "保存中" : "保存凭证" }}</button>
          <button class="settings-toggle" type="button" :disabled="saving || !config?.configured" @click="toggleAutoUpload"><span class="toggle-track" :class="{ enabled: config?.autoUploadBody }"><i></i></span><span><strong>自动上传游戏本体</strong><small>本地 ZIP 成功创建后，自动加入上传队列；失败不会影响本地游戏。</small></span></button>
          <p v-if="status?.refreshError" class="settings-hint settings-hint-warning"><XCircle :size="15" />{{ status.refreshError }}。请重新完成百度授权。</p>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><ShieldCheck :size="20" /></div><div><p class="eyebrow">授权状态</p><h2>百度网盘授权</h2><p>桌面端使用 OOB 授权，不需要本地回调地址。</p></div></div>
        <div class="settings-actions"><button class="secondary-button" type="button" :disabled="authorizing || !config?.configured" @click="prepareAuthorization">生成授权页面</button><a v-if="authorizeUrl" class="settings-link" :href="authorizeUrl" target="_blank" rel="noreferrer"><ExternalLink :size="15" />打开百度授权页面</a></div>
        <label class="code-field"><span>授权 Code</span><input v-model="code" type="text" autocomplete="off" placeholder="授权后粘贴页面显示的 Code" /><button class="secondary-button" type="button" :disabled="authorizing || !code.trim()" @click="exchangeCode">{{ authorizing ? "验证中" : "完成授权" }}</button></label>
        <p class="settings-hint">授权地址修改不会影响当前流程；百度授权页面完成后，将 Code 粘贴回这里即可。</p>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><CheckCircle2 :size="20" /></div><div><p class="eyebrow">账户概览</p><h2>网盘空间</h2><p>上传游戏本体前会自动检查剩余空间。</p></div></div>
        <div v-if="quota" class="quota-grid"><div><span>总空间</span><strong>{{ formatBytes(quota.total) }}</strong></div><div><span>已使用</span><strong>{{ formatBytes(quota.used) }}</strong></div><div><span>可用空间</span><strong>{{ formatBytes(quota.free) }}</strong></div></div>
        <p v-else class="settings-empty">完成百度网盘授权后，这里会显示空间信息。</p>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><RefreshCw :size="20" /></div><div><p class="eyebrow">跨设备同步</p><h2>GameSaver 云端档案</h2><p>同步游戏清单、启动设置、存档保护配置和云端本体版本引用。不会上传 token、密钥、日志或本机路径。</p></div><span class="settings-status" :class="{ connected: cloudAccount?.profileAvailable }">{{ !status?.authorized ? "未连接百度网盘" : cloudAccount?.profileAvailable ? "已有云端档案" : "尚未创建" }}</span></div>
        <div class="settings-actions"><button class="secondary-button" type="button" :disabled="saving || !status?.authorized || !!cloudAccountTaskId" @click="syncCloudAccount('upload')"><CloudUpload :size="16" />上传本机档案</button><button class="secondary-button" type="button" :disabled="saving || !status?.authorized || !cloudAccount?.profileAvailable || !!cloudAccountTaskId" @click="syncCloudAccount('download')"><CloudDownload :size="16" />从云端恢复</button></div>
        <div v-if="cloudAccountTaskId" class="library-migration-progress"><div class="task-progress-heading"><span>{{ cloudAccountTaskMessage }}</span><strong>{{ cloudAccountTaskProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${cloudAccountTaskProgress}%` }"></span></div></div>
        <p v-if="cloudAccount?.profileAvailable" class="settings-hint">云端档案更新时间：{{ formatCloudAccountTime(cloudAccount.remoteUpdatedAt) }}。恢复前请确认这是要使用的设备档案。</p>
        <p class="settings-hint"><ShieldCheck :size="15" />账号档案属于当前百度网盘账号；首次在新设备使用时，请先配置凭证并完成授权，再选择从云端恢复。</p>
      </section>
    </template>
  </section>
</template>
