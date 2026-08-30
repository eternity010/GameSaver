<script setup lang="ts">
import { onMounted, ref } from "vue";
import { CheckCircle2, ExternalLink, KeyRound, RefreshCw, Save, ShieldCheck, XCircle } from "@lucide/vue";
import { buildBaiduAuthorizeUrl, exchangeBaiduCode, getBaiduConfig, getBaiduQuota, getBaiduStatus, saveBaiduConfig, setBaiduAutoUpload } from "../api";
import type { BaiduConfigView, BaiduQuota, BaiduStatus } from "../api";

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

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [nextConfig, nextStatus] = await Promise.all([getBaiduConfig(), getBaiduStatus()]);
    config.value = nextConfig;
    status.value = nextStatus;
    appKey.value = nextConfig.appKey || "";
    quota.value = nextStatus.authorized && !nextStatus.expired && !nextStatus.refreshError ? await getBaiduQuota() : null;
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
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

onMounted(() => void refresh());
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
    </template>
  </section>
</template>
