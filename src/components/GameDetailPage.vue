<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, Archive, ArrowLeft, CheckCircle2, Clock3, Folder, FolderOpen, Gamepad2, HardDrive, ImagePlus, LoaderCircle, Play, RefreshCw, RotateCcw, ShieldCheck, Trash2, Upload, X } from "@lucide/vue";
import { deleteGameBodyPackage, deleteSaveVersion, getBaiduConfig, getBaiduStatus, getGameCover, getGameRuntime, getTask, launchGame, listGameBodyVersions, listSaveVersions, packageGameBody, precheckGameLaunch, pruneSaveVersions, restoreSaveVersion, saveGameCover, uninstallGameBody, updateGameBody, uploadGameBodyPackage } from "../api";
import type { BaiduConfigView, BaiduStatus } from "../api";
import type { CoverCrop, CoverPosition, Game, GameBodyVersion, GameRuntime, LaunchPrecheck, SaveVersion } from "../domain/game";

const props = defineProps<{ game: Game; initialError?: string; coverUrl?: string }>();
const emit = defineEmits<{ back: []; refresh: []; settings: [] }>();

const precheck = ref<LaunchPrecheck | null>(null);
const runtime = ref<GameRuntime | null>(null);
const versions = ref<SaveVersion[]>([]);
const bodyVersions = ref<GameBodyVersion[]>([]);
const baiduStatus = ref<BaiduStatus | null>(null);
const baiduConfig = ref<BaiduConfigView | null>(null);
const loading = ref(true);
const busy = ref(false);
const error = ref(props.initialError || "");
const message = ref("");
const taskProgress = ref(0);
const keepVersions = ref(5);
const coverInput = ref<HTMLInputElement | null>(null);
const coverDisplayUrl = ref(props.coverUrl || "");
const coverSourceUrl = ref("");
const coverImage = ref<HTMLImageElement | null>(null);
const coverOriginalBytes = ref<number[]>([]);
const coverOriginalExtension = ref("jpg");
const coverEditorOpen = ref(false);
const coverSaving = ref(false);
const coverError = ref("");
const coverZoom = ref(1);
const coverOffsetX = ref(0);
const coverOffsetY = ref(0);
const coverDragging = ref(false);
let coverOwnedUrl = "";
let coverPointerX = 0;
let coverPointerY = 0;
const COVER_STAGE_WIDTH = 640;
const COVER_STAGE_HEIGHT = 360;
let pollTimer: ReturnType<typeof setTimeout> | undefined;

async function refresh() {
  loading.value = true;
  error.value = "";
  try {
    const [nextPrecheck, nextVersions, nextRuntime, nextBodyVersions, nextBaiduStatus, nextBaiduConfig] = await Promise.all([
      precheckGameLaunch(props.game.gameUid),
      listSaveVersions(props.game.gameUid),
      getGameRuntime(props.game.gameUid),
      listGameBodyVersions(props.game.gameUid),
      getBaiduStatus(),
      getBaiduConfig(),
    ]);
    precheck.value = nextPrecheck;
    versions.value = nextVersions;
    runtime.value = nextRuntime;
    bodyVersions.value = nextBodyVersions;
    baiduStatus.value = nextBaiduStatus;
    baiduConfig.value = nextBaiduConfig;
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
    taskProgress.value = task.progress;
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

async function packageBody() {
  if (busy.value || runtime.value) return;
  if (!window.confirm("将当前受管游戏本体压缩为 ZIP，并保存到本地缓存，确定继续吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备创建游戏本体包";
  try {
    await watchTask(await packageGameBody(props.game.gameUid));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function uninstallBody() {
  if (busy.value || runtime.value) return;
  if (!window.confirm("只会删除 GameSaver 管理的本地游戏本体，存档版本、游戏设置和云端版本会保留。确定卸载吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备卸载游戏本体";
  try {
    await watchTask(await uninstallGameBody(props.game.gameUid));
    emit("refresh");
    emit("back");
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function deleteBodyPackage(version: GameBodyVersion) {
  if (busy.value || runtime.value || !version.packagePath) return;
  if (!window.confirm(version.archivePath ? "删除本地 ZIP 后仍保留旧本体目录，确定继续吗？" : "删除本地 ZIP 后将无法从这个本体版本恢复，确定继续吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备删除本地本体包";
  try {
    await watchTask(await deleteGameBodyPackage(props.game.gameUid, version.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function uploadBody(version: GameBodyVersion) {
  if (busy.value || runtime.value || !version.packagePath || !baiduReady()) return;
  if (!window.confirm("将此版本的游戏本体 ZIP 上传到百度网盘，确定继续吗？")) return;
  busy.value = true;
  error.value = "";
  message.value = "准备上传游戏本体包";
  try {
    await watchTask(await uploadGameBodyPackage(props.game.gameUid, version.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

function bodyUploadLabel(version: GameBodyVersion): string {
  if (version.uploadStatus === "synced") {
    return "已上传";
  }
  if (version.uploadStatus === "failed") {
    return "上传失败";
  }
  if (version.uploadStatus === "syncing") return "上传中";
  if (version.uploadStatus === "manifest_pending") return "清单待修复";
  return "未上传";
}

function baiduReady(): boolean {
  return !!baiduConfig.value?.configured && !!baiduStatus.value?.authorized && !baiduStatus.value.expired && !baiduStatus.value.refreshError;
}

function baiduLabel(): string {
  if (!baiduConfig.value?.configured) return "未配置";
  if (!baiduStatus.value?.authorized) return "未授权";
  if (baiduStatus.value.refreshError) return "授权需要确认";
  return baiduStatus.value.expired ? "授权已过期" : "已连接";
}

function baiduActionLabel(): string {
  if (!baiduConfig.value?.configured) return "去配置百度网盘";
  if (!baiduStatus.value?.authorized || baiduStatus.value.expired || baiduStatus.value.refreshError) return "去重新授权";
  return "打开百度设置";
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

function chooseCover() {
  if (busy.value || runtime.value || coverSaving.value) return;
  coverInput.value?.click();
}

function coverExtension(file: File): string | null {
  const extension = file.name.split(".").pop()?.toLowerCase();
  if (file.type === "image/png" || extension === "png") return "png";
  if (file.type === "image/webp" || extension === "webp") return "webp";
  if (file.type === "image/jpeg" || extension === "jpg" || extension === "jpeg") return "jpg";
  return null;
}

async function handleCoverSelected(event: Event) {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = "";
  if (!file) return;
  if (file.size > 32 * 1024 * 1024) {
    coverError.value = "封面图片不能超过 32 MB";
    return;
  }
  const extension = coverExtension(file);
  if (!extension) {
    coverError.value = "只支持 JPG、PNG 或 WebP 图片";
    return;
  }
  const token = Date.now();
  const objectUrl = URL.createObjectURL(file);
  const image = new Image();
  image.onload = () => {
    if (token !== coverLoadToken) {
      URL.revokeObjectURL(objectUrl);
      return;
    }
    releaseCoverSource();
    coverSourceUrl.value = objectUrl;
    coverImage.value = image;
    void readCoverBytes(file, token, extension);
  };
  image.onerror = () => {
    URL.revokeObjectURL(objectUrl);
    coverError.value = "无法读取这张图片";
  };
  coverLoadToken = token;
  image.src = objectUrl;
}

let coverLoadToken = 0;

async function readCoverBytes(file: File, token: number, extension: string) {
  try {
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    if (token !== coverLoadToken) return;
    coverOriginalBytes.value = bytes;
    coverOriginalExtension.value = extension;
    coverZoom.value = 1;
    resetCoverPosition();
    coverError.value = "";
    coverEditorOpen.value = true;
  } catch (reason) {
    releaseCoverSource();
    coverError.value = `读取封面失败：${String(reason)}`;
  }
}

function coverGeometry() {
  const image = coverImage.value;
  if (!image || !image.naturalWidth || !image.naturalHeight) return { width: COVER_STAGE_WIDTH, height: COVER_STAGE_HEIGHT };
  const scale = Math.max(COVER_STAGE_WIDTH / image.naturalWidth, COVER_STAGE_HEIGHT / image.naturalHeight) * coverZoom.value;
  return { width: image.naturalWidth * scale, height: image.naturalHeight * scale };
}

function clampCoverPosition() {
  const geometry = coverGeometry();
  const minimumX = Math.min(0, COVER_STAGE_WIDTH - geometry.width);
  const minimumY = Math.min(0, COVER_STAGE_HEIGHT - geometry.height);
  coverOffsetX.value = Math.min(0, Math.max(minimumX, coverOffsetX.value));
  coverOffsetY.value = Math.min(0, Math.max(minimumY, coverOffsetY.value));
}

function resetCoverPosition() {
  const geometry = coverGeometry();
  coverOffsetX.value = (COVER_STAGE_WIDTH - geometry.width) / 2;
  coverOffsetY.value = (COVER_STAGE_HEIGHT - geometry.height) / 2;
}

function beginCoverDrag(event: PointerEvent) {
  if (coverSaving.value) return;
  coverDragging.value = true;
  coverPointerX = event.clientX;
  coverPointerY = event.clientY;
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
}

function moveCoverDrag(event: PointerEvent) {
  if (!coverDragging.value) return;
  const stage = event.currentTarget as HTMLElement;
  const scale = COVER_STAGE_WIDTH / stage.clientWidth;
  coverOffsetX.value += (event.clientX - coverPointerX) * scale;
  coverOffsetY.value += (event.clientY - coverPointerY) * scale;
  coverPointerX = event.clientX;
  coverPointerY = event.clientY;
  clampCoverPosition();
}

function endCoverDrag() {
  coverDragging.value = false;
}

function releaseCoverSource() {
  if (coverSourceUrl.value) URL.revokeObjectURL(coverSourceUrl.value);
  coverSourceUrl.value = "";
  coverImage.value = null;
}

function dismissCoverEditor() {
  coverEditorOpen.value = false;
  coverOriginalBytes.value = [];
  coverError.value = "";
  releaseCoverSource();
}

function closeCoverEditor() {
  if (coverSaving.value) return;
  dismissCoverEditor();
}

async function saveCover() {
  const image = coverImage.value;
  if (!image || !coverOriginalBytes.value.length || coverSaving.value) return;
  const canvas = document.createElement("canvas");
  canvas.width = 1280;
  canvas.height = 720;
  const context = canvas.getContext("2d");
  if (!context) {
    coverError.value = "当前环境无法生成封面预览";
    return;
  }
  const geometry = coverGeometry();
  context.fillStyle = "#202a38";
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.drawImage(image, coverOffsetX.value * 2, coverOffsetY.value * 2, geometry.width * 2, geometry.height * 2);
  const displayBlob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/jpeg", 0.9));
  if (!displayBlob) {
    coverError.value = "生成展示封面失败";
    return;
  }
  coverSaving.value = true;
  coverError.value = "";
  try {
    const displayBytes = Array.from(new Uint8Array(await displayBlob.arrayBuffer()));
    const crop: CoverCrop = { aspectWidth: 16, aspectHeight: 9, outputWidth: 1280, outputHeight: 720 };
    const position: CoverPosition = { offsetXMilli: Math.round(coverOffsetX.value * 1000), offsetYMilli: Math.round(coverOffsetY.value * 1000), zoomMilli: Math.round(coverZoom.value * 1000) };
    await saveGameCover(props.game.gameUid, coverOriginalBytes.value, displayBytes, coverOriginalExtension.value, crop, position);
    replaceOwnedCover(displayBytes);
    dismissCoverEditor();
    emit("refresh");
  } catch (reason) {
    coverError.value = String(reason);
  } finally {
    coverSaving.value = false;
  }
}

function replaceOwnedCover(bytes: number[]) {
  if (coverOwnedUrl) URL.revokeObjectURL(coverOwnedUrl);
  coverOwnedUrl = URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: "image/jpeg" }));
  coverDisplayUrl.value = coverOwnedUrl;
}

async function loadCover() {
  if (props.coverUrl) return;
  try {
    const bytes = await getGameCover(props.game.gameUid);
    if (bytes && !coverDisplayUrl.value) replaceOwnedCover(bytes);
  } catch {
    // Missing covers are represented by the placeholder.
  }
}

watch(() => props.coverUrl, (value) => {
  if (!coverOwnedUrl) coverDisplayUrl.value = value || "";
});

watch(coverZoom, clampCoverPosition);

onMounted(() => {
  void refresh();
  void loadCover();
});
onUnmounted(() => {
  stopPolling();
  releaseCoverSource();
  if (coverOwnedUrl) URL.revokeObjectURL(coverOwnedUrl);
});
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
        <div class="detail-cover">
          <img v-if="coverDisplayUrl" :src="coverDisplayUrl" :alt="`${game.displayName} 封面`" />
          <Gamepad2 v-else :size="42" />
          <button class="cover-edit-button" type="button" :disabled="busy || !!runtime || coverSaving" title="上传并调整游戏封面" @click="chooseCover"><ImagePlus :size="15" />{{ coverDisplayUrl ? "更换封面" : "上传封面" }}</button>
          <input ref="coverInput" class="visually-hidden" type="file" accept="image/jpeg,image/png,image/webp" @change="handleCoverSelected" />
        </div>
        <div class="detail-hero-copy"><span class="status-label">{{ runtime ? runtimeLabel(runtime.status) : (precheck?.canLaunch ? "可启动" : "需要处理") }}</span><h2>{{ precheck?.canLaunch ? "准备就绪" : "启动前需要处理" }}</h2><p>{{ message || (precheck?.canLaunch ? "游戏本体和存档保护配置均可用。" : "完成下方检查后才能启动游戏。") }}</p><button class="primary-button detail-launch" type="button" :disabled="busy || !precheck?.canLaunch" @click="start"><LoaderCircle v-if="busy" :size="17" class="spin" /><Play v-else :size="17" />{{ busy ? "游戏运行中" : "启动游戏" }}</button></div>
      </section>

      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <div v-if="busy" class="task-progress"><div class="task-progress-heading"><strong>{{ message || "正在处理" }}</strong><span>{{ taskProgress }}%</span></div><div class="progress-track"><span :style="{ width: `${taskProgress}%` }"></span></div></div>

      <div class="detail-columns">
        <section class="detail-section"><header class="detail-section-header"><div><p class="eyebrow">启动前检查</p><h2>运行环境</h2></div><CheckCircle2 v-if="precheck?.canLaunch" class="detail-ok" :size="20" /><AlertTriangle v-else class="detail-warning" :size="20" /></header><div class="check-list"><div class="check-row"><span><Folder :size="16" />游戏本体目录</span><strong :class="{ good: game.managedPath && precheck?.canLaunch }">{{ game.managedPath ? "已找到" : "缺失" }}</strong></div><div class="check-row"><span><Play :size="16" />启动程序</span><strong :class="{ good: precheck?.executableExists }">{{ precheck?.executableExists ? "已找到" : "缺失" }}</strong></div><div class="check-row"><span><ShieldCheck :size="16" />存档保护</span><strong :class="{ good: precheck?.saveProfileReady && (precheck?.validScopeCount || 0) > 0 }">{{ precheck?.saveProfileReady ? `${precheck?.validScopeCount} 个范围` : "未设置" }}</strong></div></div><div v-if="precheck?.issues.length" class="issue-list"><p v-for="issue in precheck.issues" :key="issue">{{ issue }}</p></div></section>

        <section class="detail-section">
          <header class="detail-section-header"><div><p class="eyebrow">游戏本体</p><h2>受管目录</h2></div><HardDrive :size="20" class="detail-muted" /></header>
          <p class="managed-path">{{ game.managedPath }}</p>
          <dl class="detail-facts"><div><dt>启动文件</dt><dd>{{ game.launch.executableRelativePath }}</dd></div><div><dt>保存版本</dt><dd>{{ versions.length }} 个</dd></div><div><dt>旧本体版本</dt><dd>{{ bodyVersions.length }} 个</dd></div></dl>
          <div class="body-action-row"><button class="secondary-button" type="button" :disabled="busy || !!runtime" title="选择新版游戏文件夹并更新" @click="updateBody"><LoaderCircle v-if="busy && (message.includes('更新') || message.includes('新版'))" :size="16" class="spin" /><FolderOpen v-else :size="16" />更新游戏本体</button><button class="secondary-button" type="button" :disabled="busy || !!runtime" title="创建本体 ZIP 缓存" @click="packageBody"><LoaderCircle v-if="busy && message.includes('本体包')" :size="16" class="spin" /><Archive v-else :size="16" />创建本体包</button><button class="secondary-button danger-outline-button" type="button" :disabled="busy || !!runtime" title="删除 GameSaver 管理的本地游戏本体，保留配置和云端版本" @click="uninstallBody"><Trash2 :size="16" />卸载本体</button></div>
          <div class="cloud-summary"><span class="status-dot" :class="{ active: baiduReady() }"></span><strong>百度网盘</strong><span>{{ baiduLabel() }}</span><span v-if="baiduReady()">· 云端版本请在游戏商店管理</span><button v-if="!baiduReady()" class="secondary-button compact-button" type="button" title="配置或授权百度网盘" @click="emit('settings')">{{ baiduActionLabel() }}</button></div>
          <div v-if="bodyVersions.length" class="body-version-list">
            <p class="timeline-caption">本体版本与本地包</p>
            <article v-for="version in bodyVersions" :key="version.versionId" class="body-version-row">
              <div class="body-version-info">
                <div class="body-version-title"><strong>{{ version.packagePath ? "ZIP 本体包" : "旧本体目录" }}</strong><span>{{ formatDate(version.createdAt) }}</span></div>
                <small v-if="version.packagePath">本地缓存 · {{ version.excludedItems.length }} 项排除 · {{ bodyUploadLabel(version) }}</small>
              </div>
              <div class="body-version-size"><strong>{{ version.fileCount }} 个文件</strong><span>原始 {{ formatBytes(version.totalBytes) }}<template v-if="version.packageSize"> · ZIP {{ formatBytes(version.packageSize) }}</template></span></div>
              <div v-if="version.packagePath" class="version-actions body-version-actions">
                <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime || !baiduReady()" title="上传本体包到百度网盘" @click="uploadBody(version)"><Upload :size="15" />上传</button>
                <button class="icon-button danger-button" type="button" :disabled="busy || !!runtime" title="删除本地本体包" aria-label="删除本地本体包" @click="deleteBodyPackage(version)"><Trash2 :size="15" /></button>
              </div>
            </article>
          </div>
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

  <Teleport to="body">
    <div v-if="coverEditorOpen && coverImage" class="cover-editor-overlay" @click.self="closeCoverEditor">
      <section class="cover-editor-dialog" role="dialog" aria-modal="true" aria-label="调整游戏封面">
        <header class="cover-editor-header">
          <div><p class="eyebrow">游戏封面</p><h2>调整封面显示</h2><p>拖动图片调整焦点，封面会固定显示为 16:9。</p></div>
          <button class="icon-button" type="button" title="关闭封面编辑" aria-label="关闭封面编辑" :disabled="coverSaving" @click="closeCoverEditor"><X :size="18" /></button>
        </header>
        <div class="cover-editor-content">
          <div class="cover-editor-stage" :class="{ dragging: coverDragging }" @pointerdown="beginCoverDrag" @pointermove="moveCoverDrag" @pointerup="endCoverDrag" @pointercancel="endCoverDrag" @pointerleave="endCoverDrag">
            <img :src="coverSourceUrl" alt="封面裁剪预览" :style="{ width: `${coverGeometry().width}px`, height: `${coverGeometry().height}px`, transform: `translate(${coverOffsetX}px, ${coverOffsetY}px)` }" />
            <span class="cover-editor-ratio">16:9</span>
          </div>
          <div class="cover-editor-controls">
            <label><span>缩放</span><strong>{{ Math.round(coverZoom * 100) }}%</strong><input v-model.number="coverZoom" type="range" min="1" max="3" step="0.01" aria-label="封面缩放" /></label>
            <p>建议将游戏标题或主要角色放在裁剪框中央。保存后会同时保留原始图片。</p>
            <button class="secondary-button compact-button" type="button" :disabled="coverSaving" @click="resetCoverPosition">重置位置</button>
          </div>
          <p v-if="coverError" class="error-message" role="alert">{{ coverError }}</p>
        </div>
        <footer class="cover-editor-footer">
          <button class="secondary-button" type="button" :disabled="coverSaving" @click="closeCoverEditor">取消</button>
          <button class="primary-button" type="button" :disabled="coverSaving" @click="saveCover"><LoaderCircle v-if="coverSaving" :size="16" class="spin" /><Upload v-else :size="16" />{{ coverSaving ? "正在保存" : "保存封面" }}</button>
        </footer>
      </section>
    </div>
  </Teleport>
</template>
