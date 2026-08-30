<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { CheckCircle2, CloudDownload, CloudUpload, LoaderCircle, RefreshCw, Trash2, XCircle } from "@lucide/vue";
import { cancelTask, deleteRemoteBodyPackage, downloadGameBodyPackage, installCloudGame, listTasks, repairCloudBodyManifest, type AppTask, type CloudGameSummary, uploadGameBodyPackage } from "../api";
import type { Game } from "../domain/game";

const props = defineProps<{ games: Game[]; cloudGames: CloudGameSummary[] }>();

const tasks = ref<AppTask[]>([]);
const loading = ref(true);
const error = ref("");
const cancelling = ref("");
const retrying = ref("");
let pollTimer: ReturnType<typeof setTimeout> | undefined;

const transferTasks = computed(() => {
  const active = tasks.value.filter((task) => isTransferTask(task));
  return [...active].sort((left, right) => {
    const leftActive = isActive(left) ? 0 : 1;
    const rightActive = isActive(right) ? 0 : 1;
    return leftActive - rightActive || right.taskId.localeCompare(left.taskId);
  });
});

const activeCount = computed(() => transferTasks.value.filter(isActive).length);

async function refresh() {
  try {
    tasks.value = await listTasks();
    error.value = "";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
  scheduleRefresh();
}

function scheduleRefresh() {
  if (pollTimer) clearTimeout(pollTimer);
  if (activeCount.value > 0) {
    pollTimer = setTimeout(() => void refresh(), 700);
  }
}

async function cancel(task: AppTask) {
  if (!isActive(task) || cancelling.value) return;
  if (!window.confirm("取消后会停止后续分片处理，已上传的临时分片可能由百度网盘自动清理。确定取消吗？")) return;
  cancelling.value = task.taskId;
  try {
    await cancelTask(task.taskId);
    await refresh();
  } catch (reason) {
    error.value = String(reason);
  } finally {
    cancelling.value = "";
  }
}

async function retry(task: AppTask) {
  const retryInfo = task.retry;
  if (!retryInfo || retrying.value) return;
  retrying.value = task.taskId;
  error.value = "";
  try {
    if (retryInfo.operation === "upload_game_body_package" && retryInfo.versionId) {
      await uploadGameBodyPackage(retryInfo.gameUid, retryInfo.versionId);
    } else if (retryInfo.operation === "install_cloud_game" && retryInfo.remotePath) {
      await installCloudGame(retryInfo.gameUid, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "download_game_body_package" && retryInfo.remotePath) {
      await downloadGameBodyPackage(retryInfo.gameUid, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "delete_remote_body_package" && retryInfo.remotePath) {
      await deleteRemoteBodyPackage(retryInfo.gameUid, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "repair_cloud_body_manifest") {
      await repairCloudBodyManifest(retryInfo.gameUid);
    } else {
      throw new Error("该任务缺少可重试参数");
    }
    await refresh();
  } catch (reason) {
    error.value = String(reason);
  } finally {
    retrying.value = "";
  }
}

function isTransferTask(task: AppTask): boolean {
  return task.taskType === "upload_game_body_package" || task.taskType === "download_game_body_package" || task.taskType === "install_cloud_game" || task.taskType === "delete_remote_body_package" || task.taskType === "repair_cloud_body_manifest";
}

function isActive(task: AppTask): boolean {
  return task.status === "pending" || task.status === "running";
}

function taskTitle(task: AppTask): string {
  if (task.taskType === "upload_game_body_package") return "上传游戏本体";
  if (task.taskType === "download_game_body_package") return "下载游戏本体";
  if (task.taskType === "install_cloud_game") return "安装云端游戏";
  if (task.taskType === "delete_remote_body_package") return "删除云端本体";
  return "修复云端清单";
}

function gameName(gameUid?: string): string {
  return props.games.find((game) => game.gameUid === gameUid)?.displayName
    || props.cloudGames.find((game) => game.gameUid === gameUid)?.displayName
    || "未知游戏";
}

function retryIcon(task: AppTask) {
  return task.taskType === "upload_game_body_package" ? CloudUpload : CloudDownload;
}

function statusLabel(status: AppTask["status"]): string {
  return { pending: "等待中", running: "进行中", success: "已完成", failed: "失败", cancelled: "已取消", interrupted: "异常中断" }[status];
}

function formatError(task: AppTask): string {
  return task.error || task.message;
}

onMounted(() => void refresh());
onUnmounted(() => {
  if (pollTimer) clearTimeout(pollTimer);
});
</script>

<template>
  <section class="transfer-page page-enter">
    <header class="transfer-header">
      <div>
        <p class="eyebrow">后台任务</p>
        <h1>传输中心</h1>
        <p>管理游戏本体的上传和下载，离开游戏详情页后任务也会继续显示。</p>
      </div>
      <div class="transfer-count"><strong>{{ activeCount }}</strong><span>进行中</span></div>
    </header>

    <div v-if="loading" class="state-panel transfer-state"><span class="loader"></span><strong>正在读取传输任务</strong></div>
    <div v-else-if="error && !transferTasks.length" class="state-panel error-state"><XCircle :size="25" /><strong>传输任务读取失败</strong><p>{{ error }}</p><button type="button" @click="refresh">重试</button></div>
    <template v-else>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <div v-if="!transferTasks.length" class="state-panel empty-state transfer-state"><div class="empty-icon"><CloudUpload :size="28" /></div><strong>还没有传输任务</strong><p>从游戏详情页上传或下载游戏本体包后，任务会显示在这里。</p></div>
      <div v-else class="transfer-list">
        <article v-for="task in transferTasks" :key="task.taskId" class="transfer-card" :class="`transfer-${task.status}`">
          <div class="transfer-icon"><CloudUpload v-if="task.taskType === 'upload_game_body_package'" :size="20" /><CloudDownload v-else-if="task.taskType === 'download_game_body_package' || task.taskType === 'install_cloud_game'" :size="20" /><Trash2 v-else-if="task.taskType === 'delete_remote_body_package'" :size="20" /><RefreshCw v-else :size="20" /></div>
          <div class="transfer-copy"><div class="transfer-title"><strong>{{ taskTitle(task) }}</strong><span>{{ statusLabel(task.status) }}</span></div><p>{{ gameName(task.gameUid) }}</p><small>{{ task.message }}</small><p v-if="task.status === 'failed' || task.status === 'interrupted'" class="transfer-error">{{ formatError(task) }}</p></div>
          <div class="transfer-progress"><strong>{{ task.progress }}%</strong><div class="progress-track"><span :style="{ width: `${task.progress}%` }"></span></div></div>
          <button v-if="isActive(task)" class="secondary-button compact-button" type="button" :disabled="cancelling === task.taskId" @click="cancel(task)"><LoaderCircle v-if="cancelling === task.taskId" :size="15" class="spin" /><XCircle v-else :size="15" />取消</button>
          <button v-else-if="(task.status === 'failed' || task.status === 'cancelled' || task.status === 'interrupted') && task.retry" class="secondary-button compact-button" type="button" :disabled="retrying === task.taskId" @click="retry(task)"><LoaderCircle v-if="retrying === task.taskId" :size="15" class="spin" /><component :is="retryIcon(task)" v-else :size="15" />重试</button>
          <CheckCircle2 v-else-if="task.status === 'success'" class="transfer-success-icon" :size="20" />
        </article>
      </div>
    </template>
  </section>
</template>
