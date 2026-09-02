// KVBA TraeCode Hook 适配器:从 stdin 读 TraeCode 的 hook JSON → 归一化 → 上报本地 KVBA。
// 由 Trae 的 .trae/hooks.json 在 PostToolUse / Stop 事件触发(Windows 下经 PowerShell 执行 node)。
// 接入配置见同目录 hooks.sample.json;无需编译,装好 Node 即可用。
// 铁律:任何情况都以 exit 0 结束,绝不阻断 Trae 主流程(advisory 观察者)。
import { readFileSync } from "node:fs";

const SCHEMA = "kvba.event.v1";

/** 读取 KVBA 端点信息(Windows: %APPDATA%\dev.kvba.app\endpoint.json) */
function readEndpoint() {
  try {
    const file =
      process.env.KVBA_ENDPOINT ??
      (process.env.APPDATA
        ? `${process.env.APPDATA}\\dev.kvba.app\\endpoint.json`
        : `${process.env.HOME}/.local/share/dev.kvba.app/endpoint.json`);
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

/** hello 换 event token → 上报(重试 3 次,最后丢弃) */
async function postEvent(ep, payload) {
  const base = `http://127.0.0.1:${ep.port}`;
  try {
    let token = ep.eventToken;
    try {
      const hello = await fetch(`${base}/kvba/hello`, {
        method: "POST",
        headers: { "X-KVBA-Token": ep.helloToken },
      });
      if (hello.ok) {
        const v = await hello.json();
        if (v.token) token = v.token;
      }
    } catch {
      /* 用 endpoint.json 里的兜底 token */
    }
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const r = await fetch(`${base}/kvba/events`, {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-KVBA-Token": token },
          body: JSON.stringify(payload),
        });
        if (r.ok) return true;
      } catch {
        /* 重试 */
      }
      await new Promise((res) => setTimeout(res, 150 * (attempt + 1)));
    }
    return false;
  } catch {
    return false;
  }
}

// 轻量确定性散列(FNV-1a 64bit hex),仅用于事件 id 去重指纹
function fnv16(s) {
  let h = 0xcbf29ce484222325n;
  for (let i = 0; i < s.length; i++) {
    h ^= BigInt(s.charCodeAt(i));
    h = (h * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return h.toString(16).padStart(16, "0");
}

function scanFlags(text) {
  const t = String(text).toLowerCase();
  return {
    hasApiCall: /http|curl|fetch\(|axios|npm |pip |go get|bun add/.test(t),
    hasErrorPath: /panic!|throw/.test(t),
    todoOrFixme: /todo|fixme/.test(t),
  };
}

function pick(obj, keys) {
  for (const k of keys) {
    const v = obj?.[k];
    if (typeof v === "string" && v.length > 0) return v;
  }
  return "";
}

function extractFiles(input) {
  const out = [];
  for (const k of ["file_path", "filePath", "path", "notebook_path"]) {
    const v = input?.[k];
    if (typeof v === "string" && v) out.push(v);
  }
  for (const k of ["file_paths", "filePaths"]) {
    const v = input?.[k];
    if (Array.isArray(v)) for (const p of v) if (typeof p === "string") out.push(p);
  }
  return [...new Set(out)];
}

// Trae 工具名(首字母大写)→ KVBA 门控期望的规范小写名。
// 关键:is_write_event 只认 edit/write/patch;RunCommand 归 bash(不算写事件)。
const TOOL_MAP = {
  Write: "write",
  SearchReplace: "edit",
  Edit: "edit",
  MultiEdit: "edit",
  NotebookEdit: "edit",
  DeleteFile: "edit",
  RunCommand: "bash",
  Read: "read",
  Glob: "glob",
  Grep: "grep",
  Task: "task",
  WebSearch: "websearch",
  WebFetch: "webfetch",
};
function mapTool(name) {
  const n = String(name || "");
  return TOOL_MAP[n] ?? n.toLowerCase();
}

/** TraeCode hook JSON → KVBA NormalizedEvent(不识别的事件返回 null) */
function normalize(hook) {
  const projectPath =
    hook.cwd ||
    (Array.isArray(hook.workspace_roots) && hook.workspace_roots[0]) ||
    process.env.TRAE_PROJECT_DIR ||
    "";
  if (!projectPath) return null;

  const base = {
    schema: SCHEMA,
    emitter: "trae-hook",
    ts: Date.now(),
    projectPath,
    sessionId: hook.session_id || "",
  };

  if (hook.hook_event_name === "PostToolUse") {
    const rawTool = hook.tool_name || hook.llm_tool_name || "";
    const tool = mapTool(rawTool);
    const input = hook.tool_input || {};
    const files = extractFiles(input);
    const command = pick(input, ["command", "cmd"]);
    const content = pick(input, ["content", "new_str", "new_string", "replacement"]);
    const diff = command ? "" : content;
    const intent = command ? `$ ${command}` : files[0] ? `编辑 ${files[0]}` : `执行 ${rawTool}`;
    const respText =
      typeof hook.tool_response === "string"
        ? hook.tool_response
        : JSON.stringify(hook.tool_response ?? "");
    const flags = scanFlags(`${intent} ${diff}`);
    const isError = flags.hasErrorPath || /error|failed|exception|panic/i.test(respText);
    const lineDelta = diff ? diff.split("\n").length : 0;
    const isShell = tool === "bash";
    return {
      ...base,
      id: fnv16(`tc|${tool}|${base.ts}|${intent.slice(0, 40)}`),
      eventType: "tool.completed",
      toolName: tool,
      excerpt: {
        files,
        diffHead: (diff || respText).slice(0, 6000),
        intent: intent.slice(0, 500),
        hasErrorPath: isError,
        hasApiCall: flags.hasApiCall,
        todoOrFixme: flags.todoOrFixme,
        lineDelta: isShell ? 0 : lineDelta,
      },
    };
  }

  if (hook.hook_event_name === "Stop") {
    return {
      ...base,
      id: fnv16(`te|${base.ts}|${String(hook.last_assistant_message || "").slice(0, 40)}`),
      eventType: "turn.ended",
      toolName: "",
      excerpt: {
        files: [],
        diffHead: "",
        intent: "本轮处理结束",
        hasErrorPath: false,
        hasApiCall: false,
        todoOrFixme: false,
        lineDelta: 0,
      },
    };
  }

  return null;
}

async function main() {
  let raw = "";
  try {
    raw = readFileSync(0, "utf8"); // 读 stdin(TraeCode 传入的 hook JSON)
  } catch {
    process.exit(0);
  }
  const ep = readEndpoint();
  if (!ep) process.exit(0); // KVBA 未运行 → 静默待命
  let hook;
  try {
    hook = JSON.parse(raw);
  } catch {
    process.exit(0);
  }
  const ev = normalize(hook);
  const ok = ev ? await postEvent(ep, ev) : false;
  if (process.env.KVBA_HOOK_DEBUG) {
    console.error(`[kvba-hook] ${hook.hook_event_name} → ${ok ? "ok" : "fail/drop"}`);
  }
  process.exit(0); // 永远 0:绝不阻断 Trae
}

// 铁律兜底:任何未预期异常都以 exit 0 结束(未处理的 Promise 拒绝默认以 1 退出)
main().catch(() => process.exit(0));
