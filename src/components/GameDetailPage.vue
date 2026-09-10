<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, Archive, ArrowLeft, Camera, Check, CheckCircle2, Clock3, Cloud, CloudDownload, CloudUpload, Folder, FolderOpen, Gamepad2, HardDrive, ImagePlus, LoaderCircle, Pencil, Play, RefreshCw, RotateCcw, ShieldCheck, Trash2, Upload, X } from "@lucide/vue";
import { armGameCoverCapture, deleteCloudSaveVersion, deleteGameBodyPackage, deleteSaveVersion, discardGameCoverCapture, getBaiduConfig, getBaiduStatus, getCloudSaveOverview, getGameCover, getGameCoverCaptureUrl, getGameCoverUrl, getGameDetailView, getGameRuntime, getSaveProfile, getTask, launchGame, listGameBodyVersions, listSaveVersions, openPathInExplorer, packageGameBody, precheckGameLaunch, pruneSaveVersions, removeGameFromLibrary, renameGame, restoreSaveVersion, saveGameCover, startRestoreCloudSaveTask, startUploadSaveVersionTask, uninstallGameBody, updateGameBody, uploadGameBodyPackage, updateSaveProfileKeepVersions, updateSaveProfileScopes } from "../api";
import type { BaiduConfigView, BaiduStatus } from "../api";
import { createDefaultSaveScope, gameStatusLabel } from "../domain/game";
import type { CloudSaveManifestVersion, CloudSaveSyncStatusView, CoverCrop, CoverPosition, Game, GameBodyVersion, GameRuntime, LaunchPrecheck, SaveProfile, SaveRootType, SaveScope, SaveVersion } from "../domain/game";

const props = defineProps<{
  game: Game;
  initialError?: string;
  coverUrl?: string;
  pendingCoverCapture?: { captureId: string; gameUid: string } | null;
}>();
const emit = defineEmits<{
  back: [];
  refresh: [];
  settings: [];
  captureHandled: [captureId: string];
}>();

const precheck = ref<LaunchPrecheck | null>(null);
const runtime = ref<GameRuntime | null>(null);
const versions = ref<SaveVersion[]>([]);
const bodyVersions = ref<GameBodyVersion[]>([]);
const cloudSaveStatus = ref<CloudSaveSyncStatusView | null>(null);
const cloudSaveVersions = ref<CloudSaveManifestVersion[]>([]);
const cloudSaveDrawerOpen = ref(false);
const cloudSaveLoading = ref(false);
const cloudSaveLoaded = ref(false);
const cloudSaveError = ref("");
const baiduStatus = ref<BaiduStatus | null>(null);
const baiduConfig = ref<BaiduConfigView | null>(null);
const cloudConnectionLoading = ref(true);
const loading = ref(true);
const busy = ref(false);
const launchPending = ref(false);
const error = ref(props.initialError || "");
const message = ref("");
const taskProgress = ref(0);
const keepVersions = ref(5);
const saveProfile = ref<SaveProfile | null>(null);

const rootTypeLabel: Record<SaveRootType, string> = {
  managed_game: "游戏目录",
  app_data: "AppData",
  local_app_data: "LocalAppData",
  local_low: "LocalLow",
  documents: "文档",
  saved_games: "Saved Games",
  user_profile: "用户目录",
  custom: "自定义目录",
};

function cleanDisplayPath(rawPath: string): string {
  return rawPath.replace(/^\\\\\?\\UNC\\/i, "\\\\").replace(/^\\\\\?\\/i, "");
}

function formatScopeDisplay(scope: SaveScope): string {
  const clean = cleanDisplayPath(scope.rootPath);
  if (scope.rootType === "managed_game" && props.game?.managedPath) {
    const normManaged = cleanDisplayPath(props.game.managedPath)
      .replace(/[/\\]+/g, "\\")
      .replace(/\\+$/, "")
      .toLowerCase();
    const normClean = clean.replace(/[/\\]+/g, "\\").replace(/\\+$/, "").toLowerCase();
    if (normClean === normManaged) {
      return "<游戏根目录>";
    }
    if (normClean.startsWith(normManaged + "\\")) {
      const sub = clean.slice(cleanDisplayPath(props.game.managedPath).length).replace(/^[/\\]+/, "");
      return `<游戏根目录>\\${sub}`;
    }
  }
  return clean;
}

async function openFolder(path: string) {
  try {
    await openPathInExplorer(cleanDisplayPath(path));
  } catch (reason) {
    error.value = `打开目录失败：${String(reason)}`;
  }
}

async function changeSaveDirectory() {
  if (busy.value || runtime.value) return;
  try {
    const currentPath = saveProfile.value?.scopes?.[0]?.rootPath;
    const selected = await open({
      title: "选择游戏真实的存档所在目录",
      directory: true,
      multiple: false,
      defaultPath: currentPath,
    });
    if (!selected || typeof selected !== "string") {
      return;
    }
    busy.value = true;
    error.value = "";
    message.value = "正在更新存档保护目录...";
    const newScope = createDefaultSaveScope(selected, "custom");
    const updated = await updateSaveProfileScopes(props.game.gameUid, [newScope]);
    saveProfile.value = updated;
    message.value = `存档保护目录已更新为：${selected}`;
    emit("refresh");
  } catch (reason) {
    error.value = `更新存档目录失败：${String(reason)}`;
  } finally {
    busy.value = false;
  }
}

const coverInput = ref<HTMLInputElement | null>(null);
const coverDisplayUrl = ref(props.coverUrl || "");
const coverSourceUrl = ref("");
const coverImage = ref<HTMLImageElement | null>(null);
const coverOriginalBytes = ref<number[]>([]);
const coverOriginalExtension = ref("jpg");
const coverEditorOpen = ref(false);
const coverSaving = ref(false);
const coverError = ref("");
const coverCapturePending = ref(false);
const coverCaptureId = ref("");
const coverCaptureShortcut = ref("");
const coverZoom = ref(1);
const coverOffsetX = ref(0);
const coverOffsetY = ref(0);
const coverDragging = ref(false);
let coverPointerX = 0;
let coverPointerY = 0;
let stopCoverCaptureReady: UnlistenFn | undefined;
let stopCoverCaptureFailed: UnlistenFn | undefined;
let stopRuntimeChanged: UnlistenFn | undefined;
let stopTaskChanged: UnlistenFn | undefined;
let detailListenersDisposed = false;
const COVER_STAGE_WIDTH = 640;
const COVER_STAGE_HEIGHT = 360;
/**
 * 会话跟踪的兜底周期。
 *
 * 正常情况下会话状态由后端推送（`runtime-changed` / `task-changed`）驱动，这里
 * 只在事件丢失时才生效。此前是整场游戏会话 700ms 不息轮询，而一场会话里真正
 * 有意义的变化只有三次（开始运行 / 游戏退出 / 存档提交完成）。
 */
const TASK_POLL_FALLBACK_MS = 5000;
/** 连续到达的任务变化事件合并成一个复核窗口。 */
const TASK_EVENT_DEBOUNCE_MS = 120;
let pollTimer: ReturnType<typeof setTimeout> | undefined;
let taskEventTimer: ReturnType<typeof setTimeout> | undefined;
// 当前正在轮询的任务 id。用于「同一场会话不重复接回」，也让 reattach 不会死循环。
let watchedTaskId: string | undefined;
let refreshGeneration = 0;
let cloudOverviewPromise: Promise<void> | null = null;
let cloudOverviewGameUid = "";

const renaming = ref(false);
const editingName = ref("");
const renameSaving = ref(false);
const renameError = ref("");
const nameInputRef = ref<HTMLInputElement | null>(null);

function startRename() {
  editingName.value = props.game.displayName;
  renameError.value = "";
  renaming.value = true;
  void nextTick(() => {
    nameInputRef.value?.focus();
    nameInputRef.value?.select();
  });
}

function cancelRename() {
  renaming.value = false;
  renameError.value = "";
}

async function submitRename() {
  const trimmed = editingName.value.trim();
  if (!trimmed) {
    renameError.value = "游戏名称不能为空";
    return;
  }
  if (trimmed === props.game.displayName) {
    renaming.value = false;
    return;
  }
  renameSaving.value = true;
  renameError.value = "";
  try {
    const updated = await renameGame(props.game.gameUid, trimmed);
    props.game.displayName = updated.displayName;
    renaming.value = false;
    emit("refresh");
  } catch (reason) {
    renameError.value = String(reason);
  } finally {
    renameSaving.value = false;
  }
}

async function refresh() {
  const gameUid = props.game.gameUid;
  const generation = ++refreshGeneration;
  loading.value = true;
  error.value = "";
  cloudSaveError.value = "";
  // 云端总览必须重新读取：它含一份「本地 ↔ 云端」的对比状态，而本页每一条会导致
  // refresh() 的路径都可能改动本地存档——连「改保留数」都会按新上限删掉多余版本
  // （见 update_save_profile_keep_versions），所以不能靠缓存跳过这次复查。
  //
  // 但「重新读取」不等于「先清空」：清空会让「同步最新」变灰、版本计数消失、状态
  // 徽标闪回「查询中」，而这期间用户看到的只是同一份数据的旧快照。已有本游戏摘要
  // 时保留它，把「不许基于过期 syncState 操作」交给按钮的 cloudSaveLoading 条件。
  if (!cloudSaveLoaded.value || cloudOverviewGameUid !== gameUid) {
    cloudSaveLoaded.value = false;
    cloudConnectionLoading.value = true;
  }
  // 同步置位而不是等 loadCloudSaveOverview：refreshCloudState 在它之前还要先读两次
  // 连接状态，这几毫秒里若不置位，「同步最新」会短暂可点并基于旧 syncState 下手。
  cloudSaveLoading.value = true;
  void refreshCloudState(gameUid, generation, true);
  try {
    const detail = await getGameDetailView(gameUid);
    if (generation !== refreshGeneration || gameUid !== props.game.gameUid) return;
    precheck.value = detail.precheck;
    versions.value = detail.versions;
    runtime.value = detail.runtime;
    // 会话可能从组件卸载期间延续下来：拿到 runtime 就立刻接回轮询。
    reattachRuntimeTask();
    bodyVersions.value = detail.bodyVersions;
    saveProfile.value = detail.saveProfile;
    if (detail.saveProfile?.keepVersions) {
      keepVersions.value = detail.saveProfile.keepVersions;
    }
  } catch (reason) {
    if (generation !== refreshGeneration || gameUid !== props.game.gameUid) return;
    error.value = String(reason);
  } finally {
    if (generation === refreshGeneration && gameUid === props.game.gameUid) {
      loading.value = false;
    }
  }
}

async function refreshCloudState(gameUid: string, generation: number, forceOverview = false) {
  try {
    const [nextBaiduStatus, nextBaiduConfig] = await Promise.all([
      getBaiduStatus(),
      getBaiduConfig(),
    ]);
    if (generation !== refreshGeneration || gameUid !== props.game.gameUid) return;
    baiduStatus.value = nextBaiduStatus;
    baiduConfig.value = nextBaiduConfig;
    cloudConnectionLoading.value = false;
    if (baiduReady()) {
      await loadCloudSaveOverview(forceOverview);
    } else {
      cloudSaveStatus.value = null;
      cloudSaveVersions.value = [];
      cloudSaveLoaded.value = true;
      // 没走 loadCloudSaveOverview，它不会替我们收尾（refresh() 已先置位）。
      cloudSaveLoading.value = false;
    }
  } catch (reason) {
    if (generation !== refreshGeneration || gameUid !== props.game.gameUid) return;
    cloudConnectionLoading.value = false;
    cloudSaveError.value = String(reason);
    cloudSaveLoaded.value = true;
    cloudSaveLoading.value = false;
  }
}

function loadCloudSaveOverview(force = false): Promise<void> {
  const gameUid = props.game.gameUid;
  if (!force && cloudSaveLoaded.value && cloudOverviewGameUid === gameUid) {
    return Promise.resolve();
  }
  if (cloudOverviewPromise && cloudOverviewGameUid === gameUid) {
    return cloudOverviewPromise;
  }

  cloudOverviewGameUid = gameUid;
  cloudSaveLoading.value = true;
  cloudSaveError.value = "";
  const request = getCloudSaveOverview(gameUid)
    .then((overview) => {
      if (gameUid !== props.game.gameUid) return;
      cloudSaveStatus.value = overview.status;
      cloudSaveVersions.value = overview.versions;
      cloudSaveLoaded.value = true;
    })
    .catch((reason) => {
      if (gameUid !== props.game.gameUid) return;
      cloudSaveStatus.value = null;
      cloudSaveVersions.value = [];
      cloudSaveError.value = String(reason);
      cloudSaveLoaded.value = true;
    })
    .finally(() => {
      if (cloudOverviewPromise === request) {
        cloudOverviewPromise = null;
        cloudSaveLoading.value = false;
      }
    });
  cloudOverviewPromise = request;
  return request;
}

async function uploadSave(version: SaveVersion) {
  if (busy.value || runtime.value || !baiduReady()) return;
  busy.value = true;
  error.value = "";
  message.value = "准备上传存档到百度网盘";
  try {
    await watchTask(await startUploadSaveVersionTask(props.game.gameUid, version.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function openCloudSaveDrawer() {
  cloudSaveDrawerOpen.value = true;
  if (!baiduReady()) return;
  await loadCloudSaveOverview();
}

async function restoreCloudSave(cloudVersion: CloudSaveManifestVersion) {
  if (busy.value || runtime.value) return;
  if (
    !window.confirm(
      `从百度网盘还原 ${formatDate(cloudVersion.createdAt)} 的存档。\n还原后存档目录会回到该云端版本的状态：当前存在、但该版本里没有的存档文件会被移除。\n还原前会先保护当前本地存档，确定继续吗？`
    )
  )
    return;
  busy.value = true;
  error.value = "";
  message.value = "正在从云端还原存档";
  try {
    await watchTask(await startRestoreCloudSaveTask(props.game.gameUid, cloudVersion.versionId));
  } catch (reason) {
    busy.value = false;
    error.value = String(reason);
  }
}

async function deleteCloudSave(cloudVersion: CloudSaveManifestVersion) {
  if (busy.value || runtime.value) return;
  if (!window.confirm(`确定从百度网盘中删除 ${formatDate(cloudVersion.createdAt)} 的云端存档吗？`)) return;
  busy.value = true;
  error.value = "";
  message.value = "正在删除云端存档";
  try {
    await deleteCloudSaveVersion(props.game.gameUid, cloudVersion.versionId);
    cloudSaveLoaded.value = false;
    await loadCloudSaveOverview(true);
    message.value = "云端存档已删除";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    busy.value = false;
  }
}

async function syncLatestSave() {
  if (busy.value || runtime.value || !baiduReady()) return;
  if (!cloudSaveStatus.value) return;
  if (cloudSaveStatus.value.syncState === "local_ahead" || cloudSaveStatus.value.syncState === "no_cloud_saves") {
    const latest = versions.value[0];
    if (latest) await uploadSave(latest);
  } else if (cloudSaveStatus.value.syncState === "cloud_ahead") {
    const latestCloud = cloudSaveVersions.value[0];
    if (latestCloud) await restoreCloudSave(latestCloud);
  } else {
    await refresh();
  }
}

function cloudSaveStatusText(): string {
  // 只有还没有任何摘要时才报「查询中」；已有摘要时不因一次后台复查把徽标刷掉。
  if (!cloudSaveStatus.value && (cloudConnectionLoading.value || cloudSaveLoading.value)) return "查询中...";
  if (!baiduReady()) return "网盘未连接";
  if (cloudSaveError.value) return "读取失败";
  if (!cloudSaveStatus.value) return "等待查询";
  switch (cloudSaveStatus.value.syncState) {
    case "synced":
      return "已与云端同步";
    case "local_ahead":
      return "本地有待同步新存档";
    case "cloud_ahead":
      return "云端有更新的存档进度";
    case "no_cloud_saves":
      return "云端暂无存档";
    case "offline":
      return "离线 / 未连接";
    default:
      return "状态正常";
  }
}

function cloudSaveStatusBadgeClass(): string {
  if (!baiduReady()) return "badge-offline";
  if (!cloudSaveStatus.value) return "";
  switch (cloudSaveStatus.value.syncState) {
    case "synced":
      return "badge-synced";
    case "local_ahead":
      return "badge-ahead";
    case "cloud_ahead":
      return "badge-cloud";
    case "no_cloud_saves":
      return "badge-empty";
    default:
      return "";
  }
}

/**
 * 启动按钮的文案与可用性由 `runtime` 决定 —— 它才是「游戏是否在运行」的唯一真相源。
 *
 * 此前只看 `busy`：一旦离开详情页再回来，`busy` 归 false 而游戏仍在运行，
 * 按钮就显示成「启动游戏」且可点，点下去才由后端报「游戏已经在运行」。
 * `launchPending` 只覆盖「命令已发出、后端 runtime 尚未反映」的短窗口，
 * 不用 `busy` 是为了避免把上传/还原等其它任务的 busy 误读成"正在启动"。
 */
const launchButton = computed<{ label: string; disabled: boolean; spinning: boolean }>(() => {
  const status = runtime.value?.status;
  const spinning =
    status === "launching" ||
    status === "saving" ||
    launchPending.value ||
    (loading.value && !precheck.value);
  let label = "启动游戏";
  if (status) label = runtimeLabel(status);
  else if (launchPending.value) label = "正在启动游戏";
  else if (loading.value && !precheck.value) label = "检测中...";
  return {
    label,
    disabled: !!runtime.value || busy.value || !precheck.value?.canLaunch,
    spinning,
  };
});

async function start() {
  // runtime 是唯一真相源：只要后端报告有 runtime，就绝不再拉起。
  if (busy.value || runtime.value || !precheck.value?.canLaunch) return;
  const gameUid = props.game.gameUid;
  busy.value = true;
  launchPending.value = true;
  error.value = "";
  message.value = "正在启动游戏";
  try {
    await watchTask(await launchGame(gameUid));
  } catch (reason) {
    if (gameUid === props.game.gameUid) error.value = String(reason);
  } finally {
    // 这场会话可能跨越「用户切到了别的游戏」：只有仍然是同一场时才回收这些 UI
    // 状态，否则会把新游戏的进度面板一起清掉。
    if (gameUid === props.game.gameUid) {
      launchPending.value = false;
      busy.value = false;
    }
  }
}

async function watchTask(taskId: string, gameUid: string = props.game.gameUid) {
  // 会话属于某个具体游戏：组件已经切到别的游戏时不再跟踪，否则上一场的
  // message / 进度条会被写进当前页面。回到该游戏时 refresh() 会重新接回。
  if (gameUid !== props.game.gameUid) {
    // 组件已切到别的游戏：静默结束本场轮询。切换时的 watcher 已经停过定时器，
    // 这里**不能**再 stopPolling —— 那会误杀新游戏刚建立的轮询。
    if (watchedTaskId === taskId) watchedTaskId = undefined;
    return;
  }
  stopPolling();
  watchedTaskId = taskId;
  busy.value = true;
  try {
    const task = await getTask(taskId);
    if (gameUid !== props.game.gameUid) return;
    runtime.value = await getGameRuntime(gameUid);
    if (gameUid !== props.game.gameUid) return;
    message.value = task.message;
    taskProgress.value = task.progress;
    if (task.status === "success") {
      busy.value = false;
      await refresh();
      emit("refresh");
      return;
    }
    if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
      busy.value = false;
      const failure = task.error || task.message || (task.status === "interrupted" ? "任务异常中断" : "操作失败");
      await refresh();
      error.value = failure;
      return;
    }
    pollTimer = setTimeout(() => void watchTask(taskId, gameUid), TASK_POLL_FALLBACK_MS);
  } catch (reason) {
    if (watchedTaskId === taskId) watchedTaskId = undefined;
    if (gameUid !== props.game.gameUid) return;
    busy.value = false;
    error.value = String(reason);
  }
}

/**
 * 会话可能在组件卸载期间仍在延续（用户离开详情页又回来）。刷新时若后端仍报告
 * runtime，就用它自带的 taskId 接回轮询，让「运行中」与退出后的提交进度重新可见。
 *
 * 已处理过的 taskId 记在 `watchedTaskId` 上不再重复接回，避免失败任务造成死循环。
 */
function reattachRuntimeTask() {
  const taskId = runtime.value?.taskId;
  if (!taskId || taskId === watchedTaskId) return;
  void watchTask(taskId, props.game.gameUid);
}

/**
 * 后端推送「运行时状态变化」时的即时复核。
 *
 * 已在跟踪本场会话时直接复核任务进度（`watchTask` 会顺带刷新 runtime）；否则
 * 重新读一次 runtime —— 可能是别处（存档分析等）挂上的会话，也可能是会话结束。
 */
async function syncRuntimeFromEvent(gameUid: string) {
  if (gameUid !== props.game.gameUid) return;
  if (watchedTaskId) {
    await watchTask(watchedTaskId, gameUid);
    return;
  }
  try {
    const next = await getGameRuntime(gameUid);
    if (gameUid !== props.game.gameUid) return;
    runtime.value = next;
    reattachRuntimeTask();
  } catch {
    // 事件只是提示：读不到就交给兜底轮询，不打断页面。
  }
}

function handleRuntimeChanged(payload: { gameUid?: string } | undefined) {
  if (!payload?.gameUid) return;
  void syncRuntimeFromEvent(payload.gameUid);
}

/**
 * 后端推送「任务变化」时的即时复核。
 *
 * 匹配规则刻意收紧：
 * - 正在跟踪某场会话时，**只认任务 ID 相符**的变化。光看 `gameUid` 会把「游戏退出
 *   后的自动云同步」也算进来——那是一条独立任务，让它触发本场会话的复核，会导致
 *   错误地重读一个早已结束的任务、并连带整页刷新。
 * - 尚未跟踪任务时，才退回按 `gameUid` 判断：可能是本游戏刚被（从库页启动后）
 *   挂上会话，需要复核一次 runtime 决定是否接回。
 *
 * 短窗口合并连续事件：存档提交阶段进度会连续推进，后端已按 2% 粒度节流，这里
 * 再合并一次，避免把节流后的多次变化放大成多次 IPC。
 */
function handleTaskChanged(payload: { taskId?: string; gameUid?: string } | undefined) {
  if (!payload) return;
  if (watchedTaskId) {
    if (payload.taskId !== watchedTaskId) return;
  } else if (payload.gameUid !== props.game.gameUid) {
    return;
  }
  if (taskEventTimer) clearTimeout(taskEventTimer);
  taskEventTimer = setTimeout(() => {
    taskEventTimer = undefined;
    if (watchedTaskId) void watchTask(watchedTaskId, props.game.gameUid);
    else void syncRuntimeFromEvent(props.game.gameUid);
  }, TASK_EVENT_DEBOUNCE_MS);
}

async function restoreVersion(version: SaveVersion) {
  if (busy.value || runtime.value) return;
  if (
    !window.confirm(
      "恢复后存档目录会回到该版本的状态：当前存在、但该版本里没有的存档文件会被移除。\n恢复前会先保护当前存档，确定恢复这个版本吗？"
    )
  )
    return;
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

async function changeKeepVersions(newLimit: number) {
  keepVersions.value = newLimit;
  try {
    await updateSaveProfileKeepVersions(props.game.gameUid, newLimit);
    await refresh();
  } catch (reason) {
    error.value = String(reason);
  }
}

async function updateBody() {
  if (busy.value || runtime.value) return;
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  const confirmMsg = props.game.health === "broken"
    ? "将使用所选游戏文件夹恢复并更新游戏本体。确定继续吗？"
    : "新版游戏文件夹会覆盖当前受管游戏本体。当前存档会先保护，确定开始更新吗？";
  if (!window.confirm(confirmMsg)) return;
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

async function removeFromLibrary() {
  if (busy.value || runtime.value) return;
  if (!window.confirm(`确定要从游戏库中彻底删除《${props.game.displayName}》吗？\n\n此操作将删除该游戏在 GameSaver 中的所有记录、保护配置及受管文件，无法撤销。`)) return;
  busy.value = true;
  error.value = "";
  message.value = "正在从库中彻底删除游戏";
  try {
    await removeGameFromLibrary(props.game.gameUid);
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
  if (cloudConnectionLoading.value) return "正在读取连接状态";
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
  if (taskEventTimer) clearTimeout(taskEventTimer);
  taskEventTimer = undefined;
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
  if (busy.value || coverSaving.value || coverCapturePending.value) return;
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
  await openCoverFile(file);
}

async function openCoverFile(file: File) {
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

async function beginCoverCapture() {
  if (coverSaving.value || coverCapturePending.value) return;
  try {
    coverError.value = "";
    error.value = "";
    const capture = await armGameCoverCapture(props.game.gameUid);
    coverCaptureId.value = capture.captureId;
    coverCaptureShortcut.value = capture.shortcut;
    coverCapturePending.value = true;
    message.value = `已准备截图，请切回游戏后按 ${capture.shortcut} 截取封面。`;
    try {
      await getCurrentWindow().minimize();
      message.value = `GameSaver 已最小化，请切回游戏后按 ${capture.shortcut} 截取封面。`;
    } catch (reason) {
      error.value = `GameSaver 未能自动最小化，请手动切回游戏后按 ${capture.shortcut} 截取封面：${String(reason)}`;
    }
  } catch (reason) {
    error.value = `启动封面截图失败：${String(reason)}`;
  }
}

async function handleCoverCaptureReady(payload: { captureId: string; gameUid: string }) {
  if (payload.gameUid !== props.game.gameUid) return;
  if (coverCaptureId.value && payload.captureId !== coverCaptureId.value) return;
  emit("captureHandled", payload.captureId);
  if (!coverCaptureId.value) {
    coverCaptureId.value = payload.captureId;
    coverCaptureShortcut.value = "Ctrl + Alt + S";
  }
  coverCapturePending.value = false;
  message.value = "正在载入游戏截图...";
  try {
    const response = await fetch(getGameCoverCaptureUrl(payload.captureId));
    if (!response.ok) throw new Error("临时截图不可用");
    const image = new File([await response.blob()], "game-capture.png", { type: "image/png" });
    await openCoverFile(image);
    message.value = "已截取游戏画面，请调整裁剪范围后保存。";
  } catch (reason) {
    error.value = `读取游戏截图失败：${String(reason)}`;
    void discardGameCoverCapture(payload.captureId);
    coverCaptureId.value = "";
    coverCaptureShortcut.value = "";
  }
}

function handleCoverCaptureFailed(payload: { captureId: string; gameUid: string; message: string }) {
  if (payload.gameUid !== props.game.gameUid) return;
  if (coverCaptureId.value && payload.captureId !== coverCaptureId.value) return;
  coverCapturePending.value = false;
  coverCaptureId.value = "";
  coverCaptureShortcut.value = "";
  error.value = `截取游戏画面失败：${payload.message}`;
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
  const captureId = coverCaptureId.value;
  coverCaptureId.value = "";
  coverCaptureShortcut.value = "";
  if (captureId) void discardGameCoverCapture(captureId);
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
    coverDisplayUrl.value = getGameCoverUrl(props.game.gameUid, Date.now());
    dismissCoverEditor();
    emit("refresh");
  } catch (reason) {
    coverError.value = String(reason);
  } finally {
    coverSaving.value = false;
  }
}

function loadCover() {
  if (props.coverUrl) {
    coverDisplayUrl.value = props.coverUrl;
    return;
  }
  if (props.game.cover) {
    coverDisplayUrl.value = getGameCoverUrl(props.game.gameUid, Date.now());
    return;
  }
  // 两个来源都没有时必须显式清空：否则上一张封面会留在 ref 里，显示成新游戏的封面。
  coverDisplayUrl.value = "";
}

watch(() => props.coverUrl, (value) => {
  if (value) coverDisplayUrl.value = value;
});

watch(() => props.initialError, (value) => {
  if (value) error.value = value;
});

// 这里原先有一个 watch(() => props.game.gameUid) 用来在切换游戏时手工清理上一场的
// 会话跟踪、进度与消息。父级现在用 :key="gameUid" 渲染本组件，切换游戏会直接重建
// 实例，局部状态天然是新局，该 watcher 再也不会触发。保留这段说明是因为**不要**
// 再退回逐项手工清理：此前它总是漏掉一两项（封面、保留数都曾残留到下一个游戏）。

watch(coverZoom, clampCoverPosition);

onMounted(() => {
  void refresh();
  void loadCover();
  void listen<{ captureId: string; gameUid: string }>("cover-capture-ready", (event) => {
    void handleCoverCaptureReady(event.payload);
  }).then((unlisten) => {
    if (detailListenersDisposed) unlisten();
    else stopCoverCaptureReady = unlisten;
  });
  void listen<{ captureId: string; gameUid: string; message: string }>("cover-capture-failed", (event) => {
    handleCoverCaptureFailed(event.payload);
  }).then((unlisten) => {
    if (detailListenersDisposed) unlisten();
    else stopCoverCaptureFailed = unlisten;
  });
  // 会话状态改由后端推送驱动，轮询退化为 5s 兜底。
  void listen<{ gameUid?: string }>("runtime-changed", (event) => {
    handleRuntimeChanged(event.payload);
  }).then((unlisten) => {
    if (detailListenersDisposed) unlisten();
    else stopRuntimeChanged = unlisten;
  });
  void listen<{ taskId?: string; gameUid?: string }>("task-changed", (event) => {
    handleTaskChanged(event.payload);
  }).then((unlisten) => {
    if (detailListenersDisposed) unlisten();
    else stopTaskChanged = unlisten;
  });
  if (props.pendingCoverCapture) {
    void handleCoverCaptureReady(props.pendingCoverCapture);
  }
});
onUnmounted(() => {
  detailListenersDisposed = true;
  stopCoverCaptureReady?.();
  stopCoverCaptureFailed?.();
  stopRuntimeChanged?.();
  stopTaskChanged?.();
  if (coverCaptureId.value) void discardGameCoverCapture(coverCaptureId.value);
  stopPolling();
  releaseCoverSource();
});
</script>

<template>
  <section class="game-detail-page page-enter">
    <header class="detail-header">
      <button class="icon-button" type="button" title="返回游戏库" aria-label="返回游戏库" @click="emit('back')"><ArrowLeft :size="18" /></button>
      <div class="detail-heading">
        <p class="eyebrow">游戏详情</p>
        <div v-if="renaming" class="detail-rename-form">
          <input
            ref="nameInputRef"
            v-model="editingName"
            type="text"
            class="detail-rename-input"
            maxlength="100"
            placeholder="输入游戏名称"
            :disabled="renameSaving"
            @keydown.enter.prevent="submitRename"
            @keydown.escape.prevent="cancelRename"
          />
          <button
            class="icon-button detail-rename-action confirm"
            type="button"
            title="保存名称 (Enter)"
            aria-label="保存名称"
            :disabled="renameSaving"
            @click="submitRename"
          >
            <Check :size="16" />
          </button>
          <button
            class="icon-button detail-rename-action cancel"
            type="button"
            title="取消 (Esc)"
            aria-label="取消修改"
            :disabled="renameSaving"
            @click="cancelRename"
          >
            <X :size="16" />
          </button>
        </div>
        <div v-else class="detail-title-row">
          <h1 :title="`游戏名称：${game.displayName}`">{{ game.displayName }}</h1>
          <button
            class="icon-button inline-rename-button"
            type="button"
            title="修改游戏名称"
            aria-label="修改游戏名称"
            :disabled="busy || !!runtime"
            @click="startRename"
          >
            <Pencil :size="16" />
          </button>
        </div>
        <p v-if="renameError" class="detail-rename-error">{{ renameError }}</p>
        <p v-else>管理本体、启动和存档保护。</p>
      </div>
      <button class="icon-button detail-refresh" type="button" title="刷新游戏状态" aria-label="刷新游戏状态" :disabled="loading || busy" @click="refresh"><RefreshCw :size="17" :class="{ spin: loading }" /></button>
    </header>

    <div v-if="error && !precheck" class="state-panel error-state"><AlertTriangle :size="25" /><strong>读取游戏状态失败</strong><p>{{ error }}</p><button type="button" @click="refresh">重试</button></div>
    <template v-else>
      <section class="detail-hero">
        <div class="detail-cover">
          <img v-if="coverDisplayUrl" :src="coverDisplayUrl" :alt="`${game.displayName} 封面`" />
          <Gamepad2 v-else :size="42" />
          <div class="cover-actions">
            <button class="cover-edit-button" type="button" :disabled="coverSaving || coverCapturePending" title="隐藏 GameSaver 后，按 Ctrl + Alt + S 截取当前游戏画面" @click="beginCoverCapture"><Camera :size="15" />{{ coverCapturePending ? "等待截图" : "截取游戏画面" }}</button>
            <button class="cover-edit-button" type="button" :disabled="busy || coverSaving || coverCapturePending" title="上传并调整游戏封面" @click="chooseCover"><ImagePlus :size="15" />{{ coverDisplayUrl ? "更换封面" : "上传封面" }}</button>
          </div>
          <span v-if="coverCapturePending" class="cover-capture-hint">{{ coverCaptureShortcut }}</span>
          <input ref="coverInput" class="visually-hidden" type="file" accept="image/jpeg,image/png,image/webp" @change="handleCoverSelected" />
        </div>
        <div class="detail-hero-copy">
          <span class="status-label">{{ runtime ? runtimeLabel(runtime.status) : (precheck ? (precheck.canLaunch ? "可启动" : "需要处理") : gameStatusLabel(game)) }}</span>
          <h2>{{ precheck ? (precheck.canLaunch ? "准备就绪" : "启动前需要处理") : (game.lifecycle === 'pending_setup' ? '需要完成设置' : (game.health !== 'ready' ? '需要处理' : '环境检测中...')) }}</h2>
          <p>{{ message || (precheck ? (precheck.canLaunch ? "游戏本体和存档保护配置均可用。" : "完成下方检查后才能启动游戏。") : "正在核对启动程序与存档保护配置...") }}</p>
          <button class="primary-button detail-launch" type="button" :disabled="launchButton.disabled" @click="start">
            <LoaderCircle v-if="launchButton.spinning" :size="17" class="spin" />
            <Play v-else :size="17" />
            {{ launchButton.label }}
          </button>
        </div>
      </section>

      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <div v-if="busy" class="task-progress"><div class="task-progress-heading"><strong>{{ message || "正在处理" }}</strong><span>{{ taskProgress }}%</span></div><div class="progress-track"><span :style="{ width: `${taskProgress}%` }"></span></div></div>

      <div class="detail-columns">
        <section class="detail-section">
          <header class="detail-section-header">
            <div><p class="eyebrow">启动前检查</p><h2>运行环境</h2></div>
            <CheckCircle2 v-if="precheck?.canLaunch" class="detail-ok" :size="20" />
            <AlertTriangle v-else-if="precheck" class="detail-warning" :size="20" />
            <Clock3 v-else class="detail-muted" :size="20" />
          </header>
          <div class="check-list">
            <div class="check-row">
              <span><Folder :size="16" />游戏本体目录</span>
              <strong v-if="precheck" :class="{ good: game.managedPath && precheck.canLaunch }">{{ game.managedPath ? "已找到" : "缺失" }}</strong>
              <strong v-else :class="{ good: !!game.managedPath }">{{ game.managedPath ? "已找到" : "检测中..." }}</strong>
            </div>
            <div class="check-row">
              <span><Play :size="16" />启动程序</span>
              <strong v-if="precheck" :class="{ good: precheck.executableExists }">{{ precheck.executableExists ? "已找到" : "缺失" }}</strong>
              <strong v-else>检测中...</strong>
            </div>
            <div class="check-row">
              <span><ShieldCheck :size="16" />存档保护</span>
              <strong v-if="precheck" :class="{ good: precheck.saveProfileReady && (precheck.validScopeCount || 0) > 0 }">{{ precheck.saveProfileReady ? `${precheck.validScopeCount} 个范围` : "未设置" }}</strong>
              <strong v-else>检测中...</strong>
            </div>
          </div>
          <div v-if="precheck?.issues.length" class="issue-list"><p v-for="issue in precheck.issues" :key="issue">{{ issue }}</p></div>
        </section>

        <section class="detail-section">
          <header class="detail-section-header"><div><p class="eyebrow">游戏本体</p><h2>受管目录</h2></div><HardDrive :size="20" class="detail-muted" /></header>
          <div class="managed-path-row">
            <p class="managed-path">{{ game.managedPath }}</p>
            <button class="secondary-button compact-button" type="button" title="在文件资源管理器中打开受管游戏本体目录" @click="openFolder(game.managedPath)">
              <FolderOpen :size="14" />打开本体目录
            </button>
          </div>
          <dl class="detail-facts">
            <div><dt>启动文件</dt><dd>{{ game.launch.executableRelativePath }}</dd></div>
            <div><dt>保存版本</dt><dd>{{ loading && !precheck ? "..." : versions.length }} 个</dd></div>
            <div><dt>旧本体版本</dt><dd>{{ loading && !precheck ? "..." : bodyVersions.length }} 个</dd></div>
          </dl>
          <div class="body-action-row"><button class="secondary-button" type="button" :disabled="busy || !!runtime" title="选择新版游戏文件夹并更新" @click="updateBody"><LoaderCircle v-if="busy && (message.includes('更新') || message.includes('新版'))" :size="16" class="spin" /><FolderOpen v-else :size="16" />更新游戏本体</button><button class="secondary-button" type="button" :disabled="busy || !!runtime" title="创建本体 ZIP 缓存" @click="packageBody"><LoaderCircle v-if="busy && message.includes('本体包')" :size="16" class="spin" /><Archive v-else :size="16" />创建本体包</button><button class="secondary-button danger-outline-button" type="button" :disabled="busy || !!runtime" title="删除 GameSaver 管理的本地游戏本体，保留配置和云端版本" @click="uninstallBody"><Trash2 :size="16" />卸载本体</button><button class="secondary-button danger-outline-button" type="button" :disabled="busy || !!runtime" title="从游戏库中彻底删除该游戏记录及受管文件" @click="removeFromLibrary"><Trash2 :size="16" />彻底删除</button></div>
          <div class="cloud-summary"><span class="status-dot" :class="{ active: baiduReady() }"></span><strong>百度网盘</strong><span>{{ baiduLabel() }}</span><span v-if="baiduReady()">· 云端版本请在游戏商店管理</span><button v-if="!cloudConnectionLoading && !baiduReady()" class="secondary-button compact-button" type="button" title="配置或授权百度网盘" @click="emit('settings')">{{ baiduActionLabel() }}</button></div>
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
              <select :value="keepVersions" :disabled="busy || !!runtime" aria-label="保留保存版本数量" @change="changeKeepVersions(Number(($event.target as HTMLSelectElement).value))">
                <option :value="1">1 个</option><option :value="3">3 个</option><option :value="5">5 个</option><option :value="10">10 个</option><option :value="20">20 个</option>
              </select>
            </label>
            <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime || versions.length <= keepVersions" title="清理旧保存版本" @click="pruneVersions"><Trash2 :size="15" />清理</button>
          </div>
        </header>

        <div class="detail-scopes-card">
          <div class="detail-scopes-header">
            <p class="timeline-caption">受保护的存档目录</p>
            <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime" title="选择并更改游戏真实存档目录" @click="changeSaveDirectory">
              <Pencil :size="14" />更改存档目录
            </button>
          </div>
          <div v-if="saveProfile?.scopes?.length">
            <div v-for="scope in saveProfile.scopes" :key="scope.rootPath" class="detail-scope-row">
              <div class="detail-scope-info">
                <span class="scope-type">{{ rootTypeLabel[scope.rootType] || "存档目录" }}</span>
                <p class="detail-scope-path" :title="cleanDisplayPath(scope.rootPath)">{{ formatScopeDisplay(scope) }}</p>
                <small v-if="scope.rootType === 'managed_game' && formatScopeDisplay(scope) !== cleanDisplayPath(scope.rootPath)" class="scope-subtitle" :title="cleanDisplayPath(scope.rootPath)">实际物理路径：{{ cleanDisplayPath(scope.rootPath) }}</small>
              </div>
              <button class="secondary-button compact-button" type="button" title="在文件资源管理器中打开这个存档目录" @click="openFolder(scope.rootPath)">
                <FolderOpen :size="14" />打开存档目录
              </button>
            </div>
          </div>
          <div v-else-if="loading && !precheck" class="detail-scope-empty">
            <p class="empty-hint">正在读取受保护的存档目录...</p>
          </div>
          <div v-else class="detail-scope-empty">
            <p class="empty-hint">暂未配置存档目录，点击上方按钮指定存档位置。</p>
          </div>
        </div>

        <div class="cloud-save-banner">
          <div class="cloud-save-main">
            <div class="cloud-save-icon"><Cloud :size="18" /></div>
            <div class="cloud-save-info">
              <div class="cloud-save-status-row">
                <strong>存档云同步</strong>
                <span class="cloud-badge" :class="cloudSaveStatusBadgeClass()">{{ cloudSaveStatusText() }}</span>
              </div>
              <small v-if="baiduReady() && cloudSaveStatus">
                本地 {{ cloudSaveStatus.localVersionCount }} 个版本 · 云端 {{ cloudSaveStatus.cloudVersionCount }} 个版本
                <template v-if="cloudSaveStatus.latestCloudCreatedAt"> · 云端最新：{{ formatDate(cloudSaveStatus.latestCloudCreatedAt) }}</template>
              </small>
              <small v-else-if="cloudConnectionLoading">正在后台读取百度网盘状态，不影响本地游戏操作。</small>
              <small v-else-if="!baiduReady()">请先在【平台设置】中完成百度网盘授权，以启用云端跨设备同步。</small>
              <small v-else-if="cloudSaveError">云端存档暂时无法读取，可打开列表重试。</small>
            </div>
          </div>
          <div v-if="baiduReady()" class="cloud-save-actions">
            <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime || !cloudSaveLoaded || cloudSaveLoading" title="立即与云端同步" @click="syncLatestSave"><RefreshCw :size="14" />同步最新</button>
            <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime" title="查看百度网盘上的所有历史存档" @click="openCloudSaveDrawer"><FolderOpen :size="14" />云端存档<template v-if="cloudSaveLoaded"> ({{ cloudSaveVersions.length }})</template></button>
          </div>
          <div v-else-if="!cloudConnectionLoading" class="cloud-save-actions">
            <button class="secondary-button compact-button" type="button" @click="emit('settings')">配置网盘</button>
          </div>
        </div>

        <p class="timeline-caption">游戏退出后自动提交；恢复前会先保护当前存档，恢复后不属于该版本的存档文件会被移除</p>
        <div v-if="loading && !precheck" class="timeline-empty">
          <span class="loader"></span>
          <p>正在载入保存历史...</p>
        </div>
        <div v-else-if="!versions.length" class="timeline-empty"><ShieldCheck :size="22" /><p>还没有保存版本。启动游戏并正常退出一次后，GameSaver 会在这里记录版本。</p></div>
        <div v-else class="version-list">
          <article v-for="(version, index) in versions" :key="version.versionId" class="version-row">
            <div class="version-icon"><Clock3 :size="17" /></div>
            <div class="version-copy"><strong>{{ index === 0 ? "最近一次保存" : "保存版本" }}</strong><span>{{ formatDate(version.createdAt) }}</span></div>
            <div class="version-meta"><strong>{{ version.files.length }} 个文件</strong><span>{{ formatBytes(version.totalBytes) }}</span></div>
            <div class="version-actions">
              <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime || !baiduReady()" title="上传这个保存版本到百度网盘" @click="uploadSave(version)"><CloudUpload :size="15" />上传云端</button>
              <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime" title="恢复这个保存版本" @click="restoreVersion(version)"><RotateCcw :size="15" />恢复</button>
              <button class="icon-button danger-button" type="button" :disabled="busy || !!runtime" title="删除这个保存版本" :aria-label="`删除 ${formatDate(version.createdAt)} 保存版本`" @click="deleteVersion(version)"><Trash2 :size="15" /></button>
            </div>
          </article>
        </div>
      </section>
    </template>
  </section>

  <Teleport to="body">
    <div v-if="cloudSaveDrawerOpen" class="cover-editor-overlay" @click.self="cloudSaveDrawerOpen = false">
      <section class="cloud-save-dialog" role="dialog" aria-modal="true" aria-label="百度网盘云端存档">
        <header class="cloud-save-header">
          <div><p class="eyebrow">云端存档管理</p><h2>百度网盘云端存档</h2></div>
          <button class="icon-button" type="button" title="关闭" aria-label="关闭" @click="cloudSaveDrawerOpen = false"><X :size="18" /></button>
        </header>
        <div class="cloud-save-body">
          <div v-if="cloudSaveLoading" class="state-panel"><span class="loader"></span><strong>正在加载云端存档列表...</strong></div>
          <div v-else-if="cloudSaveError" class="state-panel error-state"><AlertTriangle :size="22" /><strong>云端存档读取失败</strong><p>{{ cloudSaveError }}</p><button type="button" @click="loadCloudSaveOverview(true)">重试</button></div>
          <div v-else-if="!cloudSaveVersions.length" class="cloud-save-empty">
            <Cloud :size="32" />
            <p>百度网盘上暂无此游戏的云端存档</p>
          </div>
          <article v-for="cVer in cloudSaveVersions" :key="cVer.versionId" class="cloud-save-row">
            <div class="cloud-save-meta">
              <strong>{{ formatDate(cVer.createdAt) }}</strong>
              <span>{{ cVer.fileCount }} 个文件 · {{ formatBytes(cVer.packageSize) }}<template v-if="cVer.deviceName"> · 来自 {{ cVer.deviceName }}</template></span>
            </div>
            <div class="cloud-save-btns">
              <button class="secondary-button compact-button" type="button" :disabled="busy || !!runtime" title="从百度网盘拉取并还原该版本" @click="restoreCloudSave(cVer)"><CloudDownload :size="14" />还原至本机</button>
              <button class="icon-button danger-button" type="button" :disabled="busy || !!runtime" title="从网盘删除" :aria-label="`删除 ${formatDate(cVer.createdAt)} 云端存档`" @click="deleteCloudSave(cVer)"><Trash2 :size="14" /></button>
            </div>
          </article>
        </div>
      </section>
    </div>

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
