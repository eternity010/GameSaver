import { invoke } from "@tauri-apps/api/core";
import type { Game, GameBodyVersion, GameRuntime, LaunchPrecheck, SaveLearningResult, SaveLearningSession, SaveProfile, SaveScope, SaveVersion } from "./domain/game";

export interface AppTask {
  taskId: string;
  taskType: string;
  status: "pending" | "running" | "success" | "failed" | "cancelled" | "interrupted";
  progress: number;
  message: string;
  gameUid?: string;
  error?: string;
  result?: unknown;
  retry?: TaskRetry;
}

export interface TaskRetry {
  operation: string;
  gameUid: string;
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

export interface BaiduConfigView {
  configured: boolean;
  appKey?: string;
  secretKeyConfigured: boolean;
  autoUploadBody: boolean;
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
}

export function listGames(): Promise<Game[]> {
  return invoke<Game[]>("list_games");
}

export function getGame(gameUid: string): Promise<Game | null> {
  return invoke<Game | null>("get_game", { gameUid });
}

export function startAddGameTask(input: {
  displayName: string;
  sourcePath: string;
  executablePath: string;
}): Promise<string> {
  return invoke<string>("start_add_game_task", input);
}

export function getTask(taskId: string): Promise<AppTask> {
  return invoke<AppTask>("get_task", { taskId });
}

export function listTasks(): Promise<AppTask[]> {
  return invoke<AppTask[]>("list_tasks");
}

export function cancelTask(taskId: string): Promise<void> {
  return invoke("cancel_task", { taskId });
}

export function startSaveLearningTask(gameUid: string): Promise<string> {
  return invoke<string>("start_save_learning_task", { gameUid });
}

export function startFinishSaveLearningTask(sessionId: string): Promise<string> {
  return invoke<string>("start_finish_save_learning_task", { sessionId });
}

export function cancelSaveLearning(sessionId: string): Promise<void> {
  return invoke("cancel_save_learning", { sessionId });
}

export function confirmSaveProfile(gameUid: string, scopes: SaveScope[], confidence: number): Promise<SaveProfile> {
  return invoke<SaveProfile>("confirm_save_profile", { gameUid, scopes, confidence });
}

export function discardPendingGame(gameUid: string): Promise<void> {
  return invoke("discard_pending_game", { gameUid });
}

export function precheckGameLaunch(gameUid: string): Promise<LaunchPrecheck> {
  return invoke<LaunchPrecheck>("precheck_game_launch", { gameUid });
}

export function launchGame(gameUid: string): Promise<string> {
  return invoke<string>("launch_game", { gameUid });
}

export function getGameRuntime(gameUid: string): Promise<GameRuntime | null> {
  return invoke<GameRuntime | null>("get_game_runtime", { gameUid });
}

export function listSaveVersions(gameUid: string): Promise<SaveVersion[]> {
  return invoke<SaveVersion[]>("list_save_versions", { gameUid });
}

export function restoreSaveVersion(gameUid: string, versionId: string): Promise<string> {
  return invoke<string>("restore_save_version", { gameUid, versionId });
}

export function deleteSaveVersion(gameUid: string, versionId: string): Promise<string> {
  return invoke<string>("delete_save_version", { gameUid, versionId });
}

export function pruneSaveVersions(gameUid: string, keepVersions: number): Promise<string> {
  return invoke<string>("prune_save_versions", { gameUid, keepVersions });
}

export function listGameBodyVersions(gameUid: string): Promise<GameBodyVersion[]> {
  return invoke<GameBodyVersion[]>("list_game_body_versions", { gameUid });
}

export function updateGameBody(gameUid: string, sourcePath: string): Promise<string> {
  return invoke<string>("update_game_body", { gameUid, sourcePath });
}

export function packageGameBody(gameUid: string): Promise<string> {
  return invoke<string>("package_game_body", { gameUid });
}

export function restoreGameBodyPackage(gameUid: string, versionId: string): Promise<string> {
  return invoke<string>("restore_game_body_package", { gameUid, versionId });
}

export function deleteGameBodyPackage(gameUid: string, versionId: string): Promise<string> {
  return invoke<string>("delete_game_body_package", { gameUid, versionId });
}

export function getBaiduStatus(): Promise<BaiduStatus> {
  return invoke<BaiduStatus>("get_baidu_status");
}

export function getBaiduConfig(): Promise<BaiduConfigView> {
  return invoke<BaiduConfigView>("get_baidu_config");
}

export function saveBaiduConfig(appKey: string, secretKey: string): Promise<BaiduConfigView> {
  return invoke<BaiduConfigView>("save_baidu_config", { appKey, secretKey });
}

export function setBaiduAutoUpload(enabled: boolean): Promise<BaiduConfigView> {
  return invoke<BaiduConfigView>("set_baidu_auto_upload", { enabled });
}

export function buildBaiduAuthorizeUrl(): Promise<string> {
  return invoke<string>("build_baidu_authorize_url");
}

export function exchangeBaiduCode(code: string): Promise<void> {
  return invoke("exchange_baidu_code", { code });
}

export function getBaiduQuota(): Promise<BaiduQuota> {
  return invoke<BaiduQuota>("get_baidu_quota");
}

export function listCloudGames(): Promise<CloudGameSummary[]> {
  return invoke<CloudGameSummary[]>("list_cloud_games");
}

export function installCloudGame(gameUid: string, remotePath: string, remoteFsId?: number): Promise<string> {
  return invoke<string>("install_cloud_game", { gameUid, remotePath, remoteFsId });
}

export function listRemoteBodyPackages(gameUid: string): Promise<RemoteBodyPackageList> {
  return invoke<RemoteBodyPackageList>("list_remote_body_packages", { gameUid });
}

export function repairCloudBodyManifest(gameUid: string): Promise<string> {
  return invoke<string>("repair_cloud_body_manifest", { gameUid });
}

export function uploadGameBodyPackage(gameUid: string, versionId: string): Promise<string> {
  return invoke<string>("upload_game_body_package", { gameUid, versionId });
}

export function downloadGameBodyPackage(gameUid: string, remotePath: string, remoteFsId?: number): Promise<string> {
  return invoke<string>("download_game_body_package", { gameUid, remotePath, remoteFsId });
}

export function deleteRemoteBodyPackage(gameUid: string, remotePath: string, remoteFsId?: number): Promise<string> {
  return invoke<string>("delete_remote_body_package", { gameUid, remotePath, remoteFsId });
}

export type { SaveLearningResult, SaveLearningSession };
