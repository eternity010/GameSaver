<script setup lang="ts">
import { computed } from "vue";
import { ChevronLeft, ChevronRight, CloudDownload, CloudUpload, Gamepad2, RefreshCw } from "@lucide/vue";
import type { CloudGameSummary } from "../api";

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
  install: [game: CloudGameSummary];
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

function packageSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function downloadLabel(game: CloudGameSummary): string {
  if (game.installed) return "已在游戏库";
  return props.installUid === game.gameUid ? "安装中" : "下载并安装";
}

function createdLabel(value?: string): string {
  if (!value) return "时间未知";
  const timestamp = Number(value);
  if (!Number.isFinite(timestamp)) return value;
  return new Date(timestamp).toLocaleDateString("zh-CN");
}
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
      <article v-for="game in visibleGames" :key="game.gameKey || game.gameUid" class="store-game-card">
        <div class="store-cover"><Gamepad2 :size="38" /><span class="store-cover-type">ZIP 本体包</span><span class="store-cover-size">{{ packageSize(game.packageSize) }}</span></div>
        <div class="store-game-body">
          <div class="store-game-heading"><div><h2>{{ game.displayName }}</h2><p>{{ game.versionId }}</p></div><span class="store-status-label" :class="{ installed: game.installed }">{{ game.installed ? "已在游戏库" : "可下载" }}</span></div>
          <div class="store-game-meta"><span>{{ game.fileCount?.toLocaleString() || "未知" }} 个文件</span><span>安装后 {{ game.totalBytes ? packageSize(game.totalBytes) : "未知" }}</span></div>
          <div class="store-action-row"><button class="primary-button store-download-button" type="button" :disabled="!!installUid || game.installed" @click="emit('install', game)"><CloudDownload :size="16" />{{ downloadLabel(game) }}</button></div>
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
</template>
