<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useToast } from "./composables/useToast";
import {
  confirmRule,
  deleteRule,
  exportMigrationZip,
  exportRules,
  finishLearning,
  getLauncherSession,
  getBackupStats,
  getLearningSession,
  precheckGameLaunch,
  getRedirectRuntimeInfo,
  getRuntimeStatus,
  importRules,
  importMigrationZip,
  launchGame,
  launchGameFromLibrary,
  listRuleConflicts,
  listBackupVersions,
  listGameLibraryItems,
  pruneBackupVersions,
  listRules,
  openCandidatePath,
  restartAsAdmin,
  restoreBackupVersion,
  setPrimaryRule,
  setBackupKeepVersions,
  setPreferredExePath,
  startLearning,
  updateRule,
} from "./api";
import type {
  BackupStatsResult,
  BackupVersion,
  CandidatePath,
  GameLibraryItem,
  GameLaunchPrecheck,
  GameSaveRule,
  LauncherMode,
  LauncherSession,
  RedirectRuntimeInfo,
  RuleConflictItem,
} from "./types";

type UiStep = "setup" | "running" | "results";
type TopTab = "learning" | "rules" | "library";
type RuleDraft = {
  gameIdText: string;
  confirmedPathsText: string;
  enabled: boolean;
};
type TabState = {
  loading: boolean;
  error: string;
};
type CardAction =
  | "bind_exe"
  | "precheck"
  | "launch"
  | "backup_stats"
  | "backup_policy_save"
  | "backup_prune"
  | "backup_versions"
  | "backup_rollback"
  | "session_logs";
type ConfirmDialogState = {
  open: boolean;
  title: string;
  message: string;
  confirmText: string;
  cancelText: string;
  danger: boolean;
};
type RestoreUndoState = {
  gameId: string;
  versionId: string;
  restoredVersionId: string;
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
const ruleConflicts = ref<RuleConflictItem[]>([]);
const ruleSearch = ref("");
const ruleDrafts = ref<Record<string, RuleDraft>>({});
const learningState = ref<TabState>({ loading: false, error: "" });
const rulesState = ref<TabState>({ loading: false, error: "" });
const libraryState = ref<TabState>({ loading: false, error: "" });
const migrationExportWaiting = ref(false);
const eventCaptureMode = ref("unknown");
const capturedEventCount = ref(0);
const eventCaptureError = ref("");
const runtimeIsAdmin = ref(false);
const runtimeMessage = ref("");
const redirectRuntimeInfo = ref<RedirectRuntimeInfo | null>(null);
const libraryItems = ref<GameLibraryItem[]>([]);
const librarySearch = ref("");
const cardLoading = ref<Record<string, Partial<Record<CardAction, boolean>>>>({});
const libraryCardErrors = ref<Record<string, string>>({});
const selectedLibraryGameId = ref("");
const backupVersionsByGame = ref<Record<string, BackupVersion[]>>({});
const backupStatsByGame = ref<Record<string, BackupStatsResult | null>>({});
const backupKeepDraftByGame = ref<Record<string, string>>({});
const sessionDetailsByGame = ref<Record<string, LauncherSession | null>>({});
const launchPrecheckByGame = ref<Record<string, GameLaunchPrecheck | null>>({});
const restoreUndoByGame = ref<Record<string, RestoreUndoState | null>>({});
const hiddenPrecheckKeys = new Set(["sandbox_runtime", "inject_artifacts", "inject_arch"]);
const { toast, showToast, closeToast } = useToast();
const confirmDialog = ref<ConfirmDialogState>({
  open: false,
  title: "",
  message: "",
  confirmText: "确认",
  cancelText: "取消",
  danger: false,
});
const blockingErrorMessage = ref("");

let confirmResolver: ((value: boolean) => void) | null = null;

const hasHighConfidence = computed(() => candidates.value.some((item) => item.score >= 45));
const candidateGroups = computed(() => [
  {
    key: "strong",
    title: "强推荐",
    description: "最像真实存档目录，通常可以直接选择。",
    items: candidates.value.filter((item) => item.recommendation === "strong"),
  },
  {
    key: "recommended",
    title: "推荐",
    description: "命中了多个有效信号，建议打开目录确认。",
    items: candidates.value.filter((item) => item.recommendation === "recommended"),
  },
  {
    key: "possible",
    title: "可能相关",
    description: "有变化但证据不足，适合人工判断。",
    items: candidates.value.filter((item) => item.recommendation === "possible"),
  },
  {
    key: "weak",
    title: "低可信",
    description: "多为配置、缓存或弱信号，不会自动勾选。",
    items: candidates.value.filter((item) => item.recommendation === "weak"),
  },
]);
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
const selectedLibraryItem = computed(() => {
  const selectedKey = cardKey(selectedLibraryGameId.value);
  if (!selectedKey) return null;
  return filteredLibraryItems.value.find((item) => cardKey(item.gameId) === selectedKey) ?? null;
});
const ruleConflictByRuleId = computed<Record<string, RuleConflictItem>>(() => {
  const map: Record<string, RuleConflictItem> = {};
  for (const conflict of ruleConflicts.value) {
    for (const ruleId of conflict.ruleIds) {
      map[ruleId] = conflict;
    }
  }
  return map;
});

function inferGameId(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const fileName = parts[parts.length - 1] ?? "";
  const exeName = fileName.toLowerCase().endsWith(".exe") ? fileName.slice(0, -4) : fileName;
  const commonIndex = parts.findIndex((part) => part.toLowerCase() === "common");
  if (commonIndex >= 0 && commonIndex + 1 < parts.length - 1) {
    return cleanInferredGameId(parts[commonIndex + 1]);
  }
  if (!isGenericExeName(exeName)) {
    return cleanInferredGameId(exeName);
  }
  for (let index = parts.length - 2; index >= 0; index -= 1) {
    const candidate = parts[index] ?? "";
    if (candidate && !isGenericPathSegment(candidate)) {
      return cleanInferredGameId(candidate);
    }
  }
  return cleanInferredGameId(exeName || fileName);
}

function cleanInferredGameId(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function isGenericExeName(value: string): boolean {
  const normalized = value.trim().toLowerCase();
  return [
    "game",
    "game.exe",
    "main",
    "launcher",
    "start",
    "play",
    "nw",
    "node-webkit",
    "rpg_rt",
    "rpgvxace",
    "rpgmaker",
    "unitycrashhandler64",
    "unitycrashhandler32",
  ].includes(normalized) || normalized.endsWith("-win64-shipping") || normalized.endsWith("-win32-shipping");
}

function isGenericPathSegment(value: string): boolean {
  const normalized = value.trim().toLowerCase();
  return [
    "bin",
    "binaries",
    "win64",
    "win32",
    "x64",
    "x86",
    "windows",
    "release",
    "debug",
    "build",
    "dist",
  ].includes(normalized);
}

function normalizeGameId(gameIdText: string): string {
  return gameIdText.trim().toLowerCase();
}

function cardKey(gameIdText: string): string {
  return normalizeGameId(gameIdText);
}

function isCardBusy(gameIdText: string, action?: CardAction): boolean {
  const loadingMap = cardLoading.value[cardKey(gameIdText)];
  if (!loadingMap) return false;
  if (action) {
    return loadingMap[action] === true;
  }
  return Object.values(loadingMap).some((value) => value === true);
}

function setCardBusy(gameIdText: string, action: CardAction, busy: boolean) {
  const key = cardKey(gameIdText);
  const current = cardLoading.value[key] ?? {};
  if (busy) {
    cardLoading.value = {
      ...cardLoading.value,
      [key]: {
        ...current,
        [action]: true,
      },
    };
    return;
  }
  const nextActions = { ...current };
  delete nextActions[action];
  if (!Object.keys(nextActions).length) {
    const next = { ...cardLoading.value };
    delete next[key];
    cardLoading.value = next;
    return;
  }
  cardLoading.value = {
    ...cardLoading.value,
    [key]: nextActions,
  };
}

function setLibraryCardError(gameIdText: string, message: string) {
  const key = cardKey(gameIdText);
  libraryCardErrors.value = {
    ...libraryCardErrors.value,
    [key]: message,
  };
}

function clearLibraryCardError(gameIdText: string) {
  const key = cardKey(gameIdText);
  if (!(key in libraryCardErrors.value)) return;
  const next = { ...libraryCardErrors.value };
  delete next[key];
  libraryCardErrors.value = next;
}

function clearAllLibraryCardErrors() {
  libraryCardErrors.value = {};
}

function libraryCardErrorFor(gameIdText: string): string {
  return libraryCardErrors.value[cardKey(gameIdText)] ?? "";
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

function hydrateRuleDrafts() {
  const next: Record<string, RuleDraft> = {};
  for (const rule of rules.value) {
    const previous = ruleDrafts.value[rule.ruleId];
    next[rule.ruleId] = {
      gameIdText: previous?.gameIdText ?? rule.gameId,
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

function ensureSelectedLibraryGame() {
  if (!filteredLibraryItems.value.length) {
    selectedLibraryGameId.value = "";
    return;
  }
  const selectedKey = cardKey(selectedLibraryGameId.value);
  const stillVisible = filteredLibraryItems.value.some((item) => cardKey(item.gameId) === selectedKey);
  if (!stillVisible) {
    selectedLibraryGameId.value = filteredLibraryItems.value[0].gameId;
  }
}

function isLibraryGameSelected(gameIdText: string): boolean {
  return cardKey(selectedLibraryGameId.value) === cardKey(gameIdText);
}

async function selectLibraryGame(gameIdText: string) {
  selectedLibraryGameId.value = gameIdText;
  await loadSelectedLibraryGameDetails();
}

async function loadSelectedLibraryGameDetails() {
  const gameIdText = selectedLibraryGameId.value;
  if (!gameIdText) return;
  await Promise.all([
    loadLaunchPrecheckForGame(gameIdText, false),
    loadBackupStatsForGame(gameIdText, false),
    loadBackupVersionsForGame(gameIdText, false),
    loadSessionDetailsForGame(gameIdText, false),
  ]);
}

function backupVersionsFor(gameIdText: string): BackupVersion[] {
  return backupVersionsByGame.value[cardKey(gameIdText)] ?? [];
}

function backupStatsFor(gameIdText: string): BackupStatsResult | null {
  return backupStatsByGame.value[cardKey(gameIdText)] ?? null;
}

function backupKeepDraftFor(gameIdText: string): string {
  const key = cardKey(gameIdText);
  const draft = backupKeepDraftByGame.value[key];
  if (typeof draft === "string") {
    return draft;
  }
  const stats = backupStatsFor(gameIdText);
  return String(stats?.keepVersions ?? 10);
}

function updateBackupKeepDraft(gameIdText: string, value: string) {
  const key = cardKey(gameIdText);
  backupKeepDraftByGame.value = {
    ...backupKeepDraftByGame.value,
    [key]: value,
  };
}

function onBackupKeepInput(gameIdText: string, event: Event) {
  const target = event.target as HTMLInputElement | null;
  updateBackupKeepDraft(gameIdText, target?.value ?? "");
}

function sessionDetailsFor(gameIdText: string): LauncherSession | null {
  return sessionDetailsByGame.value[cardKey(gameIdText)] ?? null;
}

function restoreUndoFor(gameIdText: string): RestoreUndoState | null {
  return restoreUndoByGame.value[cardKey(gameIdText)] ?? null;
}

function launchPrecheckFor(gameIdText: string): GameLaunchPrecheck | null {
  return launchPrecheckByGame.value[cardKey(gameIdText)] ?? null;
}

function visiblePrecheckChecks(gameIdText: string) {
  const precheck = launchPrecheckFor(gameIdText);
  if (!precheck) return [];
  return precheck.checks.filter((check) => !hiddenPrecheckKeys.has(check.key));
}

function candidateRecommendationLabel(item: CandidatePath): string {
  switch (item.recommendation) {
    case "strong":
      return "强推荐";
    case "recommended":
      return "推荐";
    case "possible":
      return "可能相关";
    default:
      return "低可信";
  }
}

function candidateRecommendationClass(item: CandidatePath): string {
  return item.recommendation || "weak";
}

function candidateSignalLabel(signal: string): string {
  if (signal === "time-window") return "刚刚发生变化";
  if (signal === "path-keyword" || signal === "save-path-keyword") return "路径像存档目录";
  if (signal === "game-name-path") return "路径包含游戏名";
  if (signal === "save-filename") return "文件名像存档";
  if (signal === "size-reasonable") return "文件大小合理";
  if (signal === "user-save-root") return "位于常见用户存档目录";
  if (signal === "game-dir") return "位于游戏目录";
  if (signal === "path-noise") return "包含缓存/日志等弱相关路径";
  if (signal === "filename-noise") return "文件名像配置/缓存/日志";
  if (signal.startsWith("extension:")) return `命中存档扩展名 .${signal.slice("extension:".length)}`;
  if (signal.startsWith("weak-extension:")) return `命中弱扩展名 .${signal.slice("weak-extension:".length)}`;
  return signal;
}

function candidateSignalSummary(item: CandidatePath): string {
  if (!item.matchedSignals.length) return "暂无明显理由";
  return item.matchedSignals.map(candidateSignalLabel).join(" / ");
}

function formatBytes(totalBytes: number): string {
  if (!Number.isFinite(totalBytes) || totalBytes <= 0) {
    return "0 B";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = totalBytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  const precision = value >= 100 || unitIndex === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(precision)} ${units[unitIndex]}`;
}

function parseBackupKeepDraft(gameIdText: string): number | null {
  const raw = backupKeepDraftFor(gameIdText).trim();
  if (!raw) return null;
  if (!/^\d+$/.test(raw)) {
    return null;
  }
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 1) {
    return null;
  }
  return Math.min(Math.trunc(parsed), 200);
}

function showBlockingError(message: string) {
  blockingErrorMessage.value = message;
  showToast("操作失败，请查看错误详情", "error", 3200);
}

function closeBlockingError() {
  blockingErrorMessage.value = "";
}

watch(librarySearch, () => {
  ensureSelectedLibraryGame();
  void loadSelectedLibraryGameDetails();
});

function askConfirm(options: {
  title: string;
  message: string;
  confirmText?: string;
  cancelText?: string;
  danger?: boolean;
}) {
  return new Promise<boolean>((resolve) => {
    confirmResolver = resolve;
    confirmDialog.value = {
      open: true,
      title: options.title,
      message: options.message,
      confirmText: options.confirmText ?? "确认",
      cancelText: options.cancelText ?? "取消",
      danger: options.danger ?? false,
    };
  });
}

function resolveConfirm(result: boolean) {
  const resolver = confirmResolver;
  confirmResolver = null;
  confirmDialog.value.open = false;
  if (resolver) {
    resolver(result);
  }
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
  try {
    const trimmedGameId = gameId.value.trim();
    const trimmedExePath = exePath.value.trim();
    if (!trimmedGameId || !trimmedExePath) {
      throw new Error("请先填写 gameId 并选择 exePath");
    }
    sessionId.value = await startLearning(trimmedGameId, trimmedExePath);
    pid.value = await launchGame(sessionId.value);
    step.value = "running";
  } catch (err) {
    learningState.value.error = String(err);
  } finally {
    learningState.value.loading = false;
  }
}

async function endLearning() {
  learningState.value.loading = true;
  learningState.value.error = "";
  try {
    candidates.value = await finishLearning(sessionId.value);
    const session = await getLearningSession(sessionId.value);
    eventCaptureMode.value = session.eventCaptureMode ?? "unknown";
    capturedEventCount.value = session.capturedEventCount ?? 0;
    eventCaptureError.value = session.eventCaptureError ?? "";
    const autoSelectable = candidates.value.filter(
      (item) => item.recommendation === "strong" || item.recommendation === "recommended",
    );
    selected.value = autoSelectable.slice(0, 2).map((item) => item.path);
    step.value = "results";
    if (!hasHighConfidence.value) {
      showToast("未检测到高可信候选，请确认学习阶段已执行存档动作", "info", 3600);
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
  try {
    const ruleId = await confirmRule(sessionId.value, selected.value);
    void ruleId;
    showToast("规则保存成功", "success");
    await refreshRules();
    await refreshLibraryItems();
    activeTab.value = "library";
    selectedLibraryGameId.value = gameId.value.trim();
    void loadSelectedLibraryGameDetails();
  } catch (err) {
    learningState.value.error = String(err);
    showToast("规则保存失败", "error");
  } finally {
    learningState.value.loading = false;
  }
}

async function refreshRules() {
  const [data, conflicts] = await Promise.all([listRules(), listRuleConflicts()]);
  rules.value = sortRulesByUpdatedTime(data);
  ruleConflicts.value = conflicts;
  hydrateRuleDrafts();
}

async function refreshLibraryItems() {
  const data = await listGameLibraryItems();
  libraryItems.value = sortLibraryItems(data);
  ensureSelectedLibraryGame();
}

async function reloadLibraryWithLoading() {
  libraryState.value.loading = true;
  libraryState.value.error = "";
  clearAllLibraryCardErrors();
  try {
    await refreshLibraryItems();
    await loadRedirectRuntimeInfo();
    void loadSelectedLibraryGameDetails();
    void refreshLaunchPrechecksForLibraryItems();
    void refreshBackupStatsForLibraryItems();
  } catch (err) {
    libraryState.value.error = `读取游戏库失败：${String(err)}`;
  } finally {
    libraryState.value.loading = false;
  }
}

async function refreshLaunchPrechecksForLibraryItems() {
  const items = [...libraryItems.value];
  if (!items.length) {
    launchPrecheckByGame.value = {};
    return;
  }
  await Promise.all(items.map((item) => loadLaunchPrecheckForGame(item.gameId, false)));
}

async function refreshBackupStatsForLibraryItems() {
  const items = [...libraryItems.value];
  if (!items.length) {
    backupStatsByGame.value = {};
    backupKeepDraftByGame.value = {};
    return;
  }
  await Promise.all(items.map((item) => loadBackupStatsForGame(item.gameId, false)));
}

async function loadLaunchPrecheckForGame(gameIdText: string, withCardLoading = true) {
  const key = cardKey(gameIdText);
  if (withCardLoading) {
    setCardBusy(gameIdText, "precheck", true);
  }
  clearLibraryCardError(gameIdText);
  try {
    const precheck = await precheckGameLaunch(gameIdText);
    launchPrecheckByGame.value = {
      ...launchPrecheckByGame.value,
      [key]: precheck,
    };
  } catch (err) {
    setLibraryCardError(gameIdText, `读取启动前检查失败：${String(err)}`);
    launchPrecheckByGame.value = {
      ...launchPrecheckByGame.value,
      [key]: null,
    };
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, "precheck", false);
    }
  }
}

async function loadRedirectRuntimeInfo() {
  redirectRuntimeInfo.value = await getRedirectRuntimeInfo();
}

async function loadBackupVersionsForGame(gameIdText: string, withCardLoading = true) {
  const key = cardKey(gameIdText);
  if (withCardLoading) {
    setCardBusy(gameIdText, "backup_versions", true);
  }
  clearLibraryCardError(gameIdText);
  try {
    backupVersionsByGame.value = {
      ...backupVersionsByGame.value,
      [key]: await listBackupVersions(gameIdText),
    };
  } catch (err) {
    setLibraryCardError(gameIdText, `读取备份版本失败：${String(err)}`);
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, "backup_versions", false);
    }
  }
}

async function loadBackupStatsForGame(gameIdText: string, withCardLoading = true) {
  const key = cardKey(gameIdText);
  if (withCardLoading) {
    setCardBusy(gameIdText, "backup_stats", true);
  }
  clearLibraryCardError(gameIdText);
  try {
    const stats = await getBackupStats(gameIdText);
    backupStatsByGame.value = {
      ...backupStatsByGame.value,
      [key]: stats,
    };
    backupKeepDraftByGame.value = {
      ...backupKeepDraftByGame.value,
      [key]: String(stats.keepVersions),
    };
  } catch (err) {
    backupStatsByGame.value = {
      ...backupStatsByGame.value,
      [key]: null,
    };
    setLibraryCardError(gameIdText, `读取备份统计失败：${String(err)}`);
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, "backup_stats", false);
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
    setCardBusy(gameIdText, "session_logs", true);
  }
  clearLibraryCardError(gameIdText);
  try {
    const detail = await getLauncherSession(item.lastSessionId);
    sessionDetailsByGame.value = {
      ...sessionDetailsByGame.value,
      [key]: detail,
    };
  } catch (err) {
    setLibraryCardError(gameIdText, `读取会话详情失败：${String(err)}`);
  } finally {
    if (withCardLoading) {
      setCardBusy(gameIdText, "session_logs", false);
    }
  }
}

async function choosePreferredExeForGame(gameIdText: string) {
  try {
    selectedLibraryGameId.value = gameIdText;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const chosen = await open({
      multiple: false,
      filters: [{ name: "Executable", extensions: ["exe"] }],
    });
    if (!chosen || Array.isArray(chosen)) return;
    setCardBusy(gameIdText, "bind_exe", true);
    libraryState.value.error = "";
    clearLibraryCardError(gameIdText);
    await setPreferredExePath(gameIdText, chosen);
    await refreshLibraryItems();
    await loadLaunchPrecheckForGame(gameIdText, false);
    showToast(`${gameIdText} 启动 EXE 已更新`, "success");
  } catch (err) {
    setLibraryCardError(gameIdText, `绑定 EXE 失败：${String(err)}`);
    showToast("绑定 EXE 失败", "error");
  } finally {
    setCardBusy(gameIdText, "bind_exe", false);
  }
}

async function launchLibraryGame(gameIdText: string, mode: LauncherMode = "backup") {
  selectedLibraryGameId.value = gameIdText;
  setCardBusy(gameIdText, "launch", true);
  libraryState.value.error = "";
  clearLibraryCardError(gameIdText);
  try {
    await launchGameFromLibrary(gameIdText, mode);
    await refreshLibraryItems();
    await loadLaunchPrecheckForGame(gameIdText, false);
    await Promise.all([
      loadBackupStatsForGame(gameIdText, false),
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
    showToast(`${gameIdText} 启动成功`, "success");
  } catch (err) {
    setLibraryCardError(gameIdText, String(err));
    showBlockingError(String(err));
    await refreshLibraryItems();
  } finally {
    setCardBusy(gameIdText, "launch", false);
  }
}

async function rollbackToLibraryBackupVersion(gameIdText: string, versionId: string) {
  const confirmed = await askConfirm({
    title: "确认回滚",
    message: `确定回滚 ${gameIdText} 到版本 ${versionId} 吗？此操作会覆盖当前存档。`,
    confirmText: "确认回滚",
    cancelText: "取消",
    danger: true,
  });
  if (!confirmed) {
    return;
  }
  setCardBusy(gameIdText, "backup_rollback", true);
  libraryState.value.error = "";
  clearLibraryCardError(gameIdText);
  try {
    const result = await restoreBackupVersion(gameIdText, versionId);
    await refreshLibraryItems();
    await Promise.all([
      loadBackupStatsForGame(gameIdText, false),
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
    void result;
    if (result.preRestoreVersionId) {
      restoreUndoByGame.value = {
        ...restoreUndoByGame.value,
        [cardKey(gameIdText)]: {
          gameId: gameIdText,
          versionId: result.preRestoreVersionId,
          restoredVersionId: versionId,
        },
      };
    }
    showToast(
      `回滚完成（已校验 ${result.verifiedFiles} 个文件，哈希抽样 ${result.hashSampleCount} 项）`,
      "success",
    );
  } catch (err) {
    setLibraryCardError(gameIdText, `回滚失败：${String(err)}`);
    showBlockingError(`回滚失败：${String(err)}`);
  } finally {
    setCardBusy(gameIdText, "backup_rollback", false);
  }
}

async function undoLibraryRestore(gameIdText: string) {
  const undo = restoreUndoFor(gameIdText);
  if (!undo) return;
  const confirmed = await askConfirm({
    title: "撤销本次恢复",
    message: `确定恢复到回滚前备份 ${undo.versionId} 吗？此操作会再次覆盖当前存档。`,
    confirmText: "撤销恢复",
    cancelText: "取消",
    danger: true,
  });
  if (!confirmed) return;
  setCardBusy(gameIdText, "backup_rollback", true);
  libraryState.value.error = "";
  clearLibraryCardError(gameIdText);
  try {
    await restoreBackupVersion(gameIdText, undo.versionId);
    restoreUndoByGame.value = {
      ...restoreUndoByGame.value,
      [cardKey(gameIdText)]: null,
    };
    await Promise.all([
      loadBackupStatsForGame(gameIdText, false),
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
    showToast("已撤销本次恢复", "success");
  } catch (err) {
    setLibraryCardError(gameIdText, `撤销恢复失败：${String(err)}`);
    showBlockingError(`撤销恢复失败：${String(err)}`);
  } finally {
    setCardBusy(gameIdText, "backup_rollback", false);
  }
}

async function saveBackupKeepPolicy(gameIdText: string) {
  const keep = parseBackupKeepDraft(gameIdText);
  if (!keep) {
    setLibraryCardError(gameIdText, "保留版本数必须是大于等于 1 的整数。");
    showToast("请输入有效的保留版本数", "error");
    return;
  }
  setCardBusy(gameIdText, "backup_policy_save", true);
  libraryState.value.error = "";
  clearLibraryCardError(gameIdText);
  try {
    const stats = await setBackupKeepVersions(gameIdText, keep);
    backupStatsByGame.value = {
      ...backupStatsByGame.value,
      [cardKey(gameIdText)]: stats,
    };
    backupKeepDraftByGame.value = {
      ...backupKeepDraftByGame.value,
      [cardKey(gameIdText)]: String(stats.keepVersions),
    };
    showToast("备份保留策略已保存", "success");
  } catch (err) {
    setLibraryCardError(gameIdText, `保存备份策略失败：${String(err)}`);
    showToast("保存备份策略失败", "error");
  } finally {
    setCardBusy(gameIdText, "backup_policy_save", false);
  }
}

async function pruneOldBackupsForGame(gameIdText: string) {
  const keep = parseBackupKeepDraft(gameIdText);
  if (!keep) {
    setLibraryCardError(gameIdText, "保留版本数必须是大于等于 1 的整数。");
    showToast("请输入有效的保留版本数", "error");
    return;
  }
  setCardBusy(gameIdText, "backup_prune", true);
  libraryState.value.error = "";
  clearLibraryCardError(gameIdText);
  try {
    const result = await pruneBackupVersions(gameIdText, keep);
    await Promise.all([
      loadBackupStatsForGame(gameIdText, false),
      loadBackupVersionsForGame(gameIdText, false),
    ]);
    void result;
    showToast("旧备份已清理", "success");
  } catch (err) {
    setLibraryCardError(gameIdText, `清理备份失败：${String(err)}`);
    showToast("清理备份失败", "error");
  } finally {
    setCardBusy(gameIdText, "backup_prune", false);
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

async function markPrimaryRule(rule: GameSaveRule) {
  const conflict = ruleConflictFor(rule.ruleId);
  if (!conflict) {
    rulesState.value.error = "当前规则不存在 exeHash 冲突，无需设置主规则。";
    return;
  }
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await setPrimaryRule(rule.ruleId);
    await refreshRules();
    await refreshLibraryItems();
    showToast("主规则设置成功", "success");
  } catch (err) {
    rulesState.value.error = `设置主规则失败：${String(err)}`;
    showToast("设置主规则失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function saveManagedRule(rule: GameSaveRule) {
  const draft = ruleDrafts.value[rule.ruleId];
  if (!draft) return;
  const normalizedGameId = draft.gameIdText.trim();
  if (!normalizedGameId) {
    rulesState.value.error = "游戏名不能为空。";
    return;
  }
  const normalizedPaths = normalizePaths(draft.confirmedPathsText);
  if (!normalizedPaths.length) {
    rulesState.value.error = "路径不能为空，至少保留一条路径。";
    return;
  }

  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    const updated = await updateRule(rule.ruleId, normalizedGameId, normalizedPaths, draft.enabled);
    await refreshRules();
    showToast(`规则 ${updated.gameId} 已保存`, "success");
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `保存规则失败：${String(err)}`;
    showToast("保存规则失败", "error");
  } finally {
    rulesState.value.loading = false;
  }
}

async function removeManagedRule(rule: GameSaveRule) {
  const confirmed = await askConfirm({
    title: "确认删除规则",
    message: `确定删除规则 ${rule.gameId} 吗？此操作不可恢复。`,
    confirmText: "删除",
    cancelText: "取消",
    danger: true,
  });
  if (!confirmed) {
    return;
  }
  rulesState.value.loading = true;
  rulesState.value.error = "";
  try {
    await deleteRule(rule.ruleId);
    await refreshRules();
    showToast(`规则 ${rule.gameId} 已删除`, "success");
    await refreshLibraryItems();
  } catch (err) {
    rulesState.value.error = `删除规则失败：${String(err)}`;
    showToast("删除规则失败", "error");
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
    const result = await exportRules(chosen);
    void result;
    showToast("规则导出成功", "success");
  } catch (err) {
    rulesState.value.error = `导出失败：${String(err)}`;
    showToast("规则导出失败", "error");
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
    const result = await importRules(chosen);
    await refreshRules();
    await refreshLibraryItems();
    void result;
    showToast("规则导入完成", "success");
  } catch (err) {
    rulesState.value.error = `导入失败：${String(err)}`;
    showToast("规则导入失败", "error");
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
    migrationExportWaiting.value = true;
    const result = await exportMigrationZip(chosen);
    void result;
    showToast("迁移包导出成功", "success");
  } catch (err) {
    rulesState.value.error = `导出迁移包失败：${String(err)}`;
    showToast("迁移包导出失败", "error");
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
    const result = await importMigrationZip(chosen);
    await refreshRules();
    await refreshLibraryItems();
    void result;
    showToast("迁移包导入完成", "success");
  } catch (err) {
    rulesState.value.error = `导入迁移包失败：${String(err)}`;
    showToast("迁移包导入失败", "error");
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

onUnmounted(() => {
  confirmResolver = null;
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
      <header class="panel learning-hero">
        <span class="eyebrow">学习存档</span>
        <h1>把游戏加入 GameSaver</h1>
        <p>选择游戏程序，启动后手动保存一次。GameSaver 会根据文件变化推荐存档目录。</p>
        <p v-if="learningState.error" class="error inline-error">{{ learningState.error }}</p>
        <div class="learning-progress">
          <span :class="{ active: step === 'setup', done: step !== 'setup' }">添加游戏</span>
          <span :class="{ active: step === 'running', done: step === 'results' }">执行一次存档</span>
          <span :class="{ active: step === 'results' }">选择存档目录</span>
        </div>
      </header>

      <section v-if="step === 'setup'" class="panel learning-card">
        <span class="eyebrow">第一步</span>
        <h2>添加游戏</h2>
        <p class="learning-copy">选择游戏 EXE 后会自动推断游戏名称。名称只是显示和管理用，可以手动修改。</p>
        <label class="field">
          <span>游戏名称</span>
          <input v-model="gameId" placeholder="例如：MonsterBlackMarket" />
        </label>
        <label class="field">
          <span>游戏 EXE 路径</span>
          <div class="row">
            <input v-model="exePath" placeholder="D:\\Games\\xxx\\game.exe" />
            <button type="button" @click="chooseExePath">浏览</button>
          </div>
        </label>
        <button :disabled="learningState.loading" type="button" class="primary" @click="beginLearning">
          {{ learningState.loading ? "正在启动..." : "开始学习并启动游戏" }}
        </button>
      </section>

      <section v-else-if="step === 'running'" class="panel learning-card">
        <span class="eyebrow">第二步</span>
        <h2>进入游戏并手动保存一次</h2>
        <p class="learning-copy">请在游戏里完成一次明确的存档动作。保存完成后，可以退出游戏，也可以保持游戏关闭后再点击分析。</p>
        <ul class="learning-checklist">
          <li>游戏已启动</li>
          <li>进入游戏或读取一个已有存档</li>
          <li>手动保存一次</li>
          <li>回到 GameSaver 点击分析</li>
        </ul>
        <button :disabled="learningState.loading" type="button" class="primary" @click="endLearning">
          {{ learningState.loading ? "正在分析..." : "我已经保存，开始分析" }}
        </button>
        <details class="runtime-diagnostics learning-advanced">
          <summary>采集详情（高级）</summary>
          <p>运行权限：{{ runtimeIsAdmin ? "管理员" : "普通用户" }}</p>
          <p>会话 ID：<code>{{ sessionId }}</code></p>
          <p>游戏 PID：{{ pid ?? "未获取" }}</p>
          <p>{{ runtimeMessage }}</p>
          <button v-if="!runtimeIsAdmin" type="button" @click="relaunchAsAdmin">一键管理员重启</button>
        </details>
      </section>

      <section v-else class="panel learning-card">
        <span class="eyebrow">第三步</span>
        <h2>选择存档目录</h2>
        <p class="learning-copy">优先确认“强推荐”和“推荐”。如果不确定，可以打开目录查看里面是否有存档文件。</p>
        <details class="runtime-diagnostics learning-advanced">
          <summary>采集详情（高级）</summary>
          <p>采集模式：{{ eventCaptureMode }} | 捕获事件数：{{ capturedEventCount }}</p>
          <p v-if="eventCaptureError" class="error">ETW 信息：{{ eventCaptureError }}</p>
        </details>
        <p v-if="!candidates.length" class="empty-hint">没有检测到候选目录。请确认刚才在游戏内执行了保存动作。</p>
        <div v-else class="candidate-groups">
          <section
            v-for="group in candidateGroups"
            :key="group.key"
            v-show="group.items.length"
            class="candidate-group"
          >
            <div class="candidate-group-head">
              <div>
                <h3>{{ group.title }}</h3>
                <p>{{ group.description }}</p>
              </div>
              <span>{{ group.items.length }} 项</span>
            </div>
            <ul class="candidate-list">
              <li v-for="item in group.items" :key="item.path" :class="{ collapsed: item.collapsed }">
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
                  <span class="candidate-rank" :class="candidateRecommendationClass(item)">
                    {{ candidateRecommendationLabel(item) }}
                  </span>
                  <button type="button" @click="openPath(item.path)">打开目录</button>
                </div>
                <p>
                  得分：{{ item.score }} | changed={{ item.changedFiles }} added={{ item.addedFiles }}
                  modified={{ item.modifiedFiles }}
                </p>
                <p>推荐理由：{{ candidateSignalSummary(item) }}</p>
              </li>
            </ul>
          </section>
        </div>
        <div class="row">
          <button :disabled="learningState.loading" type="button" class="primary" @click="saveLearningRule">
            保存到游戏库
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
                      @click="markPrimaryRule(rule)"
                    >
                      设为主规则
                    </button>
                  </div>
                </section>
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
              <span>游戏名（gameId）</span>
              <input
                v-model="ruleDrafts[rule.ruleId].gameIdText"
                type="text"
                class="gameid-editor"
                placeholder="例如：elden_ring"
              />
            </label>
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
        </section>
        <p v-if="libraryState.error" class="error inline-error">{{ libraryState.error }}</p>
      </header>

      <div v-if="filteredLibraryItems.length" class="library-layout">
        <div class="library-grid" :class="{ single: filteredLibraryItems.length === 1 }">
          <article
            v-for="item in filteredLibraryItems"
            :key="item.gameId"
            class="panel game-card"
            :class="{ selected: isLibraryGameSelected(item.gameId) }"
            @click="selectLibraryGame(item.gameId)"
          >
            <p v-if="libraryCardErrorFor(item.gameId)" class="error inline-error card-error">
              {{ libraryCardErrorFor(item.gameId) }}
            </p>
            <div class="library-game-row">
              <span class="game-status-dot" :class="item.preferredExePath ? 'ready' : 'missing'"></span>
              <div class="library-game-main">
                <h3>{{ item.gameId }}</h3>
                <p>
                  <span>规则 {{ item.enabledRules }}/{{ item.totalRules }}</span>
                  <span v-if="backupStatsFor(item.gameId)">
                    备份 {{ backupStatsFor(item.gameId)?.versionCount ?? 0 }} 版
                  </span>
                  <span v-else>备份读取中</span>
                </p>
              </div>
              <span v-if="item.lastSessionStatus" class="session-mini">{{ item.lastSessionStatus }}</span>
            </div>
          </article>
        </div>

        <aside v-if="selectedLibraryItem" class="panel library-detail-panel">
          <p v-if="libraryCardErrorFor(selectedLibraryItem.gameId)" class="error inline-error card-error">
            {{ libraryCardErrorFor(selectedLibraryItem.gameId) }}
          </p>
          <div class="detail-head">
            <div>
              <span class="eyebrow">当前选中</span>
              <h3>{{ selectedLibraryItem.gameId }}</h3>
            </div>
            <button
              type="button"
              class="primary"
              :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'launch')"
              @click="launchLibraryGame(selectedLibraryItem.gameId, 'backup')"
            >
              启动游戏（自动备份）
            </button>
          </div>

          <label class="field">
            <span>启动 EXE</span>
            <div class="row">
              <input
                :value="selectedLibraryItem.preferredExePath || ''"
                readonly
                placeholder="尚未绑定 EXE，先点击右侧按钮"
              />
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'bind_exe')"
                @click="choosePreferredExeForGame(selectedLibraryItem.gameId)"
              >
                选择/更换 EXE
              </button>
            </div>
          </label>

          <section class="precheck-box">
            <div class="row precheck-head">
              <strong>启动前预检查</strong>
              <div class="row precheck-head-actions">
                <span
                  v-if="launchPrecheckFor(selectedLibraryItem.gameId)"
                  class="precheck-state-pill"
                  :class="launchPrecheckFor(selectedLibraryItem.gameId)?.backupReady ? 'ok' : 'fail'"
                >
                  {{ launchPrecheckFor(selectedLibraryItem.gameId)?.backupReady ? "自动备份可启动" : "自动备份需处理" }}
                </span>
                <span v-else class="precheck-state-pill idle">未检查</span>
                <button
                  type="button"
                  :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'precheck')"
                  @click="loadLaunchPrecheckForGame(selectedLibraryItem.gameId)"
                >
                  刷新
                </button>
              </div>
            </div>
            <template v-if="launchPrecheckFor(selectedLibraryItem.gameId)">
              <details v-if="visiblePrecheckChecks(selectedLibraryItem.gameId).length" class="precheck-details">
                <summary>查看检查明细（{{ visiblePrecheckChecks(selectedLibraryItem.gameId).length }} 项）</summary>
                <ul class="precheck-list">
                  <li
                    v-for="check in visiblePrecheckChecks(selectedLibraryItem.gameId)"
                    :key="`${selectedLibraryItem.gameId}-${check.key}`"
                  >
                    <span class="precheck-badge" :class="check.ok ? 'ok' : 'fail'">{{ check.ok ? "OK" : "FAIL" }}</span>
                    <span class="precheck-label">{{ check.label }}</span>
                    <span class="precheck-detail">{{ check.detail }}</span>
                  </li>
                </ul>
              </details>
            </template>
            <p v-else class="empty-hint">尚未检查，点击“刷新”查看启动条件。</p>
          </section>

          <section class="backup-detail-stack">
            <section class="backup-policy-box">
              <div class="row backup-policy-head">
                <h4>备份空间管理</h4>
                <button
                  type="button"
                  :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'backup_stats')"
                  @click="loadBackupStatsForGame(selectedLibraryItem.gameId)"
                >
                  刷新统计
                </button>
              </div>
              <div class="backup-policy-stats">
                <span>当前占用：{{ formatBytes(backupStatsFor(selectedLibraryItem.gameId)?.totalBytes ?? 0) }}</span>
                <span>版本数：{{ backupStatsFor(selectedLibraryItem.gameId)?.versionCount ?? 0 }}</span>
                <span>当前保留策略：最近 {{ backupStatsFor(selectedLibraryItem.gameId)?.keepVersions ?? 10 }} 版</span>
                <span v-if="backupStatsFor(selectedLibraryItem.gameId)?.latestVersionId">
                  最新版本：{{ backupStatsFor(selectedLibraryItem.gameId)?.latestVersionId }}
                </span>
              </div>
              <div class="row backup-policy-controls">
                <label class="backup-keep-input">
                  <span>保留最近 N 版</span>
                  <input
                    :value="backupKeepDraftFor(selectedLibraryItem.gameId)"
                    type="number"
                    min="1"
                    max="200"
                    step="1"
                    @input="onBackupKeepInput(selectedLibraryItem.gameId, $event)"
                  />
                </label>
                <button
                  type="button"
                  :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'backup_policy_save')"
                  @click="saveBackupKeepPolicy(selectedLibraryItem.gameId)"
                >
                  保存策略
                </button>
                <button
                  type="button"
                  class="danger"
                  :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'backup_prune')"
                  @click="pruneOldBackupsForGame(selectedLibraryItem.gameId)"
                >
                  一键清理旧备份
                </button>
              </div>
            </section>

            <div class="row">
              <h4>备份版本时间线</h4>
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'backup_versions')"
                @click="loadBackupVersionsForGame(selectedLibraryItem.gameId)"
              >
                刷新版本
              </button>
            </div>
            <ul v-if="backupVersionsFor(selectedLibraryItem.gameId).length" class="backup-timeline">
              <li
                v-for="version in backupVersionsFor(selectedLibraryItem.gameId)"
                :key="version.versionId"
                :class="{ 'pre-restore': version.label === '回滚前备份' }"
              >
                <div class="backup-version-main">
                  <div class="backup-version-title">
                    <span class="backup-version-label">{{ version.label }}</span>
                    <strong>{{ formatUnixTs(version.createdAt) }}</strong>
                  </div>
                  <p>{{ version.fileCount }} 个文件</p>
                  <details class="backup-version-id">
                    <summary>查看版本 ID</summary>
                    <code>{{ version.versionId }}</code>
                  </details>
                </div>
                <div class="backup-version-actions">
                  <button
                    type="button"
                    class="primary"
                    :disabled="
                      libraryState.loading ||
                      isCardBusy(selectedLibraryItem.gameId, 'backup_rollback') ||
                      !version.restorable
                    "
                    @click="rollbackToLibraryBackupVersion(selectedLibraryItem.gameId, version.versionId)"
                  >
                    回滚到此版本
                  </button>
                </div>
              </li>
            </ul>
            <p v-else>暂无备份版本。</p>
            <section v-if="restoreUndoFor(selectedLibraryItem.gameId)" class="restore-undo-box">
              <div>
                <strong>可撤销本次恢复</strong>
                <p>
                  已从 {{ restoreUndoFor(selectedLibraryItem.gameId)?.restoredVersionId }} 恢复。
                  回滚前备份：{{ restoreUndoFor(selectedLibraryItem.gameId)?.versionId }}
                </p>
              </div>
              <button
                type="button"
                class="danger"
                :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'backup_rollback')"
                @click="undoLibraryRestore(selectedLibraryItem.gameId)"
              >
                撤销本次恢复
              </button>
            </section>

            <div class="row">
              <h4>最近会话日志</h4>
              <button
                type="button"
                :disabled="libraryState.loading || isCardBusy(selectedLibraryItem.gameId, 'session_logs')"
                @click="loadSessionDetailsForGame(selectedLibraryItem.gameId)"
              >
                刷新日志
              </button>
            </div>
            <template v-if="sessionDetailsFor(selectedLibraryItem.gameId)">
              <p>sessionId={{ sessionDetailsFor(selectedLibraryItem.gameId)?.launcherSessionId }}</p>
              <p>
                status={{ sessionDetailsFor(selectedLibraryItem.gameId)?.status }} |
                mode={{ sessionDetailsFor(selectedLibraryItem.gameId)?.launchMode ?? "backup" }} |
                pid={{ sessionDetailsFor(selectedLibraryItem.gameId)?.pid ?? "无" }}
              </p>
              <ul class="rule-list">
                <li
                  v-for="(log, idx) in (sessionDetailsFor(selectedLibraryItem.gameId)?.logs || []).slice(-10)"
                  :key="`${selectedLibraryItem.gameId}-${idx}`"
                >
                  {{ log }}
                </li>
              </ul>
            </template>
            <p v-else>暂无会话日志。</p>
          </section>
          <p class="mode-hint">当前阶段仅开放自动备份启动。沙盒/注入模式已纳入开发计划。</p>
        </aside>
      </div>
      <p v-else>暂无游戏卡片，请先在“学习存档”中生成规则。</p>

      <details v-if="redirectRuntimeInfo" class="runtime-diagnostics">
        <summary>运行时状态（高级）</summary>
        <p>备份目录（推荐模式）：<code>{{ redirectRuntimeInfo.backupRoot }}</code></p>
      </details>
    </section>

    <transition name="toast-fade">
      <div v-if="toast.visible" class="toast" :class="toast.level" role="status" aria-live="polite">
        <span>{{ toast.message }}</span>
        <button type="button" class="toast-close" @click="closeToast">关闭</button>
      </div>
    </transition>

    <div v-if="confirmDialog.open" class="modal-overlay" role="dialog" aria-modal="true">
      <section class="modal">
        <h3>{{ confirmDialog.title }}</h3>
        <p>{{ confirmDialog.message }}</p>
        <div class="row modal-actions">
          <button type="button" @click="resolveConfirm(false)">{{ confirmDialog.cancelText }}</button>
          <button
            type="button"
            :class="confirmDialog.danger ? 'danger' : 'primary'"
            @click="resolveConfirm(true)"
          >
            {{ confirmDialog.confirmText }}
          </button>
        </div>
      </section>
    </div>

    <div v-if="blockingErrorMessage" class="modal-overlay" role="dialog" aria-modal="true">
      <section class="modal blocking-modal">
        <h3>操作被阻止</h3>
        <p>{{ blockingErrorMessage }}</p>
        <div class="row modal-actions">
          <button type="button" class="primary" @click="closeBlockingError">我知道了</button>
        </div>
      </section>
    </div>
  </main>
</template>
