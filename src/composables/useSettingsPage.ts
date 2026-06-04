import { ref } from "vue";
import {
  getSettingsPaths,
  openCandidatePath,
  startMigrateDataPathTask,
  updateSettingsPaths,
} from "../api";
import type {
  DataPathKind,
  DataPathMigrationResult,
  SettingsPaths,
  TaskState,
} from "../types";

type TabState = {
  loading: boolean;
  error: string;
};

type WaitForTaskCompletion = <T>(
  taskId: string,
  onProgress?: (message: string, progress: number | null) => void,
) => Promise<TaskState<T>>;

const BYTES_PER_MB = 1024 * 1024;

function bytesToMegabytesText(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0";
  }
  return String(Math.round(bytes / BYTES_PER_MB));
}

export function useSettingsPage(options: {
  waitForTaskCompletion: WaitForTaskCompletion;
  askConfirm: (options: {
    title: string;
    message: string;
    confirmText?: string;
    cancelText?: string;
    danger?: boolean;
  }) => Promise<boolean>;
  showToast: (message: string, level?: "success" | "error" | "info", timeoutMs?: number) => void;
}) {
  const settingsState = ref<TabState>({ loading: false, error: "" });
  const settings = ref<SettingsPaths | null>(null);
  const backupRootDraft = ref("");
  const backupMaxFileMbDraft = ref("");
  const settingsMigrationKind = ref<DataPathKind | "">("");
  const settingsMigrationMessage = ref("");
  const settingsMigrationProgress = ref<number | null>(null);

  async function openDirectory(path: string) {
    if (!path.trim()) return;
    try {
      await openCandidatePath(path);
    } catch (err) {
      settingsState.value.error = `打开目录失败：${String(err)}`;
    }
  }

  async function reloadSettings() {
    settingsState.value.loading = true;
    settingsState.value.error = "";
    try {
      const data = await getSettingsPaths();
      settings.value = data;
      backupRootDraft.value = data.backupRoot;
      backupMaxFileMbDraft.value = bytesToMegabytesText(data.backupMaxFileBytes);
    } catch (err) {
      settingsState.value.error = `读取设置失败：${String(err)}`;
    } finally {
      settingsState.value.loading = false;
    }
  }

  async function chooseSettingsDirectory(_kind: DataPathKind) {
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const chosen = await open({
        multiple: false,
        directory: true,
      });
      if (!chosen || Array.isArray(chosen)) return;
      backupRootDraft.value = chosen;
    } catch (err) {
      settingsState.value.error = `无法打开目录选择器：${String(err)}`;
    }
  }

  function parseBackupMaxFileBytesDraft(): number | null {
    const raw = backupMaxFileMbDraft.value.trim();
    if (!raw || !/^\d+$/.test(raw)) {
      return null;
    }
    const mb = Number(raw);
    if (!Number.isFinite(mb) || mb < 0) {
      return null;
    }
    return Math.trunc(mb) * BYTES_PER_MB;
  }

  async function saveSettingsPath(_kind: DataPathKind) {
    const backupMaxFileBytes = parseBackupMaxFileBytesDraft();
    if (backupMaxFileBytes === null) {
      settingsState.value.error = "大文件过滤阈值必须是大于等于 0 的整数 MB，0 表示不限制。";
      options.showToast("请输入有效的大文件过滤阈值", "error");
      return;
    }
    settingsState.value.loading = true;
    settingsState.value.error = "";
    try {
      const input = {
        backupRoot: backupRootDraft.value.trim(),
        backupMaxFileBytes,
      };
      const updated = await updateSettingsPaths(input);
      settings.value = updated;
      backupRootDraft.value = updated.backupRoot;
      backupMaxFileMbDraft.value = bytesToMegabytesText(updated.backupMaxFileBytes);
      options.showToast("备份设置已保存", "success");
    } catch (err) {
      settingsState.value.error = `保存设置失败：${String(err)}`;
      options.showToast("保存设置失败", "error");
    } finally {
      settingsState.value.loading = false;
    }
  }

  async function migrateSettingsPath(kind: DataPathKind) {
    const targetPath = backupRootDraft.value.trim();
    const currentPath = settings.value ? settings.value.backupRoot : "";
    if (!targetPath || targetPath === currentPath) {
      return;
    }
    const confirmed = await options.askConfirm({
      title: "确认迁移数据目录",
      message:
        `将把当前目录内容复制到新位置：\n\n旧路径：${currentPath}\n新路径：${targetPath}\n\n迁移成功后才会切换配置，默认保留旧目录，不会自动删除。`,
      confirmText: "复制并切换",
      cancelText: "取消",
      danger: false,
    });
    if (!confirmed) return;

    settingsState.value.loading = true;
    settingsState.value.error = "";
    settingsMigrationKind.value = kind;
    settingsMigrationMessage.value = "任务已创建，准备迁移数据目录...";
    settingsMigrationProgress.value = 0;
    try {
      const taskId = await startMigrateDataPathTask(kind, targetPath);
      const finalTask = await options.waitForTaskCompletion<DataPathMigrationResult>(
        taskId,
        (message, progress) => {
          settingsMigrationMessage.value = message || "正在迁移数据目录...";
          settingsMigrationProgress.value = progress;
        },
      );
      if (finalTask.status === "failed") {
        throw new Error(finalTask.error || "迁移数据目录失败");
      }
      await reloadSettings();
      const result = finalTask.result;
      if (result) {
        options.showToast(
          `迁移完成：复制 ${result.copiedFiles} 个文件到 ${result.targetPath}，旧目录已保留`,
          "success",
          4200,
        );
      } else {
        options.showToast("数据目录迁移完成", "success");
      }
    } catch (err) {
      settingsState.value.error = `迁移数据目录失败：${String(err)}`;
      options.showToast("数据目录迁移失败", "error");
    } finally {
      settingsMigrationKind.value = "";
      settingsMigrationMessage.value = "";
      settingsMigrationProgress.value = null;
      settingsState.value.loading = false;
    }
  }

  return {
    settings,
    settingsState,
    backupRootDraft,
    backupMaxFileMbDraft,
    settingsMigrationKind,
    settingsMigrationMessage,
    settingsMigrationProgress,
    openDirectory,
    reloadSettings,
    chooseSettingsDirectory,
    saveSettingsPath,
    migrateSettingsPath,
  };
}
