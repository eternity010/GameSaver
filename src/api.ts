import { invoke } from "@tauri-apps/api/core";
import type {
  BackupVersion,
  GameLaunchPrecheck,
  ExportMigrationZipResult,
  CandidatePath,
  ExportRulesResult,
  GameLibraryItem,
  GameSaveRule,
  ImportMigrationZipResult,
  ImportRulesResult,
  LauncherSession,
  LearningSession,
  LauncherMode,
  RedirectRuntimeInfo,
  RestoreBackupResult,
  ResolveRuleResult,
  RuntimeStatus,
} from "./types";

export async function startLearning(gameId: string, exePath: string): Promise<string> {
  return invoke("start_learning", { gameId, exePath });
}

export async function launchGame(sessionId: string): Promise<number> {
  return invoke("launch_game", { sessionId });
}

export async function finishLearning(sessionId: string): Promise<CandidatePath[]> {
  return invoke("finish_learning", { sessionId });
}

export async function confirmRule(sessionId: string, selectedPaths: string[]): Promise<string> {
  return invoke("confirm_rule", { sessionId, selectedPaths });
}

export async function listRules(): Promise<GameSaveRule[]> {
  return invoke("list_rules");
}

export async function updateRule(
  ruleId: string,
  confirmedPaths: string[],
  enabled: boolean,
): Promise<GameSaveRule> {
  return invoke("update_rule", { ruleId, confirmedPaths, enabled });
}

export async function deleteRule(ruleId: string): Promise<void> {
  return invoke("delete_rule", { ruleId });
}

export async function exportRules(filePath: string): Promise<ExportRulesResult> {
  return invoke("export_rules", { filePath });
}

export async function importRules(filePath: string): Promise<ImportRulesResult> {
  return invoke("import_rules", { filePath });
}

export async function exportMigrationZip(filePath: string): Promise<ExportMigrationZipResult> {
  return invoke("export_migration_zip", { filePath });
}

export async function importMigrationZip(filePath: string): Promise<ImportMigrationZipResult> {
  return invoke("import_migration_zip", { filePath });
}

export async function openCandidatePath(path: string): Promise<void> {
  return invoke("open_candidate_path", { path });
}

export async function getLearningSession(sessionId: string): Promise<LearningSession> {
  return invoke("get_learning_session", { sessionId });
}

export async function getRuntimeStatus(): Promise<RuntimeStatus> {
  return invoke("get_runtime_status");
}

export async function restartAsAdmin(): Promise<void> {
  return invoke("restart_as_admin");
}

export async function resolveRuleForExe(exePath: string): Promise<ResolveRuleResult> {
  return invoke("resolve_rule_for_exe", { exePath });
}

export async function launchWithRule(exePath: string, launchMode: LauncherMode): Promise<LauncherSession> {
  return invoke("launch_with_rule", { exePath, launchMode });
}

export async function launchGameFromLibrary(
  gameId: string,
  launchMode: LauncherMode = "backup",
): Promise<LauncherSession> {
  return invoke("launch_game_from_library", { gameId, launchMode });
}

export async function precheckGameLaunch(gameId: string): Promise<GameLaunchPrecheck> {
  return invoke("precheck_game_launch", { gameId });
}

export async function getLauncherSession(launcherSessionId: string): Promise<LauncherSession> {
  return invoke("get_launcher_session", { launcherSessionId });
}

export async function listLauncherSessions(): Promise<LauncherSession[]> {
  return invoke("list_launcher_sessions");
}

export async function listGameLibraryItems(): Promise<GameLibraryItem[]> {
  return invoke("list_game_library_items");
}

export async function setPreferredExePath(gameId: string, exePath: string): Promise<GameLibraryItem> {
  return invoke("set_preferred_exe_path", { gameId, exePath });
}

export async function getRedirectRuntimeInfo(): Promise<RedirectRuntimeInfo> {
  return invoke("get_redirect_runtime_info");
}

export async function syncSandboxSession(launcherSessionId: string): Promise<LauncherSession> {
  return invoke("sync_sandbox_session", { launcherSessionId });
}

export async function listBackupVersions(gameId: string): Promise<BackupVersion[]> {
  return invoke("list_backup_versions", { gameId });
}

export async function restoreBackupVersion(
  gameId: string,
  versionId: string,
): Promise<RestoreBackupResult> {
  return invoke("restore_backup_version", { gameId, versionId });
}
