import { spawn } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const options = process.argv.slice(2);
const dryRun = options.length === 1 && options[0] === "--dry-run";

if (options.length > 0 && !dryRun) {
  console.error("用法：npm run build:installer:test [-- --dry-run]");
  process.exit(2);
}

const buildEnvironment = {
  CARGO_TARGET_DIR: join(root, "src-tauri", "target", "test-installer"),
  CARGO_PROFILE_RELEASE_LTO: "off",
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS: "16",
  CARGO_PROFILE_RELEASE_OPT_LEVEL: "2",
  CARGO_PROFILE_RELEASE_INCREMENTAL: "true",
  CARGO_INCREMENTAL: "1",
};
const command = [join(root, "node_modules", "@tauri-apps", "cli", "tauri.js"), "build", "--bundles", "nsis"];
const installerDirectory = join(buildEnvironment.CARGO_TARGET_DIR, "release", "bundle", "nsis");

if (dryRun) {
  console.log(JSON.stringify({ executable: process.execPath, args: command, cwd: root, environment: buildEnvironment, installerDirectory }, null, 2));
} else {
  console.log("正在构建快速测试安装包：关闭 LTO、启用增量编译，仅生成 EXE。");
  console.log(`输出目录：${installerDirectory}`);
  const child = spawn(process.execPath, command, {
    cwd: root,
    env: { ...process.env, ...buildEnvironment },
    stdio: "inherit",
    shell: false,
  });
  child.on("error", (error) => {
    console.error(`启动测试打包失败：${error.message}`);
    process.exitCode = 1;
  });
  child.on("exit", (code) => {
    process.exitCode = code ?? 1;
  });
}
