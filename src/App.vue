<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import {
  confirmRule,
  deleteRule,
  exportMigrationZip,
  exportRules,
  finishLearning,
  getLauncherSession,
  getLearningSession,
  getRedirectRuntimeInfo,
  getRuntimeStatus,
  importRules,
  importMigrationZip,
  launchGame,
  launchGameFromLibrary,
  listBackupVersions,
  listGameLibraryItems,
  listRules,
  openCandidatePath,
  restartAsAdmin,
  restoreBackupVersion,
  setPreferredExePath,
  startLearning,
  updateRule,
} from "./api";
import type {
  BackupVersion,
  CandidatePath,
  GameLibraryItem,
  GameSaveRule,
  LauncherMode,
  LauncherSession,
  RedirectRuntimeInfo,
} from "./types";

type UiStep = "setup" | "running" | "results";
type TopTab = "learning" | "rules" | "library";
type RuleDraft = {
  confirmedPathsText: string;
  enabled: boolean;
};
type TabState = {
  loading: boolean;
  info: string;
  error: string;
};

const step = ref<UiStep>("setup");
const activeTab = ref<TopTab>("library");
const gameId = ref("");
const exePath = ref("");
const sessionId = ref("");
const pid = ref<number | null>(null);
const candidates = ref<CandidatePath[]>([]);
const selected = ref<string[]>([]);
const rules = ref<GameSaveRule[]>([]);
const ruleSearch = ref("");
const ruleDrafts = ref<Record<string, RuleDraft>>({});
const learningState = ref<TabState>({ loading: false, info: "", error: "" });
const rulesState = ref<TabState>({ loading: false, info: "", error: "" });
const libraryState = ref<TabState>({ loading: false, info: "", error: "" });
const migrationExportWaiting = ref(false);
const eventCaptureMode = ref("unknown");
const capturedEventCount = ref(0);
const eventCaptureError = ref("");
const runtimeIsAdmin = ref(false);
const runtimeMessage = ref("");
const redirectRuntimeInfo = ref<RedirectRuntimeInfo | null>(null);
const libraryItems = ref<GameLibraryItem[]>([]);
const librarySearch = ref("");
const cardLoading = ref<Record<string, boolean>>({});
const expandedGames = ref<Record<string, boolean>>({});
const backupVersionsByGame = ref<Record<string, BackupVersion[]>>({});
const sessionDetailsByGame = ref<Record<string, LauncherSession | null>>({});

const hasHighConfidence = computed(() => candidates.value.some((item) => item.score >= 45));
const filteredRules = computed(() => {
  const keyword = ruleSearch.value.trim().toLowerCase();
  if (!keyword) return rules.value;
  return rules.value.filter((rule) => rule.gameId.toLowerCase().includes(keyword));
});
const filteredLibraryItems = computed(() => {
  const keyword = librarySearch.value.trim().toLowerCase();
  if (!keyword) return libraryItems.value;
  return libraryItems.value.filter((item) => item.gameId.toLowerCase().includes(keyword));
});

function inferGameId(path: string): string {
  const parts = path.split("\\");
  const fileName = parts[parts.length - 1] ?? "";
  return fileName.toLowerCase().endsWith(".exe") ? fileName.slice(0, -4) : fileName;
}

function normalizeGameId(gameIdText: string): string {
  return gameIdText.trim().toLowerCase();
}

function cardKey(gameIdText: string): string {
  return normalizeGameId(gameIdText);
}

function isCardBusy(gameIdText: string): boolean {
  return cardLoading.value[cardKey(gameIdText)] === true;
}

function setCardBusy(gameIdText: string, busy: boolean) {
  const key = cardKey(gameIdText);
  cardLoading.value = {
    ...cardLoading.value,
    [key]: busy,
  };
}

function isGameExpanded(gameIdText: string): boolean {
  return expandedGames.value[cardKey(gameIdText)] === true;
}

function setGameExpanded(gameIdText: string, expanded: boolean) {
  const key = cardKey(gameIdText);
  expandedGames.value = {
    ...expandedGames.value,
    [key]: expanded,
  };
}

function toggleSelect(path: string) {
  if (selected.value.includes(path)) {
    selected.value = selected.value.filter((item) => item !== path);
    return;
  }
  selected.value = [...selected.value, path];
}

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
  const draft = ruleDrafts.value[rule.ruleId];
  if (!draft) return false;
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

function formatUnixTs(value: string): string {
  const timestamp = Number(value);
  if (!Number.isFinite(timestamp) || timestamp <= 0) {
    return value || "未知";
  }
  const date = new Date(timestamp * 1000);
  if (Number.isNaN(date.getTime())) {
    return value || "未知";
  }
  return date.toLocaleString();
}

function hydrateRuleDrafts() {
  const next: Record<string, RuleDraft> = {};
  for (const rule of rules.value) {
    const previous = ruleDrafts.value[rule.ruleId];
    next[rule.ruleId] = {
      confirmedPathsText: previous?.confirmedPathsText ?? rule.confirmedPaths.join("\n"),
      enabled: previous?.enabled ?? rule.enabled,
    };
  }
  ruleDrafts.value = next;
}

function sortRulesByUpdatedTime(items: GameSaveRule[]): GameSaveRule[] {
  return [...items].sort((a, b) => {
    const aTime = Number(a.updatedAt || a.createdAt || "0");
    const bTime = Number(b.updatedAt || b.createdAt || "0");
    return bTime - aTime;
  });
}

function sortLibraryItems(items: GameLibraryItem[]): GameLibraryItem[] {
  return [...items].sort((a, b) => {
    const aTime = Math.max(Number(a.lastSessionUpdatedAt || "0"), Number(a.lastRuleUpdatedAt || "0"));
    const bTime = Math.max(Number(b.lastSessionUpdatedAt || "0"), Number(b.lastRuleUpdatedAt || "0"));
    return bTime - aTime;
  });
}

function backupVersionsFor(gameIdText: string): BackupVersion[] {
  return backupVersionsByGame.value[cardKey(gameIdText)] ?? [];
}

function sessionDetailsFor(gameIdText: string): LauncherSession | null {
  return sessionDetailsByGame.value[cardKey(gameIdText)] ?? null;
}

async function chooseExePath() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "Executable", extensions: ["exe"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    exePath.value = chosen;
    if (!gameId.value.trim()) {
      gameId.value = inferGameId(chosen);
    }
  } catch (err) {
    learningState.value.error = `无法打开文件选择器：${String(err)}`;
  }
}

async function beginLearning() {
  learningState.value.loading = true;
  learningState.value.error = "";
  learningState.value.info = "";
  try {
    const trimmedGameId = gameId.value.trim();
    const trimmedExePath = exePath.value.trim();
    if (!trimmedGameId || !trimmedExePath) {
      throw new Error("请先填写 gameId 并选择 exePath");
    }
    sessionId.value = await startLearning(trimmedGameId, trimmedExePath);
    pid.value = await launchGame(sessionId.value);
    step.value = "running";
    learningState.value.info = "学习会话已开始，请在游戏中执行一次存档后退出游戏。";
  } catch (err) {
    learningState.value.error = String(err);
  } finally {
    learningState.value.loading = false;
  }
}

async function endLearning() {
  learningState.value.loading = true;
  learningState.value.error = "";
  learningState.value.info = "";
  try {
    candidates.value = await finishLearning(sessionId.value);
    const session = await getLearningSession(sessionId.value);
    eventCaptureMode.value = session.eventCaptureMode ?? "unknown";
    capturedEventCount.value = session.capturedEventCount ?? 0;
    eventCaptureError.value = session.eventCaptureError ?? "";
    const topTwo = candidates.value.filter((item) => !item.collapsed).slice(0, 2);
    const prioritized = topTwo.filter((item) =>
      item.matchedSignals.some((signal) => signal.startsWith("extension:")),
    );
    const fallback = topTwo.filter((item) => item.score >= 60);
    const merged = [...prioritized];
    for (const item of fallback) {
      if (!merged.some((entry) => entry.path === item.path)) {
        merged.push(item);
      }
    }
    selected.value = merged.slice(0, 2).map((item) => item.path);
    step.value = "results";
    if (!hasHighConfidence.value) {
      learningState.value.info = "未检测到高可信候选，请确认是否在学习期间执行了存档动作。";
    }
  } catch (err) {
    learningState.value.error = String(err);
  } finally {
    learningState.value.loading = false;
  }
}

async function saveLearningRule() {
  if (!selected.value.length) {
    learningState.value.error = "请至少选择一个候选路径。";
    return;
  }
  learningState.value.loading = true;
  learningState.value.error = "";
  learningState.value.info = "";
  try {
    const ruleId = await confirmRule(sessionId.value, selected.value);
    learningState.value.info = `规则已保存，ruleId=${ruleId}`;
    await refreshRules();
    await refreshLibraryItems();
  } catch (err) {
    learningState.value.error = String(err);
  } finally {
    learningState.value.loading = false;
  }
}

async function refreshRules() {
  const data = await listRules();
  rules.value = sortRulesByUpdatedTime(data);
  hydrateRuleDrafts();
}

async function refreshLibraryItems() {
  const data = await listGameLibraryItems();
  libraryItems.value = sortLibraryItems(data);
}

async function reloadLibraryWithLoading() {
  libraryState.value.loading = true;
  libraryState.value.error = "";
  try {
    await refreshLibraryItems();
    await loadRedirectRuntimeInfo();
  } catch (err) {
    libraryState.value.error = `读取游戏库失败：${String(err)}`;
  } finally {
    libraryState.value.loading = false;
  }
}

async function loadRedirectRuntimeInfo() {
  redirectRuntimeInfo.value = await getRedirectRuntimeInfo();
}

async function loadBackupVersionsForGame(gameIdText: string, withCardLoading = true) {
  const key = cardKey(gameIdText);
  if (withCardLoading) {
    setCardBusy(gameIdText, true);
  }
  try {
    backupVersionsByGame.value = {
      ...backupVersionsByGame.value,
      [key]: await listBackupVersions(gameIdText),
    };
  } catch (err) {
    libraryState.value.error = `读取备份版本失败：${String(err)}`;
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, false);
    }
  }
}

async function loadSessionDetailsForGame(gameIdText: string, withCardLoading = true) {
  const key = cardKey(gameIdText);
  const item = libraryItems.value.find((entry) => cardKey(entry.gameId) === key);
  if (!item?.lastSessionId) {
    sessionDetailsByGame.value = {
      ...sessionDetailsByGame.value,
      [key]: null,
    };
    return;
  }
  if (withCardLoading) {
    setCardBusy(gameIdText, true);
  }
  try {
    const detail = await getLauncherSession(item.lastSessionId);
    sessionDetailsByGame.value = {
      ...sessionDetailsByGame.value,
      [key]: detail,
    };
  } catch (err) {
    libraryState.value.error = `读取会话详情失败：${String(err)}`;
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, false);
    }
  }
}

async function choosePreferredExeForGame(gameIdText: string) {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "Executable", extensions: ["exe"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    setCardBusy(gameIdText, true);
    libraryState.value.error = "";
    libraryState.value.info = "";
    await setPreferredExePath(gameIdText, chosen);
    await refreshLibraryItems();
    libraryState.value.info = `${gameIdText} 已更新启动 EXE`;
  } catch (err) {
    libraryState.value.error = `绑定 EXE 失败：${String(err)}`;
  } finally {
    setCardBusy(gameIdText, false);
  }
}

async function launchLibraryGame(gameIdText: string, mode: LauncherMode = "backup") {
  setCardBusy(gameIdText, true);
  libraryState.value.error = "";
  libraryState.value.info = "";
  try {
    const session = await launchGameFromLibrary(gameIdText, mode);
    setGameExpanded(gameIdText, true);
    await refreshLibraryItems();
    await Promise.all([
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
    libraryState.value.info = `${gameIdText} 已启动，PID=${session.pid ?? "未知"}，模式=${session.launchMode ?? mode}`;
  } catch (err) {
    libraryState.value.error = String(err);
    await refreshLibraryItems();
  } finally {
    setCardBusy(gameIdText, false);
  }
}

function toggleLibraryDetails(gameIdText: string) {
  const next = !isGameExpanded(gameIdText);
  setGameExpanded(gameIdText, next);
  if (next) {
    void loadBackupVersionsForGame(gameIdText, false);
    void loadSessionDetailsForGame(gameIdText, false);
  }
}

async function rollbackToLibraryBackupVersion(gameIdText: string, versionId: string) {
  if (!window.confirm(`确定回滚 ${gameIdText} 到版本 ${versionId} 吗？`)) {
    return;
  }
  setCardBusy(gameIdText, true);
  libraryState.value.error = "";
  libraryState.value.info = "";
  try {
    const result = await restoreBackupVersion(gameIdText, versionId);
    await refreshLibraryItems();
    await Promise.all([
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
    libraryState.value.info = `回滚完成：${result.gameId} @ ${result.versionId}，恢复文件 ${result.restoredFiles}`;
  } catch (err) {
    libraryState.value.error = `回滚失败：${String(err)}`;
  } finally {
    setCardBusy(gameIdText, false);
  }
}

async function reloadRulesWithLoading() {
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await refreshRules();
  } catch (err) {
    rulesState.value.error = String(err);
  } finally {
    rulesState.value.loading = false;
  }
}

async function saveManagedRule(rule: GameSaveRule) {
  const draft = ruleDrafts.value[rule.ruleId];
  if (!draft) return;
  const normalizedPaths = normalizePaths(draft.confirmedPathsText);
  if (!normalizedPaths.length) {
    rulesState.value.error = "路径不能为空，至少保留一条路径。";
    return;
  }

  rulesState.value.loading = true;
  rulesState.value.error = "";
  rulesState.value.info = "";
  try {
    const updated = await updateRule(rule.ruleId, normalizedPaths, draft.enabled);
    rules.value = sortRulesByUpdatedTime(
      rules.value.map((item) => (item.ruleId === updated.ruleId ? updated : item)),
    );
    ruleDrafts.value[updated.ruleId] = {
      confirmedPathsText: updated.confirmedPaths.join("\n"),
      enabled: updated.enabled,
    };
    rulesState.value.info = `规则 ${rule.gameId} 已更新`;
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `保存规则失败：${String(err)}`;
  } finally {
    rulesState.value.loading = false;
  }
}

async function removeManagedRule(rule: GameSaveRule) {
  if (!window.confirm(`确定删除规则 ${rule.gameId} 吗？此操作不可恢复。`)) {
    return;
  }
  rulesState.value.loading = true;
  rulesState.value.error = "";
  rulesState.value.info = "";
  try {
    await deleteRule(rule.ruleId);
    rules.value = rules.value.filter((item) => item.ruleId !== rule.ruleId);
    const nextDrafts = { ...ruleDrafts.value };
    delete nextDrafts[rule.ruleId];
    ruleDrafts.value = nextDrafts;
    rulesState.value.info = `规则 ${rule.gameId} 已删除`;
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `删除规则失败：${String(err)}`;
  } finally {
    rulesState.value.loading = false;
  }
}

async function exportRulesToFile() {
  try {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const chosen = await save({
      defaultPath: "gamesaver-rules.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!chosen) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    rulesState.value.info = "";
    const result = await exportRules(chosen);
    rulesState.value.info = `导出成功，共 ${result.count} 条规则`;
  } catch (err) {
    rulesState.value.error = `导出失败：${String(err)}`;
  } finally {
    rulesState.value.loading = false;
  }
}

async function importRulesFromFile() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    rulesState.value.info = "";
    const result = await importRules(chosen);
    await refreshRules();
    await refreshLibraryItems();
    rulesState.value.info = `导入完成：新增 ${result.imported}，覆盖 ${result.overwritten}，跳过 ${result.skipped}`;
  } catch (err) {
    rulesState.value.error = `导入失败：${String(err)}`;
  } finally {
    rulesState.value.loading = false;
  }
}

async function exportMigrationZipToFile() {
  try {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const chosen = await save({
      defaultPath: "gamesaver-migration.zip",
      filters: [{ name: "ZIP", extensions: ["zip"] }],
    });
    if (!chosen) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    rulesState.value.info = "";
    migrationExportWaiting.value = true;
    const result = await exportMigrationZip(chosen);
    rulesState.value.info =
      `迁移包导出成功：规则 ${result.ruleCount}，备份游戏 ${result.backupGames}，文件 ${result.exportedFiles}，` +
      `跳过 ${result.skippedBackupGames}`;
  } catch (err) {
    rulesState.value.error = `导出迁移包失败：${String(err)}`;
  } finally {
    migrationExportWaiting.value = false;
    rulesState.value.loading = false;
  }
}

async function importMigrationZipFromFile() {
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "ZIP", extensions: ["zip"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    rulesState.value.loading = true;
    rulesState.value.error = "";
    rulesState.value.info = "";
    const result = await importMigrationZip(chosen);
    await refreshRules();
    await refreshLibraryItems();
    rulesState.value.info =
      `迁移包导入完成：规则 新增 ${result.importedRules} / 覆盖 ${result.overwrittenRules} / 跳过 ${result.skippedRules}；` +
      `备份游戏 ${result.importedBackupGames}，复制文件 ${result.copiedBackupFiles}，跳过 ${result.skippedBackupGames}`;
  } catch (err) {
    rulesState.value.error = `导入迁移包失败：${String(err)}`;
  } finally {
    rulesState.value.loading = false;
  }
}

async function openPath(path: string) {
  learningState.value.error = "";
  try {
    await openCandidatePath(path);
  } catch (err) {
    learningState.value.error = `打开目录失败：${String(err)}`;
  }
}

async function loadRuntimeStatus() {
  try {
    const status = await getRuntimeStatus();
    runtimeIsAdmin.value = status.isAdmin;
    runtimeMessage.value = status.message;
  } catch (err) {
    runtimeMessage.value = `运行状态读取失败：${String(err)}`;
  }
}

async function relaunchAsAdmin() {
  learningState.value.error = "";
  try {
    await restartAsAdmin();
  } catch (err) {
    learningState.value.error = `管理员重启失败：${String(err)}`;
  }
}

onMounted(() => {
  void loadRuntimeStatus();
  void reloadRulesWithLoading();
  void reloadLibraryWithLoading();
});
</script>

<template>
  <main class="layout">
    <nav class="tabs panel">
      <button
        class="tab"
        :class="{ active: activeTab === 'library' }"
        type="button"
        @click="activeTab = 'library'"
      >
        游戏库
      </button>
      <button
        class="tab"
        :class="{ active: activeTab === 'learning' }"
        type="button"
        @click="activeTab = 'learning'"
      >
        学习存档
      </button>
      <button
        class="tab"
        :class="{ active: activeTab === 'rules' }"
        type="button"
        @click="activeTab = 'rules'"
      >
        规则管理
      </button>
    </nav>

    <template v-if="activeTab === 'learning'">
      <section class="panel" :class="runtimeIsAdmin ? 'runtime-ok' : 'runtime-warn'">
        <div class="row">
          <strong>运行权限：{{ runtimeIsAdmin ? "管理员" : "普通用户" }}</strong>
          <button v-if="!runtimeIsAdmin" type="button" class="primary" @click="relaunchAsAdmin">
            一键管理员重启
          </button>
        </div>
        <p>{{ runtimeMessage }}</p>
      </section>

      <header class="panel">
        <h1>GameSaver 学习模式 MVP</h1>
        <p>流程：选择游戏 -> 开始学习 -> 游戏存档并退出 -> 结束学习 -> 确认规则</p>
      </header>

      <section v-if="step === 'setup'" class="panel">
        <h2>Step 1 / 2：选择游戏并开始学习</h2>
        <label class="field">
          <span>Game ID</span>
          <input v-model="gameId" placeholder="例如：elden_ring" />
        </label>
        <label class="field">
          <span>游戏 EXE 路径</span>
          <div class="row">
            <input v-model="exePath" placeholder="D:\\Games\\xxx\\game.exe" />
            <button type="button" @click="chooseExePath">浏览</button>
          </div>
        </label>
        <button :disabled="learningState.loading" type="button" class="primary" @click="beginLearning">
          {{ learningState.loading ? "处理中..." : "开始学习并启动游戏" }}
        </button>
      </section>

      <section v-else-if="step === 'running'" class="panel">
        <h2>Step 3 / 4 / 5：学习进行中</h2>
        <p>会话 ID：{{ sessionId }}</p>
        <p>游戏 PID：{{ pid ?? "未获取" }}</p>
        <p>请在游戏中执行一次明确的存档动作，退出游戏后点击“结束学习”。</p>
        <button :disabled="learningState.loading" type="button" class="primary" @click="endLearning">
          {{ learningState.loading ? "分析中..." : "结束学习并分析候选路径" }}
        </button>
      </section>

      <section v-else class="panel">
        <h2>Step 6：候选结果与规则确认</h2>
        <p>采集模式：{{ eventCaptureMode }} | 捕获事件数：{{ capturedEventCount }}</p>
        <p v-if="eventCaptureError" class="error">ETW信息：{{ eventCaptureError }}</p>
        <p v-if="!candidates.length">无候选路径。</p>
        <ul v-else class="candidate-list">
          <li v-for="item in candidates" :key="item.path" :class="{ collapsed: item.collapsed }">
            <div class="candidate-header">
              <label>
                <input
                  :checked="selected.includes(item.path)"
                  type="checkbox"
                  :disabled="item.collapsed"
                  @change="toggleSelect(item.path)"
                />
                <strong>{{ item.path }}</strong>
              </label>
              <button type="button" @click="openPath(item.path)">打开目录</button>
            </div>
            <p>
              得分：{{ item.score }} | changed={{ item.changedFiles }} added={{ item.addedFiles }}
              modified={{ item.modifiedFiles }}
            </p>
            <p>信号：{{ item.matchedSignals.join(" / ") || "无" }}</p>
          </li>
        </ul>
        <div class="row">
          <button :disabled="learningState.loading" type="button" class="primary" @click="saveLearningRule">
            确认并保存规则
          </button>
          <button :disabled="learningState.loading" type="button" @click="step = 'setup'">重新学习</button>
        </div>
      </section>
    </template>

    <section v-else-if="activeTab === 'rules'" class="panel rules-shell">
      <header class="rules-header">
        <div class="rules-title-row">
          <h2>规则管理</h2>
          <button :disabled="rulesState.loading" type="button" @click="reloadRulesWithLoading">刷新</button>
        </div>
        <div class="rules-toolbar">
          <label class="rules-search">
            <span>搜索规则</span>
            <input v-model="ruleSearch" placeholder="按 gameId 搜索" />
          </label>
          <div class="rules-actions">
            <button :disabled="rulesState.loading" type="button" @click="exportRulesToFile">导出规则</button>
            <button :disabled="rulesState.loading" type="button" @click="importRulesFromFile">导入规则</button>
            <button :disabled="rulesState.loading" type="button" @click="exportMigrationZipToFile">
              导出迁移包
            </button>
            <button :disabled="rulesState.loading" type="button" @click="importMigrationZipFromFile">
              导入迁移包
            </button>
          </div>
        </div>
        <div v-if="migrationExportWaiting" class="migration-progress">
          <p>正在导出迁移包，文件较多时可能需要一点时间，请稍候...</p>
          <div class="progress-track" role="progressbar" aria-label="迁移包导出进行中">
            <span class="progress-indeterminate"></span>
          </div>
        </div>
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
              </div>
              <label class="switch">
                <input
                  v-model="ruleDrafts[rule.ruleId].enabled"
                  type="checkbox"
                />
                <span class="slider"></span>
                <span class="switch-text">启用</span>
              </label>
            </div>
            <label class="field compact-field">
              <span>存档路径（每行一条）</span>
              <textarea
                v-model="ruleDrafts[rule.ruleId].confirmedPathsText"
                rows="4"
                class="paths-editor"
                placeholder="每行一条路径"
              />
            </label>
            <div class="row rule-actions-row">
              <button
                :disabled="rulesState.loading || !hasRuleDraftChanges(rule)"
                type="button"
                class="primary"
                @click="saveManagedRule(rule)"
              >
                保存变更
              </button>
              <button :disabled="rulesState.loading" type="button" class="danger" @click="removeManagedRule(rule)">
                删除规则
              </button>
            </div>
          </template>
        </li>
      </ul>
      <p v-else class="empty-hint">暂无规则，可先在“学习存档”里生成规则。</p>
    </section>

    <section v-else class="panel library-shell">
      <header class="library-header">
        <div class="library-title-row">
          <h2>游戏库（自动备份优先）</h2>
          <button :disabled="libraryState.loading" type="button" @click="reloadLibraryWithLoading">刷新</button>
        </div>
        <label class="library-search">
          <span>搜索游戏</span>
          <input v-model="librarySearch" placeholder="按 gameId 搜索" />
        </label>
        <section v-if="redirectRuntimeInfo" class="runtime-compact">
          <span class="runtime-chip">架构 {{ redirectRuntimeInfo.arch }}</span>
          <span class="runtime-chip">备份模式可用</span>
          <span class="runtime-chip" :class="redirectRuntimeInfo.sandboxieExists ? 'ok' : 'warn'">
            Sandboxie {{ redirectRuntimeInfo.sandboxieExists ? "可用" : "缺失" }}
          </span>
          <span class="runtime-chip" :class="redirectRuntimeInfo.injectorExists ? 'ok' : 'warn'">
            Injector {{ redirectRuntimeInfo.injectorExists ? "可用" : "缺失" }}
          </span>
          <span class="runtime-chip" :class="redirectRuntimeInfo.dllExists ? 'ok' : 'warn'">
            Hook DLL {{ redirectRuntimeInfo.dllExists ? "可用" : "缺失" }}
          </span>
        </section>
      </header>

      <div v-if="filteredLibraryItems.length" class="library-grid" :class="{ single: filteredLibraryItems.length === 1 }">
        <article v-for="item in filteredLibraryItems" :key="item.gameId" class="panel game-card">
          <div class="card-head">
            <div class="card-title-block">
              <h3>{{ item.gameId }}</h3>
              <div class="card-meta">
                <span>规则 {{ item.enabledRules }}/{{ item.totalRules }} 已启用</span>
                <span>路径 {{ item.confirmedPathCount }}</span>
                <span>规则更新 {{ formatUnixTs(item.lastRuleUpdatedAt) }}</span>
                <span v-if="item.lastSessionStatus">
                  最近会话 {{ item.lastSessionStatus }} @ {{ formatUnixTs(item.lastSessionUpdatedAt || "") }}
                </span>
                <span v-if="item.lastInjectionStatus">injection={{ item.lastInjectionStatus }}</span>
              </div>
            </div>
            <button
              class="ghost"
              type="button"
              :disabled="libraryState.loading || isCardBusy(item.gameId)"
              @click="toggleLibraryDetails(item.gameId)"
            >
              {{ isGameExpanded(item.gameId) ? "收起详情" : "展开详情" }}
            </button>
          </div>

          <label class="field">
            <span>启动 EXE</span>
            <div class="row">
              <input :value="item.preferredExePath || ''" readonly placeholder="尚未绑定 EXE，先点击右侧按钮" />
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(item.gameId)"
                @click="choosePreferredExeForGame(item.gameId)"
              >
                选择/更换 EXE
              </button>
            </div>
          </label>

          <div class="row">
            <button
              type="button"
              class="primary"
              :disabled="libraryState.loading || isCardBusy(item.gameId)"
              @click="launchLibraryGame(item.gameId, 'backup')"
            >
              启动游戏（自动备份）
            </button>
          </div>

          <details class="advanced-box">
            <summary>高级模式</summary>
            <div class="row">
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(item.gameId)"
                @click="launchLibraryGame(item.gameId, 'sandbox')"
              >
                沙盒启动
              </button>
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(item.gameId)"
                @click="launchLibraryGame(item.gameId, 'inject')"
              >
                注入启动
              </button>
            </div>
          </details>

          <section v-if="isGameExpanded(item.gameId)" class="panel card-detail">
            <div class="row">
              <h4>备份版本时间线</h4>
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(item.gameId)"
                @click="loadBackupVersionsForGame(item.gameId)"
              >
                刷新版本
              </button>
            </div>
            <ul v-if="backupVersionsFor(item.gameId).length" class="rule-list">
              <li v-for="version in backupVersionsFor(item.gameId)" :key="version.versionId">
                <div class="row">
                  <div>
                    <strong>{{ formatUnixTs(version.createdAt) }}</strong>
                    <p>versionId={{ version.versionId }} | files={{ version.fileCount }}</p>
                  </div>
                  <button
                    type="button"
                    class="primary"
                    :disabled="libraryState.loading || isCardBusy(item.gameId)"
                    @click="rollbackToLibraryBackupVersion(item.gameId, version.versionId)"
                  >
                    回滚到此版本
                  </button>
                </div>
              </li>
            </ul>
            <p v-else>暂无备份版本。</p>

            <div class="row">
              <h4>最近会话日志</h4>
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(item.gameId)"
                @click="loadSessionDetailsForGame(item.gameId)"
              >
                刷新日志
              </button>
            </div>
            <template v-if="sessionDetailsFor(item.gameId)">
              <p>sessionId={{ sessionDetailsFor(item.gameId)?.launcherSessionId }}</p>
              <p>
                status={{ sessionDetailsFor(item.gameId)?.status }} |
                mode={{ sessionDetailsFor(item.gameId)?.launchMode ?? "backup" }} |
                pid={{ sessionDetailsFor(item.gameId)?.pid ?? "无" }}
              </p>
              <ul class="rule-list">
                <li
                  v-for="(log, idx) in (sessionDetailsFor(item.gameId)?.logs || []).slice(-10)"
                  :key="`${item.gameId}-${idx}`"
                >
                  {{ log }}
                </li>
              </ul>
            </template>
            <p v-else>暂无会话日志。</p>
          </section>
        </article>
      </div>
      <p v-else>暂无游戏卡片，请先在“学习存档”中生成规则。</p>

      <details v-if="redirectRuntimeInfo" class="runtime-diagnostics">
        <summary>运行时状态（高级）</summary>
        <p>备份目录（推荐模式）：<code>{{ redirectRuntimeInfo.backupRoot }}</code></p>
        <p>托管目录（注入模式）：<code>{{ redirectRuntimeInfo.managedSaveRoot }}</code></p>
        <p>沙盒根目录：<code>{{ redirectRuntimeInfo.sandboxRoot }}</code></p>
        <p>Sandboxie 路径：<code>{{ redirectRuntimeInfo.sandboxiePath }}</code></p>
        <p>Injector 路径：<code>{{ redirectRuntimeInfo.injectorPath }}</code></p>
        <p>Hook DLL 路径：<code>{{ redirectRuntimeInfo.dllPath }}</code></p>
      </details>
    </section>

    <p v-if="activeTab === 'learning' && learningState.info" class="info">{{ learningState.info }}</p>
    <p v-if="activeTab === 'learning' && learningState.error" class="error">{{ learningState.error }}</p>
    <p v-if="activeTab === 'rules' && rulesState.info" class="info">{{ rulesState.info }}</p>
    <p v-if="activeTab === 'rules' && rulesState.error" class="error">{{ rulesState.error }}</p>
    <p v-if="activeTab === 'library' && libraryState.info" class="info">{{ libraryState.info }}</p>
    <p v-if="activeTab === 'library' && libraryState.error" class="error">{{ libraryState.error }}</p>
  </main>
</template>
