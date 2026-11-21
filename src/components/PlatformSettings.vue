<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { CheckCircle2, CloudDownload, CloudUpload, ExternalLink, FolderOpen, HardDrive, KeyRound, RefreshCw, Save, ShieldCheck, XCircle } from "@lucide/vue";
import { buildBaiduAuthorizeUrl, confirmAction, exchangeBaiduCode, getBaiduConfig, getBaiduQuota, getBaiduStatus, getCloudAccountStatus, getLibrarySettings, openBaiduAuthorizePage, resumeLibraryMigrationTask, saveBaiduConfig, setBaiduAutoUpload, startDownloadCloudAccountTask, startSetLibraryRootTask, startUploadCloudAccountTask, updateBaiduSaveSyncSettings } from "../api";
import type { AppTask, BaiduConfigView, BaiduQuota, BaiduStatus, CloudAccountStatus, LibrarySettings } from "../api";
import { open } from "@tauri-apps/plugin-dialog";
import { useTaskFeed } from "../taskFeed";

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
const openingAuthorization = ref(false);
const browserOpenError = ref("");
const error = ref("");
const message = ref("");
const library = ref<LibrarySettings | null>(null);
const libraryTaskId = ref("");
const libraryTaskMessage = ref("");
const libraryTaskProgress = ref(0);
const handledLibraryTasks = new Set<string>();
const cloudAccount = ref<CloudAccountStatus | null>(null);
const cloudAccountTaskId = ref("");
const cloudAccountTaskMessage = ref("");
const cloudAccountTaskProgress = ref(0);
const accountTaskFeed = useTaskFeed();
const accountTrackingError = computed(() => cloudAccountTaskId.value && accountTaskFeed.error.value
  ? "任务状态暂时无法读取，正在自动重试；后台任务可能仍在执行。" : "");
const libraryTrackingError = computed(() => libraryTaskId.value && accountTaskFeed.error.value
  ? "迁移状态暂时无法读取，正在自动重试；请勿手动删除新旧游戏库。" : "");
let disposed = false;
let authorizationGeneration = 0;
let refreshGeneration = 0;

watch([appKey, secretKey], () => {
  authorizeUrl.value = "";
  code.value = "";
  browserOpenError.value = "";
});

async function refresh() {
  const generation = ++refreshGeneration;
  const authorization = authorizationGeneration;
  loading.value = true;
  error.value = "";
  try {
    const [nextConfig, nextStatus, nextLibrary] = await Promise.all([getBaiduConfig(), getBaiduStatus(), getLibrarySettings()]);
    if (disposed || generation !== refreshGeneration || authorization !== authorizationGeneration) return;
    config.value = nextConfig;
    status.value = nextStatus;
    library.value = nextLibrary;
    appKey.value = nextConfig.appKey || "";
    quota.value = null;
    cloudAccount.value = null;
    if (nextStatus.authorized && !nextStatus.expired && !nextStatus.refreshError) {
      const [nextQuota, nextAccount] = await Promise.all([getBaiduQuota(), getCloudAccountStatus()]);
      if (disposed || generation !== refreshGeneration || authorization !== authorizationGeneration) return;
      quota.value = nextQuota;
      cloudAccount.value = nextAccount;
    } else {
      quota.value = null;
      cloudAccount.value = null;
    }
  } catch (reason) {
    if (!disposed && generation === refreshGeneration && authorization === authorizationGeneration) error.value = String(reason);
  } finally {
    if (!disposed && generation === refreshGeneration) loading.value = false;
  }
}

async function syncCloudAccount(direction: "upload" | "download") {
  if (saving.value || authorizing.value || cloudAccountTaskId.value || !status.value?.authorized || status.value.expired || status.value.refreshError) return;
  if (direction === "download" && !(await confirmAction("将按游戏标识匹配并合并云端设置，可能更新对应游戏的名称、启动配置和存档保护范围，也会恢复自动上传设置。请先结束游戏和其他后台任务。游戏本体和本机路径不会被删除。继续吗？"))) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    cloudAccountTaskId.value = direction === "upload" ? await startUploadCloudAccountTask() : await startDownloadCloudAccountTask();
    cloudAccountTaskProgress.value = 0;
    cloudAccountTaskMessage.value = direction === "upload" ? "准备上传 GameSaver 云端档案" : "准备恢复 GameSaver 云端档案";
    await accountTaskFeed.refresh();
  } catch (reason) {
    cloudAccountTaskId.value = "";
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

function updateAccountTask(tasks: AppTask[]) {
  if (disposed) return;
  const task = cloudAccountTaskId.value
    ? tasks.find(item => item.taskId === cloudAccountTaskId.value)
    : tasks.find(item => ["cloud_account_sync", "restore_cloud_account"].includes(item.taskType)
      && ["pending", "running"].includes(item.status));
  if (!task) return;
  cloudAccountTaskId.value = task.taskId;
  cloudAccountTaskProgress.value = task.progress;
  cloudAccountTaskMessage.value = task.message;
  if (["success", "failed", "cancelled", "interrupted"].includes(task.status)) {
    cloudAccountTaskId.value = "";
    if (task.status === "success") {
      message.value = task.message;
      void refreshAccountResult(task.taskType === "restore_cloud_account");
    } else {
      error.value = task.error || task.message;
    }
    return;
  }
}

async function refreshAccountResult(restored: boolean) {
  const authorization = authorizationGeneration;
  const [nextAccount, nextConfig] = await Promise.allSettled([
    getCloudAccountStatus(), restored ? getBaiduConfig() : Promise.resolve(null),
  ]);
  if (disposed || authorization !== authorizationGeneration) return;
  if (nextAccount.status === "fulfilled") cloudAccount.value = nextAccount.value;
  if (nextConfig.status === "fulfilled" && nextConfig.value) {
    config.value = nextConfig.value;
    appKey.value = nextConfig.value.appKey || "";
  }
  const failure = [nextAccount, nextConfig].find(item => item.status === "rejected");
  if (failure?.status === "rejected") error.value = `档案任务已完成，但刷新设置失败：${String(failure.reason)}`;
}

watch(accountTaskFeed.tasks, updateAccountTask, { immediate: true });

function formatLibraryBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

async function chooseLibraryRoot() {
  if (saving.value || libraryTaskId.value || library.value?.pendingMigrationSource) return;
  try {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected !== "string" || !selected.trim() || disposed) return;
    await startLibraryTask(selected);
  } catch (reason) {
    if (!disposed) error.value = String(reason);
  }
}

async function startLibraryTask(target?: string) {
  if (saving.value || libraryTaskId.value) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    const taskId = await (target ? startSetLibraryRootTask(target) : resumeLibraryMigrationTask());
    if (disposed) return;
    libraryTaskId.value = taskId;
    libraryTaskProgress.value = 0;
    libraryTaskMessage.value = target ? "准备迁移游戏库" : "准备恢复迁移或重试清理";
    await accountTaskFeed.refresh();
  } catch (reason) {
    if (!disposed) error.value = String(reason);
  } finally {
    if (!disposed) saving.value = false;
  }
}

function updateLibraryTask(tasks: AppTask[]) {
  if (disposed) return;
  const task = libraryTaskId.value
    ? tasks.find(item => item.taskId === libraryTaskId.value)
    : tasks.find(item => ["set_library_root", "resume_library_migration"].includes(item.taskType)
      && ["pending", "running"].includes(item.status));
  if (!task) return;
  if (handledLibraryTasks.has(task.taskId)) {
    if (libraryTaskId.value === task.taskId) libraryTaskId.value = "";
    return;
  }
  libraryTaskId.value = task.taskId;
  libraryTaskProgress.value = task.progress;
  libraryTaskMessage.value = task.message;
  if (["success", "failed", "cancelled", "interrupted"].includes(task.status)) {
    handledLibraryTasks.add(task.taskId);
    libraryTaskId.value = "";
    if (task.status === "success") message.value = task.message;
    else error.value = task.error || task.message;
    void refreshLibraryResult();
  }
}

async function refreshLibraryResult() {
  try {
    const next = await getLibrarySettings();
    if (!disposed) library.value = next;
  } catch (reason) {
    if (!disposed) error.value = `迁移任务已结束，但读取存储状态失败：${String(reason)}`;
  }
}

watch(accountTaskFeed.tasks, updateLibraryTask, { immediate: true });

async function save() {
  if (disposed || saving.value || authorizing.value) return;
  const generation = ++authorizationGeneration;
  ++refreshGeneration;
  loading.value = false;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    const nextConfig = await saveBaiduConfig(appKey.value, secretKey.value);
    if (disposed || generation !== authorizationGeneration) return;
    config.value = nextConfig;
    secretKey.value = "";
    authorizeUrl.value = "";
    code.value = "";
    status.value = null;
    quota.value = null;
    cloudAccount.value = null;
    message.value = "百度应用凭证已安全保存，正在刷新授权状态";
    const nextStatus = await getBaiduStatus();
    if (disposed || generation !== authorizationGeneration) return;
    status.value = nextStatus;
    message.value = nextStatus.authorized ? "百度应用凭证已安全保存" : "百度应用凭证已安全保存，请重新完成百度授权";
  } catch (reason) {
    if (!disposed && generation === authorizationGeneration) {
      status.value = null;
      quota.value = null;
      cloudAccount.value = null;
      error.value = String(reason);
    }
  } finally {
    if (!disposed && generation === authorizationGeneration) saving.value = false;
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

async function updateSaveSyncSettings(field: "autoSyncSave" | "checkCloudSaveOnLaunch" | "cloudSaveKeepLimit", value: boolean | number) {
  if (!config.value?.configured) return;
  saving.value = true;
  error.value = "";
  message.value = "";
  try {
    const autoSyncSave = field === "autoSyncSave" ? Boolean(value) : config.value.autoSyncSave;
    const checkCloudSaveOnLaunch = field === "checkCloudSaveOnLaunch" ? Boolean(value) : config.value.checkCloudSaveOnLaunch;
    const cloudSaveKeepLimit = field === "cloudSaveKeepLimit" ? Number(value) : config.value.cloudSaveKeepLimit;
    config.value = await updateBaiduSaveSyncSettings(autoSyncSave, checkCloudSaveOnLaunch, cloudSaveKeepLimit);
    message.value = "存档云同步设置已更新";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    saving.value = false;
  }
}

async function prepareAuthorization() {
  if (disposed || saving.value || authorizing.value) return;
  const generation = ++authorizationGeneration;
  ++refreshGeneration;
  loading.value = false;
  authorizing.value = true;
  authorizeUrl.value = "";
  browserOpenError.value = "";
  code.value = "";
  error.value = "";
  message.value = "";
  try {
    const url = await buildBaiduAuthorizeUrl();
    if (disposed || generation !== authorizationGeneration) return;
    authorizeUrl.value = url;
    message.value = "请打开授权页面，完成授权后复制页面中的 Code。";
  } catch (reason) {
    if (!disposed && generation === authorizationGeneration) error.value = String(reason);
  } finally {
    if (!disposed && generation === authorizationGeneration) authorizing.value = false;
  }
}

async function openAuthorizationPage() {
  if (disposed || saving.value || authorizing.value || !config.value?.configured || !authorizeUrl.value) return;
  const generation = ++authorizationGeneration;
  ++refreshGeneration;
  loading.value = false;
  authorizing.value = true;
  openingAuthorization.value = true;
  browserOpenError.value = "";
  error.value = "";
  message.value = "";
  try {
    const url = await openBaiduAuthorizePage();
    if (disposed || generation !== authorizationGeneration) return;
    authorizeUrl.value = url;
    message.value = "已请求系统浏览器打开百度授权页面，完成后请将 Code 粘贴回来。";
  } catch (reason) {
    if (!disposed && generation === authorizationGeneration) browserOpenError.value = String(reason);
  } finally {
    if (!disposed && generation === authorizationGeneration) {
      authorizing.value = false;
      openingAuthorization.value = false;
    }
  }
}

async function exchangeCode() {
  if (disposed || saving.value || authorizing.value || !code.value.trim()) return;
  const generation = ++authorizationGeneration;
  ++refreshGeneration;
  loading.value = false;
  authorizing.value = true;
  error.value = "";
  message.value = "正在验证授权 Code";
  try {
    await exchangeBaiduCode(code.value);
    if (disposed || generation !== authorizationGeneration) return;
    code.value = "";
    authorizeUrl.value = "";
    message.value = "百度网盘授权成功";
    await refresh();
  } catch (reason) {
    if (!disposed && generation === authorizationGeneration) error.value = String(reason);
  } finally {
    if (!disposed && generation === authorizationGeneration) authorizing.value = false;
  }
}

function statusLabel(): string {
  if (!config.value?.configured) return "未配置应用凭证";
  if (!status.value?.authorized) return "已配置，未授权";
  if (status.value.refreshError) return status.value.reauthorizationRequired ? "需要重新授权" : "连接暂时不可用";
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
  disposed = true;
  ++authorizationGeneration;
  ++refreshGeneration;
  accountTaskFeed.release();
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
          <div class="library-root-row"><div><span class="settings-label">当前游戏库</span><strong class="library-path">{{ library.libraryRoot }}</strong></div><button class="secondary-button" type="button" :disabled="saving || !!libraryTaskId || !!library.pendingMigrationSource" @click="chooseLibraryRoot"><FolderOpen :size="16" />{{ libraryTaskId ? "迁移中" : "更换位置" }}</button></div>
          <div class="quota-grid library-usage-grid"><div><span>游戏本体</span><strong>{{ formatLibraryBytes(library.gamesBytes) }}</strong><small>{{ library.gamesPath }}</small></div><div><span>本体 ZIP</span><strong>{{ formatLibraryBytes(library.bodyPackagesBytes) }}</strong><small>{{ library.bodyPackagesPath }}</small></div><div><span>存档版本</span><strong>{{ formatLibraryBytes(library.savesBytes) }}</strong><small>{{ library.savesPath }}</small></div></div>
          <p class="settings-hint"><HardDrive :size="15" />已占用 {{ formatLibraryBytes(library.totalBytes) }}，包含 {{ library.fileCount.toLocaleString() }} 个文件；当前磁盘可用 {{ formatLibraryBytes(library.freeBytes) }}。迁移完成并校验后，旧位置的大文件会被清理。</p>
          <div v-if="libraryTaskId" class="library-migration-progress"><div class="task-progress-heading"><span>{{ libraryTaskMessage }}</span><strong>{{ libraryTaskProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${libraryTaskProgress}%` }"></span></div></div>
          <p v-if="libraryTrackingError" class="settings-alert settings-alert-error">{{ libraryTrackingError }}</p>
          <div v-if="library.pendingMigrationSource && !libraryTaskId" class="settings-alert settings-alert-error"><span>{{ library.migrationWarning }}</span><button class="secondary-button" type="button" :disabled="saving" @click="startLibraryTask()"><RefreshCw :size="16" />继续迁移 / 重试清理</button></div>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><KeyRound :size="20" /></div><div><p class="eyebrow">云端服务</p><h2>百度网盘</h2><p>用于上传和下载游戏本体 ZIP。GameSaver 直连百度网盘，不使用代理。</p></div><span class="settings-status" :class="{ connected: status?.authorized && !status.expired && !status.refreshError }">{{ statusLabel() }}</span></div>
        <div class="settings-form">
          <label><span>AppKey</span><input v-model="appKey" type="text" autocomplete="off" :disabled="saving || authorizing" placeholder="输入百度开放平台 AppKey" /></label>
          <label><span>SecretKey</span><input v-model="secretKey" type="password" autocomplete="new-password" :disabled="saving || authorizing" :placeholder="config?.secretKeyConfigured ? '已保存，如需更换请重新输入' : '输入百度开放平台 SecretKey'" /></label>
          <p class="settings-hint"><ShieldCheck :size="15" />SecretKey 和授权 Token 使用 Windows 本机加密保存，不会写入日志、迁移包或云端。</p>
          <button class="primary-button" type="button" :disabled="saving || authorizing || !appKey.trim() || !secretKey.trim()" @click="save"><Save :size="16" />{{ saving ? "保存中" : "保存凭证" }}</button>
          <button class="settings-toggle" type="button" :disabled="saving || !config?.configured" @click="toggleAutoUpload"><span class="toggle-track" :class="{ enabled: config?.autoUploadBody }"><i></i></span><span><strong>自动上传游戏本体</strong><small>本地 ZIP 成功创建后，自动加入上传队列；失败不会影响本地游戏。</small></span></button>
          <button class="settings-toggle" type="button" :disabled="saving || !config?.configured" @click="updateSaveSyncSettings('autoSyncSave', !config?.autoSyncSave)"><span class="toggle-track" :class="{ enabled: config?.autoSyncSave }"><i></i></span><span><strong>游戏退出后自动同步存档</strong><small>每次正常退出游戏且产生新存档后，自动增量压缩并同步至百度网盘。</small></span></button>
          <button class="settings-toggle" type="button" :disabled="saving || !config?.configured" @click="updateSaveSyncSettings('checkCloudSaveOnLaunch', !config?.checkCloudSaveOnLaunch)"><span class="toggle-track" :class="{ enabled: config?.checkCloudSaveOnLaunch }"><i></i></span><span><strong>启动游戏前检查云端存档</strong><small>启动游戏前比对本地与云端进度，发现跨设备新存档时智能提醒。</small></span></button>
          <div class="settings-select-row" v-if="config?.configured">
            <span>云端存档历史版本保留数量</span>
            <select :value="config?.cloudSaveKeepLimit || 10" :disabled="saving" @change="updateSaveSyncSettings('cloudSaveKeepLimit', Number(($event.target as HTMLSelectElement).value))">
              <option :value="1">保留 1 个最新版本</option>
              <option :value="3">保留 3 个历史版本</option>
              <option :value="5">保留 5 个历史版本</option>
              <option :value="10">保留 10 个历史版本</option>
              <option :value="20">保留 20 个历史版本</option>
            </select>
          </div>
          <p v-if="status?.refreshError" class="settings-hint settings-hint-warning"><XCircle :size="15" />{{ status.refreshError }}<template v-if="!status.reauthorizationRequired">。可稍后重试，无需立即重新授权。</template></p>
        </div>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><ShieldCheck :size="20" /></div><div><p class="eyebrow">授权状态</p><h2>百度网盘授权</h2><p>桌面端使用 OOB 授权，不需要本地回调地址。</p></div></div>
        <div class="settings-actions"><button class="secondary-button" type="button" :disabled="saving || authorizing || !config?.configured" @click="prepareAuthorization">生成授权页面</button><button v-if="authorizeUrl" class="secondary-button" type="button" :disabled="saving || authorizing" @click="openAuthorizationPage"><ExternalLink :size="15" />{{ openingAuthorization ? "正在打开浏览器" : "打开百度授权页面" }}</button></div>
        <p v-if="browserOpenError" class="settings-alert settings-alert-error" role="alert">{{ browserOpenError }}</p>
        <label v-if="authorizeUrl" class="code-field"><span>授权地址</span><input :value="authorizeUrl" type="text" readonly aria-label="百度授权地址，可复制到浏览器打开" @focus="($event.target as HTMLInputElement).select()" /></label>
        <p v-if="authorizeUrl" class="settings-hint">浏览器未打开时，可选中并复制上方授权地址，手动粘贴到浏览器访问。</p>
        <label class="code-field"><span>授权 Code</span><input v-model="code" type="text" autocomplete="off" :disabled="saving || authorizing" placeholder="授权后粘贴页面显示的 Code" /><button class="secondary-button" type="button" :disabled="saving || authorizing || !code.trim()" @click="exchangeCode">{{ authorizing && !openingAuthorization ? "验证中" : "完成授权" }}</button></label>
        <p class="settings-hint">请在系统浏览器完成百度授权，再将页面显示的 Code 粘贴回这里。</p>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><CheckCircle2 :size="20" /></div><div><p class="eyebrow">账户概览</p><h2>网盘空间</h2><p>上传游戏本体前会自动检查剩余空间。</p></div></div>
        <div v-if="quota" class="quota-grid"><div><span>总空间</span><strong>{{ formatBytes(quota.total) }}</strong></div><div><span>已使用</span><strong>{{ formatBytes(quota.used) }}</strong></div><div><span>可用空间</span><strong>{{ formatBytes(quota.free) }}</strong></div></div>
        <p v-else class="settings-empty">完成百度网盘授权后，这里会显示空间信息。</p>
      </section>

      <section class="settings-card">
        <div class="settings-card-heading"><div class="settings-card-icon"><RefreshCw :size="20" /></div><div><p class="eyebrow">跨设备同步</p><h2>GameSaver 云端档案</h2><p>同步游戏清单、启动设置、存档保护配置和云端本体版本引用。不会上传 token、密钥、日志或本机路径。</p></div><span class="settings-status" :class="{ connected: cloudAccount?.profileAvailable }">{{ !status?.authorized ? "未连接百度网盘" : cloudAccount?.profileAvailable ? "已有云端档案" : "尚未创建" }}</span></div>
        <div class="settings-actions"><button class="secondary-button" type="button" :disabled="saving || authorizing || !status?.authorized || status.expired || !!status.refreshError || !!cloudAccountTaskId" @click="syncCloudAccount('upload')"><CloudUpload :size="16" />上传本机档案</button><button class="secondary-button" type="button" :disabled="saving || authorizing || !status?.authorized || status.expired || !!status.refreshError || !cloudAccount?.profileAvailable || !!cloudAccountTaskId" @click="syncCloudAccount('download')"><CloudDownload :size="16" />从云端恢复</button></div>
        <div v-if="cloudAccountTaskId" class="library-migration-progress"><div class="task-progress-heading"><span>{{ cloudAccountTaskMessage }}</span><strong>{{ cloudAccountTaskProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${cloudAccountTaskProgress}%` }"></span></div></div>
        <p v-if="accountTrackingError" class="settings-hint">{{ accountTrackingError }}</p>
        <p v-if="cloudAccount?.profileAvailable" class="settings-hint">云端档案更新时间：{{ formatCloudAccountTime(cloudAccount.remoteUpdatedAt) }}。恢复前请确认这是要使用的设备档案。</p>
        <p class="settings-hint"><ShieldCheck :size="15" />账号档案属于当前百度网盘账号；首次在新设备使用时，请先配置凭证并完成授权，再选择从云端恢复。</p>
      </section>
    </template>
  </section>
</template>
