<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { AlertTriangle, ChevronLeft, ChevronRight, CloudDownload, CloudUpload, Gamepad2, Library, Plus, Settings, Search, ShieldCheck } from "@lucide/vue";
import { deleteRemoteBodyPackage, getElevationStatus, getGameCoverUrl, getTask, installCloudGame, launchGame, listCloudGames, listGames, listTasks, restartAsAdmin } from "./api";
import type { AppTask, ElevationStatus } from "./api";
import type { CloudGameSummary, CloudGameVersion } from "./api";
import { gameStatusLabel, type Game } from "./domain/game";
import AddGameWizard from "./components/AddGameWizard.vue";
import GameDetailPage from "./components/GameDetailPage.vue";
import GameStorePage from "./components/GameStorePage.vue";
import TransferCenter from "./components/TransferCenter.vue";
import PlatformSettings from "./components/PlatformSettings.vue";

type LibraryView = "all" | "recent" | "favorites" | "attention";
type AppPage = "library" | "store" | "add" | "detail" | "transfers" | "settings";

const games = ref<Game[]>([]);
const cloudGames = ref<CloudGameSummary[]>([]);
const coverUrls = ref<Record<string, string>>({});
const activePage = ref<AppPage>("library");
const activeView = ref<LibraryView>("all");
const search = ref("");
const loading = ref(true);
const error = ref("");
const selectedGame = ref<Game | null>(null);
const selectedGameError = ref("");
const cloudInstallUid = ref("");
const cloudInstallProgress = ref(0);
const cloudInstallMessage = ref("");
const cloudInstallError = ref("");
const cloudInstallNotice = ref("");
const storeLoading = ref(false);
const storeError = ref("");
const storeLoaded = ref(false);
const storePage = ref(1);
const storeHasMore = ref(false);
const STORE_PAGE_SIZE = 9;
const LIBRARY_PAGE_SIZE = 9;
const libraryPage = ref(1);
const elevationStatus = ref<ElevationStatus | null>(null);
const elevationLoading = ref(false);
const elevationError = ref("");
const activeTransferCount = ref(0);
let cloudInstallTimer: ReturnType<typeof setTimeout> | undefined;
let transferCountTimer: ReturnType<typeof setTimeout> | undefined;

const filteredGames = computed(() => {
  const keyword = search.value.trim().toLocaleLowerCase();
  const result = games.value.filter((game) => {
    if (keyword && !game.displayName.toLocaleLowerCase().includes(keyword)) return false;
    if (activeView.value === "attention") return game.health !== "ready";
    return true;
  });
  if (activeView.value === "recent") {
    return [...result].sort((left, right) => (right.lastPlayedAt || "").localeCompare(left.lastPlayedAt || ""));
  }
  return result;
});

const libraryPageCount = computed(() => Math.max(1, Math.ceil(filteredGames.value.length / LIBRARY_PAGE_SIZE)));
const pagedGames = computed(() => {
  const page = Math.min(libraryPage.value, libraryPageCount.value);
  const start = (page - 1) * LIBRARY_PAGE_SIZE;
  return filteredGames.value.slice(start, start + LIBRARY_PAGE_SIZE);
});

const pageTitle = computed(() => activeView.value === "all" ? "游戏库" : activeView.value === "recent" ? "最近游玩" : activeView.value === "favorites" ? "收藏" : "需要处理");

watch([search, activeView], () => {
  libraryPage.value = 1;
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
  loading.value = true;
  error.value = "";
  try {
    const loaded = await listGames();
    games.value = loaded;
    libraryPage.value = Math.min(libraryPage.value, Math.max(1, Math.ceil(loaded.length / LIBRARY_PAGE_SIZE)));
    if (selectedGame.value) {
      selectedGame.value = loaded.find((game) => game.gameUid === selectedGame.value?.gameUid) || selectedGame.value;
    }
    void loadGameCovers(loaded);
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

async function loadStore(force = false, page = 1) {
  if (storeLoading.value || (!force && storeLoaded.value && page === storePage.value)) return;
  storeLoading.value = true;
  storeError.value = "";
  try {
    const result = await listCloudGames(page, STORE_PAGE_SIZE);
    cloudGames.value = result.games;
    storePage.value = result.page;
    storeHasMore.value = result.hasMore;
    storeLoaded.value = true;
  } catch (reason) {
    if (page === 1) {
      cloudGames.value = [];
      storeHasMore.value = false;
    }
    storeError.value = String(reason);
  } finally {
    storeLoading.value = false;
  }
}

function refreshStore() {
  void loadStore(true, 1);
}

function changeStorePage(page: number) {
  if (page < 1 || (page > storePage.value && !storeHasMore.value)) return;
  void loadStore(true, page);
}

function changeLibraryPage(page: number) {
  if (page < 1 || page > libraryPageCount.value) return;
  libraryPage.value = page;
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

async function updateTransferCount() {
  try {
    const tasks = await listTasks();
    const isTransfer = (task: AppTask) =>
      task.taskType === "upload_game_body_package" ||
      task.taskType === "download_game_body_package" ||
      task.taskType === "install_cloud_game" ||
      task.taskType === "delete_remote_body_package" ||
      task.taskType === "repair_cloud_body_manifest";
    const active = tasks.filter((task) => isTransfer(task) && (task.status === "pending" || task.status === "running")).length;
    activeTransferCount.value = active;
    if (transferCountTimer) clearTimeout(transferCountTimer);
    transferCountTimer = setTimeout(() => void updateTransferCount(), active > 0 ? 1000 : 3500);
  } catch {
    if (transferCountTimer) clearTimeout(transferCountTimer);
    transferCountTimer = setTimeout(() => void updateTransferCount(), 5000);
  }
}

onMounted(() => {
  void loadGames();
  void loadElevationStatus();
  void loadStore();
  void updateTransferCount();
});
onUnmounted(() => {
  if (cloudInstallTimer) clearTimeout(cloudInstallTimer);
  if (transferCountTimer) clearTimeout(transferCountTimer);
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
  selectedGame.value = game;
  selectedGameError.value = detailError;
  activePage.value = "detail";
}

async function quickLaunch(game: Game) {
  if (game.lifecycle !== "active") {
    openGame(game);
    return;
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

async function finishAddGame() {
  activePage.value = "library";
  activeView.value = "all";
  search.value = "";
  await loadGames();
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
      <GameDetailPage v-else-if="activePage === 'detail' && selectedGame" :game="selectedGame" :cover-url="selectedGame ? coverUrls[selectedGame.gameUid] : ''" :initial-error="selectedGameError" @back="activePage = 'library'" @settings="activePage = 'settings'" @refresh="loadGames" />
      <GameStorePage v-else-if="activePage === 'store'" :games="cloudGames" :search="search" :loading="storeLoading" :load-error="storeError" :install-uid="cloudInstallUid" :install-progress="cloudInstallProgress" :install-message="cloudInstallMessage" :install-error="cloudInstallError" :install-notice="cloudInstallNotice" :page="storePage" :has-more="storeHasMore" @install="installAndLaunch" @delete-version="deleteCloudVersion" @retry="refreshStore" @refresh="refreshStore" @page-change="changeStorePage" />
      <TransferCenter v-else-if="activePage === 'transfers'" :games="games" :cloud-games="cloudGames" />
      <PlatformSettings v-else-if="activePage === 'settings'" />

      <template v-else-if="activePage === 'library'">
      <div class="library-toolbar" role="tablist" aria-label="游戏库视图">
        <button v-for="view in ([['all', '全部游戏'], ['recent', '最近游玩'], ['favorites', '收藏'], ['attention', '需要处理']] as const)" :key="view[0]" class="view-tab" :class="{ active: activeView === view[0] }" type="button" @click="activeView = view[0]">{{ view[1] }}</button>
        <span v-if="filteredGames.length" class="library-count">{{ filteredGames.length }} 个游戏</span>
        <button class="refresh-button" type="button" @click="loadGames">刷新</button>
      </div>

      <div v-if="loading" class="state-panel"><span class="loader"></span><strong>正在加载游戏库</strong></div>
      <div v-else-if="error" class="state-panel error-state"><strong>游戏库加载失败</strong><p>{{ error }}</p><button type="button" @click="loadGames">重试</button></div>
      <div v-else-if="!filteredGames.length" class="state-panel empty-state"><div class="empty-icon"><Gamepad2 :size="28" /></div><strong>{{ games.length ? "没有匹配的游戏" : "还没有加入游戏" }}</strong><p>{{ games.length ? "调整搜索或筛选条件。" : "添加游戏本体后，它会出现在这里。" }}</p><button class="primary-button" type="button" @click="openAddGame"><Plus :size="17" /> 添加游戏</button></div>
      <div v-else class="game-grid">
        <article v-for="game in pagedGames" :key="game.gameUid" class="game-card" tabindex="0" @click="openGame(game)" @keyup.enter="openGame(game)">
          <div class="game-card-cover">
            <img v-if="coverUrls[game.gameUid]" :src="coverUrls[game.gameUid]" :alt="`${game.displayName} 封面`" loading="lazy" @error="delete coverUrls[game.gameUid]" />
            <div v-else class="cover-placeholder"><Gamepad2 :size="34" /></div>
          </div>
          <div class="game-card-body"><div><h2>{{ game.displayName }}</h2><span class="status-label">{{ gameStatusLabel(game) }}</span></div><button class="launch-button" type="button" :disabled="game.lifecycle !== 'active'" @click.stop="quickLaunch(game)">启动</button></div>
        </article>
      </div>
      <nav v-if="filteredGames.length" class="library-pagination" aria-label="游戏库分页">
        <button class="icon-button" type="button" :disabled="libraryPage <= 1" title="上一页" aria-label="上一页" @click="changeLibraryPage(libraryPage - 1)"><ChevronLeft :size="18" /></button>
        <span>第 {{ libraryPage }} / {{ libraryPageCount }} 页</span>
        <button class="icon-button" type="button" :disabled="libraryPage >= libraryPageCount" title="下一页" aria-label="下一页" @click="changeLibraryPage(libraryPage + 1)"><ChevronRight :size="18" /></button>
      </nav>
      </template>
    </section>
  </main>
</template>
