export interface CandidatePath {
  path: string;
  score: number;
  changedFiles: number;
  addedFiles: number;
  modifiedFiles: number;
  matchedSignals: string[];
  recommendation: "strong" | "recommended" | "possible" | "weak";
  collapsed: boolean;
}

export type TaskStatus = "pending" | "running" | "success" | "failed";

export interface TaskState<T = unknown> {
  taskId: string;
  taskType: string;
  status: TaskStatus;
  progress?: number;
  message?: string;
  result?: T;
  error?: string;
  startedAt: string;
  updatedAt: string;
}

export interface LearningSession {
  sessionId: string;
  gameId: string;
  exePath: string;
  startedAt: string;
  endedAt?: string;
  status: "running" | "finished" | "cancelled";
  baselineSnapshotRef: string;
  finalSnapshotRef?: string;
  candidates: CandidatePath[];
  eventCaptureMode?: string;
  capturedEventCount?: number;
  trackedPids?: number[];
  eventCaptureError?: string;
  extraScanRoots?: string[];
}

export interface GameSaveRule {
  ruleId: string;
  gameId: string;
  gameUid: string;
  exeHash: string;
  confirmedPaths: string[];
  createdAt: string;
  confidence: number;
  enabled: boolean;
  updatedAt: string;
}

export interface RuntimeStatus {
  isAdmin: boolean;
  canUseEtw: boolean;
  message: string;
}

export type LauncherStatus = "idle" | "launching" | "running" | "failed" | "exited";
export type LauncherMode = "backup" | "backup_direct";

export interface LauncherSession {
  launcherSessionId: string;
  exePath: string;
  exeHash: string;
  matchedRuleId?: string;
  matchedGameId?: string;
  matchedGameUid?: string;
  launchMode?: LauncherMode;
  status: LauncherStatus;
  pid?: number;
  redirectRoot?: string;
  hookVersion?: string;
  startedAt: string;
  updatedAt: string;
  logs: string[];
}

export interface ResolveRuleResult {
  exeHash: string;
  matchedRule?: GameSaveRule;
}

export interface GameLibraryItem {
  gameId: string;
  totalRules: number;
  enabledRules: number;
  confirmedPathCount: number;
  lastRuleUpdatedAt: string;
  preferredExePath?: string;
  lastSessionId?: string;
  lastSessionStatus?: string;
  lastSessionUpdatedAt?: string;
}

export interface BackupVersion {
  versionId: string;
  createdAt: string;
  fileCount: number;
  label: string;
  restorable: boolean;
}

export interface BackupStatsResult {
  gameId: string;
  gameUid: string;
  totalBytes: number;
  versionCount: number;
  latestVersionId?: string;
  keepVersions: number;
}

export interface PruneBackupResult {
  gameId: string;
  gameUid: string;
  keepVersions: number;
  deletedVersions: number;
  freedBytes: number;
  remainingVersions: number;
  remainingBytes: number;
}

export interface RestoreBackupResult {
  gameId: string;
  versionId: string;
  restoredFiles: number;
  preRestoreVersionId?: string;
  verifiedFiles: number;
  hashSampleCount: number;
}

export interface ExportRulesResult {
  count: number;
}

export interface ImportRulesResult {
  imported: number;
  overwritten: number;
  skipped: number;
}

export interface RuleConflictItem {
  exeHash: string;
  ruleIds: string[];
  gameIds: string[];
  primaryRuleId?: string;
  conflictCount: number;
}

export interface ExportMigrationZipResult {
  ruleCount: number;
  backupGames: number;
  exportedFiles: number;
  skippedBackupGames: number;
}

export interface ImportMigrationZipResult {
  importedRules: number;
  overwrittenRules: number;
  skippedRules: number;
  importedBackupGames: number;
  copiedBackupFiles: number;
  skippedBackupGames: number;
}

export interface LaunchPrecheckCheck {
  key: string;
  label: string;
  ok: boolean;
  detail: string;
}

export interface SaveLocationSummary {
  exists: boolean;
  fileCount: number;
  totalBytes: number;
  latestModifiedAt?: string;
  resolvedPaths: string[];
  latestVersionId?: string;
}

export type LaunchSyncStatus =
  | "no_backup"
  | "backup_only"
  | "local_only"
  | "in_sync"
  | "local_newer"
  | "backup_newer"
  | "conflict_unknown";

export type LaunchSyncRecommendedAction =
  | "launch_direct"
  | "restore_then_launch"
  | "launch_after_manual_review";

export interface LaunchSyncDecision {
  status: LaunchSyncStatus;
  message: string;
  recommendedAction: LaunchSyncRecommendedAction;
  localSummary?: SaveLocationSummary;
  backupSummary?: SaveLocationSummary;
}

export interface GameLaunchPrecheck {
  gameId: string;
  preferredExePath?: string;
  exeHash?: string;
  matchedRuleId?: string;
  backupReady: boolean;
  syncDecision?: LaunchSyncDecision;
  checks: LaunchPrecheckCheck[];
  checkedAt: string;
}
