import { invoke } from "@tauri-apps/api/core";
import type { Game, GameRuntime, LaunchPrecheck, SaveLearningResult, SaveLearningSession, SaveProfile, SaveScope, SaveVersion } from "./domain/game";

export interface AppTask {
  taskId: string;
  taskType: string;
  status: "pending" | "running" | "success" | "failed" | "cancelled";
  progress: number;
  message: string;
  gameUid?: string;
  error?: string;
  result?: unknown;
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

export type { SaveLearningResult, SaveLearningSession };
