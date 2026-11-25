<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AlertTriangle, ChevronLeft, ChevronRight, CloudDownload, CloudUpload, Gamepad2, HardDrive, Library, Plus, Settings, Search, ShieldCheck } from "@lucide/vue";
import { confirmAction, confirmAppExit, deleteRemoteBodyPackage, discardGameCoverCapture, getElevationStatus, getGameCoverUrl, getGameRuntime, getTask, installCloudGame, launchGame, listCloudGames, listGames, restartAsAdmin } from "./api";
import type { AppTask, ElevationStatus } from "./api";
import type { CloudGamePage, CloudGameSummary, CloudGameVersion } from "./api";
import { gameStatusLabel, needsStoreUpload, type Game } from "./domain/game";
import { taskPolicyOf, useTaskFeed } from "./taskFeed";
import AddGameWizard from "./components/AddGameWizard.vue";
import GameDetailPage from "./components/GameDetailPage.vue";
import GameStorePage from "./components/GameStorePage.vue";
import LibraryStoragePage from "./components/LibraryStoragePage.vue";
import TransferCenter from "./components/TransferCenter.vue";
import PlatformSettings from "./components/PlatformSettings.vue";

type LibraryView = "all" | "attention";
type LibrarySort = "activity" | "last_played" | "newest_added" | "name_asc" | "oldest_added";
type AppPage = "library" | "store" | "add" | "detail" | "transfers" | "settings" | "storage";

const validSorts: LibrarySort[] = ["activity", "last_played", "newest_added", "name_asc", "oldest_added"];
const savedSort = localStorage.getItem("gamesaver_library_sort") as LibrarySort;

const games = ref<Game[]>([]);
const cloudGames = ref<CloudGameSummary[]>([]);
const pendingStoreUploadKeys = computed(() =>
  new Set(games.value.filter(needsStoreUpload).map((game) => game.gameKey)),
);
const coverUrls = ref<Record<string, string>>({});
const activePage = ref<AppPage>("library");
const storageBackPage = ref<"library" | "settings">("library");
const detailBackPage = ref<"library" | "storage">("library");
const pendingSetupUid = ref<string>();
const activeView = ref<LibraryView>("all");
const activeSort = ref<LibrarySort>(validSorts.includes(savedSort) ? savedSort : "activity");
const librarySearch = ref("");
const storeSearch = ref("");
const loading = ref(true);
const error = ref("");
/** 列表是否已有一次成功结果；只有它为真时刷新才保留旧列表而不是显示全屏加载。 */
const gamesLoaded = ref(false);
/** 上一次成功拉取的时间戳，用于 TTL 判断。 */
let gamesLoadedAt = 0;
const selectedGame = ref<Game | null>(null);
const selectedGameError = ref("");
const pendingCoverCapture = ref<{ captureId: string; gameUid: string } | null>(null);
const cloudInstallUid = ref("");
const cloudOperationTaskId = ref("");
const cloudInstallProgress = ref(0);
const cloudInstallMessage = ref("");
const cloudInstallError = ref("");
const cloudInstallNotice = ref("");
const cloudFeedbackGameKey = ref("");
const storeLoading = ref(false);
const storeError = ref("");
const storePage = ref(1);
const storeTotalCount = ref(0);
const storeTotalPages = ref(1);
const storeHasMore = ref(false);
const STORE_PAGE_SIZE = 9;
const LIBRARY_PAGE_SIZE = 9;
const storePageCache = new Map<string, { expiresAt: number; page: CloudGamePage }>();
const storePageRequests = new Map<string, { epoch: number; promise: Promise<CloudGamePage> }>();
const STORE_CACHE_TTL_MS = 5 * 60_000;
let storeCacheEpoch = 0;
/**
 * 游戏库列表的缓存有效期。
 *
 * 与商店不同，游戏库此前**完全没有**缓存守卫：任何一次 `loadGames()` 都真发请求，
 * 并在等待期间用全屏「正在加载游戏库」把已经拿到的列表整个盖掉。而列表绝大多数
 * 时候没有变化（16 个游戏 → 一次 IPC + 约 32 次文件系统健康探测）。
 *
 * 这里给一个**短 TTL**而不是永久缓存：后端的游戏状态会在没有任何用户动作时变化
 * ——云存档同步结束、启动会话结束、本体更新、自动清理——永久缓存会把这些变化挡在
 * 外面。TTL 到点后的重新拉取会保留旧列表（见模板），所以用户不会再看到全屏加载。
 *
 * 会真正改动列表的动作（添加/安装/卸载/删除/改名/封面/存档目录）一律走 `force`，
 * 不受 TTL 影响。
 */
const GAMES_CACHE_TTL_MS = 30_000;
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
const observedTaskStatuses = new Map<string, AppTask["status"]>();
const observedRemovalCommits = new Set<string>();

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
let cloudFeedbackTimer: ReturnType<typeof setTimeout> | undefined;
let stopCoverCaptureRoute: UnlistenFn | undefined;
let stopExitGuard: UnlistenFn | undefined;
let stopStoreInvalidation: UnlistenFn | undefined;
let appDisposed = false;
let exitPromptOpen = false;
let gamesLoadGeneration = 0;
let storeLoadGeneration = 0;
let cloudOperationGeneration = 0;
let quickLaunchGeneration = 0;
const quickLaunchPending = new Set<string>();
let libraryRefreshTimer: ReturnType<typeof setTimeout> | undefined;
let libraryRefreshCheckTimer: ReturnType<typeof setTimeout> | undefined;

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
  const keyword = librarySearch.value.trim().toLocaleLowerCase();
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
watch(libraryPageCount, (count) => {
  libraryPage.value = Math.min(Math.max(1, libraryPage.value), count);
}, { flush: "sync" });
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
  if (librarySearch.value.trim()) return "没有匹配的游戏";
  if (activeView.value === "attention") return "没有需要处理的游戏";
  return readyGameCount.value ? "没有匹配的游戏" : "当前没有可启动的游戏";
});
const libraryEmptyDescription = computed(() => {
  if (!games.value.length) return "添加游戏本体后，它会出现在这里。";
  if (librarySearch.value.trim()) return "调整搜索关键词，或切换到另一个视图。";
  if (activeView.value === "attention") return "所有游戏当前都可以启动。";
  return readyGameCount.value
    ? "调整搜索关键词，或切换到另一个视图。"
    : "待处理的游戏会显示在「需要处理」中。完成设置后即可在这里启动。";
});

let storeSearchTimer: ReturnType<typeof setTimeout> | undefined;

watch([librarySearch, activeView, activeSort], () => {
  if (activeSort.value) {
    localStorage.setItem("gamesaver_library_sort", activeSort.value);
  }
  libraryPage.value = 1;
});

watch(storeSearch, () => {
  if (storeSearchTimer) clearTimeout(storeSearchTimer);
  storeSearchTimer = undefined;
  storePage.value = 1;
  storeLoadGeneration++;
  if (activePage.value !== "store" || appDisposed) return;
  storeLoading.value = true;
  storeSearchTimer = setTimeout(() => {
    storeSearchTimer = undefined;
    if (activePage.value === "store" && !appDisposed) void loadStore(false, 1);
  }, 280);
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

/**
 * 正在拉列表且已有旧数据 —— 来自工具栏「刷新」或详情页动作。此时保留旧列表、只在
 * 工具栏出行内提示；`loading` 为真但**没有**旧数据时才是首次加载，仍走全屏面板。
 */
const refreshing = computed(() => loading.value && gamesLoaded.value);
/** 有旧数据可显示时的刷新失败：改为列表上方的提示条，不整页替换掉列表。 */
const refreshingError = computed(() => Boolean(error.value) && gamesLoaded.value);

/**
 * 把最新列表同步给「当前选中的游戏」，并在它已被删除时退出详情页。
 * 缓存命中也要走这一步 —— 否则详情页会拿着一个旧对象。
 */
function syncSelectedGame(loaded: Game[]) {
  if (!selectedGame.value) return;
  const refreshedGame = loaded.find((game) => game.gameUid === selectedGame.value?.gameUid);
  selectedGame.value = refreshedGame || null;
  if (!refreshedGame && activePage.value === "detail") {
    activePage.value = "library";
  }
}

/**
 * 拉取游戏库。
 *
 * `force` 为真时忽略缓存 —— 所有会改动列表的用户动作都必须走这条；TTL 只用来挡住
 * 纯粹的重复拉取（切页面、事件抖动）。
 */
async function loadGames(force = false) {
  // 缓存命中：不发请求，也不碰 `loading`。`loading` 只在**从没有过数据**时才置真，
  // 所以刷新期间旧列表继续显示，由工具栏的行内提示表达「正在刷新」。
  if (!force && gamesLoaded.value && Date.now() - gamesLoadedAt < GAMES_CACHE_TTL_MS) {
    syncSelectedGame(games.value);
    return;
  }
  const generation = ++gamesLoadGeneration;
  loading.value = !gamesLoaded.value;
  error.value = "";
  try {
    const loaded = await listGames();
    if (generation !== gamesLoadGeneration) return;
    games.value = loaded;
    gamesLoaded.value = true;
    gamesLoadedAt = Date.now();
    syncSelectedGame(loaded);
    void loadGameCovers(loaded);
  } catch (reason) {
    if (generation !== gamesLoadGeneration) return;
    error.value = String(reason);
    // 失败也要记时间，否则紧接着的每次导航都会再打一次后端（失败时反而最费）。
    // 已有数据时保留旧列表：一次刷新失败不该让整个库变成错误页。
    gamesLoadedAt = Date.now();
  } finally {
    if (generation === gamesLoadGeneration) {
      loading.value = false;
    }
  }
}

function scheduleLibraryRefresh() {
  if (libraryRefreshTimer) clearTimeout(libraryRefreshTimer);
  libraryRefreshTimer = setTimeout(() => {
    libraryRefreshTimer = undefined;
    void loadGames(true);
  }, 120);
}

function scheduleLibraryRefreshCheck() {
  if (libraryRefreshCheckTimer) clearTimeout(libraryRefreshCheckTimer);
  if (activePage.value !== "library" || appDisposed) {
    libraryRefreshCheckTimer = undefined;
    return;
  }
  libraryRefreshCheckTimer = setTimeout(() => {
    libraryRefreshCheckTimer = undefined;
    if (activePage.value !== "library" || appDisposed) return;
    void loadGames().finally(scheduleLibraryRefreshCheck);
  }, GAMES_CACHE_TTL_MS);
}

watch(activePage, (page, previousPage) => {
  quickLaunchGeneration++;
  if (previousPage === "store" && page !== "store") {
    clearCloudInstallFeedback();
    if (storeSearchTimer) clearTimeout(storeSearchTimer);
    storeSearchTimer = undefined;
    storeLoadGeneration++;
    storeLoading.value = false;
  }
  if (page === "library" && previousPage !== "library") {
    void loadGames().finally(scheduleLibraryRefreshCheck);
  } else if (page !== "library") {
    scheduleLibraryRefreshCheck();
  }
}, { flush: "sync" });

watch(tasks, (nextTasks) => {
  let sessionFinished = false;
  let installationFinished = false;
  let accountRestored = false;
  let libraryMigrated = false;
  let localRemovalChanged = false;
  let cloudStoreChanged = false;
  let cloudPublicationFinished = false;
  for (const task of nextTasks) {
    const previousStatus = observedTaskStatuses.get(task.taskId);
    if (["upload_game_body_package", "delete_remote_body_package", "repair_cloud_body_manifest"].includes(task.taskType)
        && !isActiveTask(task) && previousStatus !== task.status) {
      cloudStoreChanged = true;
    }
    if (["upload_game_body_package", "repair_cloud_body_manifest"].includes(task.taskType)
        && task.status === "success" && previousStatus !== "success") {
      cloudPublicationFinished = true;
    }
    if (["uninstall_game_body", "remove_game_from_library", "retry_game_removal", "update_game_body", "retry_game_body_update"].includes(task.taskType)) {
      if ((task.progress >= 30 && !observedRemovalCommits.has(task.taskId))
          || (!isActiveTask(task) && previousStatus !== task.status)) {
        observedRemovalCommits.add(task.taskId);
        localRemovalChanged = true;
      }
    }
    if (task.category === "session" && previousStatus && isActiveTask({ ...task, status: previousStatus }) && !isActiveTask(task)) {
      sessionFinished = true;
    }
    if (task.taskType === "install_cloud_game" && task.status === "success" && previousStatus !== "success") {
      installationFinished = true;
    }
    if (task.taskType === "restore_cloud_account" && task.status === "success" && previousStatus !== "success") {
      accountRestored = true;
    }
    if (["set_library_root", "resume_library_migration"].includes(task.taskType) && task.status === "success" && previousStatus !== "success") {
      libraryMigrated = true;
    }
    observedTaskStatuses.set(task.taskId, task.status);
  }
  if (sessionFinished || installationFinished || accountRestored || libraryMigrated || localRemovalChanged || cloudPublicationFinished) {
    if (accountRestored || libraryMigrated || localRemovalChanged || cloudPublicationFinished) gamesLoadedAt = 0;
    scheduleLibraryRefresh();
  }
  if (installationFinished || cloudStoreChanged || accountRestored || localRemovalChanged) {
    invalidateStoreCache();
    if (activePage.value === "store" && !cloudInstallUid.value) void loadStore(false, storePage.value);
  }
});

function scrollToContentTop() {
  const container = document.querySelector(".content-area");
  if (container) {
    container.scrollTo({ top: 0, behavior: "smooth" });
  }
}

function invalidateStoreCache() {
  storePageCache.clear();
  storePageRequests.clear();
  storeCacheEpoch++;
  storeLoadGeneration++;
  storeLoading.value = false;
}

function handleStoreInvalidated(payload: { reset?: boolean } | undefined) {
  if (appDisposed) return;
  invalidateStoreCache();
  if (payload?.reset) {
    cloudGames.value = [];
    storePage.value = 1;
    storeTotalCount.value = 0;
    storeTotalPages.value = 1;
    storeHasMore.value = false;
    storeError.value = "";
    endCloudOperation();
    clearCloudInstallFeedback();
  }
  if (activePage.value === "store") void loadStore(false);
}

function applyStorePage(result: CloudGamePage) {
  cloudGames.value = result.games;
  storePage.value = result.page;
  storeTotalCount.value = result.totalCount;
  storeTotalPages.value = result.totalPages;
  storeHasMore.value = result.hasMore;
}

async function loadStore(force = false, page = storePage.value, keyword = storeSearch.value) {
  if (appDisposed) return;
  const normalizedKeyword = keyword.trim();
  if (force) invalidateStoreCache();
  const key = JSON.stringify([page, normalizedKeyword]);
  const epoch = storeCacheEpoch;
  const generation = ++storeLoadGeneration;
  storeError.value = "";
  const cached = storePageCache.get(key);
  if (cached && Date.now() < cached.expiresAt) {
    applyStorePage(cached.page);
    storeLoading.value = false;
    return;
  }
  storeLoading.value = true;
  const pending = storePageRequests.get(key);
  const requestedAt = Date.now();
  const request = pending?.epoch === epoch
    ? pending.promise
    : listCloudGames(page, STORE_PAGE_SIZE, normalizedKeyword, force);
  storePageRequests.set(key, { epoch, promise: request });
  try {
    const result = await request;
    if (epoch !== storeCacheEpoch || appDisposed) return;
    storePageCache.set(key, {
      expiresAt: requestedAt + Math.min(result.cacheTtlMs ?? STORE_CACHE_TTL_MS, STORE_CACHE_TTL_MS),
      page: result,
    });
    if (storePageCache.size > 12) {
      const oldestKey = storePageCache.keys().next().value;
      if (oldestKey !== undefined) storePageCache.delete(oldestKey);
    }
    if (generation !== storeLoadGeneration) return;
    applyStorePage(result);
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
    if (storePageRequests.get(key)?.promise === request) storePageRequests.delete(key);
    if (generation === storeLoadGeneration) {
      storeLoading.value = false;
    }
  }
}

function refreshStore() {
  void loadStore(true, storePage.value, storeSearch.value);
}

function changeStorePage(page: number) {
  if (page < 1 || page > storeTotalPages.value || page === storePage.value) return;
  void loadStore(false, page, storeSearch.value).then(() => {
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

async function handleExitBlocked(payload: { runningCount?: number; activeTaskCount?: number } | undefined) {
  if (exitPromptOpen) return;
  exitPromptOpen = true;
  try {
    const count = payload?.runningCount ?? 0;
    const taskCount = payload?.activeTaskCount ?? 0;
    const warnings: string[] = [];
    if (count > 0) {
      warnings.push(`有 ${count} 个游戏正在运行。\n现在关闭 GameSaver，本次游玩的存档将不会被自动提交，也不会生成新的存档版本。`);
    }
    if (taskCount > 0) {
      warnings.push(`有 ${taskCount} 项后台任务尚未完成。\n关闭会中断正在进行的同步、压缩、上传等操作，未完成的任务可能需要重试。`);
    }
    const confirmed = await confirmAction(
      [...warnings, "仍要关闭 GameSaver 吗？"].join("\n\n"),
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
  void listen<{ reset: boolean }>("cloud-store-invalidated", (event) => {
    handleStoreInvalidated(event.payload);
  }).then((unlisten) => {
    if (appDisposed) unlisten();
    else stopStoreInvalidation = unlisten;
  }).catch((reason) => {
    console.error("监听商店缓存变更失败", reason);
  });
  void loadGames().finally(scheduleLibraryRefreshCheck);
  void loadElevationStatus();
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
    detailBackPage.value = activePage.value === "storage" ? "storage" : "library";
    activePage.value = "detail";
  }).then((unlisten) => {
    if (appDisposed) unlisten();
    else stopCoverCaptureRoute = unlisten;
  }).catch((reason) => {
    console.error("监听封面截图跳转事件失败", reason);
  });
  void listen<{ runningCount: number; activeTaskCount: number }>("app-exit-blocked", (event) => {
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
  clearCloudInstallFeedback();
  stopCoverCaptureRoute?.();
  stopExitGuard?.();
  stopStoreInvalidation?.();
  taskFeed.release();
  endCloudOperation();
  if (storeSearchTimer) clearTimeout(storeSearchTimer);
  if (libraryRefreshTimer) clearTimeout(libraryRefreshTimer);
  if (libraryRefreshCheckTimer) clearTimeout(libraryRefreshCheckTimer);
  coverUrls.value = {};
});

function openAddGame() {
  activePage.value = "add";
}

function resumeGameSetup(gameUid: string) {
  pendingSetupUid.value = gameUid;
  activePage.value = "add";
}

function leaveAddGame() {
  pendingSetupUid.value = undefined;
  activePage.value = "library";
  void loadGames(true);
}

function openStore() {
  activePage.value = "store";
  void loadStore(false);
}

function findGameInStore(game: Game) {
  const keyword = game.gameKey || game.displayName;
  activePage.value = "store";
  if (storeSearch.value === keyword) {
    void loadStore(false, 1, keyword);
  } else {
    storeSearch.value = keyword;
  }
}

function openGame(game: Game, detailError = "") {
  detailBackPage.value = activePage.value === "storage" ? "storage" : "library";
  pendingCoverCapture.value = null;
  selectedGame.value = game;
  selectedGameError.value = detailError;
  activePage.value = "detail";
}

function openStorage(from: "library" | "settings") {
  storageBackPage.value = from;
  activePage.value = "storage";
}

async function openStorageGame(gameUid: string) {
  let game = games.value.find((item) => item.gameUid === gameUid);
  if (!game) {
    await loadGames(true);
    game = games.value.find((item) => item.gameUid === gameUid);
  }
  if (game && activePage.value === "storage") openGame(game);
}

function finishPendingCoverCapture(captureId: string) {
  if (pendingCoverCapture.value?.captureId === captureId) pendingCoverCapture.value = null;
}

async function quickLaunch(game: Game) {
  if (appDisposed || quickLaunchPending.has(game.gameUid)) return;
  if (game.lifecycle !== "active") {
    openGame(game);
    return;
  }
  // 已在运行的游戏绝不再拉起第二个实例：后端最终也会拒绝，但先跳到详情页更符合
  // 预期 —— 那里能看到「运行中」和本次会话的提交进度。
  quickLaunchPending.add(game.gameUid);
  const generation = ++quickLaunchGeneration;
  const sourcePage = activePage.value;
  const showResult = (failure = "") => {
    if (!appDisposed && generation === quickLaunchGeneration && activePage.value === sourcePage) {
      openGame(game, failure);
    }
  };
  try {
    try {
      if (await getGameRuntime(game.gameUid)) {
        showResult();
        return;
      }
    } catch {
    }
    if (appDisposed) return;
    try {
      await launchGame(game.gameUid);
      showResult();
    } catch (reason) {
      showResult(String(reason));
    }
  } finally {
    quickLaunchPending.delete(game.gameUid);
  }
}

function beginCloudOperation(cloudGame: CloudGameSummary, message: string): number {
  const generation = ++cloudOperationGeneration;
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = undefined;
  cloudOperationTaskId.value = "";
  cloudInstallUid.value = cloudGame.gameUid;
  cloudInstallProgress.value = 0;
  cloudInstallMessage.value = message;
  clearCloudInstallFeedback();
  return generation;
}

function endCloudOperation(generation = cloudOperationGeneration) {
  if (generation !== cloudOperationGeneration) return;
  cloudOperationGeneration++;
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = undefined;
  cloudOperationTaskId.value = "";
  cloudInstallUid.value = "";
  cloudInstallProgress.value = 0;
  cloudInstallMessage.value = "";
}

function cloudOperationIsCurrent(generation: number, taskId?: string): boolean {
  return !appDisposed && generation === cloudOperationGeneration && Boolean(cloudInstallUid.value)
    && (taskId === undefined || taskId === cloudOperationTaskId.value);
}

function handleMissingCloudTask(cloudGame: CloudGameSummary, generation: number, reason: unknown): boolean {
  if (!String(reason).includes("task not found")) return false;
  endCloudOperation(generation);
  showCloudInstallFeedback(cloudGame, "error", "任务记录已不存在，正在重新读取游戏状态，请确认操作结果。");
  invalidateStoreCache();
  void loadGames(true);
  if (activePage.value === "store") void loadStore(false, storePage.value);
  return true;
}

function clearCloudInstallFeedback() {
  if (cloudFeedbackTimer) clearTimeout(cloudFeedbackTimer);
  cloudFeedbackTimer = undefined;
  cloudInstallError.value = "";
  cloudInstallNotice.value = "";
  cloudFeedbackGameKey.value = "";
}

function showCloudInstallFeedback(cloudGame: CloudGameSummary, kind: "error" | "notice", message: string) {
  clearCloudInstallFeedback();
  if (appDisposed || activePage.value !== "store") return;
  cloudFeedbackGameKey.value = cloudGame.gameKey || cloudGame.gameUid;
  const text = `${cloudGame.displayName}：${message}`;
  if (kind === "error") {
    cloudInstallError.value = text;
  } else {
    cloudInstallNotice.value = text;
    cloudFeedbackTimer = setTimeout(clearCloudInstallFeedback, 5000);
  }
}

async function installAndLaunch(cloudGame: CloudGameSummary, version: CloudGameVersion) {
  if (appDisposed || cloudInstallUid.value) return;
  if (!cloudGame.executableRelativePath) {
    showCloudInstallFeedback(cloudGame, "error", "这个云端游戏缺少启动信息，请重新上传游戏本体包。");
    return;
  }
  const generation = beginCloudOperation(cloudGame, "准备下载游戏本体");
  try {
    const localGame = games.value.find((game) => game.gameKey === cloudGame.gameKey);
    const replaceExisting = cloudGame.installed || Boolean(localGame);
    if (replaceExisting && !(await confirmAction(
      `将下载“${cloudGame.displayName}”版本 ${version.versionId} 并替换本地游戏本体。\n\n` +
      "本地只保留一个版本；已配置的受保护存档会保留，其他游戏文件会被替换。安装完成后不会自动启动。\n\n确定继续吗？",
    ))) {
      endCloudOperation(generation);
      return;
    }
    if (!cloudOperationIsCurrent(generation)) return;
    const taskId = await installCloudGame(cloudGame.gameUid, cloudGame.gameKey, version.path, version.fsId, replaceExisting);
    if (!cloudOperationIsCurrent(generation)) return;
    cloudOperationTaskId.value = taskId;
    await watchCloudInstall(taskId, cloudGame, generation);
  } catch (reason) {
    if (!cloudOperationIsCurrent(generation)) return;
    endCloudOperation(generation);
    showCloudInstallFeedback(cloudGame, "error", String(reason));
  }
}

async function deleteCloudVersion(cloudGame: CloudGameSummary, version: CloudGameVersion) {
  if (appDisposed || cloudInstallUid.value) return;
  const generation = beginCloudOperation(cloudGame, "等待确认删除云端版本");
  const warning = `将永久删除百度网盘中的“${cloudGame.displayName}”版本 ${version.versionId}（${formatBytes(version.size)}）。此操作无法撤销，确定继续吗？`;
  try {
    if (!(await confirmAction(warning))) {
      endCloudOperation(generation);
      return;
    }
    if (!cloudOperationIsCurrent(generation)) return;
    if (!(await confirmAction("请再次确认：仅云端版本会被删除，本地游戏本体不会受影响。"))) {
      endCloudOperation(generation);
      return;
    }
    if (!cloudOperationIsCurrent(generation)) return;
    cloudInstallMessage.value = "准备删除云端版本";
    const taskId = await deleteRemoteBodyPackage(cloudGame.gameUid, cloudGame.gameKey, version.path, version.fsId);
    if (!cloudOperationIsCurrent(generation)) return;
    cloudOperationTaskId.value = taskId;
    await watchCloudDeletion(taskId, cloudGame, generation);
  } catch (reason) {
    if (!cloudOperationIsCurrent(generation)) return;
    endCloudOperation(generation);
    showCloudInstallFeedback(cloudGame, "error", String(reason));
  }
}

async function watchCloudDeletion(taskId: string, cloudGame: CloudGameSummary, generation = cloudOperationGeneration) {
  if (!cloudOperationIsCurrent(generation, taskId)) return;
  let task: AppTask;
  try {
    task = await getTask(taskId);
  } catch (reason) {
    if (!cloudOperationIsCurrent(generation, taskId)) return;
    if (handleMissingCloudTask(cloudGame, generation, reason)) return;
    cloudInstallMessage.value = `读取删除进度暂时失败，正在自动重试：${String(reason)}`;
    if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
    cloudInstallTimer = setTimeout(() => void watchCloudDeletion(taskId, cloudGame, generation), 2000);
    return;
  }
  if (!cloudOperationIsCurrent(generation, taskId)) return;
  cloudInstallProgress.value = task.progress;
  cloudInstallMessage.value = task.message;
  if (task.status === "success") {
    endCloudOperation(generation);
    showCloudInstallFeedback(cloudGame, "notice", "云端版本已删除。");
    invalidateStoreCache();
    if (activePage.value === "store") await loadStore(false, storePage.value);
    return;
  }
  if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
    endCloudOperation(generation);
    showCloudInstallFeedback(cloudGame, "error", task.error || task.message);
    invalidateStoreCache();
    if (activePage.value === "store") await loadStore(false, storePage.value);
    return;
  }
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = setTimeout(() => void watchCloudDeletion(taskId, cloudGame, generation), 700);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

async function watchCloudInstall(taskId: string, cloudGame: CloudGameSummary, generation = cloudOperationGeneration) {
  if (!cloudOperationIsCurrent(generation, taskId)) return;
  let task: AppTask;
  try {
    task = await getTask(taskId);
  } catch (reason) {
    if (!cloudOperationIsCurrent(generation, taskId)) return;
    if (handleMissingCloudTask(cloudGame, generation, reason)) return;
    cloudInstallMessage.value = `读取安装进度暂时失败，正在自动重试：${String(reason)}`;
    if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
    cloudInstallTimer = setTimeout(() => void watchCloudInstall(taskId, cloudGame, generation), 2000);
    return;
  }
  if (!cloudOperationIsCurrent(generation, taskId)) return;
  cloudInstallProgress.value = task.progress;
  cloudInstallMessage.value = task.message;
  if (task.status === "success") {
    endCloudOperation(generation);
    const cleanupPending = (task.result as { cleanupPending?: boolean } | undefined)?.cleanupPending === true;
    showCloudInstallFeedback(cloudGame, "notice", cleanupPending
      ? "安装已提交，但旧文件清理尚未完成。请重启 GameSaver 完成恢复后再操作该游戏。"
      : "安装完成，游戏已加入游戏库。可以从游戏库手动启动。");
    invalidateStoreCache();
    await loadGames(true);
    if (activePage.value === "store") await loadStore(false, storePage.value);
    return;
  }
  if (task.status === "failed" || task.status === "cancelled" || task.status === "interrupted") {
    endCloudOperation(generation);
    showCloudInstallFeedback(cloudGame, "error", task.error || task.message);
    await loadGames(true);
    invalidateStoreCache();
    if (activePage.value === "store") await loadStore(false, storePage.value);
    return;
  }
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  cloudInstallTimer = setTimeout(() => void watchCloudInstall(taskId, cloudGame, generation), 700);
}

async function finishAddGame(completedGame?: Game) {
  pendingSetupUid.value = undefined;
  activePage.value = "library";
  activeView.value = "all";
  librarySearch.value = "";
  // 必须 force：添加流程刚改过列表，缓存守卫会把新游戏挡住。
  await loadGames(true);
  if (completedGame && !games.value.some((game) => game.gameUid === completedGame.gameUid)) {
    await loadGames(true);
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
        <button class="nav-item" :class="{ active: activePage === 'library' || activePage === 'storage' }" type="button" @click="activePage = 'library'"><Library :size="18" /><span>游戏库</span></button>
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
        <div class="library-header-actions">
          <button class="secondary-button" type="button" @click="openStorage('library')"><HardDrive :size="16" />存储总览</button>
          <label class="search-box">
            <Search :size="17" />
            <input v-model="librarySearch" type="search" placeholder="搜索游戏" aria-label="搜索游戏" />
          </label>
        </div>
      </header>
      <header v-else-if="activePage === 'store'" class="topbar">
        <div>
          <p class="breadcrumb">GameSaver <span>/</span> 游戏商店</p>
          <h1>游戏商店</h1>
        </div>
        <label class="search-box">
          <Search :size="17" />
          <input v-model="storeSearch" type="search" placeholder="搜索云端游戏" aria-label="搜索云端游戏" />
        </label>
      </header>

      <AddGameWizard v-if="activePage === 'add'" :game-uid="pendingSetupUid" @pending="pendingSetupUid = $event" @back="leaveAddGame" @completed="finishAddGame" />
      <GameDetailPage v-else-if="activePage === 'detail' && selectedGame" :key="selectedGame.gameUid" :game="selectedGame" :cover-url="selectedGame ? coverUrls[selectedGame.gameUid] : ''" :initial-error="selectedGameError" :pending-cover-capture="pendingCoverCapture" @back="activePage = detailBackPage" @settings="activePage = 'settings'" @resume-setup="resumeGameSetup(selectedGame.gameUid)" @find-in-store="findGameInStore(selectedGame)" @refresh="loadGames(true)" @capture-handled="finishPendingCoverCapture" />
      <GameStorePage v-else-if="activePage === 'store'" :games="cloudGames" :pending-store-upload-keys="pendingStoreUploadKeys" :search="storeSearch" :loading="storeLoading" :load-error="storeError" :install-uid="cloudInstallUid" :install-progress="cloudInstallProgress" :install-message="cloudInstallMessage" :install-error="cloudInstallError" :install-notice="cloudInstallNotice" :feedback-game-key="cloudFeedbackGameKey" :page="storePage" :page-size="STORE_PAGE_SIZE" :total-count="storeTotalCount" :total-pages="storeTotalPages" :has-more="storeHasMore" @install="installAndLaunch" @delete-version="deleteCloudVersion" @retry="refreshStore" @refresh="refreshStore" @page-change="changeStorePage" />
      <TransferCenter v-else-if="activePage === 'transfers'" :games="games" :cloud-games="cloudGames" />
      <PlatformSettings v-else-if="activePage === 'settings'" @storage="openStorage('settings')" />
      <LibraryStoragePage v-else-if="activePage === 'storage'" @back="activePage = storageBackPage" @open-game="openStorageGame" />

      <template v-else-if="activePage === 'library'">
      <div class="library-toolbar" role="tablist" aria-label="游戏库视图">
        <button v-for="view in ([['all', '可启动'], ['attention', '需要处理']] as const)" :key="view[0]" class="view-tab" :class="{ active: activeView === view[0] }" type="button" @click="activeView = view[0]">{{ view[1] }}</button>
        <span v-if="filteredGames.length" class="library-count">{{ filteredGames.length }} 个游戏</span>
        <span v-if="refreshing" class="library-refreshing" role="status"><span class="loader loader-inline"></span>正在刷新</span>

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

        <button class="refresh-button" type="button" :disabled="loading" @click="loadGames(true)">{{ loading ? "刷新中" : "刷新" }}</button>
      </div>

      <div v-if="loading && !gamesLoaded" class="state-panel"><span class="loader"></span><strong>正在加载游戏库</strong></div>
      <div v-else-if="error && !gamesLoaded" class="state-panel error-state"><strong>游戏库加载失败</strong><p>{{ error }}</p><button type="button" @click="loadGames(true)">重试</button></div>
      <div v-else>
        <div v-if="refreshingError" class="library-refresh-error" role="alert"><AlertTriangle :size="16" /><span>{{ error }}</span><button type="button" @click="loadGames(true)">重试</button></div>
        <div v-if="!filteredGames.length" class="state-panel empty-state"><div class="empty-icon"><Gamepad2 :size="28" /></div><strong>{{ libraryEmptyTitle }}</strong><p>{{ libraryEmptyDescription }}</p><button v-if="!games.length || activeView === 'all'" class="primary-button" type="button" @click="openAddGame"><Plus :size="17" /> 添加游戏</button><button v-else-if="activeView === 'attention' && readyGameCount > 0" type="button" @click="activeView = 'all'">查看可启动游戏</button></div>
        <div v-else class="game-grid">
        <article v-for="game in pagedGames" :key="game.gameUid" class="game-card" tabindex="0" @click="openGame(game)" @keyup.enter="openGame(game)">
          <div class="game-card-cover">
            <img v-if="coverUrls[game.gameUid]" :src="coverUrls[game.gameUid]" :alt="`${game.displayName} 封面`" loading="lazy" @error="delete coverUrls[game.gameUid]" />
            <div v-else class="cover-placeholder"><Gamepad2 :size="34" /></div>
          </div>
          <div class="game-card-body">
            <div>
              <h2>{{ game.displayName }}</h2>
              <div class="game-card-status">
                <span class="status-label">{{ gameStatusLabel(game) }}</span>
                <span v-if="game.lifecycle === 'active' && !game.saveProfileId" class="status-label status-label-unprotected">未启用存档保护</span>
                <span v-if="needsStoreUpload(game)" class="store-upload-badge">待上传商店</span>
                <span v-if="unsyncedGameUids.has(game.gameUid)" class="status-label status-label-warn" title="最近一次自动同步存档失败，可在传输中心重试">存档未同步</span>
              </div>
            </div>
            <button v-if="activeView === 'attention' && !runningGameUids.has(game.gameUid)" class="launch-button" type="button" @click.stop="openGame(game)" @keyup.enter.stop>处理</button>
            <button v-else class="launch-button" type="button" :disabled="runningGameUids.has(game.gameUid)" @click.stop="quickLaunch(game)" @keyup.enter.stop>{{ runningGameUids.has(game.gameUid) ? "运行中" : "启动" }}</button>
          </div>
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
      </div>
      </template>
    </section>
  </main>
</template>

<style scoped>
.library-header-actions { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; }
</style>
