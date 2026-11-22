import { ref, type Ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { listTasks, type AppTask, type TaskCategory } from "./api";

/**
 * 任务分类的呈现策略——**全应用唯一**的一张表。
 *
 * 分类本身由后端给出（`AppTask.category`），这里只声明「每类任务在前端长什么样」。
 * 此前这份判断是 `App.vue` 与 `TransferCenter.vue` 各写一份的逐字重复白名单，只
 * 覆盖 6/20 个任务类型。`satisfies` 把完整性钉在编译期：后端一旦新增分类，这里
 * 会直接报错，而不是静默漏掉。
 */
interface TaskCategoryPolicy {
  /** 是否进入传输中心的任务列表。 */
  visible: boolean;
  /** 是否提供「取消」按钮。 */
  cancellable: boolean;
  /** 是否计入导航栏的进行中角标。 */
  badge: boolean;
  /** 列表标题的兜底文案（具体任务类型另有更细的标题）。 */
  label: string;
  /** 取消确认文案。 */
  cancelHint: string;
}

const TASK_CATEGORY_POLICY = {
  body_transfer: {
    visible: true,
    cancellable: true,
    badge: true,
    label: "本体传输",
    cancelHint: "取消后会中断当前的本体操作，未完成的部分不会生效，可稍后重试。确定取消吗？",
  },
  cloud_save_sync: {
    visible: true,
    cancellable: true,
    badge: true,
    label: "云存档同步",
    cancelHint:
      "取消后会中断本次云存档同步，已上传的临时分片可能由百度网盘自动清理。确定取消吗？",
  },
  save_restore: {
    visible: true,
    // 还原会先 commit 保护当前存档、再成批覆盖存档目录；中途放弃会留下写了一半的
    // 存档，要支持取消得先设计回滚。所以这里明确不给取消入口，而不是给一个按了
    // 没反应的按钮。
    cancellable: false,
    badge: true,
    label: "恢复存档",
    cancelHint: "",
  },
  session: {
    visible: true,
    // 取消即结束整场会话：后端会终止整棵进程树，且本次会话不保存为存档版本。
    cancellable: true,
    // 游戏会话不是「传输」，运行中已由库卡片与详情页表达，不重复计入角标。
    badge: false,
    label: "启动游戏",
    cancelHint:
      "取消会立即结束这场游戏会话并终止游戏进程，本次会话不会保存为新的存档版本。确定取消吗？",
  },
  maintenance: {
    visible: false,
    cancellable: false,
    badge: false,
    label: "后台维护",
    cancelHint: "",
  },
} satisfies Record<TaskCategory, TaskCategoryPolicy>;

/**
 * 上传任务是唯一需要「已上传的分片由网盘清理」这句具体交代的类型，单独覆盖文案；
 * 其余同类任务用分类级兜底即可。
 */
const CANCEL_HINT_OVERRIDES: Partial<Record<string, string>> = {
  update_game_body:
    "检查和复制阶段可以取消；开始交换游戏本体后将关闭取消入口，并完成存档保护和记录提交。确定取消吗？",
  retry_game_body_update:
    "开始恢复更新事务后将关闭取消入口，系统会保留可恢复现场。确定取消吗？",
  uninstall_game_body:
    "仅提交前可以取消；开始移出本体后将关闭取消入口，操作进度可在传输中心查看。确定取消吗？",
  remove_game_from_library:
    "仅提交前可以取消；开始移除游戏记录后将关闭取消入口，云端文件不会受影响。确定取消吗？",
  upload_game_body_package:
    "取消后会停止后续分片处理，已上传的临时分片可能由百度网盘自动清理。确定取消吗？",
};

/** 取任务的分类。认不出的分类按 `maintenance`（不展示）处理——宁可少显示，也不误报。 */
export function taskCategoryOf(task: AppTask): TaskCategory {
  const category = task.category as string | undefined;
  if (category && Object.prototype.hasOwnProperty.call(TASK_CATEGORY_POLICY, category)) {
    return category as TaskCategory;
  }
  return "maintenance";
}

/** 取任务的分类策略。 */
export function taskPolicyOf(task: AppTask): TaskCategoryPolicy {
  const policy = TASK_CATEGORY_POLICY[taskCategoryOf(task)];
  return task.cancellable === false ? { ...policy, cancellable: false } : policy;
}

/** 取消确认文案：优先按任务类型取精确文案，否则用分类级兜底。 */
export function taskCancelHint(task: AppTask): string {
  return CANCEL_HINT_OVERRIDES[task.taskType] ?? taskPolicyOf(task).cancelHint;
}

/**
 * 全应用共享的任务流。
 *
 * 后端在任何任务状态变化时推送 `task-changed`，这里收到后就地刷新；定时轮询只
 * **兜底**（有活跃任务 2s、空闲 10s），不再由各组件各拉一遍 —— 此前 `App.vue`
 * 与 `TransferCenter.vue` 各轮询一次 `listTasks`，且根组件那份最小周期 1s、
 * 自续期永不自停，连窗口最小化时也在跑。
 *
 * 推送是尽力而为的：监听失败只影响实时性，兜底轮询仍能保证状态最终正确。
 * 多个组件同时订阅会复用同一份数据与同一个监听器，组件卸载时务必 `release()`。
 */
const tasks = ref<AppTask[]>([]);
const loading = ref(true);
const error = ref("");

const TASK_CHANGED_EVENT = "task-changed";
/** 后端一次进度推进可能连发多个事件，合并成一个刷新窗口。 */
const EVENT_DEBOUNCE_MS = 80;
const ACTIVE_FALLBACK_MS = 2000;
const IDLE_FALLBACK_MS = 10000;

let subscribers = 0;
let loadGeneration = 0;
let inflight: Promise<void> | undefined;
let refreshRequested = false;
let debounceTimer: ReturnType<typeof setTimeout> | undefined;
let fallbackTimer: ReturnType<typeof setTimeout> | undefined;
let stopListening: UnlistenFn | undefined;
let listening = false;

export interface TaskFeed {
  tasks: Ref<AppTask[]>;
  loading: Ref<boolean>;
  error: Ref<string>;
  refresh: () => Promise<void>;
  release: () => void;
}

function hasActiveTask(): boolean {
  return tasks.value.some((task) => task.status === "pending" || task.status === "running");
}

function scheduleFallback() {
  if (fallbackTimer) clearTimeout(fallbackTimer);
  if (subscribers === 0) return;
  fallbackTimer = setTimeout(
    () => void refresh(),
    hasActiveTask() ? ACTIVE_FALLBACK_MS : IDLE_FALLBACK_MS,
  );
}

/** 重新拉取任务列表；并发调用合并为同一次请求。 */
export function refresh(): Promise<void> {
  refreshRequested = true;
  if (inflight) return inflight;
  const generation = ++loadGeneration;
  const request = (async () => {
    do {
      refreshRequested = false;
      try {
        const loaded = await listTasks();
        if (generation !== loadGeneration) return;
        if (refreshRequested) continue;
        tasks.value = loaded;
        error.value = "";
      } catch (reason) {
        if (generation !== loadGeneration) return;
        if (refreshRequested) continue;
        error.value = String(reason);
      }
    } while (refreshRequested && generation === loadGeneration);
  })().finally(() => {
    if (inflight !== request) return;
    inflight = undefined;
    if (refreshRequested) return refresh();
    loading.value = false;
    scheduleFallback();
  });
  inflight = request;
  return inflight;
}

function scheduleEventRefresh() {
  if (subscribers === 0) return;
  if (debounceTimer) clearTimeout(debounceTimer);
  debounceTimer = setTimeout(() => void refresh(), EVENT_DEBOUNCE_MS);
}

async function startListening() {
  if (listening || stopListening) return;
  listening = true;
  try {
    const unlisten = await listen(TASK_CHANGED_EVENT, () => scheduleEventRefresh());
    if (subscribers === 0) unlisten();
    else stopListening = unlisten;
  } catch (reason) {
    console.error("监听任务变更事件失败，将只依赖兜底轮询", reason);
  } finally {
    listening = false;
  }
}

function release() {
  subscribers = Math.max(0, subscribers - 1);
  if (subscribers > 0) return;
  loadGeneration++;
  refreshRequested = false;
  inflight = undefined;
  stopListening?.();
  stopListening = undefined;
  if (debounceTimer) clearTimeout(debounceTimer);
  debounceTimer = undefined;
  if (fallbackTimer) clearTimeout(fallbackTimer);
  fallbackTimer = undefined;
}

/** 订阅共享任务流。 */
export function useTaskFeed(): TaskFeed {
  subscribers += 1;
  if (subscribers === 1) {
    void startListening();
    void refresh();
  }
  let released = false;
  return {
    tasks, loading, error, refresh,
    release: () => {
      if (released) return;
      released = true;
      release();
    },
  };
}
