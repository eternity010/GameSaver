import { computed, ref, watch, type Ref } from "vue";
import {
  getBackupStats,
  getLauncherSession,
  launchGameFromLibrary,
  listBackupVersions,
  listGameLibraryItems,
  precheckGameLaunch,
  pruneBackupVersions,
  setBackupKeepVersions,
  setPreferredExePath,
  startRestoreBackupVersionTask,
} from "../api";
import type {
  BackupStatsResult,
  BackupVersion,
  GameLaunchPrecheck,
  GameLibraryItem,
  GameSaveRule,
  LaunchSyncDecision,
  LauncherMode,
  LauncherSession,
  RestoreBackupResult,
  TaskState,
} from "../types";

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

type RestoreUndoState = {
  gameId: string;
  versionId: string;
  restoredVersionId: string;
};

export type LibraryGameProductStatus = {
  label: string;
  description: string;
  tone: "ready" | "warning" | "paused" | "busy";
  actionHint: string;
};

type WaitForTaskCompletion = <T>(
  taskId: string,
  onProgress?: (message: string, progress: number | null) => void,
) => Promise<TaskState<T>>;

const PATH_ANCHOR_TOKENS = [
  "%GAME_DIR%",
  "%SAVED_GAMES%",
  "%DOCUMENTS%",
  "%LOCALLOW%",
  "%LOCALAPPDATA%",
  "%APPDATA%",
  "%USERPROFILE%",
] as const;

function normalizeGameId(gameIdText: string): string {
  return gameIdText.trim().toLowerCase();
}

function cardKey(gameIdText: string): string {
  return normalizeGameId(gameIdText);
}

function sortLibraryItems(items: GameLibraryItem[]): GameLibraryItem[] {
  return [...items].sort((a, b) => {
    const aTime = Math.max(Number(a.lastSessionUpdatedAt || "0"), Number(a.lastRuleUpdatedAt || "0"));
    const bTime = Math.max(Number(b.lastSessionUpdatedAt || "0"), Number(b.lastRuleUpdatedAt || "0"));
    return bTime - aTime;
  });
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

function ruleAnchorTokens(rule: GameSaveRule | null | undefined): string[] {
  if (!rule) return [];
  return collectAnchorTokens(rule.confirmedPaths);
}

function ruleUsesGameDirToken(rule: GameSaveRule | null | undefined): boolean {
  if (!rule) return false;
  return rule.confirmedPaths.some((path) => path.toUpperCase().includes("%GAME_DIR%"));
}

function restoreProtectionSummary(result: RestoreBackupResult): string {
  if (result.preRestoreVersionId) {
    return `已先创建恢复前备份 ${result.preRestoreVersionId}`;
  }
  return "当前本地存档无新增变化，未额外创建恢复前备份";
}

export function useLibraryPage(options: {
  rules: Ref<GameSaveRule[]>;
  waitForTaskCompletion: WaitForTaskCompletion;
  askConfirm: (options: {
    title: string;
    message: string;
    confirmText?: string;
    cancelText?: string;
    danger?: boolean;
  }) => Promise<boolean>;
  showToast: (message: string, level?: "success" | "error" | "info", timeoutMs?: number) => void;
  showBlockingError: (message: string) => void;
}) {
  const libraryState = ref<TabState>({ loading: false, error: "" });
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
  const restoreTaskMessageByGame = ref<Record<string, string>>({});
  const restoreTaskProgressByGame = ref<Record<string, number | null>>({});
  const hiddenPrecheckKeys = new Set<string>();

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

  function isCardBusy(gameIdText: string, action?: CardAction): boolean {
    const loadingMap = cardLoading.value[cardKey(gameIdText)];
    if (!loadingMap) return false;
    if (action) {
      return loadingMap[action] === true;
    }
    return Object.values(loadingMap).some((value) => value === true);
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

  function sessionDetailsFor(gameIdText: string): LauncherSession | null {
    return sessionDetailsByGame.value[cardKey(gameIdText)] ?? null;
  }

  function restoreUndoFor(gameIdText: string): RestoreUndoState | null {
    return restoreUndoByGame.value[cardKey(gameIdText)] ?? null;
  }

  function restoreTaskMessageFor(gameIdText: string): string {
    return restoreTaskMessageByGame.value[cardKey(gameIdText)] ?? "";
  }

  function restoreTaskProgressFor(gameIdText: string): number | null {
    const value = restoreTaskProgressByGame.value[cardKey(gameIdText)];
    return typeof value === "number" ? value : null;
  }

  function setRestoreTaskState(gameIdText: string, message: string, progress: number | null) {
    const key = cardKey(gameIdText);
    restoreTaskMessageByGame.value = {
      ...restoreTaskMessageByGame.value,
      [key]: message,
    };
    restoreTaskProgressByGame.value = {
      ...restoreTaskProgressByGame.value,
      [key]: progress,
    };
  }

  function clearRestoreTaskState(gameIdText: string) {
    setRestoreTaskState(gameIdText, "", null);
  }

  function launchPrecheckFor(gameIdText: string): GameLaunchPrecheck | null {
    return launchPrecheckByGame.value[cardKey(gameIdText)] ?? null;
  }

  function visiblePrecheckChecks(gameIdText: string) {
    const precheck = launchPrecheckFor(gameIdText);
    if (!precheck) return [];
    return precheck.checks.filter((check) => !hiddenPrecheckKeys.has(check.key));
  }

  function precheckCheckFor(gameIdText: string, key: string) {
    return launchPrecheckFor(gameIdText)?.checks.find((check) => check.key === key) ?? null;
  }

  function syncDecisionFor(gameIdText: string): LaunchSyncDecision | null {
    return launchPrecheckFor(gameIdText)?.syncDecision ?? null;
  }

  function selectedRuleForGame(gameIdText: string): GameSaveRule | null {
    const normalized = cardKey(gameIdText);
    if (!normalized) return null;
    return options.rules.value.find((rule) => cardKey(rule.gameId) === normalized) ?? null;
  }

  function gameUsesGameDirToken(gameIdText: string): boolean {
    return ruleUsesGameDirToken(selectedRuleForGame(gameIdText));
  }

  function gameDirResolutionIssue(gameIdText: string): string {
    const check = precheckCheckFor(gameIdText, "rule_path_resolution");
    if (!check || check.ok) return "";
    if (!gameUsesGameDirToken(gameIdText)) return "";
    return check.detail;
  }

  function gameDirStatusLabel(gameIdText: string): string {
    const issue = gameDirResolutionIssue(gameIdText);
    if (issue) return "需绑定 EXE";
    if (gameUsesGameDirToken(gameIdText)) return "游戏目录规则";
    return "";
  }

  function selectedRuleAnchorTokens(gameIdText: string): string[] {
    return ruleAnchorTokens(selectedRuleForGame(gameIdText));
  }

  function syncStatusLabel(status: string): string {
    switch (status) {
      case "no_backup":
        return "暂无备份";
      case "backup_only":
        return "仅备份存在";
      case "local_only":
        return "仅本地存在";
      case "in_sync":
        return "看起来一致";
      case "local_newer":
        return "本地较新";
      case "backup_newer":
        return "备份较新";
      default:
        return "需人工判断";
    }
  }

  function syncStatusClass(status: string): string {
    switch (status) {
      case "in_sync":
      case "local_only":
      case "no_backup":
        return "ok";
      case "backup_only":
      case "local_newer":
      case "backup_newer":
        return "warn";
      default:
        return "fail";
    }
  }

  function cardSyncStatusLabel(gameIdText: string): string {
    const status = syncDecisionFor(gameIdText)?.status;
    if (!status) return "";
    return syncStatusLabel(status);
  }

  function libraryGameProductStatus(item: GameLibraryItem): LibraryGameProductStatus {
    if (isCardBusy(item.gameId, "launch")) {
      return {
        label: "保护中",
        description: "游戏正在运行，退出后会自动检查并备份存档。",
        tone: "busy",
        actionHint: "等待游戏退出后完成备份",
      };
    }
    if (item.enabledRules === 0) {
      return {
        label: item.totalRules > 0 ? "保护已暂停" : "需要学习存档",
        description: item.totalRules > 0
          ? "当前没有启用的存档规则，GameSaver 不会自动保护这个游戏。"
          : "还没有可用的存档规则，先学习一次存档位置。",
        tone: "paused",
        actionHint: item.totalRules > 0 ? "启用规则后再启动" : "先学习存档规则",
      };
    }
    if (!item.preferredExePath) {
      return {
        label: "需要设置",
        description: "还没有绑定本机启动程序，选择 EXE 后即可像 Steam 一样启动。",
        tone: "warning",
        actionHint: "先选择启动 EXE",
      };
    }
    if (gameDirResolutionIssue(item.gameId)) {
      return {
        label: "需要设置",
        description: "规则里包含游戏目录路径，需要重新确认当前绑定的 EXE。",
        tone: "warning",
        actionHint: "确认或更换启动 EXE",
      };
    }

    const syncDecision = syncDecisionFor(item.gameId);
    if (syncDecision?.status === "backup_only" || syncDecision?.status === "backup_newer") {
      return {
        label: "存档需确认",
        description: "历史备份看起来比本地存档更新，启动前会让你选择恢复或直接启动。",
        tone: "warning",
        actionHint: "启动时确认使用哪份存档",
      };
    }
    if (syncDecision?.status === "conflict_unknown") {
      return {
        label: "存档需确认",
        description: "本地和备份状态无法可靠判断，启动前建议确认一次。",
        tone: "warning",
        actionHint: "查看存档状态后启动",
      };
    }

    return {
      label: "可启动",
      description: "存档保护已就绪，启动后退出游戏会自动备份变化。",
      tone: "ready",
      actionHint: "点击启动游戏",
    };
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

  async function loadSelectedLibraryGameSessionAndVersions() {
    const gameIdText = selectedLibraryGameId.value;
    if (!gameIdText) return;
    await Promise.all([
      loadBackupVersionsForGame(gameIdText, false),
      loadSessionDetailsForGame(gameIdText, false),
    ]);
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

  async function refreshLibraryItems() {
    const data = await listGameLibraryItems();
    libraryItems.value = sortLibraryItems(data);
    ensureSelectedLibraryGame();
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

  async function reloadLibraryWithLoading() {
    libraryState.value.loading = true;
    libraryState.value.error = "";
    clearAllLibraryCardErrors();
    try {
      await refreshLibraryItems();
      void loadSelectedLibraryGameSessionAndVersions();
      void refreshLaunchPrechecksForLibraryItems();
      void refreshBackupStatsForLibraryItems();
    } catch (err) {
      libraryState.value.error = `读取游戏库失败：${String(err)}`;
    } finally {
      libraryState.value.loading = false;
    }
  }

  async function selectLibraryGame(gameIdText: string) {
    selectedLibraryGameId.value = gameIdText;
    await loadSelectedLibraryGameDetails();
  }

  async function choosePreferredExeForGame(gameIdText: string) {
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const chosen = await open({
        multiple: false,
        filters: [{ name: "Executable", extensions: ["exe"] }],
      });
      if (!chosen || Array.isArray(chosen)) return;
      selectedLibraryGameId.value = gameIdText;
      setCardBusy(gameIdText, "bind_exe", true);
      libraryState.value.error = "";
      clearLibraryCardError(gameIdText);
      await setPreferredExePath(gameIdText, chosen);
      await refreshLibraryItems();
      await loadLaunchPrecheckForGame(gameIdText, false);
      options.showToast(`${gameIdText} 启动 EXE 已更新`, "success");
    } catch (err) {
      setLibraryCardError(gameIdText, `绑定 EXE 失败：${String(err)}`);
      options.showToast("绑定 EXE 失败", "error");
    } finally {
      setCardBusy(gameIdText, "bind_exe", false);
    }
  }

  async function resolveBackupLaunchMode(gameIdText: string): Promise<LauncherMode | null> {
    const decision = syncDecisionFor(gameIdText);
    if (!decision) {
      return "backup_direct";
    }
    switch (decision.status) {
      case "no_backup":
      case "local_only":
      case "local_newer":
      case "in_sync":
        return "backup_direct";
      case "backup_only":
      case "backup_newer": {
        const restoreFirst = await options.askConfirm({
          title: "检测到较新的历史备份",
          message: `${decision.message}\n\n点击“恢复后启动”会先恢复最近备份；点击“直接启动”会保留当前本地状态直接进入游戏。`,
          confirmText: "恢复后启动",
          cancelText: "直接启动",
          danger: false,
        });
        return restoreFirst ? "backup" : "backup_direct";
      }
      case "conflict_unknown": {
        const continueDirect = await options.askConfirm({
          title: "同步状态无法可靠判断",
          message: `${decision.message}\n\n建议先查看备份时间线；如果你确认要继续，可以直接启动并保留当前本地状态。`,
          confirmText: "直接启动",
          cancelText: "取消",
          danger: false,
        });
        return continueDirect ? "backup_direct" : null;
      }
      default:
        return "backup_direct";
    }
  }

  async function launchLibraryGame(gameIdText: string, mode: LauncherMode = "backup") {
    selectedLibraryGameId.value = gameIdText;
    setCardBusy(gameIdText, "launch", true);
    libraryState.value.error = "";
    clearLibraryCardError(gameIdText);
    try {
      let actualMode: LauncherMode | null = mode;
      if (mode === "backup") {
        if (!syncDecisionFor(gameIdText)) {
          await loadLaunchPrecheckForGame(gameIdText, false);
        }
        actualMode = await resolveBackupLaunchMode(gameIdText);
        if (!actualMode) {
          return;
        }
      }
      await launchGameFromLibrary(gameIdText, actualMode);
      await refreshLibraryItems();
      await loadLaunchPrecheckForGame(gameIdText, false);
      await Promise.all([
        loadBackupStatsForGame(gameIdText, false),
        loadBackupVersionsForGame(gameIdText, false),
        loadSessionDetailsForGame(gameIdText, false),
      ]);
      if (mode === "backup") {
        const launchLabel = actualMode === "backup" ? "恢复后启动" : "直接启动";
        options.showToast(`${gameIdText} ${launchLabel}成功`, "success");
      } else {
        options.showToast(`${gameIdText} 启动成功`, "success");
      }
    } catch (err) {
      setLibraryCardError(gameIdText, String(err));
      options.showBlockingError(String(err));
      await refreshLibraryItems();
    } finally {
      setCardBusy(gameIdText, "launch", false);
    }
  }

  async function rollbackToLibraryBackupVersion(gameIdText: string, versionId: string) {
    const confirmed = await options.askConfirm({
      title: "确认恢复备份",
      message: `确定将 ${gameIdText} 恢复到版本 ${versionId} 吗？\n\n执行恢复前，GameSaver 会先尝试为当前本地存档创建一份“恢复前备份”，然后再覆盖目标存档。`,
      confirmText: "恢复并继续",
      cancelText: "取消",
      danger: true,
    });
    if (!confirmed) {
      return;
    }
    setCardBusy(gameIdText, "backup_rollback", true);
    libraryState.value.error = "";
    clearLibraryCardError(gameIdText);
    setRestoreTaskState(gameIdText, "任务已创建，准备回滚...", 0);
    try {
      const taskId = await startRestoreBackupVersionTask(gameIdText, versionId);
      const finalTask = await options.waitForTaskCompletion<RestoreBackupResult>(
        taskId,
        (message, progress) => {
          setRestoreTaskState(gameIdText, message || "正在回滚备份版本...", progress);
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "回滚失败");
      }
      const result = finalTask.result;
      if (!result) {
        throw new Error("回滚失败：任务结果为空");
      }
      await refreshLibraryItems();
      await Promise.all([
        loadBackupStatsForGame(gameIdText, false),
        loadBackupVersionsForGame(gameIdText, false),
        loadSessionDetailsForGame(gameIdText, false),
      ]);
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
      options.showToast(
        `恢复完成，${restoreProtectionSummary(result)}（已校验 ${result.verifiedFiles} 个文件，哈希抽样 ${result.hashSampleCount} 项）`,
        "success",
      );
    } catch (err) {
      setLibraryCardError(gameIdText, `回滚失败：${String(err)}`);
      options.showBlockingError(`回滚失败：${String(err)}`);
    } finally {
      clearRestoreTaskState(gameIdText);
      setCardBusy(gameIdText, "backup_rollback", false);
    }
  }

  async function undoLibraryRestore(gameIdText: string) {
    const undo = restoreUndoFor(gameIdText);
    if (!undo) return;
    const confirmed = await options.askConfirm({
      title: "撤销本次恢复",
      message: `确定恢复到刚才自动创建的恢复前备份 ${undo.versionId} 吗？此操作会再次覆盖当前存档。`,
      confirmText: "撤销恢复",
      cancelText: "取消",
      danger: true,
    });
    if (!confirmed) return;
    setCardBusy(gameIdText, "backup_rollback", true);
    libraryState.value.error = "";
    clearLibraryCardError(gameIdText);
    setRestoreTaskState(gameIdText, "任务已创建，准备撤销恢复...", 0);
    try {
      const taskId = await startRestoreBackupVersionTask(gameIdText, undo.versionId);
      const finalTask = await options.waitForTaskCompletion<RestoreBackupResult>(
        taskId,
        (message, progress) => {
          setRestoreTaskState(gameIdText, message || "正在撤销恢复...", progress);
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "撤销恢复失败");
      }
      restoreUndoByGame.value = {
        ...restoreUndoByGame.value,
        [cardKey(gameIdText)]: null,
      };
      await refreshLibraryItems();
      await Promise.all([
        loadBackupStatsForGame(gameIdText, false),
        loadBackupVersionsForGame(gameIdText, false),
        loadSessionDetailsForGame(gameIdText, false),
      ]);
      options.showToast("已撤销本次恢复", "success");
    } catch (err) {
      setLibraryCardError(gameIdText, `撤销恢复失败：${String(err)}`);
      options.showBlockingError(`撤销恢复失败：${String(err)}`);
    } finally {
      clearRestoreTaskState(gameIdText);
      setCardBusy(gameIdText, "backup_rollback", false);
    }
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

  async function saveBackupKeepPolicy(gameIdText: string) {
    const keep = parseBackupKeepDraft(gameIdText);
    if (!keep) {
      setLibraryCardError(gameIdText, "保留版本数必须是大于等于 1 的整数。");
      options.showToast("请输入有效的保留版本数", "error");
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
      options.showToast("备份保留策略已保存", "success");
    } catch (err) {
      setLibraryCardError(gameIdText, `保存备份策略失败：${String(err)}`);
      options.showToast("保存备份策略失败", "error");
    } finally {
      setCardBusy(gameIdText, "backup_policy_save", false);
    }
  }

  async function pruneOldBackupsForGame(gameIdText: string) {
    const keep = parseBackupKeepDraft(gameIdText);
    if (!keep) {
      setLibraryCardError(gameIdText, "保留版本数必须是大于等于 1 的整数。");
      options.showToast("请输入有效的保留版本数", "error");
      return;
    }
    setCardBusy(gameIdText, "backup_prune", true);
    libraryState.value.error = "";
    clearLibraryCardError(gameIdText);
    try {
      await pruneBackupVersions(gameIdText, keep);
      await Promise.all([
        loadBackupStatsForGame(gameIdText, false),
        loadBackupVersionsForGame(gameIdText, false),
      ]);
      options.showToast("旧备份已清理", "success");
    } catch (err) {
      setLibraryCardError(gameIdText, `清理备份失败：${String(err)}`);
      options.showToast("清理备份失败", "error");
    } finally {
      setCardBusy(gameIdText, "backup_prune", false);
    }
  }

  watch(librarySearch, () => {
    const previousKey = cardKey(selectedLibraryGameId.value);
    ensureSelectedLibraryGame();
    if (cardKey(selectedLibraryGameId.value) !== previousKey) {
      void loadSelectedLibraryGameDetails();
    }
  });

  return {
    libraryState,
    librarySearch,
    filteredLibraryItems,
    selectedLibraryItem,
    libraryCardErrorFor,
    isLibraryGameSelected,
    gameDirResolutionIssue,
    cardSyncStatusLabel,
    syncStatusClass,
    syncDecisionFor,
    gameDirStatusLabel,
    libraryGameProductStatus,
    backupStatsFor,
    isCardBusy,
    launchPrecheckFor,
    selectedRuleAnchorTokens,
    visiblePrecheckChecks,
    backupKeepDraftFor,
    backupVersionsFor,
    restoreUndoFor,
    restoreTaskMessageFor,
    restoreTaskProgressFor,
    sessionDetailsFor,
    refreshLibraryItems,
    reloadLibraryWithLoading,
    selectLibraryGame,
    choosePreferredExeForGame,
    launchLibraryGame,
    updateBackupKeepDraft,
    saveBackupKeepPolicy,
    pruneOldBackupsForGame,
    rollbackToLibraryBackupVersion,
    undoLibraryRestore,
    loadSelectedLibraryGameDetails,
  };
}
