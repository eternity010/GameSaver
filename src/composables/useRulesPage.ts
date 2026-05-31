import { computed, ref } from "vue";
import {
  deleteRule,
  exportRules,
  importRules,
  listRuleConflicts,
  listRules,
  setPrimaryRule,
  startExportMigrationZipTask,
  startImportMigrationZipTask,
  updateRule,
} from "../api";
import type {
  ExportMigrationZipResult,
  GameSaveRule,
  ImportMigrationZipResult,
  RuleConflictItem,
  TaskState,
} from "../types";

export type RuleDraft = {
  gameIdText: string;
  confirmedPathsText: string;
  enabled: boolean;
};

type TabState = {
  loading: boolean;
  error: string;
};

type WaitForTaskCompletion = <T>(
  taskId: string,
  onProgress?: (message: string, progress: number | null) => void,
) => Promise<TaskState<T>>;

export function normalizeRulePaths(rawText: string): string[] {
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

function sortRulesByUpdatedTime(items: GameSaveRule[]): GameSaveRule[] {
  return [...items].sort((a, b) => {
    const aTime = Number(a.updatedAt || a.createdAt || "0");
    const bTime = Number(b.updatedAt || b.createdAt || "0");
    return bTime - aTime;
  });
}

export function useRulesPage(options: {
  waitForTaskCompletion: WaitForTaskCompletion;
  askConfirm: (options: {
    title: string;
    message: string;
    confirmText?: string;
    cancelText?: string;
    danger?: boolean;
  }) => Promise<boolean>;
  showToast: (message: string, level?: "success" | "error" | "info", timeoutMs?: number) => void;
  refreshLibraryItems: () => Promise<void>;
}) {
  const rules = ref<GameSaveRule[]>([]);
  const ruleConflicts = ref<RuleConflictItem[]>([]);
  const ruleSearch = ref("");
  const ruleDrafts = ref<Record<string, RuleDraft>>({});
  const rulesState = ref<TabState>({ loading: false, error: "" });
  const migrationExportWaiting = ref(false);
  const migrationExportMessage = ref("");
  const migrationExportProgress = ref<number | null>(null);
  const migrationImportWaiting = ref(false);
  const migrationImportMessage = ref("");
  const migrationImportProgress = ref<number | null>(null);

  const ruleConflictByRuleId = computed<Record<string, RuleConflictItem>>(() => {
    const map: Record<string, RuleConflictItem> = {};
    for (const conflict of ruleConflicts.value) {
      for (const ruleId of conflict.ruleIds) {
        map[ruleId] = conflict;
      }
    }
    return map;
  });

  function updateRuleDraft(ruleId: string, patch: Partial<RuleDraft>) {
    const current = ruleDrafts.value[ruleId];
    if (!current) return;
    ruleDrafts.value = {
      ...ruleDrafts.value,
      [ruleId]: {
        ...current,
        ...patch,
      },
    };
  }

  function ruleConflictFor(ruleId: string): RuleConflictItem | null {
    return ruleConflictByRuleId.value[ruleId] ?? null;
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

  async function refreshRules() {
    const [data, conflicts] = await Promise.all([listRules(), listRuleConflicts()]);
    rules.value = sortRulesByUpdatedTime(data);
    ruleConflicts.value = conflicts;
    hydrateRuleDrafts();
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
      await options.refreshLibraryItems();
      options.showToast("主规则设置成功", "success");
    } catch (err) {
      rulesState.value.error = `设置主规则失败：${String(err)}`;
      options.showToast("设置主规则失败", "error");
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
    const normalizedPaths = normalizeRulePaths(draft.confirmedPathsText);
    if (!normalizedPaths.length) {
      rulesState.value.error = "路径不能为空，至少保留一条路径。";
      return;
    }

    rulesState.value.loading = true;
    rulesState.value.error = "";
    try {
      const updated = await updateRule(rule.ruleId, normalizedGameId, normalizedPaths, draft.enabled);
      await refreshRules();
      options.showToast(`规则 ${updated.gameId} 已保存`, "success");
      await options.refreshLibraryItems();
    } catch (err) {
      rulesState.value.error = `保存规则失败：${String(err)}`;
      options.showToast("保存规则失败", "error");
    } finally {
      rulesState.value.loading = false;
    }
  }

  async function removeManagedRule(rule: GameSaveRule) {
    const confirmed = await options.askConfirm({
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
      options.showToast(`规则 ${rule.gameId} 已删除`, "success");
      await options.refreshLibraryItems();
    } catch (err) {
      rulesState.value.error = `删除规则失败：${String(err)}`;
      options.showToast("删除规则失败", "error");
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
      await exportRules(chosen);
      options.showToast("规则导出成功", "success");
    } catch (err) {
      rulesState.value.error = `导出失败：${String(err)}`;
      options.showToast("规则导出失败", "error");
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
      await importRules(chosen);
      await refreshRules();
      await options.refreshLibraryItems();
      options.showToast("规则导入完成", "success");
    } catch (err) {
      rulesState.value.error = `导入失败：${String(err)}`;
      options.showToast("规则导入失败", "error");
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
      migrationExportMessage.value = "任务已创建，准备导出迁移包...";
      migrationExportProgress.value = 0;
      const taskId = await startExportMigrationZipTask(chosen);
      const finalTask = await options.waitForTaskCompletion<ExportMigrationZipResult>(
        taskId,
        (message, progress) => {
          migrationExportMessage.value = message || "正在导出迁移包...";
          migrationExportProgress.value = progress;
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "导出迁移包失败");
      }
      const result = finalTask.result;
      if (result) {
        options.showToast(
          `迁移包导出成功（规则 ${result.ruleCount} 条，备份游戏 ${result.backupGames} 个，文件 ${result.exportedFiles} 个）`,
          "success",
          4200,
        );
      } else {
        options.showToast("迁移包导出成功", "success");
      }
    } catch (err) {
      rulesState.value.error = `导出迁移包失败：${String(err)}`;
      options.showToast("迁移包导出失败", "error");
    } finally {
      migrationExportWaiting.value = false;
      migrationExportMessage.value = "";
      migrationExportProgress.value = null;
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
      migrationImportWaiting.value = true;
      migrationImportMessage.value = "任务已创建，准备导入迁移包...";
      migrationImportProgress.value = 0;
      const taskId = await startImportMigrationZipTask(chosen);
      const finalTask = await options.waitForTaskCompletion<ImportMigrationZipResult>(
        taskId,
        (message, progress) => {
          migrationImportMessage.value = message || "正在导入迁移包...";
          migrationImportProgress.value = progress;
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "导入迁移包失败");
      }
      const result = finalTask.result;
      await refreshRules();
      await options.refreshLibraryItems();
      if (result) {
        options.showToast(
          `迁移包导入完成（新增规则 ${result.importedRules}，覆盖 ${result.overwrittenRules}，导入备份游戏 ${result.importedBackupGames}）`,
          "success",
          4200,
        );
      } else {
        options.showToast("迁移包导入完成", "success");
      }
    } catch (err) {
      rulesState.value.error = `导入迁移包失败：${String(err)}`;
      options.showToast("迁移包导入失败", "error");
    } finally {
      migrationImportWaiting.value = false;
      migrationImportMessage.value = "";
      migrationImportProgress.value = null;
      rulesState.value.loading = false;
    }
  }

  return {
    rules,
    ruleConflicts,
    ruleSearch,
    ruleDrafts,
    rulesState,
    migrationExportWaiting,
    migrationExportMessage,
    migrationExportProgress,
    migrationImportWaiting,
    migrationImportMessage,
    migrationImportProgress,
    updateRuleDraft,
    refreshRules,
    reloadRulesWithLoading,
    markPrimaryRule,
    saveManagedRule,
    removeManagedRule,
    exportRulesToFile,
    importRulesFromFile,
    exportMigrationZipToFile,
    importMigrationZipFromFile,
  };
}
