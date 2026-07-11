<script setup lang="ts">
import { computed, ref } from "vue";
import { AlertTriangle, ArrowDownAZ, Clock3, Download, RefreshCw, Search, Upload } from "@lucide/vue";
import type { GameSaveRule, RuleConflictItem } from "../../types";

type RuleDraft = { gameIdText: string; confirmedPathsText: string; enabled: boolean };
type TabState = { loading: boolean; error: string };
type RuleFilter = "all" | "enabled" | "disabled" | "conflict";
type RuleSort = "updated" | "name";

const props = defineProps<{
  rules: GameSaveRule[];
  ruleConflicts: RuleConflictItem[];
  ruleSearch: string;
  ruleDrafts: Record<string, RuleDraft>;
  rulesState: TabState;
}>();

const emit = defineEmits<{
  (e: "update:ruleSearch", value: string): void;
  (e: "update:ruleDraft", payload: { ruleId: string; patch: Partial<RuleDraft> }): void;
  (e: "reload"): void;
  (e: "export-rules"): void;
  (e: "import-rules"): void;
  (e: "open-migration-settings"): void;
  (e: "mark-primary", rule: GameSaveRule): void;
  (e: "save-rule", rule: GameSaveRule): void;
  (e: "toggle-rule", payload: { rule: GameSaveRule; enabled: boolean }): void;
  (e: "remove-rule", rule: GameSaveRule): void;
}>();

const ruleFilter = ref<RuleFilter>("all");
const ruleSort = ref<RuleSort>("updated");

const conflictByRuleId = computed<Record<string, RuleConflictItem>>(() => {
  const output: Record<string, RuleConflictItem> = {};
  for (const conflict of props.ruleConflicts) {
    for (const ruleId of conflict.ruleIds) output[ruleId] = conflict;
  }
  return output;
});

const filteredRules = computed(() => {
  const keyword = props.ruleSearch.trim().toLowerCase();
  const items = props.rules.filter((rule) => {
    if (keyword && !rule.gameId.toLowerCase().includes(keyword)) return false;
    if (ruleFilter.value === "enabled" && !rule.enabled) return false;
    if (ruleFilter.value === "disabled" && rule.enabled) return false;
    if (ruleFilter.value === "conflict" && !conflictByRuleId.value[rule.ruleId]) return false;
    return true;
  });
  return [...items].sort((a, b) => {
    if (ruleSort.value === "name") return a.gameId.localeCompare(b.gameId);
    return Number(b.updatedAt || b.createdAt || "0") - Number(a.updatedAt || a.createdAt || "0");
  });
});

function normalizePaths(rawText: string): string[] {
  return [...new Set(rawText.split(/\r?\n/).map((item) => item.trim()).filter(Boolean))];
}

function hasRuleDraftChanges(rule: GameSaveRule): boolean {
  const draft = props.ruleDrafts[rule.ruleId];
  if (!draft) return false;
  const paths = normalizePaths(draft.confirmedPathsText);
  return draft.gameIdText.trim() !== rule.gameId
    || paths.length !== rule.confirmedPaths.length
    || paths.some((path, index) => path !== rule.confirmedPaths[index]);
}

function formatUnixTs(value: string): string {
  const timestamp = Number(value || "0");
  if (!Number.isFinite(timestamp) || timestamp <= 0) return "未知";
  return new Date(timestamp * 1000).toLocaleString();
}

function shortValue(value: string): string {
  return value.length <= 18 ? value : `${value.slice(0, 8)}...${value.slice(-8)}`;
}

function ruleConflict(ruleId: string): RuleConflictItem | null {
  return conflictByRuleId.value[ruleId] || null;
}

function isPrimary(ruleId: string): boolean {
  return ruleConflict(ruleId)?.primaryRuleId === ruleId;
}

function pathLabel(path: string): string {
  const normalized = path.toUpperCase();
  if (normalized.startsWith("%GAME_DIR%")) return "游戏目录";
  if (normalized.startsWith("%SAVED_GAMES%")) return "Saved Games";
  if (normalized.startsWith("%DOCUMENTS%")) return "文档";
  if (normalized.startsWith("%LOCALLOW%")) return "LocalLow";
  if (normalized.startsWith("%LOCALAPPDATA%")) return "Local";
  if (normalized.startsWith("%APPDATA%")) return "Roaming";
  if (normalized.startsWith("%USERPROFILE%")) return "用户目录";
  return "自定义目录";
}

function updateDraft(ruleId: string, patch: Partial<RuleDraft>) {
  emit("update:ruleDraft", { ruleId, patch });
}
</script>

<template>
  <section class="rules-shell compact-rules-page">
    <header class="rules-header">
      <div class="rules-title-row">
        <div>
          <h1>规则管理</h1>
          <p class="rules-copy">管理游戏的存档保护状态和存档目录。</p>
        </div>
        <button class="icon-button" :disabled="rulesState.loading" type="button" title="刷新规则" @click="emit('reload')">
          <RefreshCw :size="18" :class="{ spinning: rulesState.loading }" />
        </button>
      </div>

      <div class="rules-toolbar compact-rules-toolbar">
        <label class="rules-search compact-search">
          <Search :size="17" />
          <input
            :value="ruleSearch"
            placeholder="搜索游戏"
            aria-label="搜索规则"
            @input="emit('update:ruleSearch', ($event.target as HTMLInputElement).value)"
          />
        </label>
        <div class="rule-filter-control" aria-label="规则状态筛选">
          <button type="button" :class="{ active: ruleFilter === 'all' }" @click="ruleFilter = 'all'">全部</button>
          <button type="button" :class="{ active: ruleFilter === 'enabled' }" @click="ruleFilter = 'enabled'">已启用</button>
          <button type="button" :class="{ active: ruleFilter === 'disabled' }" @click="ruleFilter = 'disabled'">已暂停</button>
          <button type="button" :class="{ active: ruleFilter === 'conflict' }" @click="ruleFilter = 'conflict'">有冲突</button>
        </div>
        <div class="rule-sort-control" aria-label="规则排序">
          <button type="button" :class="{ active: ruleSort === 'updated' }" title="按最近更新排序" @click="ruleSort = 'updated'"><Clock3 :size="15" />最近</button>
          <button type="button" :class="{ active: ruleSort === 'name' }" title="按名称排序" @click="ruleSort = 'name'"><ArrowDownAZ :size="15" />名称</button>
        </div>
      </div>

      <div class="rules-secondary-actions">
        <button :disabled="rulesState.loading" type="button" @click="emit('export-rules')"><Download :size="16" />导出规则</button>
        <button :disabled="rulesState.loading" type="button" @click="emit('import-rules')"><Upload :size="16" />导入规则</button>
        <button type="button" class="link-button" @click="emit('open-migration-settings')">换电脑或迁移数据</button>
      </div>

      <p v-if="ruleConflicts.length" class="conflict-summary">
        <AlertTriangle :size="16" />{{ ruleConflicts.length }} 组规则需要指定优先项。
      </p>
      <p v-if="rulesState.error" class="error inline-error">{{ rulesState.error }}</p>
    </header>

    <div v-if="filteredRules.length" class="compact-rule-list">
      <article
        v-for="rule in filteredRules"
        :key="rule.ruleId"
        class="compact-rule-item"
        :class="{ conflict: !!ruleConflict(rule.ruleId), disabled: !rule.enabled }"
      >
        <template v-if="ruleDrafts[rule.ruleId]">
          <div class="compact-rule-summary">
            <div class="rule-avatar" aria-hidden="true">{{ rule.gameId.trim().charAt(0).toUpperCase() || 'G' }}</div>
            <div class="compact-rule-main">
              <div class="rule-name-row">
                <h3>{{ rule.gameId }}</h3>
                <span v-if="ruleConflict(rule.ruleId)" class="status-pill conflict-pill">冲突</span>
                <span v-if="hasRuleDraftChanges(rule)" class="pending-chip">未保存</span>
              </div>
              <p>{{ rule.confirmedPaths.length }} 个存档位置 · 更新于 {{ formatUnixTs(rule.updatedAt) }}</p>
              <div class="rule-path-chips">
                <span v-for="path in rule.confirmedPaths.slice(0, 3)" :key="`${rule.ruleId}-${path}`" class="anchor-chip compact">
                  {{ pathLabel(path) }}
                </span>
                <span v-if="rule.confirmedPaths.length > 3" class="anchor-chip compact">+{{ rule.confirmedPaths.length - 3 }}</span>
              </div>
            </div>
            <label class="switch compact-rule-switch" :title="rule.enabled ? '暂停存档保护' : '启用存档保护'">
              <input
                :checked="rule.enabled"
                :disabled="rulesState.loading"
                type="checkbox"
                @change="emit('toggle-rule', { rule, enabled: ($event.target as HTMLInputElement).checked })"
              />
              <span class="slider"></span>
              <span class="switch-text">{{ rule.enabled ? "已启用" : "已暂停" }}</span>
            </label>
          </div>

          <section v-if="ruleConflict(rule.ruleId)" class="compact-conflict-row">
            <span>{{ isPrimary(rule.ruleId) ? "当前优先使用此规则" : `同一程序匹配 ${ruleConflict(rule.ruleId)?.conflictCount} 条规则` }}</span>
            <button :disabled="rulesState.loading || isPrimary(rule.ruleId)" type="button" @click="emit('mark-primary', rule)">
              {{ isPrimary(rule.ruleId) ? "已设为主要规则" : "设为主要规则" }}
            </button>
          </section>

          <details class="rule-manage-details" :open="hasRuleDraftChanges(rule)">
            <summary>{{ hasRuleDraftChanges(rule) ? "继续编辑" : "管理规则" }}</summary>
            <div class="rule-manage-body">
              <label class="field compact-field">
                <span>游戏名称</span>
                <input
                  :value="ruleDrafts[rule.ruleId].gameIdText"
                  type="text"
                  @input="updateDraft(rule.ruleId, { gameIdText: ($event.target as HTMLInputElement).value })"
                />
              </label>
              <label class="field compact-field">
                <span>存档路径（每行一条）</span>
                <textarea
                  :value="ruleDrafts[rule.ruleId].confirmedPathsText"
                  rows="4"
                  @input="updateDraft(rule.ruleId, { confirmedPathsText: ($event.target as HTMLTextAreaElement).value })"
                ></textarea>
              </label>
              <details class="rule-technical-details">
                <summary>技术信息</summary>
                <div class="rule-tech-grid">
                  <span>ruleId {{ rule.ruleId }}</span>
                  <span>gameUid {{ rule.gameUid }}</span>
                  <span>exeHash {{ shortValue(rule.exeHash) }}</span>
                  <span>置信度 {{ rule.confidence }}</span>
                </div>
              </details>
              <div class="rule-manage-actions">
                <button :disabled="rulesState.loading || !hasRuleDraftChanges(rule)" type="button" class="primary" @click="emit('save-rule', rule)">保存变更</button>
                <button :disabled="rulesState.loading" type="button" class="danger" @click="emit('remove-rule', rule)">删除规则</button>
              </div>
            </div>
          </details>
        </template>
      </article>
    </div>
    <p v-else class="empty-hint">当前筛选条件下没有规则。</p>
  </section>
</template>
