<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, ArrowLeft, CheckCircle2, Clock3, Folder, FolderOpen, Gamepad2, HardDrive, LoaderCircle, Play, RefreshCw, RotateCcw, ShieldCheck, Trash2 } from "@lucide/vue";
import { deleteSaveVersion, getGameRuntime, getTask, launchGame, listGameBodyVersions, listSaveVersions, precheckGameLaunch, pruneSaveVersions, restoreSaveVersion, updateGameBody } from "../api";
import type { Game, GameBodyVersion, GameRuntime, LaunchPrecheck, SaveVersion } from "../domain/game";

const props = defineProps<{ game: Game; initialError?: string }>();
const emit = defineEmits<{ back: []; refresh: [] }>();

const precheck = ref<LaunchPrecheck | null>(null);
const runtime = ref<GameRuntime | null>(null);
const versions = ref<SaveVersion[]>([]);
const bodyVersions = ref<GameBodyVersion[]>([]);
const loading = ref(true);
const busy = ref(false);
const error = ref(props.initialError || "");
const message = ref("");
const keepVersions = ref(5);
let pollTimer: ReturnType<typeof setTimeout> | undefined;

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [nextPrecheck, nextVersions, nextRuntime, nextBodyVersions] = await Promise.all([
      precheckGameLaunch(props.game.gameUid),
      listSaveVersions(props.game.gameUid),
      getGameRuntime(props.game.gameUid),
      listGameBodyVersions(props.game.gameUid),
    ]);
    precheck.value = nextPrecheck;
    versions.value = nextVersions;
    runtime.value = nextRuntime;
    bodyVersions.value = nextBodyVersions;
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

async function start() {
  if (busy.value || !precheck.value?.canLaunch) return;
  busy.value = true;
  error.value = "";
  message.value = "正在启动游戏";
  try {
    const taskId = await launchGame(props.game.gameUid);
    await watchTask(taskId);
  } catch (reason) {
    error.value = String(reason);
    busy.value = false;
  }
}

async function watchTask(taskId: string) {
  try {
    const task = await getTask(taskId);
    runtime.value = await getGameRuntime(props.game.gameUid);
    message.value = task.message;
    if (task.status === "success") {
      busy.value = false;
      await refresh();
      emit("refresh");
      return;
    }
    if (task.status === "failed" || task.status === "cancelled") {
      busy.value = false;
      const failure = task.error || task.message;
      await refresh();
      error.value = failure;
      return;
    }
    pollTimer = setTimeout(() => void watchTask(taskId), 700);
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function restoreVersion(version: SaveVersion) {
  if (busy.value || runtime.value) return;
  if (!window.confirm("恢复前会先保护当前存档，确定恢复这个版本吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备恢复保存版本";
  try {
    await watchTask(await restoreSaveVersion(props.game.gameUid, version.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function deleteVersion(version: SaveVersion) {
  if (busy.value || runtime.value) return;
  if (!window.confirm("删除后将无法从这个版本恢复，确定继续吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备删除保存版本";
  try {
    await watchTask(await deleteSaveVersion(props.game.gameUid, version.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function pruneVersions() {
  if (busy.value || runtime.value) return;
  if (!window.confirm(`仅保留最近 ${keepVersions.value} 个版本，确定清理旧版本吗？`)) return;
  busy.value = true;
  error.value = "";
  message.value = "准备清理旧保存版本";
  try {
    await watchTask(await pruneSaveVersions(props.game.gameUid, keepVersions.value));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function updateBody() {
  if (busy.value || runtime.value) return;
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  if (!window.confirm("新版游戏文件夹会覆盖当前受管游戏本体。当前存档会先保护，确定开始更新吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备更新游戏本体";
  try {
    await watchTask(await updateGameBody(props.game.gameUid, selected));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

function stopPolling() {
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = undefined;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

function formatDate(value: string): string {
  const timestamp = Number(value);
  const date = Number.isFinite(timestamp) ? new Date(timestamp * 1000) : new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function runtimeLabel(status: GameRuntime["status"]): string {
  if (status === "launching") return "正在启动";
  if (status === "running") return "运行中";
  return "正在保护存档";
}

onMounted(() => void refresh());
onUnmounted(stopPolling);
</script>

<template>
  <section class="game-detail-page page-enter">
    <header class="detail-header">
      <button class="icon-button" type="button" title="返回游戏库" aria-label="返回游戏库" @click="emit('back')"><ArrowLeft :size="18" /></button>
      <div class="detail-heading"><p class="eyebrow">游戏详情</p><h1>{{ game.displayName }}</h1><p>管理本体、启动和存档保护。</p></div>
      <button class="icon-button detail-refresh" type="button" title="刷新游戏状态" aria-label="刷新游戏状态" :disabled="loading || busy" @click="refresh"><RefreshCw :size="17" /></button>
    </header>

    <div v-if="loading" class="state-panel detail-loading"><span class="loader"></span><strong>正在读取游戏状态</strong></div>
    <div v-else-if="error && !precheck" class="state-panel error-state"><AlertTriangle :size="25" /><strong>读取游戏状态失败</strong><p>{{ error }}</p><button type="button" @click="refresh">重试</button></div>
    <template v-else>
      <section class="detail-hero">
        <div class="detail-cover"><Gamepad2 :size="42" /></div>
        <div class="detail-hero-copy"><span class="status-label">{{ runtime ? runtimeLabel(runtime.status) : (precheck?.canLaunch ? "可启动" : "需要处理") }}</span><h2>{{ precheck?.canLaunch ? "准备就绪" : "启动前需要处理" }}</h2><p>{{ message || (precheck?.canLaunch ? "游戏本体和存档保护配置均可用。" : "完成下方检查后才能启动游戏。") }}</p><button class="primary-button detail-launch" type="button" :disabled="busy || !precheck?.canLaunch" @click="start"><LoaderCircle v-if="busy" :size="17" class="spin" /><Play v-else :size="17" />{{ busy ? "游戏运行中" : "启动游戏" }}</button></div>
      </section>

      <p v-if="error" class="error-message" role="alert">{{ error }}</p>

      <div class="detail-columns">
        <section class="detail-section"><header class="detail-section-header"><div><p class="eyebrow">启动前检查</p><h2>运行环境</h2></div><CheckCircle2 v-if="precheck?.canLaunch" class="detail-ok" :size="20" /><AlertTriangle v-else class="detail-warning" :size="20" /></header><div class="check-list"><div class="check-row"><span><Folder :size="16" />游戏本体目录</span><strong :class="{ good: game.managedPath && precheck?.canLaunch }">{{ game.managedPath ? "已找到" : "缺失" }}</strong></div><div class="check-row"><span><Play :size="16" />启动程序</span><strong :class="{ good: precheck?.executableExists }">{{ precheck?.executableExists ? "已找到" : "缺失" }}</strong></div><div class="check-row"><span><ShieldCheck :size="16" />存档保护</span><strong :class="{ good: precheck?.saveProfileReady && (precheck?.validScopeCount || 0) > 0 }">{{ precheck?.saveProfileReady ? `${precheck?.validScopeCount} 个范围` : "未设置" }}</strong></div></div><div v-if="precheck?.issues.length" class="issue-list"><p v-for="issue in precheck.issues" :key="issue">{{ issue }}</p></div></section>

        <section class="detail-section">
          <header class="detail-section-header"><div><p class="eyebrow">游戏本体</p><h2>受管目录</h2></div><HardDrive :size="20" class="detail-muted" /></header>
          <p class="managed-path">{{ game.managedPath }}</p>
          <dl class="detail-facts"><div><dt>启动文件</dt><dd>{{ game.launch.executableRelativePath }}</dd></div><div><dt>保存版本</dt><dd>{{ versions.length }} 个</dd></div><div><dt>旧本体版本</dt><dd>{{ bodyVersions.length }} 个</dd></div></dl>
          <button class="secondary-button body-update-button" type="button" :disabled="busy || !!runtime" title="选择新版游戏文件夹并更新" @click="updateBody"><LoaderCircle v-if="busy && (message.includes('更新') || message.includes('新版'))" :size="16" class="spin" /><FolderOpen v-else :size="16" />更新游戏本体</button>
          <div v-if="bodyVersions.length" class="body-version-list"><p class="timeline-caption">保留的旧本体</p><div v-for="version in bodyVersions" :key="version.versionId" class="body-version-row"><span>{{ formatDate(version.createdAt) }}</span><strong>{{ version.fileCount }} 个文件 · {{ formatBytes(version.totalBytes) }}</strong></div></div>
        </section>
      </div>

      <section class="detail-section save-timeline">
        <header class="detail-section-header version-header">
          <div><p class="eyebrow">存档保护</p><h2>保存版本</h2></div>
          <div class="version-tools">
            <label>保留
              <select v-model.number="keepVersions" :disabled="busy || !!runtime" aria-label="保留保存版本数量">
                <option :value="1">1 个</option><option :value="3">3 个</option><option :value="5">5 个</option><option :value="10">10 个</option>
              </select>
            </label>
            <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime || versions.length <= keepVersions" title="清理旧保存版本" @click="pruneVersions"><Trash2 :size="15" />清理</button>
          </div>
        </header>
        <p class="timeline-caption">游戏退出后自动提交，恢复前会先保护当前存档</p>
        <div v-if="!versions.length" class="timeline-empty"><ShieldCheck :size="22" /><p>还没有保存版本。启动游戏并正常退出一次后，GameSaver 会在这里记录版本。</p></div>
        <div v-else class="version-list">
          <article v-for="(version, index) in versions" :key="version.versionId" class="version-row">
            <div class="version-icon"><Clock3 :size="17" /></div>
            <div class="version-copy"><strong>{{ index === 0 ? "最近一次保存" : "保存版本" }}</strong><span>{{ formatDate(version.createdAt) }}</span></div>
            <div class="version-meta"><strong>{{ version.files.length }} 个文件</strong><span>{{ formatBytes(version.totalBytes) }}</span></div>
            <div class="version-actions">
              <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime" title="恢复这个保存版本" @click="restoreVersion(version)"><RotateCcw :size="15" />恢复</button>
              <button class="icon-button danger-button" type="button" :disabled="busy || !!runtime" title="删除这个保存版本" :aria-label="`删除 ${formatDate(version.createdAt)} 保存版本`" @click="deleteVersion(version)"><Trash2 :size="15" /></button>
            </div>
          </article>
        </div>
      </section>
    </template>
  </section>
</template>
