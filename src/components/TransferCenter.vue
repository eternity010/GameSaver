<script setup lang="ts">
import { computed, onUnmounted, ref, type Component } from "vue";
import { CheckCircle2, CloudDownload, CloudUpload, History, LoaderCircle, Play, RefreshCw, Trash2, XCircle } from "@lucide/vue";
import { cancelTask, deleteRemoteBodyPackage, deleteTasks, downloadGameBodyPackage, installCloudGame, repairCloudBodyManifest, startRestoreCloudSaveTask, startUploadSaveVersionTask, type AppTask, type CloudGameSummary, type TaskCategory, uploadGameBodyPackage } from "../api";
import type { Game } from "../domain/game";
import { taskCancelHint, taskCategoryOf, taskPolicyOf, useTaskFeed } from "../taskFeed";

const props = defineProps<{ games: Game[]; cloudGames: CloudGameSummary[] }>();

// 任务数据来自全应用共享的任务流：后端推送 `task-changed`，轮询只作兜底。此前
// 这里自建 700ms 定时器，与 App.vue 的根轮询各拉一遍同一份 `listTasks`。
const taskFeed = useTaskFeed();
const tasks = taskFeed.tasks;
const loading = taskFeed.loading;
const refresh = taskFeed.refresh;
// 这里只放操作（取消/重试/删除）产生的错误；读取失败由任务流自己持有，两者合并展示。
const actionError = ref("");
const error = computed(() => actionError.value || taskFeed.error.value);
const cancelling = ref("");
const retrying = ref("");
const deleting = ref("");
const sortMode = ref<"newest" | "oldest" | "status" | "game">("newest");
const statusFilter = ref<"all" | "active" | "success" | "failed" | "cancelled" | "interrupted">("all");

// 是否进入列表由后端的分类策略决定（`taskFeed.ts`），不再枚举任务类型。
const allTransferTasks = computed(() => tasks.value.filter((task) => taskPolicyOf(task).visible));

const transferTasks = computed(() => {
  const filtered = allTransferTasks.value.filter((task) => {
    if (statusFilter.value === "all") return true;
    if (statusFilter.value === "active") return isActive(task);
    return task.status === statusFilter.value;
  });
  return [...filtered].sort((left, right) => {
    if (sortMode.value === "status") {
      return statusRank(left) - statusRank(right) || compareCreatedAt(right, left);
    }
    if (sortMode.value === "game") {
      return gameName(left.gameUid).localeCompare(gameName(right.gameUid), "zh-CN") || compareCreatedAt(right, left);
    }
    return (isActive(left) ? 0 : 1) - (isActive(right) ? 0 : 1)
      || (sortMode.value === "oldest" ? compareCreatedAt(left, right) : compareCreatedAt(right, left));
  });
});

const activeCount = computed(() => allTransferTasks.value.filter(isActive).length);
const failedCount = computed(() => allTransferTasks.value.filter((task) => task.status === "failed" || task.status === "interrupted").length);
const finishedTransferTasks = computed(() => allTransferTasks.value.filter((task) => !isActive(task)));

async function cancel(task: AppTask) {
  // 不可取消的任务不提供入口（见 `taskPolicyOf`），这里再兜一次，避免误触。
  if (!isActive(task) || !taskPolicyOf(task).cancellable || cancelling.value) return;
  if (!window.confirm(taskCancelHint(task))) return;
  cancelling.value = task.taskId;
  try {
    await cancelTask(task.taskId);
    await refresh();
  } catch (reason) {
    actionError.value = String(reason);
  } finally {
    cancelling.value = "";
  }
}

async function retry(task: AppTask) {
  const retryInfo = task.retry;
  if (!retryInfo || retrying.value) return;
  retrying.value = task.taskId;
  actionError.value = "";
  try {
    let newTaskId = "";
    if (retryInfo.operation === "upload_game_body_package" && retryInfo.versionId) {
      newTaskId = await uploadGameBodyPackage(retryInfo.gameUid, retryInfo.versionId);
    } else if (retryInfo.operation === "install_cloud_game" && retryInfo.remotePath) {
      newTaskId = await installCloudGame(retryInfo.gameUid, retryInfo.gameKey, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "download_game_body_package" && retryInfo.remotePath) {
      newTaskId = await downloadGameBodyPackage(retryInfo.gameUid, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "delete_remote_body_package" && retryInfo.remotePath) {
      if (!retryInfo.gameKey) throw new Error("该删除任务缺少云端游戏标识，无法重试");
      newTaskId = await deleteRemoteBodyPackage(retryInfo.gameUid, retryInfo.gameKey, retryInfo.remotePath, retryInfo.remoteFsId);
    } else if (retryInfo.operation === "repair_cloud_body_manifest") {
      newTaskId = await repairCloudBodyManifest(retryInfo.gameUid);
    } else if (retryInfo.operation === "sync_cloud_save" && retryInfo.versionId) {
      newTaskId = await startUploadSaveVersionTask(retryInfo.gameUid, retryInfo.versionId);
    } else if (retryInfo.operation === "restore_cloud_save" && retryInfo.versionId) {
      newTaskId = await startRestoreCloudSaveTask(retryInfo.gameUid, retryInfo.versionId);
    } else {
      throw new Error("该任务缺少可重试参数");
    }
    if (newTaskId && newTaskId !== task.taskId) {
      await deleteTasks([task.taskId]);
    }
    await refresh();
  } catch (reason) {
    actionError.value = String(reason);
  } finally {
    retrying.value = "";
  }
}

async function removeTask(task: AppTask) {
  if (isActive(task) || deleting.value) return;
  if (!window.confirm(`只删除“${taskTitle(task)}”的任务记录，不会删除游戏本体或云端文件。确定删除吗？`)) return;
  deleting.value = task.taskId;
  actionError.value = "";
  try {
    await deleteTasks([task.taskId]);
    await refresh();
  } catch (reason) {
    actionError.value = String(reason);
  } finally {
    deleting.value = "";
  }
}

async function clearFinished() {
  const ids = finishedTransferTasks.value.map((task) => task.taskId);
  if (!ids.length || deleting.value) return;
  if (!window.confirm(`将删除 ${ids.length} 条已结束的传输记录，不会删除游戏本体或云端文件。确定继续吗？`)) return;
  deleting.value = "all";
  actionError.value = "";
  try {
    await deleteTasks(ids);
    await refresh();
  } catch (reason) {
    actionError.value = String(reason);
  } finally {
    deleting.value = "";
  }
}

function isActive(task: AppTask): boolean {
  return task.status === "pending" || task.status === "running";
}

/**
 * 游戏会话没有「百分比」可言——整场会话期间它一直停在 10%。挂一条几小时不动的
 * 进度条看起来像卡死，所以这一类改用一个静止的状态词。
 */
function isSessionTask(task: AppTask): boolean {
  return taskCategoryOf(task) === "session";
}

/**
 * 类型 → 标题。缺失时回退到分类级标题，不会再像以前那样把没列出的类型一律
 * 显示成「修复云端清单」。
 */
const TASK_TITLES: Record<string, string> = {
  upload_game_body_package: "上传游戏本体",
  download_game_body_package: "下载游戏本体",
  install_cloud_game: "安装云端游戏",
  delete_remote_body_package: "删除云端本体",
  repair_cloud_body_manifest: "修复云端清单",
  update_game_body: "更新游戏本体",
  package_game_body: "打包游戏本体",
  uninstall_game_body: "卸载游戏本体",
  delete_game_body_package: "删除本地本体包",
  sync_cloud_save: "云存档同步",
  restore_save_version: "恢复存档版本",
  launch_game: "启动游戏",
};

/** 类型 → 图标；同样以分类兜底，新增类型只会退化成同类图标，不会变成空白。 */
const TASK_ICONS: Record<string, Component> = {
  upload_game_body_package: CloudUpload,
  download_game_body_package: CloudDownload,
  install_cloud_game: CloudDownload,
  delete_remote_body_package: Trash2,
  delete_game_body_package: Trash2,
  uninstall_game_body: Trash2,
  repair_cloud_body_manifest: RefreshCw,
  update_game_body: RefreshCw,
  package_game_body: CloudUpload,
};

const CATEGORY_ICONS: Record<TaskCategory, Component> = {
  body_transfer: CloudUpload,
  cloud_save_sync: RefreshCw,
  save_restore: History,
  session: Play,
  maintenance: RefreshCw,
};

function taskTitle(task: AppTask): string {
  return TASK_TITLES[task.taskType] ?? taskPolicyOf(task).label;
}

function taskIcon(task: AppTask): Component {
  return TASK_ICONS[task.taskType] ?? CATEGORY_ICONS[taskCategoryOf(task)];
}

function gameName(gameUid?: string): string {
  return props.games.find((game) => game.gameUid === gameUid)?.displayName
    || props.cloudGames.find((game) => game.gameUid === gameUid)?.displayName
    || "未知游戏";
}

function retryIcon(task: AppTask) {
  const operation = task.retry?.operation;
  return operation === "upload_game_body_package" || operation === "sync_cloud_save" ? CloudUpload : CloudDownload;
}

function statusLabel(status: AppTask["status"]): string {
  return { pending: "等待中", running: "进行中", success: "已完成", failed: "失败", cancelled: "已取消", interrupted: "异常中断" }[status];
}

function formatError(task: AppTask): string {
  return task.error || task.message;
}

function createdTimestamp(task: AppTask): number {
  const timestamp = Number(task.createdAt);
  if (Number.isFinite(timestamp)) return timestamp;
  const parsed = Date.parse(task.createdAt || "");
  return Number.isFinite(parsed) ? parsed : 0;
}

function compareCreatedAt(left: AppTask, right: AppTask): number {
  return createdTimestamp(left) - createdTimestamp(right) || left.taskId.localeCompare(right.taskId);
}

function statusRank(task: AppTask): number {
  return { running: 0, pending: 1, failed: 2, interrupted: 3, cancelled: 4, success: 5 }[task.status];
}

function formatTaskTime(value?: string): string {
  if (!value) return "";
  const timestamp = Number(value);
  if (!Number.isFinite(timestamp)) return value;
  return new Date(timestamp).toLocaleString("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
}

// 无需在挂载时主动刷新：任务流在首个订阅者（App.vue）出现时已完成首次拉取，
// 之后的每次变化都由后端推送驱动。
onUnmounted(() => {
  taskFeed.release();
});
</script>

<template>
  <section class="transfer-page page-enter">
    <header class="transfer-header">
      <div>
        <p class="eyebrow">后台任务</p>
        <h1>传输中心</h1>
        <p>管理游戏本体传输、云存档同步与游戏会话，离开游戏详情页后任务也会继续显示。</p>
      </div>
      <div class="transfer-header-side"><div class="transfer-count"><strong>{{ activeCount }}</strong><span>进行中</span></div><button class="icon-button" type="button" title="刷新任务列表" aria-label="刷新任务列表" :disabled="loading || !!deleting" @click="refresh"><RefreshCw :size="17" /></button></div>
    </header>

    <div v-if="loading" class="state-panel transfer-state"><span class="loader"></span><strong>正在读取传输任务</strong></div>
    <div v-else-if="error && !transferTasks.length" class="state-panel error-state"><XCircle :size="25" /><strong>传输任务读取失败</strong><p>{{ error }}</p><button type="button" @click="refresh">重试</button></div>
    <template v-else>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <div v-if="allTransferTasks.length" class="transfer-toolbar">
        <div class="transfer-filters">
          <label>状态<select v-model="statusFilter"><option value="all">全部任务</option><option value="active">进行中</option><option value="success">已完成</option><option value="failed">失败</option><option value="interrupted">异常中断</option><option value="cancelled">已取消</option></select></label>
          <label>排序<select v-model="sortMode"><option value="newest">最新创建</option><option value="oldest">最早创建</option><option value="status">按状态</option><option value="game">按游戏</option></select></label>
        </div>
        <div class="transfer-toolbar-actions"><span v-if="failedCount" class="transfer-alert-count">{{ failedCount }} 条需关注</span><button class="secondary-button compact-button" type="button" :disabled="!finishedTransferTasks.length || deleting === 'all'" title="删除全部已结束的传输记录" @click="clearFinished"><LoaderCircle v-if="deleting === 'all'" :size="15" class="spin" /><Trash2 v-else :size="15" />清理已结束</button></div>
      </div>
      <div v-if="!transferTasks.length" class="state-panel empty-state transfer-state"><div class="empty-icon"><CloudUpload :size="28" /></div><strong>{{ allTransferTasks.length ? "没有符合条件的任务" : "还没有传输任务" }}</strong><p>{{ allTransferTasks.length ? "可以切换状态筛选或清除筛选条件。" : "上传/下载游戏本体包、游戏退出后自动同步存档，或启动游戏时，任务会显示在这里。" }}</p></div>
      <div v-else class="transfer-list">
        <article v-for="task in transferTasks" :key="task.taskId" class="transfer-card" :class="`transfer-${task.status}`">
          <div class="transfer-icon"><component :is="taskIcon(task)" :size="20" /></div>
          <div class="transfer-copy"><div class="transfer-title"><strong>{{ taskTitle(task) }}</strong><span>{{ statusLabel(task.status) }}</span></div><p>{{ gameName(task.gameUid) }}</p><small>{{ task.message }} · {{ formatTaskTime(task.createdAt) }}</small><p v-if="task.status === 'failed' || task.status === 'interrupted'" class="transfer-error">{{ formatError(task) }}</p></div>
          <div class="transfer-progress"><strong>{{ isSessionTask(task) ? "运行中" : `${task.progress}%` }}</strong><div v-if="!isSessionTask(task)" class="progress-track"><span :style="{ width: `${task.progress}%` }"></span></div></div>
          <div class="transfer-card-actions"><button v-if="isActive(task) && taskPolicyOf(task).cancellable" class="secondary-button compact-button" type="button" :disabled="cancelling === task.taskId" @click="cancel(task)"><LoaderCircle v-if="cancelling === task.taskId" :size="15" class="spin" /><XCircle v-else :size="15" />取消</button><span v-else-if="isActive(task)" class="transfer-locked" title="该任务在完成前无法安全取消：中途放弃会留下不一致的状态">不可取消</span><button v-else-if="(task.status === 'failed' || task.status === 'cancelled' || task.status === 'interrupted') && task.retry" class="secondary-button compact-button" type="button" :disabled="retrying === task.taskId" @click="retry(task)"><LoaderCircle v-if="retrying === task.taskId" :size="15" class="spin" /><component :is="retryIcon(task)" v-else :size="15" />重试</button><CheckCircle2 v-if="task.status === 'success'" class="transfer-success-icon" :size="20" /><button v-if="!isActive(task)" class="icon-button danger-button" type="button" :disabled="deleting === task.taskId || deleting === 'all'" title="删除任务记录" aria-label="删除任务记录" @click="removeTask(task)"><LoaderCircle v-if="deleting === task.taskId" :size="15" class="spin" /><Trash2 v-else :size="15" /></button></div>
        </article>
      </div>
    </template>
  </section>
</template>
