<script setup lang="ts">
import { computed } from "vue";
import { CloudDownload, CloudUpload, Gamepad2 } from "@lucide/vue";
import type { CloudGameSummary } from "../api";

const props = defineProps<{
  games: CloudGameSummary[];
  search: string;
  installUid: string;
  installProgress: number;
  installMessage: string;
  installError: string;
}>();

const emit = defineEmits<{ install: [game: CloudGameSummary] }>();

const availableGames = computed(() => {
  const keyword = props.search.trim().toLocaleLowerCase();
  return props.games.filter((game) => {
    if (game.installed) return false;
    return !keyword || game.displayName.toLocaleLowerCase().includes(keyword);
  });
});

function packageSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function downloadLabel(game: CloudGameSummary): string {
  return props.installUid === game.gameUid ? "安装中" : "下载并安装";
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
      <div class="store-count"><CloudUpload :size="18" /><span>{{ availableGames.length }} 个可下载游戏</span></div>
    </header>

    <div v-if="!availableGames.length" class="state-panel empty-state store-empty">
      <div class="empty-icon"><Gamepad2 :size="28" /></div>
      <strong>{{ games.length ? "没有匹配的云端游戏" : "还没有可下载的游戏" }}</strong>
      <p>{{ games.length ? "调整搜索关键词后重试。" : "完成百度网盘授权并上传游戏本体后，云端游戏会显示在这里。" }}</p>
    </div>

    <div v-else class="store-grid">
      <article v-for="game in availableGames" :key="game.gameUid" class="store-game-card">
        <div class="store-cover"><CloudUpload :size="38" /></div>
        <div class="store-game-body">
          <div class="store-game-heading">
            <div>
              <h2>{{ game.displayName }}</h2>
              <p>云端版本 {{ game.versionId }}</p>
            </div>
            <span class="store-size">{{ packageSize(game.packageSize) }}</span>
          </div>
          <dl class="store-facts">
            <div><dt>本体包</dt><dd>ZIP</dd></div>
            <div><dt>包含文件</dt><dd>{{ game.fileCount?.toLocaleString() || "未知" }}</dd></div>
            <div><dt>安装后大小</dt><dd>{{ game.totalBytes ? packageSize(game.totalBytes) : "未知" }}</dd></div>
          </dl>
          <div class="store-action-row">
            <button class="primary-button store-download-button" type="button" :disabled="!!installUid" @click="emit('install', game)">
              <CloudDownload :size="17" />{{ downloadLabel(game) }}
            </button>
          </div>
          <div v-if="installUid === game.gameUid" class="store-install-progress">
            <div class="task-progress-heading"><span>{{ installMessage }}</span><strong>{{ installProgress }}%</strong></div>
            <div class="progress-track"><span :style="{ width: `${installProgress}%` }"></span></div>
          </div>
        </div>
      </article>
    </div>
    <p v-if="installError" class="error-message store-install-error" role="alert">{{ installError }}</p>
  </section>
</template>
