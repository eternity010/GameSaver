<script setup lang="ts">
import type { BackupStatsResult, GameLibraryItem } from "../../types";

defineProps<{
  item: GameLibraryItem;
  selected: boolean;
  warning: boolean;
  cardError: string;
  syncStatusLabel: string;
  syncStatusClass: string;
  gameDirStatusLabel: string;
  backupStats: BackupStatsResult | null;
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
      <span class="game-status-dot" :class="item.preferredExePath ? 'ready' : 'missing'"></span>
      <div class="library-game-main">
        <h3>{{ item.gameId }}</h3>
        <p>
          <span>规则 {{ item.enabledRules }}/{{ item.totalRules }}</span>
          <span v-if="backupStats">
            备份 {{ backupStats.versionCount ?? 0 }} 版
          </span>
          <span v-else>备份读取中</span>
        </p>
        <div v-if="syncStatusLabel" class="library-sync-row">
          <span
            class="precheck-state-pill library-sync-pill"
            :class="syncStatusClass"
          >
            {{ syncStatusLabel }}
          </span>
        </div>
        <p v-if="gameDirStatusLabel" class="library-warning-text">
          {{ gameDirStatusLabel }}
        </p>
      </div>
      <span v-if="item.lastSessionStatus" class="session-mini">{{ item.lastSessionStatus }}</span>
    </div>
  </article>
</template>
