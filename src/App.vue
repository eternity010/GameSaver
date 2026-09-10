<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AlertTriangle, ChevronLeft, ChevronRight, CloudDownload, CloudUpload, Gamepad2, Library, Plus, Settings, Search, ShieldCheck } from "@lucide/vue";
import { confirmAppExit, deleteRemoteBodyPackage, discardGameCoverCapture, getElevationStatus, getGameCoverUrl, getGameRuntime, getTask, installCloudGame, launchGame, listCloudGames, listGames, restartAsAdmin } from "./api";
import type { AppTask, ElevationStatus } from "./api";
import type { CloudGameSummary, CloudGameVersion } from "./api";
import { gameStatusLabel, type Game } from "./domain/game";
import { taskPolicyOf, useTaskFeed } from "./taskFeed";
import AddGameWizard from "./components/AddGameWizard.vue";
import GameDetailPage from "./components/GameDetailPage.vue";
import GameStorePage from "./components/GameStorePage.vue";
import TransferCenter from "./components/TransferCenter.vue";
import PlatformSettings from "./components/PlatformSettings.vue";

type LibraryView = "all" | "attention";
type LibrarySort = "activity" | "last_played" | "newest_added" | "name_asc" | "oldest_added";
type AppPage = "library" | "store" | "add" | "detail" | "transfers" | "settings";

const validSorts: LibrarySort[] = ["activity", "last_played", "newest_added", "name_asc", "oldest_added"];
const savedSort = localStorage.getItem("gamesaver_library_sort") as LibrarySort;

const games = ref<Game[]>([]);
const cloudGames = ref<CloudGameSummary[]>([]);
const coverUrls = ref<Record<string, string>>({});
const activePage = ref<AppPage>("library");
const activeView = ref<LibraryView>("all");
const activeSort = ref<LibrarySort>(validSorts.includes(savedSort) ? savedSort : "activity");
const search = ref("");
const loading = ref(true);
const error = ref("");
const selectedGame = ref<Game | null>(null);
const selectedGameError = ref("");
const pendingCoverCapture = ref<{ captureId: string; gameUid: string } | null>(null);
const cloudInstallUid = ref("");
const cloudInstallProgress = ref(0);
const cloudInstallMessage = ref("");
const cloudInstallError = ref("");
const cloudInstallNotice = ref("");
const storeLoading = ref(false);
const storeError = ref("");
const storeLoaded = ref(false);
const storePage = ref(1);
const storeTotalCount = ref(0);
const storeTotalPages = ref(1);
const storeHasMore = ref(false);
const STORE_PAGE_SIZE = 9;
const LIBRARY_PAGE_SIZE = 9;
const libraryPage = ref(1);
const elevationStatus = ref<ElevationStatus | null>(null);
const elevationLoading = ref(false);
const elevationError = ref("");
/**
 * 任务列表由全应用共享的任务流提供：后端推送 `task-changed`，定时轮询只作兜底。
 * 这里把它派生成三个信号——角标计数、「该游戏是否在运行」、「云存档未同步」。
 *
 * 分流依据一律取自后端的 `category`（策略表在 `taskFeed.ts`）。前端不再维护任务
 * 类型白名单：那份白名单此前写了两份、且只覆盖 20 个任务类型里的 6 个。
 */
const taskFeed = useTaskFeed();
const tasks = taskFeed.tasks;

const isActiveTask = (task: AppTask) => task.status === "pending" || task.status === "running";

const activeTransferCount = computed(
  () => tasks.value.filter((task) => isActiveTask(task) && taskPolicyOf(task).badge).length,
);

// launch_game 的生命周期 == 整场游戏会话，所以它就是「该游戏是否在运行」在这个
// 列表里现成的信号，与详情页的 runtime 同源（后端都会在会话结束时清掉）。
const runningGameUids = computed(
  () =>
    new Set(
      tasks.value
        .filter((task) => task.category === "session" && isActiveTask(task) && task.gameUid)
        .map((task) => task.gameUid as string),
    ),
);

// 自动同步是后台静默触发的，唯一的「响」就落在失败信号 + 传输中心的重试入口上。
// 同一游戏可能堆着多条历史同步记录，只有「最近一条」才代表当前状态——否则一次
// 早先的失败会永远钉在卡片上，哪怕后来已经同步成功。
const unsyncedSyncTasks = computed(() => {
  const latestSyncByGame = new Map<string, AppTask>();
  for (const task of tasks.value) {
    if (task.category !== "cloud_save_sync" || !task.gameUid) continue;
    const current = latestSyncByGame.get(task.gameUid);
    if (!current || parseTimestamp(task.createdAt) >= parseTimestamp(current.createdAt)) {
      latestSyncByGame.set(task.gameUid, task);
    }
  }
  return [...latestSyncByGame.values()].filter(
    (task) => task.status === "failed" || task.status === "interrupted",
  );
});
const unsyncedGameUids = computed(
  () => new Set(unsyncedSyncTasks.value.map((task) => task.gameUid as string)),
);
const syncAttentionCount = computed(() => unsyncedSyncTasks.value.length);
let cloudInstallTimer: ReturnType<typeof setTimeout> | undefined;
let stopCoverCaptureRoute: UnlistenFn | undefined;
let stopExitGuard: UnlistenFn | undefined;
let appDisposed = false;
let exitPromptOpen = false;
let gamesLoadGeneration = 0;
let storeLoadGeneration = 0;

function parseTimestamp(raw?: string): number {
  if (!raw) return 0;
  const num = Number(raw);
  if (Number.isFinite(num) && num > 0) {
    return num > 1e11 ? num : num * 1000;
  }
  const parsed = Date.parse(raw);
  return Number.isFinite(parsed) ? parsed : 0;
}

const filteredGames = computed(() => {
  const keyword = search.value.trim().toLocaleLowerCase();
  const result = games.value.filter((game) => {
    if (keyword && !game.displayName.toLocaleLowerCase().includes(keyword)) return false;
    const needsAttention = gameStatusLabel(game) !== "可启动";
    if (activeView.value === "attention") return needsAttention;
    return !needsAttention;
  });

  const orderMap = new Map<string, number>();
  games.value.forEach((g, idx) => orderMap.set(g.gameUid, idx));

  const getAddedTime = (g: Game): number => {
    if (g.addedAt) {
      const t = parseTimestamp(g.addedAt);
      if (t > 0) return t;
    }
    const idx = orderMap.get(g.gameUid) ?? 0;
    return 1700000000000 + idx * 1000;
  };

  const getPlayedTime = (g: Game): number => {
    return parseTimestamp(g.lastPlayedAt);
  };

  const getActivityTime = (g: Game): number => {
    return Math.max(getPlayedTime(g), getAddedTime(g));
  };

  return [...result].sort((a, b) => {
    switch (activeSort.value) {
      case "activity": {
        const diff = getActivityTime(b) - getActivityTime(a);
        if (diff !== 0) return diff;
        return (orderMap.get(b.gameUid) ?? 0) - (orderMap.get(a.gameUid) ?? 0);
      }
      case "last_played": {
        const playedA = getPlayedTime(a);
        const playedB = getPlayedTime(b);
        if (playedA !== playedB) return playedB - playedA;
        const addedDiff = getAddedTime(b) - getAddedTime(a);
        if (addedDiff !== 0) return addedDiff;
        return (orderMap.get(b.gameUid) ?? 0) - (orderMap.get(a.gameUid) ?? 0);
      }
      case "newest_added": {
        const addedDiff = getAddedTime(b) - getAddedTime(a);
        if (addedDiff !== 0) return addedDiff;
        return (orderMap.get(b.gameUid) ?? 0) - (orderMap.get(a.gameUid) ?? 0);
      }
      case "oldest_added": {
        const addedDiff = getAddedTime(a) - getAddedTime(b);
        if (addedDiff !== 0) return addedDiff;
        return (orderMap.get(a.gameUid) ?? 0) - (orderMap.get(b.gameUid) ?? 0);
      }
      case "name_asc": {
        const cmp = a.displayName.localeCompare(b.displayName, "zh-CN", { numeric: true });
        if (cmp !== 0) return cmp;
        return (orderMap.get(a.gameUid) ?? 0) - (orderMap.get(b.gameUid) ?? 0);
      }
      default:
        return 0;
    }
  });
});

const libraryPageCount = computed(() => Math.max(1, Math.ceil(filteredGames.value.length / LIBRARY_PAGE_SIZE)));
const pagedGames = computed(() => {
  const page = Math.min(libraryPage.value, libraryPageCount.value);
  const start = (page - 1) * LIBRARY_PAGE_SIZE;
  return filteredGames.value.slice(start, start + LIBRARY_PAGE_SIZE);
});

const libraryPageItems = computed(() => {
  const current = libraryPage.value;
  const total = libraryPageCount.value;
  if (total <= 7) {
    return Array.from({ length: total }, (_, i) => i + 1);
  }
  const items: (number | string)[] = [];
  items.push(1);
  if (current > 3) {
    items.push("...");
  }
  const start = Math.max(2, current - 1);
  const end = Math.min(total - 1, current + 1);
  for (let i = start; i <= end; i++) {
    items.push(i);
  }
  if (current < total - 2) {
    items.push("...");
  }
  items.push(total);
  return items;
});

const pageTitle = computed(() => activeView.value === "all" ? "游戏库" : "需要处理");
const readyGameCount = computed(() => games.value.filter((game) => gameStatusLabel(game) === "可启动").length);
const libraryEmptyTitle = computed(() => {
  if (!games.value.length) return "还没有加入游戏";
  if (search.value.trim()) return "没有匹配的游戏";
  if (activeView.value === "attention") return "没有需要处理的游戏";
  return readyGameCount.value ? "没有匹配的游戏" : "当前没有可启动的游戏";
});
const libraryEmptyDescription = computed(() => {
  if (!games.value.length) return "添加游戏本体后，它会出现在这里。";
  if (search.value.trim()) return "调整搜索关键词，或切换到另一个视图。";
  if (activeView.value === "attention") return "所有游戏当前都可以启动。";
  return readyGameCount.value
    ? "调整搜索关键词，或切换到另一个视图。"
    : "待处理的游戏会显示在「需要处理」中。完成设置后即可在这里启动。";
});

let storeSearchTimer: ReturnType<typeof setTimeout> | undefined;

watch([search, activeView, activeSort], () => {
  if (activeSort.value) {
    localStorage.setItem("gamesaver_library_sort", activeSort.value);
  }
  libraryPage.value = 1;
});

watch(search, (val) => {
  if (activePage.value === "store") {
    if (storeSearchTimer) clearTimeout(storeSearchTimer);
    storeSearchTimer = setTimeout(() => {
      storePage.value = 1;
      void loadStore(true, 1, val);
    }, 280);
  }
});

function loadGameCovers(list: Game[]) {
  const nextUrls: Record<string, string> = {};
  for (const game of list) {
    if (game.cover) {
      const tag = game.cover.displayPath || game.lastPlayedAt || "1";
      nextUrls[game.gameUid] = getGameCoverUrl(game.gameUid, tag);
    }
  }
  coverUrls.value = nextUrls;
}

async function loadGames() {
  const generation = ++gamesLoadGeneration;
  loading.value = true;
  error.value = "";
  try {
    const loaded = await listGames();
    if (generation !== gamesLoadGeneration) return;
    games.value = loaded;
    libraryPage.value = Math.min(libraryPage.value, Math.max(1, Math.ceil(loaded.length / LIBRARY_PAGE_SIZE)));
    if (selectedGame.value) {
      const refreshedGame = loaded.find((game) => game.gameUid === selectedGame.value?.gameUid);
      selectedGame.value = refreshedGame || null;
      if (!refreshedGame && activePage.value === "detail") {
        activePage.value = "library";
      }
    }
    void loadGameCovers(loaded);
  } catch (reason) {
    if (generation !== gamesLoadGeneration) return;
    error.value = String(reason);
  } finally {
    if (generation === gamesLoadGeneration) {
      loading.value = false;
    }
  }
}

function scrollToContentTop() {
  const container = document.querySelector(".content-area");
  if (container) {
    container.scrollTo({ top: 0, behavior: "smooth" });
  }
}

async function loadStore(force = false, page = storePage.value, keyword = search.value) {
  if ((!force && storeLoading.value) || (!force && storeLoaded.value && page === storePage.value)) return;
  const generation = ++storeLoadGeneration;
  storeLoading.value = true;
  storeError.value = "";
  try {
    const result = await listCloudGames(page, STORE_PAGE_SIZE, keyword);
    if (generation !== storeLoadGeneration) return;
    cloudGames.value = result.games;
    storePage.value = result.page;
    storeTotalCount.value = result.totalCount;
    storeTotalPages.value = result.totalPages;
    storeHasMore.value = result.hasMore;
    storeLoaded.value = true;
  } catch (reason) {
    if (generation !== storeLoadGeneration) return;
    if (page === 1) {
      cloudGames.value = [];
      storeTotalCount.value = 0;
      storeTotalPages.value = 1;
      storeHasMore.value = false;
    }
    storeError.value = String(reason);
  } finally {
    if (generation === storeLoadGeneration) {
      storeLoading.value = false;
    }
  }
}

function refreshStore() {
  void loadStore(true, storePage.value, search.value);
}

function changeStorePage(page: number) {
  if (page < 1 || page > storeTotalPages.value || page === storePage.value) return;
  void loadStore(true, page, search.value).then(() => {
    scrollToContentTop();
  });
}

function changeLibraryPage(page: number) {
  if (page < 1 || page > libraryPageCount.value || page === libraryPage.value) return;
  libraryPage.value = page;
  scrollToContentTop();
}

async function loadElevationStatus() {
  try {
    elevationStatus.value = await getElevationStatus();
  } catch (reason) {
    elevationError.value = String(reason);
  }
}

async function restartWithAdmin() {
  elevationLoading.value = true;
  elevationError.value = "";
  try {
    await restartAsAdmin();
  } catch (reason) {
    elevationLoading.value = false;
    elevationError.value = String(reason);
  }
}

/**
 * 后端在「有游戏运行中却收到关闭请求」时会拦截关闭，并推送此事件。
 *
 * 这次确认是唯一能阻止「静默丢失本次存档」的关口：用户一旦确认退出，
 * 承载游戏会话的线程随进程消亡，本次游玩的存档不会被提交。
 */
async function handleExitBlocked(payload: { runningCount?: number } | undefined) {
  if (exitPromptOpen) return;
  exitPromptOpen = true;
  try {
    const count = payload?.runningCount ?? 1;
    const confirmed = window.confirm(
      `有 ${count} 个游戏正在运行。\n\n` +
        "现在关闭 GameSaver，本次游玩的存档将不会被自动提交，也不会生成新的存档版本。\n\n" +
        "仍要关闭吗？",
    );
    if (!confirmed) return;
    await confirmAppExit();
  } catch (reason) {
    console.error("确认退出失败", reason);
  } finally {
    exitPromptOpen = false;
  }
}

onMounted(() => {
  void loadGames();
  void loadElevationStatus();
  void loadStore();
  void listen<{ captureId: string; gameUid: string }>("cover-capture-ready", async (event) => {
    if (activePage.value === "detail" && selectedGame.value?.gameUid === event.payload.gameUid) return;
    let game = games.value.find((item) => item.gameUid === event.payload.gameUid);
    if (!game) {
      await loadGames();
      game = games.value.find((item) => item.gameUid === event.payload.gameUid);
    }
    if (!game) {
      void discardGameCoverCapture(event.payload.captureId);
      return;
    }
    selectedGame.value = game;
    selectedGameError.value = "";
    pendingCoverCapture.value = event.payload;
    activePage.value = "detail";
  }).then((unlisten) => {
    if (appDisposed) unlisten();
    else stopCoverCaptureRoute = unlisten;
  }).catch((reason) => {
    console.error("监听封面截图跳转事件失败", reason);
  });
  void listen<{ runningCount: number }>("app-exit-blocked", (event) => {
    void handleExitBlocked(event.payload);
  }).then((unlisten) => {
    if (appDisposed) unlisten();
    else stopExitGuard = unlisten;
  }).catch((reason) => {
    console.error("监听退出拦截事件失败", reason);
  });
});
onUnmounted(() => {
  appDisposed = true;
  stopCoverCaptureRoute?.();
  stopExitGuard?.();
  taskFeed.release();
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  coverUrls.value = {};
});

function openAddGame() {
  activePage.value = "add";
}

function openStore() {
  activePage.value = "store";
  void loadStore();
}

function openGame(game: Game, detailError = "") {
  pendingCoverCapture.value = null;
  selectedGame.value = game;
  selectedGameError.value = detailError;
  activePage.value = "detail";
}

function finishPendingCoverCapture(captureId: string) {
  if (pendingCoverCapture.value?.captureId === captureId) pendingCoverCapture.value = null;
}

async function quickLaunch(game: Game) {
  if (game.lifecycle !== "active") {
    openGame(game);
    return;
  }
  // 已在运行的游戏绝不再拉起第二个实例：后端最终也会拒绝，但先跳到详情页更符合
  // 预期 —— 那里能看到「运行中」和本次会话的提交进度。
  try {
    if (await getGameRuntime(game.gameUid)) {
      openGame(game);
      return;
    }
  } catch {
    // runtime 查询失败不阻断启动，交给后端做最终判定
  }
  try {
    await launchGame(game.gameUid);
    openGame(game);
  } catch (reason) {
    openGame(game, String(reason));
  }
}

async function installAndLaunch(cloudGame: CloudGameSummary, version: CloudGameVersion) {
  if (cloudInstallUid.value) return;
  if (!cloudGame.executableRelativePath) {
    cloudInstallError.value = "这个云端游戏缺少启动信息，请重新上传游戏本体包。";
    return;
  }
  cloudInstallUid.value = cloudGame.gameUid;
  cloudInstallProgress.value = 0;
  cloudInstallMessage.value = "准备下载游戏本体";
  cloudInstallError.value = "";
  cloudInstallNotice.value = "";
  try {
    const taskId = await installCloudGame(cloudGame.gameUid, cloudGame.gameKey, version.path, version.fsId);
    await watchCloudInstall(taskId, cloudGame);
  } catch (reason) {
    cloudInstallUid.value = "";
    cloudInstallError.value = String(reason);
  }
}

async function deleteCloudVersion(cloudGame: CloudGameSummary, version: CloudGameVersion) {
  if (cloudInstallUid.value) return;
  const warning = `将永久删除百度网盘中的“${cloudGame.displayName}”版本 ${version.versionId}（${formatBytes(version.size)}）。此操作无法撤销，确定继续吗？`;
  if (!window.confirm(warning) || !window.confirm("请再次确认：仅云端版本会被删除，本地游戏本体不会受影响。")) return;
  cloudInstallUid.value = cloudGame.gameUid;
  cloudInstallProgress.value = 0;
  cloudInstallMessage.value = "准备删除云端版本";
  cloudInstallError.value = "";
  cloudInstallNotice.value = "";
  try {
    const taskId = await deleteRemoteBodyPackage(cloudGame.gameUid, cloudGame.gameKey, version.path, version.fsId);
    await watchCloudDeletion(taskId);
  } catch (reason) {
    cloudInstallUid.value = "";
    cloudInstallError.value = String(reason);
  }
}

async function watchCloudDeletion(taskId: string) {
  const task = await getTask(taskId);
  cloudInstallProgress.value = task.progress;
  cloudInstallMessage.value = task.message;
  if (task.status === "success") {
    cloudInstallUid.value = "";
    cloudInstallNotice.value = "云端版本已删除。";
    await loadStore(true, storePage.value);
    return;
  }
  if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
    cloudInstallUid.value = "";
    cloudInstallError.value = task.error || task.message;
    return;
  }
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = setTimeout(() => void watchCloudDeletion(taskId), 700);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

async function watchCloudInstall(taskId: string, cloudGame: CloudGameSummary) {
  const task = await getTask(taskId);
  cloudInstallProgress.value = task.progress;
  cloudInstallMessage.value = task.message;
  if (task.status === "success") {
    cloudInstallUid.value = "";
    cloudInstallNotice.value = "安装完成，游戏已加入游戏库。可以从游戏库手动启动。";
    await loadGames();
    await loadStore(true, storePage.value);
    return;
  }
  if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
    cloudInstallUid.value = "";
    cloudInstallError.value = task.error || task.message;
    return;
  }
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = setTimeout(() => void watchCloudInstall(taskId, cloudGame), 700);
}

async function finishAddGame(completedGame?: Game) {
  activePage.value = "library";
  activeView.value = "all";
  search.value = "";
  await loadGames();
  if (completedGame && !games.value.some((game) => game.gameUid === completedGame.gameUid)) {
    await loadGames();
  }
}
</script>

<template>
  <main class="app-shell">
    <aside class="sidebar">
      <div class="brand">
        <span class="brand-mark"><Gamepad2 :size="20" /></span>
        <span>GameSaver</span>
      </div>
      <nav class="primary-nav" aria-label="主导航">
        <button class="nav-item" :class="{ active: activePage === 'library' }" type="button" @click="activePage = 'library'"><Library :size="18" /><span>游戏库</span></button>
        <button class="nav-item" :class="{ active: activePage === 'store' }" type="button" @click="openStore"><CloudDownload :size="18" /><span>游戏商店</span></button>
        <button class="nav-item" :class="{ active: activePage === 'add' }" type="button" @click="openAddGame"><Plus :size="18" /><span>添加游戏</span></button>
        <button class="nav-item" :class="{ active: activePage === 'transfers' }" type="button" @click="activePage = 'transfers'">
          <CloudUpload :size="18" />
          <span>传输中心</span>
          <span v-if="activeTransferCount > 0" class="nav-badge">{{ activeTransferCount }}</span>
          <span v-else-if="syncAttentionCount > 0" class="nav-badge nav-badge-alert" :title="`${syncAttentionCount} 个游戏的存档未能同步到网盘，可在传输中心重试`">{{ syncAttentionCount }}</span>
        </button>
      </nav>
      <div class="sidebar-bottom">
        <button class="nav-item" :class="{ active: activePage === 'settings' }" type="button" @click="activePage = 'settings'"><Settings :size="18" /><span>GameSaver 设置</span></button>
        <span class="local-status"><i></i> 本地优先</span>
      </div>
    </aside>

    <section class="content-area">
      <div v-if="elevationStatus && !elevationStatus.isAdmin" class="admin-banner" role="status">
        <div class="admin-banner-icon"><AlertTriangle :size="18" /></div>
        <div class="admin-banner-copy"><strong>当前未以管理员模式运行</strong><span>ETW 存档学习和部分本体操作可能受限。重启后会弹出 Windows 权限确认。</span><small v-if="elevationError">{{ elevationError }}</small></div>
        <button v-if="elevationStatus.canRestartAsAdmin" class="admin-restart-button" type="button" :disabled="elevationLoading" @click="restartWithAdmin"><ShieldCheck :size="16" />{{ elevationLoading ? "正在重启" : "管理员重启" }}</button>
      </div>
      <div v-else-if="elevationError" class="admin-banner admin-banner-error" role="alert"><div class="admin-banner-icon"><AlertTriangle :size="18" /></div><div class="admin-banner-copy"><strong>无法检测应用权限</strong><span>{{ elevationError }}</span></div></div>
      <header v-if="activePage === 'library'" class="topbar">
        <div>
          <p class="breadcrumb">GameSaver <span>/</span> {{ pageTitle }}</p>
          <h1>{{ pageTitle }}</h1>
        </div>
        <label class="search-box">
          <Search :size="17" />
          <input v-model="search" type="search" placeholder="搜索游戏" aria-label="搜索游戏" />
        </label>
      </header>
      <header v-else-if="activePage === 'store'" class="topbar">
        <div>
          <p class="breadcrumb">GameSaver <span>/</span> 游戏商店</p>
          <h1>游戏商店</h1>
        </div>
        <label class="search-box">
          <Search :size="17" />
          <input v-model="search" type="search" placeholder="搜索云端游戏" aria-label="搜索云端游戏" />
        </label>
      </header>

      <AddGameWizard v-if="activePage === 'add'" @back="activePage = 'library'" @completed="finishAddGame" />
      <GameDetailPage v-else-if="activePage === 'detail' && selectedGame" :key="selectedGame.gameUid" :game="selectedGame" :cover-url="selectedGame ? coverUrls[selectedGame.gameUid] : ''" :initial-error="selectedGameError" :pending-cover-capture="pendingCoverCapture" @back="activePage = 'library'" @settings="activePage = 'settings'" @refresh="loadGames" @capture-handled="finishPendingCoverCapture" />
      <GameStorePage v-else-if="activePage === 'store'" :games="cloudGames" :search="search" :loading="storeLoading" :load-error="storeError" :install-uid="cloudInstallUid" :install-progress="cloudInstallProgress" :install-message="cloudInstallMessage" :install-error="cloudInstallError" :install-notice="cloudInstallNotice" :page="storePage" :page-size="STORE_PAGE_SIZE" :total-count="storeTotalCount" :total-pages="storeTotalPages" :has-more="storeHasMore" @install="installAndLaunch" @delete-version="deleteCloudVersion" @retry="refreshStore" @refresh="refreshStore" @page-change="changeStorePage" />
      <TransferCenter v-else-if="activePage === 'transfers'" :games="games" :cloud-games="cloudGames" />
      <PlatformSettings v-else-if="activePage === 'settings'" />

      <template v-else-if="activePage === 'library'">
      <div class="library-toolbar" role="tablist" aria-label="游戏库视图">
        <button v-for="view in ([['all', '可启动'], ['attention', '需要处理']] as const)" :key="view[0]" class="view-tab" :class="{ active: activeView === view[0] }" type="button" @click="activeView = view[0]">{{ view[1] }}</button>
        <span v-if="filteredGames.length" class="library-count">{{ filteredGames.length }} 个游戏</span>

        <div class="library-sort">
          <label for="library-sort-select" class="sort-label">排序：</label>
          <select id="library-sort-select" v-model="activeSort" class="sort-select" aria-label="游戏排序方式">
            <option value="activity">最近活跃（游玩/添加）</option>
            <option value="last_played">最近游玩优先</option>
            <option value="newest_added">最新添加优先</option>
            <option value="name_asc">游戏名称 (A-Z)</option>
            <option value="oldest_added">最早添加优先</option>
          </select>
        </div>

        <button class="refresh-button" type="button" @click="loadGames">刷新</button>
      </div>

      <div v-if="loading" class="state-panel"><span class="loader"></span><strong>正在加载游戏库</strong></div>
      <div v-else-if="error" class="state-panel error-state"><strong>游戏库加载失败</strong><p>{{ error }}</p><button type="button" @click="loadGames">重试</button></div>
      <div v-else-if="!filteredGames.length" class="state-panel empty-state"><div class="empty-icon"><Gamepad2 :size="28" /></div><strong>{{ libraryEmptyTitle }}</strong><p>{{ libraryEmptyDescription }}</p><button v-if="!games.length || activeView === 'all'" class="primary-button" type="button" @click="openAddGame"><Plus :size="17" /> 添加游戏</button><button v-else-if="activeView === 'attention' && readyGameCount > 0" type="button" @click="activeView = 'all'">查看可启动游戏</button></div>
      <div v-else class="game-grid">
        <article v-for="game in pagedGames" :key="game.gameUid" class="game-card" tabindex="0" @click="openGame(game)" @keyup.enter="openGame(game)">
          <div class="game-card-cover">
            <img v-if="coverUrls[game.gameUid]" :src="coverUrls[game.gameUid]" :alt="`${game.displayName} 封面`" loading="lazy" @error="delete coverUrls[game.gameUid]" />
            <div v-else class="cover-placeholder"><Gamepad2 :size="34" /></div>
          </div>
          <div class="game-card-body"><div><h2>{{ game.displayName }}</h2><span class="status-label">{{ gameStatusLabel(game) }}</span><span v-if="unsyncedGameUids.has(game.gameUid)" class="status-label status-label-warn" title="最近一次自动同步存档失败，可在传输中心重试">存档未同步</span></div><button class="launch-button" type="button" :disabled="game.lifecycle !== 'active' || runningGameUids.has(game.gameUid)" @click.stop="quickLaunch(game)">{{ runningGameUids.has(game.gameUid) ? "运行中" : "启动" }}</button></div>
        </article>
      </div>
      <nav v-if="filteredGames.length" class="library-pagination" aria-label="游戏库分页">
        <button
          class="pagination-btn"
          type="button"
          :disabled="libraryPage <= 1"
          title="上一页"
          aria-label="上一页"
          @click="changeLibraryPage(libraryPage - 1)"
        >
          <ChevronLeft :size="18" />
        </button>
        <div class="pagination-pages">
          <template v-for="(item, idx) in libraryPageItems" :key="idx">
            <span v-if="item === '...'" class="pagination-ellipsis">…</span>
            <button
              v-else
              type="button"
              class="pagination-num"
              :class="{ active: item === libraryPage }"
              :disabled="item === libraryPage"
              :title="`前往第 ${item} 页`"
              @click="changeLibraryPage(Number(item))"
            >
              {{ item }}
            </button>
          </template>
        </div>
        <button
          class="pagination-btn"
          type="button"
          :disabled="libraryPage >= libraryPageCount"
          title="下一页"
          aria-label="下一页"
          @click="changeLibraryPage(libraryPage + 1)"
        >
          <ChevronRight :size="18" />
        </button>
        <span class="pagination-summary">
          第 {{ libraryPage }} / {{ libraryPageCount }} 页（共 {{ filteredGames.length }} 项）
        </span>
      </nav>
      </template>
    </section>
  </main>
</template>
