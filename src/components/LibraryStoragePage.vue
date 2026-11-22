<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { ArrowLeft, ChevronRight, HardDrive, RefreshCw } from "@lucide/vue";
import { getLibraryStorageOverview, type LibraryStorageOverview } from "../api";

const emit = defineEmits<{ back: []; openGame: [gameUid: string] }>();
const overview = ref<LibraryStorageOverview | null>(null);
const loading = ref(false);
const error = ref("");
const measuredAt = ref("");
const categories = [
  { key: "gameBytes", label: "游戏本体", color: "#78aaff" },
  { key: "packageBytes", label: "本地 ZIP", color: "#b498f9" },
  { key: "saveBytes", label: "独占存档", color: "#56cbb6" },
] as const;
const rows = computed(() => (overview.value?.games ?? [])
  .map((game) => ({ ...game, total: game.gameBytes + game.packageBytes + game.saveBytes }))
  .sort((left, right) => right.total - left.total || left.displayName.localeCompare(right.displayName)));
const scale = computed(() => Math.max(1, rows.value[0]?.total ?? 0));
const formatter = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 1 });

function sizeLabel(bytes: number | null): string {
  if (bytes === null) return "未知";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (bytes >= 1024 && unit < units.length - 1) {
    bytes /= 1024;
    unit++;
  }
  return formatter.format(bytes) + " " + units[unit];
}

async function refresh() {
  if (loading.value) return;
  loading.value = true;
  error.value = "";
  try {
    overview.value = await getLibraryStorageOverview();
    measuredAt.value = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  } catch (reason) {
    error.value = String(reason);
  } finally {
    loading.value = false;
  }
}

onMounted(refresh);
</script>

<template>
  <section class="storage-page" aria-labelledby="storage-heading">
    <header class="storage-header">
      <div>
        <button class="storage-back" type="button" @click="emit('back')"><ArrowLeft :size="16" /> 返回</button>
        <h1 id="storage-heading">存储总览</h1>
        <p>快速找出占用空间最多的游戏。</p>
      </div>
      <button class="secondary-button" type="button" :disabled="loading" @click="refresh">
        <RefreshCw :size="16" :class="{ spin: loading }" />{{ loading ? "统计中" : "刷新统计" }}
      </button>
    </header>

    <div v-if="error" class="storage-message" role="alert">
      <strong>{{ overview ? "刷新失败，下方仍显示上次结果" : "无法统计游戏库" }}</strong>
      <span>{{ error }}</span>
    </div>
    <div v-else-if="loading && !overview" class="storage-message" role="status">
      <span class="loader"></span><span>正在读取文件大小，不会修改游戏或存档。</span>
    </div>

    <template v-if="overview">
      <div class="storage-location"><HardDrive :size="16" /><span :title="overview.libraryRoot">{{ overview.libraryRoot }}</span><small v-if="measuredAt">统计于 {{ measuredAt }}</small></div>
      <dl class="storage-summary">
        <div><dt>游戏库总占用</dt><dd>{{ sizeLabel(overview.totalBytes) }}</dd></div>
        <div><dt>所在磁盘剩余空间</dt><dd>{{ sizeLabel(overview.freeBytes) }}</dd></div>
        <div><dt>跨游戏共享存档</dt><dd>{{ sizeLabel(overview.sharedSaveBytes) }}</dd></div>
        <div><dt>其他受管文件</dt><dd>{{ sizeLabel(overview.otherBytes) }}</dd></div>
      </dl>

      <div class="storage-list">
        <div class="storage-list-heading">
          <div><h2>各游戏占用</h2><p>按占用从大到小排列 · 点击游戏查看详情</p></div>
          <div class="storage-legend">
            <span v-for="category in categories" :key="category.key"><i :style="{ backgroundColor: category.color }"></i>{{ category.label }}</span>
          </div>
        </div>
        <p v-if="!rows.length" class="storage-empty">游戏库还没有游戏。</p>
        <ul v-else class="storage-rows">
          <li v-for="row in rows" :key="row.gameUid">
            <button class="storage-row" type="button" @click="emit('openGame', row.gameUid)">
              <span class="storage-row-main">
                <strong>{{ row.displayName }}</strong>
                <span class="storage-bar" aria-hidden="true">
                  <i v-for="category in categories" :key="category.key"
                    :style="{ width: (row[category.key] / scale * 100) + '%', backgroundColor: category.color }"></i>
                </span>
                <span class="storage-breakdown">
                  <span v-for="category in categories" :key="category.key">{{ category.label }} <b>{{ sizeLabel(row[category.key]) }}</b></span>
                </span>
              </span>
              <strong class="storage-row-total">{{ sizeLabel(row.total) }}</strong>
              <ChevronRight :size="16" aria-hidden="true" />
            </button>
          </li>
        </ul>
      </div>
      <p class="storage-footnote">按游戏库中的文件大小统计，不含网盘文件和游戏库以外的原始存档。共享存档只计一次；「其他」包括封面、临时文件和无法归属的文件。</p>
    </template>
  </section>
</template>

<style scoped>
.storage-page { width: 100%; max-width: 1320px; margin: 0 auto; display: grid; gap: 16px; min-width: 0; }
.storage-header, .storage-list-heading { display: flex; justify-content: space-between; align-items: end; gap: 14px; flex-wrap: wrap; }
.storage-header h1, .storage-list-heading h2 { margin: 0; }
.storage-header h1 { font-size: 30px; }
.storage-list-heading h2 { font-size: 18px; }
.storage-header p, .storage-list-heading p { margin: 5px 0 0; color: var(--text-secondary); font-size: 13px; }
.storage-back { display: inline-flex; align-items: center; gap: 6px; color: var(--text-secondary); background: transparent; border: 0; padding: 0; margin-bottom: 8px; }
.storage-back:hover { color: var(--text-primary); }
.storage-location { display: flex; align-items: center; gap: 8px; color: var(--text-secondary); font-size: 13px; }
.storage-location span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.storage-location small { margin-left: auto; white-space: nowrap; }
.storage-summary { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 10px; margin: 0; }
.storage-summary > div, .storage-list, .storage-message { padding: 15px; background: var(--surface-raised); border: 1px solid #343d4b; border-radius: 9px; }
.storage-summary dt { color: var(--text-secondary); font-size: 12px; }
.storage-summary dd { margin: 6px 0 0; font-size: 23px; font-weight: 700; }
.storage-list-heading { padding-bottom: 13px; }
.storage-legend, .storage-breakdown { display: flex; flex-wrap: wrap; gap: 6px 14px; color: var(--text-secondary); font-size: 12px; }
.storage-legend span { display: inline-flex; align-items: center; gap: 6px; }
.storage-legend i { display: inline-block; width: 8px; height: 8px; border-radius: 2px; }
.storage-rows { list-style: none; margin: 0; padding: 0; }
.storage-rows li { border-top: 1px solid #343d4b; }
.storage-row { background: transparent; border: 0; width: 100%; color: var(--text-primary); display: grid; grid-template-columns: minmax(0, 1fr) auto auto; align-items: center; gap: 12px; padding: 12px 5px; text-align: left; }
.storage-row:hover { background: #273244; }
.storage-row-main { min-width: 0; display: grid; gap: 7px; }
.storage-row-main strong { overflow-wrap: anywhere; }
.storage-bar { display: flex; width: 100%; height: 6px; background: #293345; border-radius: 4px; overflow: hidden; }
.storage-row-total { white-space: nowrap; font-variant-numeric: tabular-nums; }
.storage-footnote, .storage-empty { margin: 0; color: var(--text-secondary); font-size: 12px; }
.storage-message { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
</style>
