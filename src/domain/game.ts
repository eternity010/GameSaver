export type GameLifecycle = "pending_setup" | "active" | "needs_repair" | "removing";
export type GameHealth = "needs_setup" | "ready" | "needs_attention" | "broken";
export type CloudStatus = "disabled" | "local_only" | "syncing" | "synced" | "failed";
export type GameRuntimeStatus = "launching" | "running" | "saving";

export interface LaunchConfig {
  executableRelativePath: string;
  arguments: string[];
  workingDirectoryRelativePath?: string;
}

export interface CoverCrop {
  aspectWidth: number;
  aspectHeight: number;
  outputWidth: number;
  outputHeight: number;
}

export interface CoverPosition {
  offsetXMilli: number;
  offsetYMilli: number;
  zoomMilli: number;
}

export interface GameCover {
  originalPath: string;
  displayPath: string;
  crop: CoverCrop;
  position: CoverPosition;
}

export interface Game {
  gameUid: string;
  gameKey: string;
  displayName: string;
  managedPath: string;
  lifecycle: GameLifecycle;
  health: GameHealth;
  cloudStatus: CloudStatus;
  launch: LaunchConfig;
  cover?: GameCover;
  saveProfileId?: string;
  lastPlayedAt?: string;
  latestSaveVersionId?: string;
}

export interface GameRuntime {
  gameUid: string;
  status: GameRuntimeStatus;
  pid?: number;
  startedAt?: string;
  taskId?: string;
}

export interface GameBodyVersion {
  versionId: string;
  gameUid: string;
  createdAt: string;
  archivePath: string;
  fileCount: number;
  totalBytes: number;
  packagePath?: string;
  packageSize?: number;
  sha256?: string;
  excludedItems: string[];
  uploadStatus?: string;
  remotePath?: string;
  remoteFsId?: number;
  remoteSize?: number;
}

export interface LaunchPrecheck {
  gameUid: string;
  canLaunch: boolean;
  executableExists: boolean;
  saveProfileReady: boolean;
  validScopeCount: number;
  issues: string[];
}

export interface SaveFileEntry {
  rootType: SaveRootType;
  rootPath?: string;
  relativePath: string;
  objectHash?: string;
  size: number;
  deleted: boolean;
}

export interface SaveVersion {
  versionId: string;
  gameUid: string;
  createdAt: string;
  files: SaveFileEntry[];
  totalBytes: number;
}

export interface SaveProfile {
  profileId: string;
  gameUid: string;
  executableHash: string;
  scopes: SaveScope[];
  detectionEvidence: string[];
  confidence: number;
  enabled: boolean;
  keepVersions?: number;
  createdAt: string;
  updatedAt: string;
}

export type SaveRootType = "managed_game" | "app_data" | "local_app_data" | "local_low" | "documents" | "saved_games" | "user_profile" | "custom";
export type UnknownFilePolicy = "protect" | "ignore";

export interface SaveScope {
  rootType: SaveRootType;
  rootPath: string;
  confirmedFiles: string[];
  includeDirectories: string[];
  excludeExact: string[];
  excludePatterns: string[];
  excludeDirectories: string[];
  unknownFilePolicy: UnknownFilePolicy;
  maxFileBytes?: number;
}

export interface SaveScopeDraft {
  scope: SaveScope;
  changedFiles: string[];
  confidence: number;
}

export interface SaveLearningSession {
  sessionId: string;
  gameUid: string;
  rootPid: number;
  startedAt: string;
  status: "capturing" | "finished" | "cancelled";
}

export interface SaveLearningResult {
  sessionId: string;
  changedFiles: string[];
  scopeDrafts: SaveScopeDraft[];
  confidence: number;
  notes: string[];
  eventCaptureMode: "etw" | "snapshot" | string;
  transactionSummary?: SaveTransactionSummary;
}

export interface SaveTransactionSummary {
  status: "completed" | "candidate" | "insufficient_evidence" | string;
  confidence: number;
  transactionCount: number;
  affectedFiles: string[];
  affectedDirectories: string[];
  startedAt?: string;
  endedAt?: string;
  operationCount: number;
  notes: string[];
}

export interface CloudSaveManifestVersion {
  versionId: string;
  createdAt: string;
  packagePath: string;
  packageFsId: number;
  packageSize: number;
  packageSha256?: string;
  fileCount: number;
  totalBytes: number;
  deviceName?: string;
}

export interface CloudSaveSyncStatusView {
  autoSyncSave: boolean;
  localVersionCount: number;
  cloudVersionCount: number;
  latestLocalCreatedAt?: string;
  latestCloudCreatedAt?: string;
  latestCloudVersionId?: string;
  syncState: "synced" | "local_ahead" | "cloud_ahead" | "no_cloud_saves" | "offline" | string;
  warnings: string[];
}

export function gameStatusLabel(game: Game): string {
  if (game.lifecycle === "pending_setup") return "需要设置";
  if (game.lifecycle === "needs_repair" || game.health === "broken" || game.health === "needs_attention") {
    return "需要处理";
  }
  return "可启动";
}
