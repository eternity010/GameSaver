export interface CandidatePath {
  path: string;
  score: number;
  changedFiles: number;
  addedFiles: number;
  modifiedFiles: number;
  matchedSignals: string[];
  collapsed: boolean;
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
export type InjectionStatus = "not_required" | "pending" | "noop_injected" | "failed";
export type LauncherMode = "inject" | "sandbox" | "backup";

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
  injectionStatus: InjectionStatus;
  redirectRoot?: string;
  injectorExitCode?: number;
  hookVersion?: string;
  sandboxBoxName?: string;
  sandboxMirrorPaths?: string[];
  startedAt: string;
  updatedAt: string;
  logs: string[];
}

export interface ResolveRuleResult {
  exeHash: string;
  matchedRule?: GameSaveRule;
}

export interface RedirectRuntimeInfo {
  arch: string;
  injectorPath: string;
  dllPath: string;
  managedSaveRoot: string;
  backupRoot: string;
  injectorExists: boolean;
  dllExists: boolean;
  sandboxRoot: string;
  sandboxiePath: string;
  sandboxieExists: boolean;
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
  lastInjectionStatus?: string;
}

export interface BackupVersion {
  versionId: string;
  createdAt: string;
  fileCount: number;
}

export interface RestoreBackupResult {
  gameId: string;
  versionId: string;
  restoredFiles: number;
}

export interface ExportRulesResult {
  count: number;
}

export interface ImportRulesResult {
  imported: number;
  overwritten: number;
  skipped: number;
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
