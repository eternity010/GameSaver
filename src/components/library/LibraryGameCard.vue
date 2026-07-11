<script setup lang="ts">
import type { LibraryGameProductStatus } from "../../composables/useLibraryPage";
import type { GameLibraryItem } from "../../types";
import { BookOpenCheck, Clock3, Play, Settings2, SlidersHorizontal } from "@lucide/vue";

const props = defineProps<{
  item: GameLibraryItem;
  iconUrl: string;
  selected: boolean;
  warning: boolean;
  cardError: string;
  productStatus: LibraryGameProductStatus;
  busy: boolean;
}>();

const emit = defineEmits<{
  (e: "select", gameId: string): void;
  (e: "primary-action", payload: { gameId: string; action: LibraryGameProductStatus["action"] }): void;
}>();

function actionLabel(): string {
  switch (props.productStatus.action) {
    case "launch":
      return "启动";
    case "bind_exe":
      return "设置";
    case "enable_rule":
      return "启用";
    case "learn":
      return "添加";
    default:
      return "处理中";
  }
}

function gameInitial(): string {
  return props.item.gameId.trim().charAt(0).toUpperCase() || "G";
}

function lastPlayedLabel(): string {
  const timestamp = Number(props.item.lastSessionUpdatedAt || "0");
  if (!Number.isFinite(timestamp) || timestamp <= 0) {
    return "尚未游玩";
  }
  return `最近 ${new Date(timestamp * 1000).toLocaleDateString()}`;
}
</script>

<template>
  <article
    class="game-card"
    :class="{ selected, warning }"
    @click="emit('select', item.gameId)"
  >
    <p v-if="cardError" class="error inline-error card-error">
      {{ cardError }}
    </p>
    <div class="library-game-row">
      <div class="game-art" aria-hidden="true">
        <img v-if="iconUrl" :src="iconUrl" alt="" />
        <span v-else>{{ gameInitial() }}</span>
        <i class="game-status-dot" :class="productStatus.tone === 'ready' ? 'ready' : 'missing'"></i>
      </div>
      <div class="library-game-main">
        <h3>{{ item.gameId }}</h3>
        <p>{{ productStatus.description }}</p>
        <div class="library-sync-row">
          <span
            class="precheck-state-pill library-sync-pill"
            :class="productStatus.tone === 'ready' ? 'ok' : productStatus.tone === 'paused' ? 'fail' : 'warn'"
          >
            {{ productStatus.label }}
          </span>
          <span class="last-played"><Clock3 :size="12" />{{ lastPlayedLabel() }}</span>
        </div>
      </div>
      <span v-if="item.lastSessionStatus" class="session-mini">{{ item.lastSessionStatus }}</span>
      <button
        v-if="productStatus.action !== 'wait'"
        type="button"
        class="game-card-action"
        :class="{ primary: productStatus.action === 'launch' }"
        :disabled="busy"
        :title="productStatus.actionHint"
        @click.stop="emit('primary-action', { gameId: item.gameId, action: productStatus.action })"
      >
        <Play v-if="productStatus.action === 'launch'" :size="16" fill="currentColor" />
        <Settings2 v-else-if="productStatus.action === 'bind_exe'" :size="16" />
        <SlidersHorizontal v-else-if="productStatus.action === 'enable_rule'" :size="16" />
        <BookOpenCheck v-else :size="16" />
        <span>{{ busy ? "处理中" : actionLabel() }}</span>
      </button>
    </div>
  </article>
</template>
