<script setup lang="ts">
import { computed } from "vue";
import type { GameSaveRule, RuleConflictItem } from "../../types";

type RuleDraft = {
  gameIdText: string;
  confirmedPathsText: string;
  enabled: boolean;
};

type TabState = {
  loading: boolean;
  error: string;
};

const props = defineProps<{
  rules: GameSaveRule[];
  ruleConflicts: RuleConflictItem[];
  ruleSearch: string;
  ruleDrafts: Record<string, RuleDraft>;
  rulesState: TabState;
  migrationExportWaiting: boolean;
  migrationExportMessage: string;
  migrationExportProgress: number | null;
  migrationImportWaiting: boolean;
  migrationImportMessage: string;
  migrationImportProgress: number | null;
}>();

const emit = defineEmits<{
  (e: "update:ruleSearch", value: string): void;
  (e: "update:ruleDraft", payload: { ruleId: string; patch: Partial<RuleDraft> }): void;
  (e: "reload"): void;
  (e: "export-rules"): void;
  (e: "import-rules"): void;
  (e: "export-migration"): void;
  (e: "import-migration"): void;
  (e: "mark-primary", rule: GameSaveRule): void;
  (e: "save-rule", rule: GameSaveRule): void;
  (e: "remove-rule", rule: GameSaveRule): void;
}>();

const PATH_ANCHOR_TOKENS = [
  "%GAME_DIR%",
  "%SAVED_GAMES%",
  "%DOCUMENTS%",
  "%LOCALLOW%",
  "%LOCALAPPDATA%",
  "%APPDATA%",
  "%USERPROFILE%",
] as const;

const filteredRules = computed(() => {
  const keyword = props.ruleSearch.trim().toLowerCase();
  if (!keyword) return props.rules;
  return props.rules.filter((rule) => rule.gameId.toLowerCase().includes(keyword));
});

const ruleConflictByRuleId = computed<Record<string, RuleConflictItem>>(() => {
  const map: Record<string, RuleConflictItem> = {};
  for (const conflict of props.ruleConflicts) {
    for (const ruleId of conflict.ruleIds) {
      map[ruleId] = conflict;
    }
  }
  return map;
});

function normalizePaths(rawText: string): string[] {
  const dedup = new Set<string>();
  const output: string[] = [];
  for (const line of rawText.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (!dedup.has(trimmed)) {
      dedup.add(trimmed);
      output.push(trimmed);
    }
  }
  return output;
}

function hasRuleDraftChanges(rule: GameSaveRule): boolean {
  const draft = props.ruleDrafts[rule.ruleId];
  if (!draft) return false;
  if (draft.gameIdText.trim() !== rule.gameId) {
    return true;
  }
  const draftPaths = normalizePaths(draft.confirmedPathsText);
  const savedPaths = rule.confirmedPaths;
  if (draft.enabled !== rule.enabled) {
    return true;
  }
  if (draftPaths.length !== savedPaths.length) {
    return true;
  }
  return draftPaths.some((path, index) => path !== savedPaths[index]);
}

function ruleConflictFor(ruleId: string): RuleConflictItem | null {
  return ruleConflictByRuleId.value[ruleId] ?? null;
}

function isPrimaryConflictRule(ruleId: string): boolean {
  const conflict = ruleConflictFor(ruleId);
  return !!conflict && conflict.primaryRuleId === ruleId;
}

function shortExeHash(exeHash: string): string {
  if (exeHash.length <= 16) return exeHash;
  return `${exeHash.slice(0, 8)}...${exeHash.slice(-8)}`;
}

function formatUnixTs(value: string): string {
  const timestamp = Number(value.startsWith("pre_restore_") ? value.slice("pre_restore_".length) : value);
  if (!Number.isFinite(timestamp) || timestamp <= 0) {
    return value || "未知";
  }
  const date = new Date(timestamp * 1000);
  if (Number.isNaN(date.getTime())) {
    return value || "未知";
  }
  return date.toLocaleString();
}

function extractPathAnchorToken(path: string): string | null {
  const normalized = path.trim().replace(/\//g, "\\").toUpperCase();
  for (const token of PATH_ANCHOR_TOKENS) {
    if (normalized === token || normalized.startsWith(`${token}\\`)) {
      return token;
    }
  }
  return null;
}

function collectAnchorTokens(paths: string[]): string[] {
  const ordered = new Set<string>();
  for (const path of paths) {
    const token = extractPathAnchorToken(path);
    if (token) {
      ordered.add(token);
    }
  }
  return Array.from(ordered);
}

function ruleDraftAnchorTokens(ruleId: string): string[] {
  const raw = props.ruleDrafts[ruleId]?.confirmedPathsText ?? "";
  const paths = raw
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
  return collectAnchorTokens(paths);
}

function ruleUsesGameDirToken(rule: GameSaveRule | null | undefined): boolean {
  if (!rule) return false;
  return rule.confirmedPaths.some((path) => path.toUpperCase().includes("%GAME_DIR%"));
}

function pathAnchorLabel(token: string): string {
  switch (token.toUpperCase()) {
    case "%GAME_DIR%":
      return "游戏目录";
    case "%SAVED_GAMES%":
      return "Saved Games";
    case "%DOCUMENTS%":
      return "文档";
    case "%LOCALLOW%":
      return "LocalLow";
    case "%LOCALAPPDATA%":
      return "Local";
    case "%APPDATA%":
      return "Roaming";
    case "%USERPROFILE%":
      return "用户目录（兼容）";
    default:
      return token;
  }
}

function pathAnchorDescription(token: string): string {
  switch (token.toUpperCase()) {
    case "%GAME_DIR%":
      return "跟随当前绑定的游戏 EXE 所在目录动态解析";
    case "%SAVED_GAMES%":
      return "Windows 的 Saved Games 存档目录";
    case "%DOCUMENTS%":
      return "当前用户的 Documents 目录";
    case "%LOCALLOW%":
      return "当前用户的 AppData\\LocalLow 目录";
    case "%LOCALAPPDATA%":
      return "当前用户的 AppData\\Local 目录";
    case "%APPDATA%":
      return "当前用户的 AppData\\Roaming 目录";
    case "%USERPROFILE%":
      return "当前用户根目录，属于兼容兜底锚点，范围较宽，优先级低于文档和 AppData 类锚点";
    default:
      return "使用路径锚点进行动态解析";
  }
}

function ruleAnchorHint(tokens: string[]): string {
  if (!tokens.length) {
    return "";
  }
  if (tokens.includes("%USERPROFILE%")) {
    return "当前规则含“用户目录（兼容）”锚点，建议优先使用更具体的文档 / AppData / 游戏目录锚点。";
  }
  if (tokens.includes("%GAME_DIR%")) {
    return "当前规则含“游戏目录”锚点，路径会跟随已绑定 EXE 所在目录动态解析。";
  }
  return "当前规则已使用具体路径锚点，跨机器时会比纯用户目录规则更稳定。";
}

function updateRuleDraft(ruleId: string, patch: Partial<RuleDraft>) {
  emit("update:ruleDraft", { ruleId, patch });
}
</script>

<template>
  <section class="panel rules-shell">
    <header class="rules-header">
      <div class="rules-title-row">
        <h2>规则管理</h2>
        <button :disabled="rulesState.loading" type="button" @click="emit('reload')">刷新</button>
      </div>
      <div class="rules-toolbar">
        <label class="rules-search">
          <span>搜索规则</span>
          <input
            :value="ruleSearch"
            placeholder="按 gameId 搜索"
            @input="emit('update:ruleSearch', ($event.target as HTMLInputElement).value)"
          />
        </label>
        <div class="rules-actions">
          <button :disabled="rulesState.loading" type="button" @click="emit('export-rules')">导出规则</button>
          <button :disabled="rulesState.loading" type="button" @click="emit('import-rules')">导入规则</button>
          <button :disabled="rulesState.loading" type="button" @click="emit('export-migration')">导出迁移包</button>
          <button :disabled="rulesState.loading" type="button" @click="emit('import-migration')">导入迁移包</button>
        </div>
      </div>
      <div v-if="migrationExportWaiting" class="migration-progress">
        <p>{{ migrationExportMessage || "正在导出迁移包，文件较多时可能需要一点时间，请稍候..." }}</p>
        <div class="progress-track" role="progressbar" aria-label="迁移包导出进行中">
          <span v-if="migrationExportProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${migrationExportProgress}%` }"
          ></span>
        </div>
        <p v-if="migrationExportProgress !== null">当前进度：{{ migrationExportProgress }}%</p>
      </div>
      <div v-if="migrationImportWaiting" class="migration-progress">
        <p>{{ migrationImportMessage || "正在导入迁移包，文件较多时可能需要一点时间，请稍候..." }}</p>
        <div class="progress-track" role="progressbar" aria-label="迁移包导入进行中">
          <span v-if="migrationImportProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${migrationImportProgress}%` }"
          ></span>
        </div>
        <p v-if="migrationImportProgress !== null">当前进度：{{ migrationImportProgress }}%</p>
      </div>
      <p v-if="ruleConflicts.length" class="conflict-summary">
        检测到 {{ ruleConflicts.length }} 组 exeHash 冲突，建议为每组指定主规则。
      </p>
      <p v-if="rulesState.error" class="error inline-error">{{ rulesState.error }}</p>
    </header>

    <ul v-if="filteredRules.length" class="rule-list rules-grid">
      <li v-for="rule in filteredRules" :key="rule.ruleId" class="rule-card">
        <template v-if="ruleDrafts[rule.ruleId]">
          <div class="rule-head">
            <div class="rule-title-block">
              <div class="rule-name-row">
                <strong>{{ rule.gameId }}</strong>
                <span class="status-pill" :class="rule.enabled ? 'enabled' : 'disabled'">
                  {{ rule.enabled ? "启用" : "禁用" }}
                </span>
                <span v-if="hasRuleDraftChanges(rule)" class="pending-chip">未保存变更</span>
              </div>
              <div class="rule-meta">
                <span>ruleId {{ rule.ruleId }}</span>
                <span>置信度 {{ rule.confidence }}</span>
                <span>更新 {{ formatUnixTs(rule.updatedAt) }}</span>
              </div>
              <section v-if="ruleConflictFor(rule.ruleId)" class="rule-conflict-box">
                <p>
                  冲突：同 exeHash 命中 {{ ruleConflictFor(rule.ruleId)?.conflictCount }} 条规则
                  （涉及 {{ ruleConflictFor(rule.ruleId)?.gameIds.join(" / ") }}）
                </p>
                <p class="conflict-warning">
                  {{ isPrimaryConflictRule(rule.ruleId) ? "已指定主规则，启动不会被冲突拦截" : "未指定主规则会阻止启动，请先设置主规则" }}
                </p>
                <p>hash {{ shortExeHash(ruleConflictFor(rule.ruleId)?.exeHash || "") }}</p>
                <div class="row">
                  <span class="conflict-primary" :class="isPrimaryConflictRule(rule.ruleId) ? 'on' : 'off'">
                    {{ isPrimaryConflictRule(rule.ruleId) ? "当前主规则" : "非主规则" }}
                  </span>
                  <button
                    type="button"
                    :disabled="rulesState.loading || isPrimaryConflictRule(rule.ruleId)"
                    @click="emit('mark-primary', rule)"
                  >
                    设为主规则
                  </button>
                </div>
              </section>
            </div>
            <label class="switch">
              <input
                :checked="ruleDrafts[rule.ruleId].enabled"
                type="checkbox"
                @change="updateRuleDraft(rule.ruleId, { enabled: ($event.target as HTMLInputElement).checked })"
              />
              <span class="slider"></span>
              <span class="switch-text">启用</span>
            </label>
          </div>
          <label class="field compact-field">
            <span>游戏名（gameId）</span>
            <input
              :value="ruleDrafts[rule.ruleId].gameIdText"
              type="text"
              class="gameid-editor"
              placeholder="例如：elden_ring"
              @input="updateRuleDraft(rule.ruleId, { gameIdText: ($event.target as HTMLInputElement).value })"
            />
          </label>
          <label class="field compact-field">
            <span>存档路径（每行一条）</span>
            <div v-if="ruleDraftAnchorTokens(rule.ruleId).length" class="anchor-chip-row">
              <span
                v-for="token in ruleDraftAnchorTokens(rule.ruleId)"
                :key="`${rule.ruleId}-${token}`"
                class="anchor-chip"
                :class="{ warning: token === '%GAME_DIR%', fallback: token === '%USERPROFILE%' }"
                :title="pathAnchorDescription(token)"
              >
                {{ pathAnchorLabel(token) }}
              </span>
            </div>
            <p v-if="ruleDraftAnchorTokens(rule.ruleId).length" class="field-note anchor-note">
              {{ ruleAnchorHint(ruleDraftAnchorTokens(rule.ruleId)) }}
            </p>
            <p v-if="ruleUsesGameDirToken(rule)" class="field-note token-note">
              此规则包含 <code>%GAME_DIR%</code>，路径会跟随当前绑定的游戏 EXE 所在目录动态解析。
            </p>
            <textarea
              :value="ruleDrafts[rule.ruleId].confirmedPathsText"
              rows="4"
              class="paths-editor"
              placeholder="每行一条路径"
              @input="updateRuleDraft(rule.ruleId, { confirmedPathsText: ($event.target as HTMLTextAreaElement).value })"
            />
          </label>
          <div class="row rule-actions-row">
            <button
              :disabled="rulesState.loading || !hasRuleDraftChanges(rule)"
              type="button"
              class="primary"
              @click="emit('save-rule', rule)"
            >
              保存变更
            </button>
            <button :disabled="rulesState.loading" type="button" class="danger" @click="emit('remove-rule', rule)">
              删除规则
            </button>
          </div>
        </template>
      </li>
    </ul>
    <p v-else class="empty-hint">暂无规则，可先在“学习存档”里生成规则。</p>
  </section>
</template>
