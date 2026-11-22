const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");
const { createRequire } = require("node:module");

const root = path.resolve(process.argv[2] || path.join(__dirname, ".."));
const projectRequire = createRequire(path.join(process.argv[3] || root, "package.json"));
const ts = projectRequire("typescript");
const vue = projectRequire("vue");
const passed = [];

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((accept, decline) => { resolve = accept; reject = decline; });
  return { promise, resolve, reject };
}

function clock() {
  let nextId = 0;
  const timers = new Map();
  return {
    setTimeout(callback, delay) { timers.set(++nextId, { callback, delay }); return nextId; },
    clearTimeout(timer) { timers.delete(timer); },
    fire(delay) {
      for (const [timer, entry] of [...timers]) {
        if (entry.delay === delay) { timers.delete(timer); entry.callback(); }
      }
    },
    delays() { return [...timers.values()].map(entry => entry.delay); },
  };
}

const emptyPage = { games: [], page: 1, totalCount: 0, totalPages: 1, hasMore: false };
const gameA = { gameUid: "game-a", gameKey: "key-a", displayName: "Game A", lifecycle: "active", health: "ready", executableRelativePath: "game.exe" };
const gameB = { ...gameA, gameUid: "game-b", gameKey: "key-b", displayName: "Game B" };
const version = { versionId: "v1", path: "/v1.zip", size: 10, fsId: 1 };

function load(relative, exposed, replacements = {}) {
  const source = fs.readFileSync(path.join(root, relative), "utf8");
  const original = relative.endsWith(".vue") ? source.match(/<script setup lang="ts">([\s\S]*?)<\/script>/)[1] : source;
  const parsed = ts.createSourceFile(relative, original, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  let script = original;
  for (const statement of [...parsed.statements].reverse()) {
    if (ts.isImportDeclaration(statement)) script = script.slice(0, statement.getFullStart()) + script.slice(statement.end);
  }
  const timer = clock();
  const unmount = [];
  const feed = { tasks: vue.ref([]), loading: vue.ref(false), error: vue.ref(""), refresh: async () => {}, release() {} };
  const context = {
    ref: vue.ref, computed: vue.computed, watch: vue.watch, nextTick: vue.nextTick,
    onMounted() {}, onUnmounted: callback => unmount.push(callback),
    defineProps: () => ({ game: gameA }), defineEmits: () => () => {},
    useTaskFeed: () => feed, listen: async () => () => {},
    taskPolicyOf: () => ({ badge: true }), gameStatusLabel: game => game.health === "ready" ? "可启动" : "需要处理",
    localStorage: { getItem: () => null, setItem() {} },
    setTimeout: timer.setTimeout, clearTimeout: timer.clearTimeout,
    listCloudGames: async () => emptyPage, listGames: async () => [], getGameCoverUrl: () => "",
    document: { querySelector: () => null }, console,
    ...replacements,
  };
  const compiled = ts.transpileModule(`${script}\nexports.probe = { ${exposed.join(", ")} };`, {
    fileName: relative.replace(/\.vue$/, ".ts"), reportDiagnostics: true,
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
  });
  assert.equal(compiled.diagnostics.filter(entry => entry.category === ts.DiagnosticCategory.Error).length, 0);
  const exports = {};
  const scope = vue.effectScope();
  scope.run(() => new Function("exports", ...Object.keys(context), compiled.outputText)(exports, ...Object.values(context)));
  return { ...exports.probe, clock: timer, dispose() { for (const callback of unmount) callback(); scope.stop(); } };
}

async function flush() {
  for (let turn = 0; turn < 12; turn++) { await Promise.resolve(); await vue.nextTick(); }
}

async function check(name, run) {
  try { await run(); passed.push(name); }
  catch (error) { error.message = `${name}: ${error.message}`; throw error; }
}

const appFields = ["activePage", "games", "cloudInstallUid", "cloudOperationTaskId", "cloudInstallProgress", "cloudInstallMessage", "cloudInstallNotice", "cloudInstallError", "cloudFeedbackGameKey", "beginCloudOperation", "endCloudOperation", "watchCloudInstall", "watchCloudDeletion", "deleteCloudVersion", "installAndLaunch"];

async function taskFeedChecks() {
  await check("task refresh drains changes received during a request", async () => {
    const requests = [];
    let changed;
    const feed = load("src/taskFeed.ts", ["useTaskFeed", "tasks"], {
      listTasks: () => { const request = deferred(); requests.push(request); return request.promise; },
      listen: async (_, callback) => { changed = callback; return () => {}; },
    });
    const subscription = feed.useTaskFeed();
    await flush();
    changed(); feed.clock.fire(80);
    const refreshed = subscription.refresh();
    requests[0].resolve([{ taskId: "deleted-task", status: "success" }]);
    await flush();
    assert.equal(requests.length, 2);
    assert.equal(feed.tasks.value.length, 0);
    requests[1].resolve([]);
    await refreshed;
    assert.equal(feed.tasks.value.length, 0);
    subscription.release(); feed.dispose();
  });
  await check("released task requests cannot overwrite a new subscription", async () => {
    const requests = [];
    const feed = load("src/taskFeed.ts", ["useTaskFeed", "tasks"], {
      listTasks: () => { const request = deferred(); requests.push(request); return request.promise; },
    });
    const previous = feed.useTaskFeed();
    previous.release();
    const current = feed.useTaskFeed();
    previous.release();
    requests[0].resolve([{ taskId: "old" }]); await flush();
    assert.equal(feed.tasks.value.length, 0);
    requests[1].resolve([{ taskId: "new", status: "running" }]); await flush();
    assert.equal(feed.tasks.value[0].taskId, "new");
    assert.ok(feed.clock.delays().includes(2000));
    current.release();
    assert.equal(feed.clock.delays().length, 0);
    feed.dispose();
  });
  await check("a refresh requested by a task watcher also drains", async () => {
    let calls = 0;
    let queued = false;
    const feed = load("src/taskFeed.ts", ["useTaskFeed", "tasks", "refresh"], { listTasks: async () => { calls++; return []; } });
    const stop = vue.watch(feed.tasks, () => { if (!queued) { queued = true; void feed.refresh(); } });
    const subscription = feed.useTaskFeed();
    await flush();
    assert.equal(calls, 2);
    stop(); subscription.release(); feed.dispose();
  });
}

async function overviewChecks() {
  for (const staleFails of [false, true]) {
    await check(`forced cloud overview rejects stale ${staleFails ? "failure" : "success"}`, async () => {
      const requests = [];
      const detail = load("src/components/GameDetailPage.vue", ["loadCloudSaveOverview", "cloudSaveStatus", "cloudSaveLoading", "cloudSaveError"], {
        getCloudSaveOverview: () => { const request = deferred(); requests.push(request); return request.promise; },
      });
      const previous = detail.loadCloudSaveOverview();
      assert.equal(detail.loadCloudSaveOverview(), previous);
      const current = detail.loadCloudSaveOverview(true);
      assert.notEqual(previous, current);
      assert.equal(requests.length, 2);
      requests[1].resolve({ status: { syncState: "local_ahead" }, versions: [] });
      await current;
      if (staleFails) requests[0].reject(new Error("old request failed"));
      else requests[0].resolve({ status: { syncState: "cloud_ahead" }, versions: [] });
      await previous;
      assert.equal(detail.cloudSaveStatus.value.syncState, "local_ahead");
      assert.equal(detail.cloudSaveError.value, "");
      assert.equal(detail.cloudSaveLoading.value, false);
      detail.dispose();
    });
  }
  await check("unmounted cloud overview cannot publish", async () => {
    const request = deferred();
    const detail = load("src/components/GameDetailPage.vue", ["loadCloudSaveOverview", "cloudSaveStatus"], { getCloudSaveOverview: () => request.promise });
    const pending = detail.loadCloudSaveOverview();
    detail.dispose();
    request.resolve({ status: { syncState: "cloud_ahead" }, versions: [] });
    await pending;
    assert.equal(detail.cloudSaveStatus.value, null);
  });
}

async function cloudOperationChecks() {
  await check("deletion owns the confirmation window and blocks duplicate submission", async () => {
    const confirmation = deferred();
    let confirms = 0;
    const started = [];
    const app = load("src/App.vue", appFields, {
      confirmAction: () => ++confirms === 1 ? confirmation.promise : Promise.resolve(true),
      deleteRemoteBodyPackage: async uid => { started.push(uid); return "task-a"; },
      getTask: async () => ({ status: "running", progress: 30, message: "deleting" }),
    });
    app.activePage.value = "store";
    const pending = app.deleteCloudVersion(gameA, version);
    await app.deleteCloudVersion(gameB, version);
    assert.equal(app.cloudInstallUid.value, gameA.gameUid);
    assert.equal(confirms, 1);
    confirmation.resolve(true); await pending;
    assert.deepEqual(started, [gameA.gameUid]);
    assert.equal(app.cloudOperationTaskId.value, "task-a");
    app.dispose();
  });
  for (const cancelAt of [1, 2]) {
    await check(`cancelling deletion confirmation ${cancelAt} releases ownership`, async () => {
      let confirms = 0;
      let started = false;
      const app = load("src/App.vue", appFields, {
        confirmAction: async () => ++confirms !== cancelAt,
        deleteRemoteBodyPackage: async () => { started = true; return "unexpected"; },
      });
      await app.deleteCloudVersion(gameA, version);
      assert.equal(app.cloudInstallUid.value, "");
      assert.equal(started, false);
      app.dispose();
    });
  }
  for (const watcher of ["watchCloudInstall", "watchCloudDeletion"]) {
    await check(`${watcher} reconnects after a transient read failure`, async () => {
      let task = null;
      const app = load("src/App.vue", appFields, {
        getTask: async () => { if (!task) throw new Error("temporary IPC error"); return task; },
      });
      app.activePage.value = "store";
      const generation = app.beginCloudOperation(gameA, "starting");
      app.cloudOperationTaskId.value = "task-a";
      await app[watcher]("task-a", gameA, generation);
      assert.equal(app.cloudInstallUid.value, gameA.gameUid);
      assert.ok(app.clock.delays().includes(2000));
      task = { status: "running", progress: 42, message: "working" };
      app.clock.fire(2000); await flush();
      assert.equal(app.cloudInstallProgress.value, 42);
      assert.equal(app.cloudInstallMessage.value, "working");
      task = { status: "success", progress: 100, message: "done" };
      app.clock.fire(700); await flush();
      assert.equal(app.cloudInstallUid.value, "");
      assert.equal(app.cloudFeedbackGameKey.value, gameA.gameKey);
      assert.ok(app.cloudInstallNotice.value.includes(gameA.displayName));
      app.clock.fire(5000);
      assert.equal(app.cloudInstallNotice.value, "");
      app.dispose();
    });
    for (const staleFails of [false, true]) {
      await check(`${watcher} ignores a previous operation's ${staleFails ? "failure" : "success"}`, async () => {
        const request = deferred();
        const app = load("src/App.vue", appFields, { getTask: () => request.promise });
        const oldGeneration = app.beginCloudOperation(gameA, "first");
        app.cloudOperationTaskId.value = "task-a";
        const pending = app[watcher]("task-a", gameA, oldGeneration);
        app.endCloudOperation();
        app.beginCloudOperation(gameB, "second");
        app.cloudOperationTaskId.value = "task-b";
        if (staleFails) request.reject(new Error("obsolete request failed"));
        else request.resolve({ status: "success", progress: 100, message: "old completion" });
        await pending;
        assert.equal(app.cloudInstallUid.value, gameB.gameUid);
        assert.equal(app.cloudInstallMessage.value, "second");
        assert.equal(app.cloudInstallNotice.value, "");
        app.dispose();
      });
    }
    await check(`${watcher} exits safely when a finished record has been removed`, async () => {
      const app = load("src/App.vue", appFields, { getTask: async () => { throw new Error("task not found"); } });
      app.activePage.value = "store";
      const generation = app.beginCloudOperation(gameA, "starting");
      app.cloudOperationTaskId.value = "task-a";
      await app[watcher]("task-a", gameA, generation);
      assert.equal(app.cloudInstallUid.value, "");
      assert.equal(app.clock.delays().includes(2000), false);
      assert.ok(app.cloudInstallError.value.includes("任务记录已不存在"));
      app.dispose();
    });
  }
}

async function navigationChecks() {
  for (const source of ["runtime", "launch"]) {
    await check(`late ${source} result preserves subsequent navigation`, async () => {
      const request = deferred();
      const app = load("src/App.vue", ["quickLaunch", "activePage", "selectedGame"], {
        getGameRuntime: () => source === "runtime" ? request.promise : Promise.resolve(null),
        launchGame: () => source === "launch" ? request.promise : Promise.resolve("task"),
      });
      const pending = app.quickLaunch(gameA);
      await flush();
      app.activePage.value = "settings";
      request.resolve(source === "runtime" ? null : "task");
      await pending;
      assert.equal(app.activePage.value, "settings");
      assert.equal(app.selectedGame.value, null);
      app.dispose();
    });
  }
  await check("repeated quick launch submits once and still opens its result", async () => {
    const request = deferred();
    let launches = 0;
    const app = load("src/App.vue", ["quickLaunch", "activePage", "selectedGame"], {
      getGameRuntime: () => request.promise,
      launchGame: async () => { launches++; return "task"; },
    });
    const pending = app.quickLaunch(gameA);
    await app.quickLaunch(gameA);
    request.resolve(null); await pending;
    assert.equal(launches, 1);
    assert.equal(app.activePage.value, "detail");
    assert.equal(app.selectedGame.value.gameUid, gameA.gameUid);
    app.dispose();
  });
}

async function searchAndPaginationChecks() {
  await check("library and store searches are isolated and leaving cancels debounce", async () => {
    const requests = [];
    const app = load("src/App.vue", ["librarySearch", "storeSearch", "openStore", "activePage"], {
      listCloudGames: async (_, __, keyword) => { requests.push(keyword); return emptyPage; },
    });
    app.librarySearch.value = "local"; await flush();
    app.openStore(); await flush();
    assert.deepEqual(requests, [""]);
    app.storeSearch.value = "cloud"; await flush();
    app.activePage.value = "library";
    app.clock.fire(280); await flush();
    assert.deepEqual(requests, [""]);
    assert.equal(app.librarySearch.value, "local");
    assert.equal(app.storeSearch.value, "cloud");
    app.openStore(); await flush();
    assert.deepEqual(requests, ["", "cloud"]);
    app.dispose();
  });
  await check("old search results cannot publish during the next debounce", async () => {
    const requests = [];
    const app = load("src/App.vue", ["storeSearch", "openStore", "cloudGames"], {
      listCloudGames: () => { const request = deferred(); requests.push(request); return request.promise; },
    });
    app.openStore();
    app.storeSearch.value = "new"; await flush();
    requests[0].resolve({ ...emptyPage, games: [gameA] }); await flush();
    assert.equal(app.cloudGames.value.length, 0);
    app.clock.fire(280); await flush();
    requests[1].resolve({ ...emptyPage, games: [gameB] }); await flush();
    assert.equal(app.cloudGames.value[0].gameUid, gameB.gameUid);
    app.dispose();
  });
  await check("pagination clamps using the displayed games after a refresh", async () => {
    const fixtures = activeCount => Array.from({ length: 25 }, (_, index) => ({
      gameUid: `game-${index}`, displayName: `Game ${index}`, health: index < activeCount ? "ready" : "needs_setup",
    }));
    const app = load("src/App.vue", ["loadGames", "games", "libraryPage", "libraryPageCount", "pagedGames"], { listGames: async () => fixtures(5) });
    app.games.value = fixtures(18);
    app.libraryPage.value = 2;
    await app.loadGames(true);
    assert.equal(app.libraryPage.value, 1);
    assert.equal(app.libraryPageCount.value, 1);
    assert.equal(app.pagedGames.value.length, 5);
    app.dispose();
  });
}

const watchdog = setTimeout(() => { console.error("State regression timed out"); process.exit(1); }, 15000);
(async () => {
  await taskFeedChecks();
  await overviewChecks();
  await cloudOperationChecks();
  await navigationChecks();
  await searchAndPaginationChecks();
  console.log(JSON.stringify({ passed: passed.length, tests: passed }));
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => clearTimeout(watchdog));
