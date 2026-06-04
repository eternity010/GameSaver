<script setup lang="ts">
import type { LibraryGameProductStatus } from "../../composables/useLibraryPage";
import type { GameLibraryItem } from "../../types";

defineProps<{
  item: GameLibraryItem;
  selected: boolean;
  warning: boolean;
  cardError: string;
  productStatus: LibraryGameProductStatus;
}>();

const emit = defineEmits<{
  (e: "select", gameId: string): void;
}>();
</script>

<template>
  <article
    class="panel game-card"
    :class="{ selected, warning }"
    @click="emit('select', item.gameId)"
  >
    <p v-if="cardError" class="error inline-error card-error">
      {{ cardError }}
    </p>
    <div class="library-game-row">
      <span class="game-status-dot" :class="productStatus.tone === 'ready' ? 'ready' : 'missing'"></span>
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
        </div>
      </div>
      <span v-if="item.lastSessionStatus" class="session-mini">{{ item.lastSessionStatus }}</span>
    </div>
  </article>
</template>
