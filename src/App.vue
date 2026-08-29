<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { CloudUpload, Gamepad2, Library, Plus, Settings, Search } from "@lucide/vue";
import { launchGame, listGames } from "./api";
import { gameStatusLabel, type Game } from "./domain/game";
import AddGameWizard from "./components/AddGameWizard.vue";
import GameDetailPage from "./components/GameDetailPage.vue";

type LibraryView = "all" | "recent" | "favorites" | "attention";
type AppPage = "library" | "add" | "detail";

const games = ref<Game[]>([]);
const activePage = ref<AppPage>("library");
const activeView = ref<LibraryView>("all");
const search = ref("");
const loading = ref(true);
const error = ref("");
const selectedGame = ref<Game | null>(null);
const selectedGameError = ref("");

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

const pageTitle = computed(() => activeView.value === "all" ? "游戏库" : activeView.value === "recent" ? "最近游玩" : activeView.value === "favorites" ? "收藏" : "需要处理");

async function loadGames() {
  loading.value = true;
  error.value = "";
  try {
    games.value = await listGames();
    if (selectedGame.value) {
      selectedGame.value = games.value.find((game) => game.gameUid === selectedGame.value?.gameUid) || selectedGame.value;
    }
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

onMounted(() => void loadGames());

function openAddGame() {
  activePage.value = "add";
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

async function finishAddGame() {
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
        <button class="nav-item" :class="{ active: activePage === 'add' }" type="button" @click="openAddGame"><Plus :size="18" /><span>添加游戏</span></button>
        <button class="nav-item disabled-nav" type="button" disabled title="传输中心将在本体版本阶段开放"><CloudUpload :size="18" /><span>传输中心</span></button>
      </nav>
      <div class="sidebar-bottom">
        <button class="nav-item disabled-nav" type="button" disabled title="平台设置将在后续阶段开放"><Settings :size="18" /><span>GameSaver 设置</span></button>
        <span class="local-status"><i></i> 本地优先</span>
      </div>
    </aside>

    <section class="content-area">
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

      <AddGameWizard v-if="activePage === 'add'" @back="activePage = 'library'" @completed="finishAddGame" />
      <GameDetailPage v-else-if="activePage === 'detail' && selectedGame" :game="selectedGame" :initial-error="selectedGameError" @back="activePage = 'library'" @refresh="loadGames" />

      <template v-else-if="activePage === 'library'">
      <div class="library-toolbar" role="tablist" aria-label="游戏库视图">
        <button v-for="view in ([['all', '全部游戏'], ['recent', '最近游玩'], ['favorites', '收藏'], ['attention', '需要处理']] as const)" :key="view[0]" class="view-tab" :class="{ active: activeView === view[0] }" type="button" @click="activeView = view[0]">{{ view[1] }}</button>
        <button class="refresh-button" type="button" @click="loadGames">刷新</button>
      </div>

      <div v-if="loading" class="state-panel"><span class="loader"></span><strong>正在加载游戏库</strong></div>
      <div v-else-if="error" class="state-panel error-state"><strong>游戏库加载失败</strong><p>{{ error }}</p><button type="button" @click="loadGames">重试</button></div>
      <div v-else-if="!filteredGames.length" class="state-panel empty-state"><div class="empty-icon"><Gamepad2 :size="28" /></div><strong>{{ games.length ? "没有匹配的游戏" : "还没有加入游戏" }}</strong><p>{{ games.length ? "调整搜索或筛选条件。" : "添加游戏本体后，它会出现在这里。" }}</p><button class="primary-button" type="button" @click="openAddGame"><Plus :size="17" /> 添加游戏</button></div>
      <div v-else class="game-grid">
        <article v-for="game in filteredGames" :key="game.gameUid" class="game-card" tabindex="0" @click="openGame(game)" @keyup.enter="openGame(game)">
          <div class="cover-placeholder"><Gamepad2 :size="34" /></div>
          <div class="game-card-body"><div><h2>{{ game.displayName }}</h2><span class="status-label">{{ gameStatusLabel(game) }}</span></div><button class="launch-button" type="button" :disabled="game.lifecycle !== 'active'" @click.stop="quickLaunch(game)">启动</button></div>
        </article>
      </div>
      </template>
    </section>
  </main>
</template>
