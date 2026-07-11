<script setup lang="ts">
import { computed } from "vue";
import {
  BookOpenCheck,
  FolderOpen,
  Gamepad2,
  Play,
  RotateCcw,
  Save,
  ShieldCheck,
  X,
} from "@lucide/vue";
import type { CandidatePath, RepresentativeChangedFile } from "../../types";

type UiStep = "setup" | "running" | "results";
type TabState = { loading: boolean; error: string };
type LearningBusyStage = "" | "starting" | "analyzing" | "saving";

const props = defineProps<{
  step: UiStep;
  gameId: string;
  exePath: string;
  extraScanRootsText: string;
  sessionId: string;
  pid: number | null;
  candidates: CandidatePath[];
  selectedPaths: string[];
  learningState: TabState;
  learningBusyStage: LearningBusyStage;
  learningTaskMessage: string;
  learningTaskProgress: number | null;
}>();

const emit = defineEmits<{
  (e: "update:gameId", value: string): void;
  (e: "update:exePath", value: string): void;
  (e: "update:extraScanRootsText", value: string): void;
  (e: "update:step", value: UiStep): void;
  (e: "choose-exe"): void;
  (e: "choose-extra-scan-root"): void;
  (e: "begin-learning"): void;
  (e: "end-learning"): void;
  (e: "toggle-select", path: string): void;
  (e: "open-path", path: string): void;
  (e: "save-learning-rule"): void;
  (e: "retry-learning-analysis"): void;
  (e: "abandon-learning"): void;
}>();

const hasHighConfidence = computed(() => props.candidates.some((item) => item.score >= 45));
const primaryCandidates = computed(() => props.candidates.slice(0, 3));
const remainingCandidates = computed(() => props.candidates.slice(3));
const isAnalyzing = computed(() => props.learningState.loading && props.learningBusyStage === "analyzing");

function candidateRecommendationLabel(item: CandidatePath): string {
  switch (item.recommendation) {
    case "strong": return "强推荐";
    case "recommended": return "推荐";
    case "possible": return "可能相关";
    default: return "低可信";
  }
}

function candidateRecommendationClass(item: CandidatePath): string {
  return item.recommendation || "weak";
}

function candidateSignalLabel(signal: string): string {
  if (signal === "time-window") return "刚刚发生变化";
  if (signal === "path-keyword" || signal === "save-path-keyword") return "路径像存档目录";
  if (signal === "game-name-path") return "路径包含游戏名";
  if (signal === "save-filename") return "文件名像存档";
  if (signal === "size-reasonable") return "文件大小合理";
  if (signal === "user-save-root") return "位于常见存档目录";
  if (signal === "game-dir") return "位于游戏目录";
  if (signal.startsWith("extension:")) return `命中存档扩展名 .${signal.slice("extension:".length)}`;
  return signal;
}

function candidateSignalSummary(item: CandidatePath): string {
  if (!item.matchedSignals.length) return "暂无明显依据";
  return item.matchedSignals.slice(0, 4).map(candidateSignalLabel).join(" / ");
}

function representativeFilesPreview(item: CandidatePath): RepresentativeChangedFile[] {
  return (item.representativeChangedFiles ?? []).slice(0, 3);
}

function changedFileName(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).pop() || path;
}

function changedFileRelativePath(filePath: string, parentPath: string): string {
  const normalizedFile = filePath.replace(/\//g, "\\");
  const normalizedParent = parentPath.replace(/\//g, "\\").replace(/\\+$/, "");
  if (normalizedFile.toLowerCase().startsWith(`${normalizedParent.toLowerCase()}\\`)) {
    return normalizedFile.slice(normalizedParent.length + 1);
  }
  return normalizedFile;
}

function changedFileKindLabel(kind: string): string {
  return kind === "added" ? "新增" : "修改";
}

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let size = value;
  let unitIndex = 0;
  while (size >= 1024 && unitIndex < units.length - 1) {
    size /= 1024;
    unitIndex += 1;
  }
  return `${size >= 10 || unitIndex === 0 ? size.toFixed(0) : size.toFixed(1)} ${units[unitIndex]}`;
}

function formatUnixTime(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "时间未知";
  return new Date(value * 1000).toLocaleString();
}

function learningBusyLabel(): string {
  return props.learningTaskMessage.trim() || "正在分析存档变化，请稍候...";
}
</script>

<template>
  <div class="learning-page add-game-wizard">
    <header class="learning-wizard-header">
      <div>
        <span class="eyebrow">添加游戏</span>
        <h1>启用自动存档保护</h1>
        <p>选择游戏并完成一次保存，GameSaver 会自动识别存档位置。</p>
      </div>
      <div class="wizard-steps" aria-label="添加游戏进度">
        <span :class="{ active: step === 'setup', done: step !== 'setup' }"><Gamepad2 :size="16" />选择游戏</span>
        <span :class="{ active: step === 'running' && !isAnalyzing, done: step === 'results' || isAnalyzing }"><Save :size="16" />保存一次</span>
        <span :class="{ active: isAnalyzing, done: step === 'results' }"><BookOpenCheck :size="16" />分析变化</span>
        <span :class="{ active: step === 'results' }"><ShieldCheck :size="16" />确认保护</span>
      </div>
      <p v-if="learningState.error" class="error inline-error">{{ learningState.error }}</p>
    </header>

    <section v-if="step === 'setup'" class="wizard-surface wizard-select-game">
      <div class="wizard-icon"><Gamepad2 :size="30" /></div>
      <div class="wizard-intro">
        <span class="eyebrow">第一步</span>
        <h2>选择游戏程序</h2>
        <p>找到平时用于启动游戏的 EXE，游戏名称会自动填写。</p>
      </div>
      <button type="button" class="wizard-file-picker" @click="emit('choose-exe')">
        <Gamepad2 :size="22" />
        <span>
          <strong>{{ exePath ? gameId || "已选择游戏" : "选择游戏 EXE" }}</strong>
          <small>{{ exePath || "例如 Steam 游戏目录中的主程序" }}</small>
        </span>
      </button>
      <details class="advanced-box wizard-advanced">
        <summary>高级设置</summary>
        <div class="wizard-advanced-body">
          <label class="field">
            <span>游戏名称</span>
            <input
              :value="gameId"
              placeholder="自动从游戏路径识别"
              @input="emit('update:gameId', ($event.target as HTMLInputElement).value)"
            />
          </label>
          <label class="field">
            <span>额外扫描目录</span>
            <textarea
              :value="extraScanRootsText"
              rows="3"
              placeholder="仅在存档位于特殊目录时添加，每行一个目录"
              @input="emit('update:extraScanRootsText', ($event.target as HTMLTextAreaElement).value)"
            ></textarea>
          </label>
          <button type="button" @click="emit('choose-extra-scan-root')"><FolderOpen :size="16" />添加目录</button>
        </div>
      </details>
      <button :disabled="learningState.loading || !exePath" type="button" class="primary wizard-primary" @click="emit('begin-learning')">
        <Play :size="18" fill="currentColor" />{{ learningState.loading ? "正在启动..." : "启动游戏" }}
      </button>
    </section>

    <section v-else-if="step === 'running'" class="wizard-surface wizard-save-step">
      <template v-if="isAnalyzing">
        <div class="wizard-icon analyzing"><BookOpenCheck :size="30" /></div>
        <div class="wizard-intro">
          <span class="eyebrow">第三步</span>
          <h2>正在分析存档变化</h2>
          <p>{{ learningBusyLabel() }}</p>
        </div>
        <div class="progress-track wizard-progress" role="progressbar" aria-label="正在分析存档变化">
          <span v-if="learningTaskProgress === null" class="progress-indeterminate"></span>
          <span v-else class="progress-determinate" :style="{ width: `${learningTaskProgress}%` }"></span>
        </div>
        <p v-if="learningTaskProgress !== null" class="wizard-progress-label">{{ learningTaskProgress }}%</p>
      </template>
      <template v-else>
        <div class="wizard-icon"><Save :size="30" /></div>
        <div class="wizard-intro">
          <span class="eyebrow">第二步</span>
          <h2>在游戏中保存一次</h2>
          <p>进入游戏或读取已有进度，然后执行一次明确的手动保存。</p>
        </div>
        <ol class="wizard-save-list">
          <li><span>1</span>进入游戏或读取已有存档</li>
          <li><span>2</span>完成一次手动保存</li>
          <li><span>3</span>回到这里继续分析</li>
        </ol>
        <div class="wizard-actions">
          <button :disabled="learningState.loading" type="button" class="primary" @click="emit('end-learning')"><Save :size="17" />我已经保存</button>
          <button :disabled="learningState.loading" type="button" class="ghost danger-text" @click="emit('abandon-learning')"><X :size="17" />放弃添加</button>
        </div>
      </template>
      <details class="runtime-diagnostics wizard-diagnostics">
        <summary>诊断信息</summary>
        <p>会话 ID：<code>{{ sessionId }}</code></p>
        <p>游戏 PID：{{ pid ?? "未获取" }}</p>
      </details>
    </section>

    <section v-else class="wizard-surface wizard-results">
      <div class="wizard-results-head">
        <div>
          <span class="eyebrow">第四步</span>
          <h2>确认要保护的存档目录</h2>
          <p>{{ hasHighConfidence ? "已优先选出最可能的存档位置。" : "结果可信度较低，建议打开目录确认后再启用。" }}</p>
        </div>
        <span class="wizard-result-count">{{ candidates.length }} 个候选</span>
      </div>
      <p v-if="!candidates.length" class="empty-hint">没有检测到存档变化。回到游戏再保存一次，然后重新分析。</p>
      <ul v-else class="candidate-list wizard-candidate-list">
        <li v-for="item in primaryCandidates" :key="item.path" :class="{ selected: selectedPaths.includes(item.path) }">
          <div class="candidate-header">
            <label>
              <input :checked="selectedPaths.includes(item.path)" type="checkbox" :disabled="item.collapsed" @change="emit('toggle-select', item.path)" />
              <strong>{{ item.path }}</strong>
            </label>
            <span class="candidate-rank" :class="candidateRecommendationClass(item)">{{ candidateRecommendationLabel(item) }}</span>
            <button class="icon-button" type="button" title="打开目录" @click="emit('open-path', item.path)"><FolderOpen :size="16" /></button>
          </div>
          <p>{{ candidateSignalSummary(item) }}</p>
          <details v-if="representativeFilesPreview(item).length" class="candidate-evidence">
            <summary>查看代表性变更文件</summary>
            <div class="candidate-file-evidence">
              <ul>
                <li v-for="file in representativeFilesPreview(item)" :key="file.path">
                  <div><span class="candidate-file-name">{{ changedFileName(file.path) }}</span><code>{{ changedFileRelativePath(file.path, item.path) }}</code></div>
                  <span>{{ changedFileKindLabel(file.changeKind) }}</span>
                  <span>{{ formatBytes(file.size) }}</span>
                  <time>{{ formatUnixTime(file.modifiedUnix) }}</time>
                </li>
              </ul>
            </div>
          </details>
        </li>
      </ul>
      <details v-if="remainingCandidates.length" class="advanced-box other-candidates">
        <summary>查看其他候选（{{ remainingCandidates.length }}）</summary>
        <ul class="candidate-list compact-candidate-list">
          <li v-for="item in remainingCandidates" :key="item.path">
            <label>
              <input :checked="selectedPaths.includes(item.path)" type="checkbox" :disabled="item.collapsed" @change="emit('toggle-select', item.path)" />
              <span>{{ item.path }}</span>
            </label>
            <span class="candidate-rank" :class="candidateRecommendationClass(item)">{{ candidateRecommendationLabel(item) }}</span>
            <button class="icon-button" type="button" title="打开目录" @click="emit('open-path', item.path)"><FolderOpen :size="15" /></button>
          </li>
        </ul>
      </details>
      <div class="wizard-actions wizard-result-actions">
        <button :disabled="learningState.loading || !selectedPaths.length" type="button" class="primary" @click="emit('save-learning-rule')"><ShieldCheck :size="18" />{{ learningBusyStage === "saving" ? "正在启用..." : "启用存档保护" }}</button>
        <button :disabled="learningState.loading" type="button" @click="emit('retry-learning-analysis')"><RotateCcw :size="17" />回到游戏再保存</button>
        <button :disabled="learningState.loading" type="button" class="ghost danger-text" @click="emit('abandon-learning')"><X :size="17" />放弃添加</button>
      </div>
    </section>
  </div>
</template>
