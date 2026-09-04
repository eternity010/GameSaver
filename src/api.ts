import { invoke } from "@tauri-apps/api/core";
import type { CoverCrop, CoverPosition, Game, GameBodyVersion, GameCover, GameRuntime, LaunchPrecheck, SaveLearningResult, SaveLearningSession, SaveProfile, SaveScope, SaveVersion } from "./domain/game";

export interface FrontendErrorReport {
  source: string;
  message: string;
  stack?: string;
  url?: string;
  line?: number;
  column?: number;
}

function errorMessage(reason: unknown): string {
  if (reason instanceof Error) return reason.message;
  if (typeof reason === "string") return reason;
  try {
    return JSON.stringify(reason);
  } catch {
    return String(reason);
  }
}

export function reportFrontendError(report: FrontendErrorReport): Promise<void> {
  return invoke<void>("report_frontend_error", { error: report });
}

function invokeCommand<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(command, args).catch((reason) => {
    void reportFrontendError({
      source: "tauri_command",
      message: `${command}: ${errorMessage(reason)}`,
      stack: reason instanceof Error ? reason.stack : undefined,
    }).catch(() => undefined);
    throw reason;
  });
}

export interface AppTask {
  taskId: string;
  taskType: string;
  status: "pending" | "running" | "success" | "failed" | "cancelled" | "interrupted";
  progress: number;
  message: string;
  gameUid?: string;
  createdAt?: string;
  error?: string;
  result?: unknown;
  retry?: TaskRetry;
}

export interface TaskRetry {
  operation: string;
  gameUid: string;
  gameKey?: string;
  versionId?: string;
  remotePath?: string;
  remoteFsId?: number;
}

export interface BaiduStatus {
  authorized: boolean;
  tokenPath?: string;
  expiresAt?: number;
  expired: boolean;
  refreshError?: string;
}

export interface ElevationStatus {
  isAdmin: boolean;
  canRestartAsAdmin: boolean;
}

export function getElevationStatus(): Promise<ElevationStatus> {
  return invokeCommand<ElevationStatus>("get_elevation_status");
}

export function restartAsAdmin(): Promise<void> {
  return invokeCommand<void>("restart_as_admin");
}

export interface BaiduConfigView {
  configured: boolean;
  appKey?: string;
  secretKeyConfigured: boolean;
  autoUploadBody: boolean;
  autoSyncSave: boolean;
  checkCloudSaveOnLaunch: boolean;
  cloudSaveKeepLimit: number;
}

export interface BaiduQuota {
  total: number;
  used: number;
  free: number;
  expiresSoon: boolean;
}

export interface RemoteFile {
  path: string;
  fsId: number;
  size: number;
  md5?: string;
  isDir: boolean;
  serverMtime?: number;
}

export interface RemoteBodyPackage extends RemoteFile {
  versionId: string;
  packageSha256?: string;
  fileCount?: number;
  totalBytes?: number;
  createdAt?: string;
  syncState: "synced" | "remote_only" | "mismatch" | "unverified" | string;
  manifestVerified: boolean;
}

export interface RemoteBodyPackageList {
  packages: RemoteBodyPackage[];
  manifestAvailable: boolean;
  manifestStatus: "synced" | "missing" | "invalid" | string;
  manifestUpdatedAt?: string;
  warnings: string[];
}

export interface CloudGameSummary {
  gameKey: string;
  gameUid: string;
  displayName: string;
  executableRelativePath?: string;
  arguments: string[];
  workingDirectoryRelativePath?: string;
  versionId: string;
  packagePath: string;
  packageFsId: number;
  packageSize: number;
  packageSha256?: string;
  fileCount?: number;
  totalBytes?: number;
  createdAt?: string;
  installed: boolean;
  versions: CloudGameVersion[];
  hasCover?: boolean;
}

export interface CloudGameVersion {
  versionId: string;
  path: string;
  fsId: number;
  size: number;
  packageSha256?: string;
  fileCount?: number;
  totalBytes?: number;
  createdAt?: string;
  syncState: string;
  manifestVerified: boolean;
}

export function listGames(): Promise<Game[]> {
  return invokeCommand<Game[]>("list_games");
}

export function getGame(gameUid: string): Promise<Game | null> {
  return invokeCommand<Game | null>("get_game", { gameUid });
}

export function renameGame(gameUid: string, newDisplayName: string): Promise<Game> {
  return invokeCommand<Game>("rename_game", { gameUid, newDisplayName });
}

export function saveGameCover(gameUid: string, originalBytes: number[], displayBytes: number[], originalExtension: string, crop: CoverCrop, position: CoverPosition): Promise<GameCover> {
  return invokeCommand<GameCover>("save_game_cover", { gameUid, originalBytes, displayBytes, originalExtension, crop, position });
}

export function getGameCover(gameUid: string): Promise<number[] | null> {
  return invokeCommand<number[] | null>("get_game_cover", { gameUid });
}

export function getGameCoverPath(gameUid: string): Promise<string | null> {
  return invokeCommand<string | null>("get_game_cover_path", { gameUid });
}

export function getGameCoverPaths(): Promise<Record<string, string>> {
  return invokeCommand<Record<string, string>>("get_game_cover_paths");
}

export function getCloudGameCoverPath(gameKey: string): Promise<string | null> {
  return invokeCommand<string | null>("get_cloud_game_cover_path", { gameKey });
}

export function getCloudGameCoverPaths(): Promise<Record<string, string>> {
  return invokeCommand<Record<string, string>>("get_cloud_game_cover_paths");
}

export function getGameCoverUrl(gameUid: string, cacheKey?: string | number): string {
  const base = `http://gamesaver-cover.localhost/game/${encodeURIComponent(gameUid)}`;
  return cacheKey ? `${base}?v=${encodeURIComponent(cacheKey)}` : base;
}

export function getCloudGameCoverUrl(gameKey: string): string {
  return `http://gamesaver-cover.localhost/cloud/${encodeURIComponent(gameKey)}`;
}

export function startAddGameTask(input: {
  displayName: string;
  gameKey: string;
  sourcePath: string;
  executablePath: string;
  allowLargeSource: boolean;
}): Promise<string> {
  return invokeCommand<string>("start_add_game_task", input);
}

export function getTask(taskId: string): Promise<AppTask> {
  return invokeCommand<AppTask>("get_task", { taskId });
}

export function listTasks(): Promise<AppTask[]> {
  return invokeCommand<AppTask[]>("list_tasks");
}

export function cancelTask(taskId: string): Promise<void> {
  return invokeCommand<void>("cancel_task", { taskId });
}

export function deleteTasks(taskIds: string[]): Promise<number> {
  return invokeCommand<number>("delete_tasks", { taskIds });
}

export interface LibrarySettings {
  libraryRoot: string;
  gamesPath: string;
  bodyPackagesPath: string;
  savesPath: string;
  gamesBytes: number;
  bodyPackagesBytes: number;
  savesBytes: number;
  totalBytes: number;
  fileCount: number;
  freeBytes: number;
}

export function getLibrarySettings(): Promise<LibrarySettings> {
  return invokeCommand<LibrarySettings>("get_library_settings");
}

export function startSetLibraryRootTask(targetRoot: string): Promise<string> {
  return invokeCommand<string>("start_set_library_root_task", { targetRoot });
}

export function startSaveLearningTask(gameUid: string): Promise<string> {
  return invokeCommand<string>("start_save_learning_task", { gameUid });
}

export function startFinishSaveLearningTask(sessionId: string): Promise<string> {
  return invokeCommand<string>("start_finish_save_learning_task", { sessionId });
}

export function cancelSaveLearning(sessionId: string): Promise<void> {
  return invokeCommand<void>("cancel_save_learning", { sessionId });
}

export function confirmSaveProfile(gameUid: string, scopes: SaveScope[], confidence: number): Promise<SaveProfile> {
  return invokeCommand<SaveProfile>("confirm_save_profile", { gameUid, scopes, confidence });
}

export function getSaveProfile(gameUid: string): Promise<SaveProfile | null> {
  return invokeCommand<SaveProfile | null>("get_save_profile", { gameUid });
}

export function updateSaveProfileKeepVersions(gameUid: string, keepVersions: number): Promise<SaveProfile> {
  return invokeCommand<SaveProfile>("update_save_profile_keep_versions", { gameUid, keepVersions });
}

export function updateSaveProfileScopes(gameUid: string, scopes: SaveScope[]): Promise<SaveProfile> {
  return invokeCommand<SaveProfile>("update_save_profile_scopes", { gameUid, scopes });
}

export function discardPendingGame(gameUid: string): Promise<void> {
  return invokeCommand<void>("discard_pending_game", { gameUid });
}

export function openPathInExplorer(path: string): Promise<void> {
  return invokeCommand<void>("open_path_in_explorer", { path });
}

export interface DefaultSaveExclusions {
  excludePatterns: string[];
  excludeDirectories: string[];
}

export function getDefaultSaveExclusions(): Promise<DefaultSaveExclusions> {
  return invokeCommand<DefaultSaveExclusions>("get_default_save_exclusions");
}

export function precheckGameLaunch(gameUid: string): Promise<LaunchPrecheck> {
  return invokeCommand<LaunchPrecheck>("precheck_game_launch", { gameUid });
}

export function launchGame(gameUid: string): Promise<string> {
  return invokeCommand<string>("launch_game", { gameUid });
}

export function getGameRuntime(gameUid: string): Promise<GameRuntime | null> {
  return invokeCommand<GameRuntime | null>("get_game_runtime", { gameUid });
}

export function listSaveVersions(gameUid: string): Promise<SaveVersion[]> {
  return invokeCommand<SaveVersion[]>("list_save_versions", { gameUid });
}

export function restoreSaveVersion(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("restore_save_version", { gameUid, versionId });
}

export function deleteSaveVersion(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("delete_save_version", { gameUid, versionId });
}

export function pruneSaveVersions(gameUid: string, keepVersions: number): Promise<string> {
  return invokeCommand<string>("prune_save_versions", { gameUid, keepVersions });
}

export function listGameBodyVersions(gameUid: string): Promise<GameBodyVersion[]> {
  return invokeCommand<GameBodyVersion[]>("list_game_body_versions", { gameUid });
}

export function updateGameBody(gameUid: string, sourcePath: string): Promise<string> {
  return invokeCommand<string>("update_game_body", { gameUid, sourcePath });
}

export function packageGameBody(gameUid: string): Promise<string> {
  return invokeCommand<string>("package_game_body", { gameUid });
}

export function deleteGameBodyPackage(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("delete_game_body_package", { gameUid, versionId });
}

export function uninstallGameBody(gameUid: string): Promise<string> {
  return invokeCommand<string>("uninstall_game_body", { gameUid });
}

export function getBaiduStatus(): Promise<BaiduStatus> {
  return invokeCommand<BaiduStatus>("get_baidu_status");
}

export function getBaiduConfig(): Promise<BaiduConfigView> {
  return invokeCommand<BaiduConfigView>("get_baidu_config");
}

export function saveBaiduConfig(appKey: string, secretKey: string): Promise<BaiduConfigView> {
  return invokeCommand<BaiduConfigView>("save_baidu_config", { appKey, secretKey });
}

export function setBaiduAutoUpload(enabled: boolean): Promise<BaiduConfigView> {
  return invokeCommand<BaiduConfigView>("set_baidu_auto_upload", { enabled });
}

export function buildBaiduAuthorizeUrl(): Promise<string> {
  return invokeCommand<string>("build_baidu_authorize_url");
}

export function exchangeBaiduCode(code: string): Promise<void> {
  return invokeCommand<void>("exchange_baidu_code", { code });
}

export interface CloudAccountStatus {
  profileAvailable: boolean;
  remoteSize?: number;
  remoteUpdatedAt?: number;
}

export function getCloudAccountStatus(): Promise<CloudAccountStatus> {
  return invokeCommand<CloudAccountStatus>("get_cloud_account_status");
}

export function startUploadCloudAccountTask(): Promise<string> {
  return invokeCommand<string>("start_upload_cloud_account_task");
}

export function startDownloadCloudAccountTask(): Promise<string> {
  return invokeCommand<string>("start_download_cloud_account_task");
}

export function getBaiduQuota(): Promise<BaiduQuota> {
  return invokeCommand<BaiduQuota>("get_baidu_quota");
}

export interface CloudGamePage {
  games: CloudGameSummary[];
  page: number;
  pageSize: number;
  hasMore: boolean;
}

export function listCloudGames(page = 1, pageSize = 9): Promise<CloudGamePage> {
  return invokeCommand<CloudGamePage>("list_cloud_games", { page, pageSize });
}

export function getCloudGameCover(gameKey: string): Promise<number[] | null> {
  return invokeCommand<number[] | null>("get_cloud_game_cover", { gameKey });
}

export function installCloudGame(gameUid: string, gameKey: string | undefined, remotePath: string, remoteFsId?: number): Promise<string> {
  return invokeCommand<string>("install_cloud_game", { gameUid, gameKey, remotePath, remoteFsId });
}

export function listRemoteBodyPackages(gameUid: string): Promise<RemoteBodyPackageList> {
  return invokeCommand<RemoteBodyPackageList>("list_remote_body_packages", { gameUid });
}

export function repairCloudBodyManifest(gameUid: string): Promise<string> {
  return invokeCommand<string>("repair_cloud_body_manifest", { gameUid });
}

export function uploadGameBodyPackage(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("upload_game_body_package", { gameUid, versionId });
}

export function downloadGameBodyPackage(gameUid: string, remotePath: string, remoteFsId?: number): Promise<string> {
  return invokeCommand<string>("download_game_body_package", { gameUid, remotePath, remoteFsId });
}

export function deleteRemoteBodyPackage(gameUid: string, gameKey: string, remotePath: string, remoteFsId?: number): Promise<string> {
  return invokeCommand<string>("delete_remote_body_package", { gameUid, gameKey, remotePath, remoteFsId });
}

export function updateBaiduSaveSyncSettings(
  autoSyncSave: boolean,
  checkCloudSaveOnLaunch: boolean,
  cloudSaveKeepLimit: number,
): Promise<BaiduConfigView> {
  return invokeCommand<BaiduConfigView>("update_baidu_save_sync_settings", {
    autoSyncSave,
    checkCloudSaveOnLaunch,
    cloudSaveKeepLimit,
  });
}

export function getCloudSaveStatus(gameUid: string): Promise<import("./domain/game").CloudSaveSyncStatusView> {
  return invokeCommand<import("./domain/game").CloudSaveSyncStatusView>("get_cloud_save_status", { gameUid });
}

export function listCloudSaveVersions(gameUid: string): Promise<import("./domain/game").CloudSaveManifestVersion[]> {
  return invokeCommand<import("./domain/game").CloudSaveManifestVersion[]>("list_cloud_save_versions", { gameUid });
}

export function startUploadSaveVersionTask(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("start_upload_save_version_task", { gameUid, versionId });
}

export function startRestoreCloudSaveTask(gameUid: string, versionId: string): Promise<string> {
  return invokeCommand<string>("start_restore_cloud_save_task", { gameUid, versionId });
}

export function deleteCloudSaveVersion(gameUid: string, versionId: string): Promise<import("./domain/game").CloudSaveManifestVersion[]> {
  return invokeCommand<import("./domain/game").CloudSaveManifestVersion[]>("delete_cloud_save_version", { gameUid, versionId });
}

export type { SaveLearningResult, SaveLearningSession };
