<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";
import { ArrowLeft, Check, CheckCircle2, FolderOpen, Gamepad2, LoaderCircle, Plus, Trash2, X } from "@lucide/vue";
import { open } from "@tauri-apps/plugin-dialog";
import {
  cancelSaveLearning,
  cancelTask,
  confirmSaveProfile,
  discardPendingGame,
  getGame,
  getTask,
  startAddGameTask,
  startFinishSaveLearningTask,
  startSaveLearningTask,
  type AppTask,
} from "../api";
import type { Game, SaveLearningResult, SaveLearningSession, SaveRootType, SaveScope } from "../domain/game";

type WizardPhase = "form" | "copying" | "ready" | "capturing" | "analyzing" | "review" | "done";

const emit = defineEmits<{ (event: "back"): void; (event: "completed", game: Game): void }>();

const phase = ref<WizardPhase>("form");
const displayName = ref("");
const gameKey = ref("");
const sourcePath = ref("");
const executablePath = ref("");
const taskId = ref("");
const session = ref<SaveLearningSession | null>(null);
const learningResult = ref<SaveLearningResult | null>(null);
const reviewScopes = ref<SaveScope[]>([]);
const confidence = ref(0);
const progress = ref(0);
const message = ref("");
const error = ref("");
const completedGame = ref<Game | null>(null);
const confirming = ref(false);
const newFileByScope = ref<Record<number, string>>({});
const newPatternByScope = ref<Record<number, string>>({});
let pollTimer: ReturnType<typeof setTimeout> | undefined;

const isBusy = computed(() => phase.value === "copying" || phase.value === "analyzing");
const canStart = computed(() => Boolean(displayName.value.trim() && sourcePath.value.trim() && executablePath.value.trim()) && phase.value === "form");
const stepNumber = computed(() => phase.value === "form" || phase.value === "copying" ? 1 : phase.value === "ready" || phase.value === "capturing" ? 2 : phase.value === "analyzing" || phase.value === "review" ? 3 : 4);
const stepTitle = computed(() => phase.value === "form" || phase.value === "copying" ? "选择并复制游戏本体" : phase.value === "ready" || phase.value === "capturing" ? "完成一次游戏内保存" : phase.value === "analyzing" ? "分析存档变化" : phase.value === "review" ? "确认存档保护范围" : "添加完成");
const canConfirm = computed(() => reviewScopes.value.length > 0 && reviewScopes.value.every((scope) => scope.confirmedFiles.length > 0 || scope.includeDirectories.length > 0));

const rootTypeLabel: Record<SaveRootType, string> = {
  managed_game: "游戏目录",
  app_data: "AppData",
  local_app_data: "LocalAppData",
  documents: "文档",
  user_profile: "用户目录",
  custom: "自定义目录",
};

async function chooseSource() {
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  sourcePath.value = selected;
  if (!displayName.value) {
    displayName.value = selected.split(/[\\/]/).filter(Boolean).pop() || "";
    gameKey.value = normalizeGameKey(displayName.value);
  }
}

function normalizeGameKey(value: string): string {
  return value.trim().split(/\s+/).join(" ").toLocaleLowerCase();
}

async function chooseExecutable() {
  const selected = await open({ directory: false, multiple: false, filters: [{ name: "Windows 程序", extensions: ["exe"] }] });
  if (typeof selected === "string") executablePath.value = selected;
}

function stopPolling() {
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = undefined;
}

function handleTaskFailure(task: AppTask, cancelledMessage: string, failedMessage: string) {
  stopPolling();
  taskId.value = "";
  error.value = task.error || (task.status === "cancelled" ? cancelledMessage : failedMessage);
  phase.value = completedGame.value ? "ready" : "form";
}

async function watchTask(onSuccess: (task: AppTask) => Promise<void>, cancelledMessage: string, failedMessage: string, onFailure?: (task: AppTask) => Promise<void>) {
  const watchedTaskId = taskId.value;
  if (!watchedTaskId) return;
  try {
    const task = await getTask(watchedTaskId);
    progress.value = task.progress;
    message.value = task.message;
    if (task.status === "success") {
      stopPolling();
      taskId.value = "";
      await onSuccess(task);
      return;
    }
    if (task.status === "failed" || task.status === "cancelled") {
      handleTaskFailure(task, cancelledMessage, failedMessage);
      if (onFailure) await onFailure(task);
      return;
    }
    pollTimer = setTimeout(() => void watchTask(onSuccess, cancelledMessage, failedMessage, onFailure), 350);
  } catch (reason) {
    stopPolling();
    taskId.value = "";
    error.value = String(reason);
    phase.value = completedGame.value ? "ready" : "form";
  }
}

async function submitAdd(allowLargeSource: boolean) {
  if (!canStart.value) return;
  error.value = "";
  progress.value = 0;
  message.value = "准备复制游戏本体";
  phase.value = "copying";
  try {
    taskId.value = await startAddGameTask({ displayName: displayName.value, gameKey: normalizeGameKey(gameKey.value || displayName.value), sourcePath: sourcePath.value, executablePath: executablePath.value, allowLargeSource });
    await watchTask(async (task) => {
      completedGame.value = task.gameUid ? await getGame(task.gameUid) : null;
      if (!completedGame.value) throw new Error("游戏已复制，但没有找到待设置的游戏记录");
      phase.value = "ready";
      message.value = "游戏本体已准备好";
    }, "已取消复制", "添加游戏失败", async (task) => {
      const warning = task.error || "";
      if (!allowLargeSource && warning.includes("超过 3 GB")) {
        error.value = "";
        if (window.confirm(warning)) await submitAdd(true);
        else error.value = warning;
      }
    });
  } catch (reason) {
    phase.value = "form";
    error.value = String(reason);
  }
}

async function start() {
  await submitAdd(false);
}

async function beginLearning() {
  if (!completedGame.value || phase.value !== "ready") return;
  error.value = "";
  message.value = "准备启动游戏并记录存档变化";
  phase.value = "capturing";
  try {
    taskId.value = await startSaveLearningTask(completedGame.value.gameUid);
    await watchTask(async (task) => {
      const learnedSession = task.result as SaveLearningSession | undefined;
      if (!learnedSession?.sessionId) throw new Error("学习会话没有正确建立");
      session.value = learnedSession;
      message.value = "游戏已启动，请完成一次保存";
    }, "已取消存档识别", "启动存档识别失败");
  } catch (reason) {
    phase.value = "ready";
    error.value = String(reason);
  }
}

async function analyze() {
  if (!session.value || phase.value !== "capturing") return;
  error.value = "";
  message.value = "正在分析保存前后的变化";
  progress.value = 0;
  phase.value = "analyzing";
  try {
    taskId.value = await startFinishSaveLearningTask(session.value.sessionId);
    await watchTask(async (task) => {
      const result = task.result as SaveLearningResult | undefined;
      if (!result) throw new Error("分析完成，但没有返回学习结果");
      learningResult.value = result;
      reviewScopes.value = result.scopeDrafts.map((draft) => ({ ...draft.scope, confirmedFiles: [...draft.scope.confirmedFiles], includeDirectories: [...draft.scope.includeDirectories], excludeExact: [...draft.scope.excludeExact], excludePatterns: [...draft.scope.excludePatterns], excludeDirectories: [...draft.scope.excludeDirectories] }));
      confidence.value = result.confidence;
      phase.value = "review";
    }, "已取消分析", "存档分析失败");
  } catch (reason) {
    phase.value = "capturing";
    error.value = String(reason);
  }
}

function addFile(scopeIndex: number) {
  const value = (newFileByScope.value[scopeIndex] || "").trim().replace(/\\/g, "/");
  if (!value) return;
  const scope = reviewScopes.value[scopeIndex];
  if (scope && !scope.confirmedFiles.includes(value)) scope.confirmedFiles.push(value);
  newFileByScope.value[scopeIndex] = "";
}

function addPattern(scopeIndex: number) {
  const value = (newPatternByScope.value[scopeIndex] || "").trim();
  if (!value) return;
  const scope = reviewScopes.value[scopeIndex];
  if (scope && !scope.excludePatterns.includes(value)) scope.excludePatterns.push(value);
  newPatternByScope.value[scopeIndex] = "";
}

function removeFile(scopeIndex: number, fileIndex: number) {
  reviewScopes.value[scopeIndex]?.confirmedFiles.splice(fileIndex, 1);
}

function removePattern(scopeIndex: number, patternIndex: number) {
  reviewScopes.value[scopeIndex]?.excludePatterns.splice(patternIndex, 1);
}

function removeScope(scopeIndex: number) {
  reviewScopes.value.splice(scopeIndex, 1);
}

async function addDirectoryScope() {
  const selected = await open({ directory: true, multiple: false });
  if (typeof selected !== "string") return;
  reviewScopes.value.push({ rootType: "custom", rootPath: selected, confirmedFiles: [], includeDirectories: ["."], excludeExact: [], excludePatterns: [], excludeDirectories: [], unknownFilePolicy: "protect", maxFileBytes: 10 * 1024 * 1024 });
}

async function confirm() {
  if (!completedGame.value || !canConfirm.value || confirming.value) return;
  error.value = "";
  confirming.value = true;
  try {
    await confirmSaveProfile(completedGame.value.gameUid, reviewScopes.value, confidence.value);
    completedGame.value = await getGame(completedGame.value.gameUid);
    if (!completedGame.value) throw new Error("存档保护已保存，但游戏记录读取失败");
    phase.value = "done";
  } catch (reason) {
    error.value = String(reason);
  } finally {
    confirming.value = false;
  }
}

async function cancelTaskOrLearning() {
  if (taskId.value) {
    await cancelTask(taskId.value);
    message.value = "正在取消...";
    return;
  }
  if (session.value) await cancelSaveLearning(session.value.sessionId);
}

async function abandonPendingGame() {
  if (!completedGame.value) {
    emit("back");
    return;
  }
  if (phase.value === "capturing") await cancelSaveLearning(session.value?.sessionId || "");
  if (phase.value === "capturing" || phase.value === "analyzing") {
    error.value = "请先结束当前分析任务，再放弃这次添加";
    return;
  }
  try {
    await discardPendingGame(completedGame.value.gameUid);
    emit("back");
  } catch (reason) {
    error.value = String(reason);
  }
}

onUnmounted(stopPolling);
</script>

<template>
  <section class="wizard-page page-enter">
    <header class="wizard-header">
      <button class="icon-button" type="button" title="返回游戏库" aria-label="返回游戏库" :disabled="isBusy || phase === 'capturing'" @click="abandonPendingGame"><ArrowLeft :size="18" /></button>
      <div><p class="eyebrow">添加游戏</p><h1>把游戏加入 GameSaver</h1><p>先复制游戏本体，再确认它的存档保护范围。</p></div>
    </header>

    <div class="step-indicator">
      <span class="step" :class="{ active: stepNumber >= 1 }">1</span><i></i><span class="step" :class="{ active: stepNumber >= 2 }">2</span><i></i><span class="step" :class="{ active: stepNumber >= 3 }">3</span><i></i><span class="step" :class="{ active: stepNumber >= 4 }">4</span>
      <div><strong>{{ stepTitle }}</strong><small>第 {{ stepNumber }} 步，共 4 步</small></div>
    </div>

    <form v-if="phase === 'form' || phase === 'copying'" class="wizard-form" @submit.prevent="start">
      <section class="wizard-section"><h2>游戏信息</h2><label class="field"><span>游戏名称</span><input v-model="displayName" :disabled="phase === 'copying'" type="text" placeholder="例如：Black Market" @input="!gameKey && (gameKey = normalizeGameKey(displayName))" /></label><label class="field"><span>游戏标识</span><input v-model="gameKey" :disabled="phase === 'copying'" type="text" placeholder="用于关联云端游戏" /><small class="field-note">默认由游戏名称生成，确认后不随显示名称变化。</small></label></section>
      <section class="wizard-section"><h2>游戏本体目录</h2><p class="field-note">GameSaver 会复制一份本体到自己的游戏库，原始目录不会被修改。</p><div class="path-row"><input v-model="sourcePath" :disabled="phase === 'copying'" type="text" placeholder="选择游戏所在文件夹" /><button type="button" :disabled="phase === 'copying'" title="选择游戏目录" @click="chooseSource"><FolderOpen :size="17" />选择</button></div></section>
      <section class="wizard-section"><h2>启动程序</h2><p class="field-note">启动程序必须位于游戏本体目录内。</p><div class="path-row"><input v-model="executablePath" :disabled="phase === 'copying'" type="text" placeholder="选择游戏 EXE" /><button type="button" :disabled="phase === 'copying'" title="选择启动程序" @click="chooseExecutable"><Gamepad2 :size="17" />选择</button></div></section>
      <div v-if="phase === 'copying'" class="task-progress"><div class="task-progress-heading"><span>{{ message || "正在处理" }}</span><strong>{{ progress }}%</strong></div><div class="progress-track"><span :style="{ width: `${progress}%` }"></span></div><button class="secondary-button" type="button" @click="cancelTaskOrLearning"><X :size="16" />取消复制</button></div>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="phase === 'copying'" @click="emit('back')">取消</button><button class="primary-button" type="submit" :disabled="!canStart"><LoaderCircle v-if="phase === 'copying'" :size="17" class="spin" />{{ phase === 'copying' ? "正在复制" : "开始添加" }}</button></footer>
    </form>

    <section v-else-if="phase === 'ready' || phase === 'capturing'" class="wizard-form">
      <section class="wizard-section learning-intro"><div class="section-icon"><Gamepad2 :size="22" /></div><div><h2>{{ completedGame?.displayName }} 的存档保护</h2><p>启动受管游戏，在游戏内完成一次保存。回来后点击分析，GameSaver 会根据变化生成候选范围。</p></div></section>
      <section class="wizard-section"><div class="task-progress-heading"><span>学习会话</span><strong v-if="session">PID {{ session.rootPid }}</strong><strong v-else>尚未启动</strong></div><p v-if="phase === 'ready'" class="field-note">只会记录本次学习期间的文件变化，不会立即创建正式存档版本。</p><p v-else class="field-note">完成一次保存后，先退出游戏，再回来分析本次变化。</p><div v-if="phase === 'capturing'" class="capture-state"><span class="loader"></span><strong>正在记录文件变化</strong><span>{{ message }}</span></div></section>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="phase === 'capturing'" @click="abandonPendingGame">放弃添加</button><button v-if="phase === 'capturing'" class="secondary-button" type="button" @click="cancelTaskOrLearning"><X :size="16" />停止识别</button><button v-if="phase === 'ready'" class="primary-button" type="button" @click="beginLearning"><Gamepad2 :size="17" />启动并开始识别</button><button v-else class="primary-button" type="button" @click="analyze"><Check :size="17" />完成保存，开始分析</button></footer>
    </section>

    <section v-else-if="phase === 'analyzing'" class="wizard-form">
      <section class="wizard-section analysis-state"><span class="loader"></span><h2>正在分析存档变化</h2><p>{{ message || "正在整理候选文件夹" }}</p><div class="progress-track"><span :style="{ width: `${progress}%` }"></span></div><strong>{{ progress }}%</strong></section>
      <footer class="wizard-actions"><button class="secondary-button" type="button" @click="cancelTaskOrLearning"><X :size="16" />取消分析</button></footer>
    </section>

    <section v-else-if="phase === 'review'" class="wizard-form">
      <section class="wizard-section result-summary"><div><p class="eyebrow">识别结果</p><h2>确认存档保护范围</h2><p>{{ learningResult?.changedFiles.length || 0 }} 个文件发生变化，已按目录整理为 {{ reviewScopes.length }} 个候选范围。</p><div class="evidence-summary"><span>{{ learningResult?.eventCaptureMode === "etw" ? "ETW + 快照证据" : "快照差异证据" }}</span><span v-if="learningResult?.transactionSummary">事务 {{ learningResult.transactionSummary.transactionCount }} 个 · {{ learningResult.transactionSummary.operationCount }} 条操作 · {{ learningResult.transactionSummary.status === "completed" ? "已确认" : learningResult.transactionSummary.status === "candidate" ? "候选" : "证据不足" }}</span></div></div><div class="confidence-score"><strong>{{ confidence }}%</strong><span>识别置信度</span></div></section>
      <section v-for="(scope, scopeIndex) in reviewScopes" :key="`${scope.rootPath}-${scopeIndex}`" class="wizard-section scope-editor">
        <header class="scope-heading"><div><span class="scope-type">{{ rootTypeLabel[scope.rootType] }}</span><h2>{{ scope.rootPath }}</h2></div><button class="icon-button danger-icon" type="button" title="删除这个保护范围" :aria-label="`删除 ${scope.rootPath}`" @click="removeScope(scopeIndex)"><Trash2 :size="16" /></button></header>
        <div class="editor-block"><div class="editor-label"><strong>保护文件</strong><span>{{ scope.confirmedFiles.length }} 项</span></div><div class="chip-list"><span v-for="(file, fileIndex) in scope.confirmedFiles" :key="file" class="file-chip">{{ file }}<button type="button" :aria-label="`删除 ${file}`" title="删除文件" @click="removeFile(scopeIndex, fileIndex)"><X :size="13" /></button></span><span v-if="!scope.confirmedFiles.length && !scope.includeDirectories.length" class="muted-text">暂无确认文件</span></div><div class="inline-editor"><input v-model="newFileByScope[scopeIndex]" type="text" placeholder="输入相对文件名，例如 save.dat" @keyup.enter="addFile(scopeIndex)" /><button class="secondary-button" type="button" @click="addFile(scopeIndex)"><Plus :size="15" />添加文件</button></div></div>
        <div v-if="scope.includeDirectories.length" class="editor-block"><div class="editor-label"><strong>保护目录</strong><span>{{ scope.includeDirectories.length }} 项</span></div><div class="chip-list"><span v-for="directory in scope.includeDirectories" :key="directory" class="file-chip directory-chip">{{ directory }}</span></div></div>
        <div class="editor-block"><div class="editor-label"><strong>排除模式</strong><span>{{ scope.excludePatterns.length }} 项</span></div><div class="chip-list"><span v-for="(pattern, patternIndex) in scope.excludePatterns" :key="pattern" class="file-chip exclude-chip">{{ pattern }}<button type="button" :aria-label="`删除排除模式 ${pattern}`" title="删除排除模式" @click="removePattern(scopeIndex, patternIndex)"><X :size="13" /></button></span><span v-if="!scope.excludePatterns.length" class="muted-text">暂未添加排除模式</span></div><div class="inline-editor"><input v-model="newPatternByScope[scopeIndex]" type="text" placeholder="输入排除模式，例如 *.log" @keyup.enter="addPattern(scopeIndex)" /><button class="secondary-button" type="button" @click="addPattern(scopeIndex)"><Plus :size="15" />添加排除</button></div></div>
        <p class="scope-note">大于 10 MB 的文件默认不会自动加入；确认后仍可在游戏设置中调整。</p>
      </section>
      <div v-if="!reviewScopes.length" class="empty-review"><strong>没有自动识别到存档范围</strong><p>可以手动添加一个存档目录，或放弃本次设置稍后重新学习。</p></div>
      <button class="secondary-button add-scope-button" type="button" @click="addDirectoryScope"><Plus :size="16" />手动添加存档目录</button>
      <div v-if="learningResult?.notes.length" class="notes-panel"><strong>识别说明</strong><p v-for="note in learningResult.notes" :key="note">{{ note }}</p></div>
      <p v-if="error" class="error-message" role="alert">{{ error }}</p>
      <footer class="wizard-actions"><button class="secondary-button" type="button" :disabled="confirming" @click="abandonPendingGame">放弃添加</button><button class="primary-button" type="button" :disabled="!canConfirm || confirming" @click="confirm"><LoaderCircle v-if="confirming" :size="17" class="spin" /><Check v-else :size="17" />{{ confirming ? "正在保存" : "确认并加入游戏库" }}</button></footer>
    </section>

    <div v-else class="wizard-success"><CheckCircle2 :size="34" /><div><h2>{{ completedGame?.displayName }} 已加入游戏库</h2><p>存档保护范围已确认，现在可以从游戏库启动它。</p></div><button class="primary-button" type="button" @click="emit('completed', completedGame!)">返回游戏库</button></div>
  </section>
</template>
