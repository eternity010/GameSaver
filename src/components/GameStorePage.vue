<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { ChevronLeft, ChevronRight, CloudDownload, CloudUpload, Gamepad2, RefreshCw, Trash2, X } from "@lucide/vue";
import type { CloudGameSummary, CloudGameVersion } from "../api";

const props = defineProps<{
  games: CloudGameSummary[];
  search: string;
  loading: boolean;
  loadError: string;
  installUid: string;
  installProgress: number;
  installMessage: string;
  installError: string;
  installNotice: string;
  page: number;
  hasMore: boolean;
}>();

const emit = defineEmits<{
  install: [game: CloudGameSummary, version: CloudGameVersion];
  deleteVersion: [game: CloudGameSummary, version: CloudGameVersion];
  retry: [];
  refresh: [];
  pageChange: [page: number];
}>();

const visibleGames = computed(() => {
  const keyword = props.search.trim().toLocaleLowerCase();
  return props.games.filter((game) => {
    return !keyword || game.displayName.toLocaleLowerCase().includes(keyword);
  });
});

const selectedGame = ref<CloudGameSummary | null>(null);
const selectedVersion = ref<CloudGameVersion | null>(null);

function cloudGameKey(game: CloudGameSummary): string {
  return game.gameKey || game.gameUid;
}

function openDetail(game: CloudGameSummary) {
  selectedGame.value = game;
  selectedVersion.value = latestVersion(game);
}

function closeDetail() {
  selectedGame.value = null;
  selectedVersion.value = null;
}

function selectVersion(version: CloudGameVersion) {
  selectedVersion.value = version;
}

function latestVersion(game: CloudGameSummary): CloudGameVersion | null {
  return game.versions.find((version) => version.path === game.packagePath && version.fsId === game.packageFsId) || game.versions[0] || null;
}

function versionSelected(version: CloudGameVersion): boolean {
  return selectedVersion.value?.fsId === version.fsId;
}

function packageSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function downloadLabel(game: CloudGameSummary): string {
  if (game.installed) return "已在游戏库";
  return props.installUid === game.gameUid ? "安装中" : "下载并安装";
}

function versionStatus(version: CloudGameVersion): string {
  if (version.syncState === "synced") return "可下载";
  if (version.syncState === "mismatch") return "需要检查";
  return "待确认";
}

function createdLabel(value?: string): string {
  if (!value) return "时间未知";
  const timestamp = Number(value);
  if (!Number.isFinite(timestamp)) return value;
  return new Date(timestamp).toLocaleDateString("zh-CN");
}

function handleKeydown(event: KeyboardEvent) {
  if (event.key === "Escape" && selectedGame.value) closeDetail();
}

watch(
  () => props.games,
  (games) => {
    const openedGame = selectedGame.value;
    if (!openedGame) return;

    const refreshedGame = games.find((game) => cloudGameKey(game) === cloudGameKey(openedGame));
    if (!refreshedGame) {
      closeDetail();
      return;
    }

    const openedVersion = selectedVersion.value;
    selectedGame.value = refreshedGame;
    selectedVersion.value = openedVersion
      ? refreshedGame.versions.find((version) => version.fsId === openedVersion.fsId) || latestVersion(refreshedGame)
      : latestVersion(refreshedGame);
  },
);

onMounted(() => window.addEventListener("keydown", handleKeydown));
onUnmounted(() => window.removeEventListener("keydown", handleKeydown));
</script>

<template>
  <section class="store-page page-enter">
    <header class="store-header">
      <div>
        <p class="eyebrow">百度网盘游戏库</p>
        <h1>游戏商店</h1>
        <p>从云端下载游戏本体，安装完成后它会出现在游戏库。</p>
      </div>
      <div class="store-header-actions"><div class="store-count"><CloudUpload :size="18" /><span>第 {{ page }} 页 · {{ visibleGames.length }} 个云端游戏</span></div><button class="refresh-button store-refresh-button" type="button" :disabled="loading" title="刷新云端游戏列表" @click="emit('refresh')"><RefreshCw :size="16" :class="{ spin: loading }" />刷新</button></div>
    </header>

    <div v-if="loading" class="state-panel store-empty"><span class="loader"></span><strong>正在读取云端游戏</strong></div>
    <div v-else-if="loadError" class="state-panel error-state store-empty"><strong>云端游戏读取失败</strong><p>{{ loadError }}</p><button type="button" @click="emit('retry')">重试</button></div>
    <div v-else-if="!visibleGames.length" class="state-panel empty-state store-empty">
      <div class="empty-icon"><Gamepad2 :size="28" /></div>
      <strong>{{ games.length ? "没有匹配的云端游戏" : "还没有可下载的游戏" }}</strong>
      <p>{{ games.length ? "调整搜索关键词后重试。" : "完成百度网盘授权并上传游戏本体后，云端游戏会显示在这里。" }}</p>
    </div>

    <div v-else class="store-grid">
      <article v-for="game in visibleGames" :key="game.gameKey || game.gameUid" class="store-game-card" role="button" tabindex="0" :aria-label="`查看 ${game.displayName} 的云端详情`" @click="openDetail(game)" @keydown.enter.prevent="openDetail(game)" @keydown.space.prevent="openDetail(game)">
        <div class="store-cover"><Gamepad2 :size="38" /><span class="store-cover-type">ZIP 本体包</span><span class="store-cover-size">{{ packageSize(game.packageSize) }}</span></div>
        <div class="store-game-body">
          <div class="store-game-heading"><div><h2>{{ game.displayName }}</h2><p>{{ game.versions.length }} 个云端版本</p></div><span class="store-status-label" :class="{ installed: game.installed }">{{ game.installed ? "已在游戏库" : "可下载" }}</span></div>
          <div class="store-game-meta"><span>{{ game.fileCount?.toLocaleString() || "未知" }} 个文件</span><span>安装后 {{ game.totalBytes ? packageSize(game.totalBytes) : "未知" }}</span></div>
          <span class="store-card-detail-hint">查看云端详情</span>
          <div v-if="installUid === game.gameUid" class="store-install-progress"><div class="task-progress-heading"><span>{{ installMessage }}</span><strong>{{ installProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${installProgress}%` }"></span></div></div>
        </div>
      </article>
    </div>
    <p v-if="installNotice" class="notice-message store-install-notice" role="status">{{ installNotice }}</p>
    <p v-if="installError" class="error-message store-install-error" role="alert">{{ installError }}</p>
    <nav v-if="!loading && !loadError" class="store-pagination" aria-label="云端游戏分页">
      <button class="icon-button" type="button" :disabled="page <= 1" title="上一页" aria-label="上一页" @click="emit('pageChange', page - 1)"><ChevronLeft :size="18" /></button>
      <span>第 {{ page }} 页</span>
      <button class="icon-button" type="button" :disabled="!hasMore" title="下一页" aria-label="下一页" @click="emit('pageChange', page + 1)"><ChevronRight :size="18" /></button>
    </nav>
  </section>

  <Teleport to="body">
    <div v-if="selectedGame && selectedVersion" class="store-detail-overlay" @click.self="closeDetail">
      <section class="store-detail-dialog" role="dialog" aria-modal="true" :aria-label="`${selectedGame.displayName} 云端游戏详情`">
        <header class="store-detail-header">
          <div><p class="eyebrow">云端游戏详情</p><h2>{{ selectedGame.displayName }}</h2></div>
          <button class="icon-button" type="button" title="关闭详情" aria-label="关闭详情" @click="closeDetail"><X :size="18" /></button>
        </header>

        <div class="store-detail-content">
          <div class="store-detail-overview">
            <div class="store-detail-cover"><Gamepad2 :size="46" /><span>ZIP 本体包</span></div>
            <div class="store-detail-summary">
              <span class="store-status-label" :class="{ installed: selectedGame.installed }">{{ selectedGame.installed ? "已在游戏库" : "可下载" }}</span>
              <p>{{ selectedGame.installed ? "本地游戏仍由游戏库管理；云端版本可在此查看或删除。" : "选择一个云端版本后下载并安装到游戏库。" }}</p>
              <dl class="store-detail-facts">
                <div><dt>云端版本</dt><dd>{{ selectedGame.versions.length }} 个</dd></div>
                <div><dt>游戏文件</dt><dd>{{ selectedGame.fileCount?.toLocaleString() || "未知" }} 个</dd></div>
                <div><dt>安装大小</dt><dd>{{ selectedGame.totalBytes ? packageSize(selectedGame.totalBytes) : "未知" }}</dd></div>
              </dl>
            </div>
          </div>

          <section class="store-detail-versions" aria-label="云端版本">
            <div class="store-detail-section-heading"><div><h3>云端版本</h3><p>选择要下载的版本；删除只影响百度网盘中的本体包。</p></div><span>{{ selectedGame.versions.length }} 个</span></div>
            <article v-for="version in selectedGame.versions" :key="version.fsId" class="store-version-option" :class="{ selected: versionSelected(version) }" role="button" tabindex="0" :aria-label="`选择云端版本 ${version.versionId}`" @click="selectVersion(version)" @keydown.enter.prevent="selectVersion(version)" @keydown.space.prevent="selectVersion(version)">
              <div class="store-version-copy"><strong>{{ version.versionId }}</strong><span>{{ createdLabel(version.createdAt) }} · {{ packageSize(version.size) }}</span></div>
              <div class="store-version-meta"><span>{{ version.fileCount?.toLocaleString() || "未知" }} 个文件</span><strong>{{ versionStatus(version) }}</strong></div>
              <button class="icon-button danger-button" type="button" :disabled="!!installUid" title="删除这个云端版本" :aria-label="`删除云端版本 ${version.versionId}`" @click.stop="emit('deleteVersion', selectedGame, version)"><Trash2 :size="16" /></button>
            </article>
          </section>

          <div v-if="installUid === selectedGame.gameUid" class="store-detail-progress"><div class="task-progress-heading"><span>{{ installMessage }}</span><strong>{{ installProgress }}%</strong></div><div class="progress-track"><span :style="{ width: `${installProgress}%` }"></span></div></div>
          <p v-if="installError" class="error-message store-detail-message" role="alert">{{ installError }}</p>
          <p v-if="installNotice" class="notice-message store-detail-message" role="status">{{ installNotice }}</p>
        </div>

        <footer class="store-detail-footer">
          <div class="store-detail-selection"><span>已选择</span><strong>{{ selectedVersion.versionId }}</strong><small>{{ packageSize(selectedVersion.size) }}</small></div>
          <button class="primary-button" type="button" :disabled="!!installUid || selectedGame.installed" @click="emit('install', selectedGame, selectedVersion)"><CloudDownload :size="16" />{{ downloadLabel(selectedGame) }}</button>
        </footer>
      </section>
    </div>
  </Teleport>
</template>
