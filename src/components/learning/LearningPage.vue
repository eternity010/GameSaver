<script setup lang="ts">
import { computed } from "vue";
import type { CandidatePath, RepresentativeChangedFile } from "../../types";

type UiStep = "setup" | "running" | "results";
type TabState = {
  loading: boolean;
  error: string;
};
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
  eventCaptureMode: string;
  capturedEventCount: number;
  eventCaptureError: string;
  runtimeIsAdmin: boolean;
  runtimeMessage: string;
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
  (e: "relaunch-as-admin"): void;
}>();

const hasHighConfidence = computed(() => props.candidates.some((item) => item.score >= 45));
const candidateGroups = computed(() => [
  {
    key: "strong",
    title: "强推荐",
    description: "最像真实存档目录，通常可以直接选择。",
    items: props.candidates.filter((item) => item.recommendation === "strong"),
  },
  {
    key: "recommended",
    title: "推荐",
    description: "命中了多个有效信号，建议打开目录确认。",
    items: props.candidates.filter((item) => item.recommendation === "recommended"),
  },
  {
    key: "possible",
    title: "可能相关",
    description: "证据还不够强，只在你确认它是存档目录时选择。",
    items: props.candidates.filter((item) => item.recommendation === "possible"),
  },
  {
    key: "weak",
    title: "低可信",
    description: hasHighConfidence.value
      ? "多为配置、缓存或弱信号，不会自动勾选。"
      : "当前没有高可信候选，这一组也值得人工复查。",
    items: props.candidates.filter((item) => item.recommendation === "weak"),
  },
]);

function candidateRecommendationLabel(item: CandidatePath): string {
  switch (item.recommendation) {
    case "strong":
      return "强推荐";
    case "recommended":
      return "推荐";
    case "possible":
      return "可能相关";
    default:
      return "低可信";
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
  if (signal === "user-save-root") return "位于常见用户存档目录";
  if (signal === "game-dir") return "位于游戏目录";
  if (signal === "path-noise") return "包含缓存/日志等弱相关路径";
  if (signal === "path-noise-strong") return "命中强噪声目录";
  if (signal === "system-noise") return "像系统或常驻应用目录";
  if (signal === "filename-noise") return "文件名像配置/缓存/日志";
  if (signal.startsWith("extension:")) return `命中存档扩展名 .${signal.slice("extension:".length)}`;
  if (signal.startsWith("weak-extension:")) return `命中弱扩展名 .${signal.slice("weak-extension:".length)}`;
  if (signal.startsWith("noise-extension:")) return `命中噪声扩展名 .${signal.slice("noise-extension:".length)}`;
  return signal;
}

function candidateSignalSummary(item: CandidatePath): string {
  if (!item.matchedSignals.length) return "暂无明显理由";
  return item.matchedSignals.map(candidateSignalLabel).join(" / ");
}

function representativeFiles(item: CandidatePath) {
  return item.representativeChangedFiles ?? [];
}

function representativeFilesPreview(item: CandidatePath): RepresentativeChangedFile[] {
  return representativeFiles(item).slice(0, 3);
}

function representativeFilesRemaining(item: CandidatePath): RepresentativeChangedFile[] {
  return representativeFiles(item).slice(3);
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
  const date = new Date(value * 1000);
  if (Number.isNaN(date.getTime())) return "时间未知";
  return date.toLocaleString();
}

function learningBusyLabel(): string {
  if (props.learningBusyStage === "analyzing" && props.learningTaskMessage.trim()) {
    return props.learningTaskMessage.trim();
  }
  switch (props.learningBusyStage) {
    case "starting":
      return "正在启动游戏并创建学习会话...";
    case "analyzing":
      return "正在分析存档变化，这一步可能需要几十秒，请耐心等待。";
    case "saving":
      return "正在保存规则并同步到游戏库...";
    default:
      return "处理中...";
  }
}
</script>

<template>
  <div class="learning-page">
    <header class="panel learning-hero">
      <span class="eyebrow">学习存档</span>
      <h1>把游戏加入 GameSaver</h1>
      <p>选择游戏程序，进游戏保存一次，GameSaver 会帮你找出该备份哪些存档。</p>
      <p v-if="learningState.error" class="error inline-error">{{ learningState.error }}</p>
      <div class="learning-progress">
        <span :class="{ active: step === 'setup', done: step !== 'setup' }">添加游戏</span>
        <span :class="{ active: step === 'running', done: step === 'results' }">执行一次存档</span>
        <span :class="{ active: step === 'results' }">选择存档目录</span>
      </div>
    </header>

    <section v-if="step === 'setup'" class="panel learning-card">
      <span class="eyebrow">第一步</span>
      <h2>选择要保护的游戏</h2>
      <p class="learning-copy">先选择游戏 EXE。GameSaver 会自动填入游戏名称，之后也可以在规则里改。</p>
      <label class="field">
        <span>游戏名称</span>
        <input
          :value="gameId"
          placeholder="例如：MonsterBlackMarket"
          @input="emit('update:gameId', ($event.target as HTMLInputElement).value)"
        />
      </label>
      <label class="field">
        <span>游戏 EXE 路径</span>
        <div class="row">
          <input
            :value="exePath"
            placeholder="D:\\Games\\xxx\\game.exe"
            @input="emit('update:exePath', ($event.target as HTMLInputElement).value)"
          />
          <button type="button" @click="emit('choose-exe')">浏览</button>
        </div>
      </label>
      <details class="advanced-box learning-advanced-options">
        <summary>找不到存档时再添加扫描目录</summary>
        <p class="field-note">
          默认会扫描常见用户目录和游戏目录。只有存档在特殊位置时，才需要在这里补充目录。
        </p>
        <label class="field compact-field">
          <span>额外扫描目录</span>
          <textarea
            :value="extraScanRootsText"
            rows="3"
            placeholder="每行一个目录，例如：&#10;D:\\SteamLibrary\\steamapps\\compatdata&#10;E:\\Games\\SaveData"
            @input="emit('update:extraScanRootsText', ($event.target as HTMLTextAreaElement).value)"
          ></textarea>
        </label>
        <div class="row learning-advanced-actions">
          <button type="button" @click="emit('choose-extra-scan-root')">添加目录</button>
        </div>
      </details>
      <button :disabled="learningState.loading" type="button" class="primary" @click="emit('begin-learning')">
        {{ learningState.loading ? "正在启动游戏..." : "启动游戏，开始识别存档" }}
      </button>
    </section>

    <section v-else-if="step === 'running'" class="panel learning-card">
      <span class="eyebrow">第二步</span>
      <h2>进入游戏并手动保存一次</h2>
      <p class="learning-copy">在游戏里完成一次明确的保存动作。保存完成后，回到 GameSaver 继续分析。</p>
      <section v-if="learningState.loading && learningBusyStage === 'analyzing'" class="learning-loading-box">
        <strong>{{ learningBusyLabel() }}</strong>
        <div class="progress-track" role="progressbar" aria-label="正在分析存档变化">
          <span v-if="learningTaskProgress === null" class="progress-indeterminate"></span>
          <span
            v-else
            class="progress-determinate"
            :style="{ width: `${learningTaskProgress}%` }"
          ></span>
        </div>
        <p v-if="learningTaskProgress !== null">当前进度：{{ learningTaskProgress }}%</p>
        <p>期间请不要重复点击按钮，也不要关闭程序窗口。</p>
      </section>
      <ul class="learning-checklist">
        <li>游戏已启动</li>
        <li>进入游戏或读取一个已有存档</li>
        <li>手动保存一次</li>
        <li>回到 GameSaver 继续</li>
      </ul>
      <button :disabled="learningState.loading" type="button" class="primary" @click="emit('end-learning')">
        {{ learningState.loading ? "正在分析..." : "我已保存，查找存档目录" }}
      </button>
      <details class="runtime-diagnostics learning-advanced">
        <summary>采集详情（高级）</summary>
        <p>运行权限：{{ runtimeIsAdmin ? "管理员" : "普通用户" }}</p>
        <p>会话 ID：<code>{{ sessionId }}</code></p>
        <p>游戏 PID：{{ pid ?? "未获取" }}</p>
        <p>{{ runtimeMessage }}</p>
        <button v-if="!runtimeIsAdmin" type="button" @click="emit('relaunch-as-admin')">一键管理员重启</button>
      </details>
    </section>

    <section v-else class="panel learning-card">
      <span class="eyebrow">第三步</span>
      <h2>选择存档目录</h2>
      <p class="learning-copy">通常选择“强推荐”或“推荐”即可。不确定时，打开目录看看里面是否有存档文件。</p>
      <details class="runtime-diagnostics learning-advanced">
        <summary>采集详情（高级）</summary>
        <p>采集模式：{{ eventCaptureMode }} | 捕获事件数：{{ capturedEventCount }}</p>
        <p v-if="eventCaptureError" class="error">ETW 信息：{{ eventCaptureError }}</p>
      </details>
      <p v-if="!candidates.length" class="empty-hint">没有检测到候选目录。请确认刚才在游戏内执行了保存动作。</p>
      <div v-else class="candidate-groups">
        <section
          v-for="group in candidateGroups"
          :key="group.key"
          v-show="group.items.length"
          class="candidate-group"
        >
          <div class="candidate-group-head">
            <div>
              <h3>{{ group.title }}</h3>
              <p>{{ group.description }}</p>
            </div>
            <span>{{ group.items.length }} 项</span>
          </div>
          <ul class="candidate-list">
            <li v-for="item in group.items" :key="item.path" :class="{ collapsed: item.collapsed }">
              <div class="candidate-header">
                <label>
                  <input
                    :checked="selectedPaths.includes(item.path)"
                    type="checkbox"
                    :disabled="item.collapsed"
                    @change="emit('toggle-select', item.path)"
                  />
                  <strong>{{ item.path }}</strong>
                </label>
                <span class="candidate-rank" :class="candidateRecommendationClass(item)">
                  {{ candidateRecommendationLabel(item) }}
                </span>
                <button type="button" @click="emit('open-path', item.path)">打开目录</button>
              </div>
              <p>
                {{ candidateSignalSummary(item) }}
              </p>
              <details class="candidate-evidence">
                <summary>查看依据</summary>
                <p>
                  得分 {{ item.score }} · changed {{ item.changedFiles }} · added {{ item.addedFiles }} ·
                  modified {{ item.modifiedFiles }}
                </p>
                <div v-if="representativeFiles(item).length" class="candidate-file-evidence">
                  <strong>代表性变更文件</strong>
                  <ul>
                    <li v-for="file in representativeFilesPreview(item)" :key="file.path">
                      <div>
                        <span class="candidate-file-name">{{ changedFileName(file.path) }}</span>
                        <code>{{ changedFileRelativePath(file.path, item.path) }}</code>
                      </div>
                      <span>{{ changedFileKindLabel(file.changeKind) }}</span>
                      <span>{{ formatBytes(file.size) }}</span>
                      <time>{{ formatUnixTime(file.modifiedUnix) }}</time>
                    </li>
                  </ul>
                  <details v-if="representativeFilesRemaining(item).length" class="candidate-file-more">
                    <summary>还有 {{ representativeFilesRemaining(item).length }} 个代表性变更文件</summary>
                    <ul>
                      <li v-for="file in representativeFilesRemaining(item)" :key="`${item.path}-${file.path}`">
                        <div>
                          <span class="candidate-file-name">{{ changedFileName(file.path) }}</span>
                          <code>{{ changedFileRelativePath(file.path, item.path) }}</code>
                        </div>
                        <span>{{ changedFileKindLabel(file.changeKind) }}</span>
                        <span>{{ formatBytes(file.size) }}</span>
                        <time>{{ formatUnixTime(file.modifiedUnix) }}</time>
                      </li>
                    </ul>
                  </details>
                </div>
              </details>
            </li>
          </ul>
        </section>
      </div>
      <div class="row">
        <button :disabled="learningState.loading" type="button" class="primary" @click="emit('save-learning-rule')">
          保存规则并加入游戏库
        </button>
        <button :disabled="learningState.loading" type="button" @click="emit('update:step', 'setup')">重新学习</button>
      </div>
    </section>
  </div>
</template>
