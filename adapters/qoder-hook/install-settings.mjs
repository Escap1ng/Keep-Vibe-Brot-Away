// 把 KVBA Qoder 钩子合并进 Qoder 设置文件。
// 用法:
//   node adapters/qoder-hook/install-settings.mjs              # 自动选当前构建的设置文件
//   node adapters/qoder-hook/install-settings.mjs --dry-run    # 只报告将改什么
//   node adapters/qoder-hook/install-settings.mjs --settings <path>
//
// 行为:先备份 → 只在 hooks 下追加缺失的事件 → 校验 JSON → 不打印文件内容。
// 幂等:命令已存在的事件不会重复追加。
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const hookScript = path.join(here, "kvba-qoder-hook.mjs");

function argValue(flag) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function defaultSettings() {
  const home = os.homedir();
  const candidates = [
    path.join(home, ".qoder-cn", "settings.json"),
    path.join(home, ".qoder", "settings.json"),
  ];
  return candidates.find((p) => fs.existsSync(p)) ?? candidates[0];
}

const dryRun = process.argv.includes("--dry-run");
const settingsPath = argValue("--settings") ?? defaultSettings();

if (!fs.existsSync(hookScript)) {
  console.error(`找不到钩子脚本: ${hookScript}`);
  process.exit(1);
}
if (!fs.existsSync(settingsPath)) {
  console.error(`找不到设置文件: ${settingsPath}`);
  console.error("用 --settings <path> 指定当前 Qoder 构建实际加载的设置文件。");
  process.exit(1);
}

let settings;
try {
  settings = JSON.parse(fs.readFileSync(settingsPath, "utf8"));
} catch (error) {
  console.error(`设置文件不是合法 JSON,未做任何修改: ${error.message}`);
  process.exit(1);
}

const command = `node "${hookScript}"`;
const handler = { type: "command", command, timeout: 5, async: true };
const events = {
  UserPromptSubmit: {},
  PostToolUse: { matcher: "*" },
  PostToolUseFailure: { matcher: "*" },
  Stop: {},
  SubagentStop: {},
  SessionEnd: {},
};

if (settings.hooks != null && typeof settings.hooks !== "object") {
  console.error("设置里的 hooks 不是对象,未做任何修改。");
  process.exit(1);
}
settings.hooks = settings.hooks ?? {};

const added = [];
const skipped = [];
for (const [event, group] of Object.entries(events)) {
  const list = Array.isArray(settings.hooks[event]) ? settings.hooks[event] : [];
  const exists = list.some(
    (g) => Array.isArray(g?.hooks) && g.hooks.some((h) => h?.command === command),
  );
  if (exists) {
    skipped.push(event);
    continue;
  }
  list.push({ ...group, hooks: [{ ...handler }] });
  settings.hooks[event] = list;
  added.push(event);
}

console.log(`设置文件: ${settingsPath}`);
console.log(`钩子脚本: ${hookScript}`);
console.log(`新增: ${added.length ? added.join(", ") : "无"}`);
console.log(`已存在: ${skipped.length ? skipped.join(", ") : "无"}`);

if (dryRun) {
  console.log("dry-run: 未写入。去掉 --dry-run 执行实际修改。");
  process.exit(0);
}
if (!added.length) {
  console.log("无需修改。");
  process.exit(0);
}

const backup = `${settingsPath}.kvba-backup-${new Date()
  .toISOString()
  .replace(/[:.]/g, "-")}`;
fs.copyFileSync(settingsPath, backup);
fs.writeFileSync(settingsPath, `${JSON.stringify(settings, null, 2)}\n`, "utf8");
JSON.parse(fs.readFileSync(settingsPath, "utf8"));
console.log(`已写入。备份: ${backup}`);
console.log("完全退出并重启 Qoder 后生效（不支持热加载）。");
