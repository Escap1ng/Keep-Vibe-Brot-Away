// KVBA Qoder Hook 适配器:stdin 收 Qoder hook JSON → 归一化 → 上报本地 KVBA。
// 由 Qoder 设置的 hooks 执行(配置见同目录 hooks.sample.json),装好 Node 即可用。
//
// 铁律:任何情况都以 exit 0 结束。Qoder 里 exit 2 是阻断语义,其它非零退出会把
// hook 的失败变成用户可见的报错 —— 观察者绝不允许干预主流程。
//
// 能力边界:
// - PostToolUse 的 tool_response 在 IDE 里目前是字符串,不能直接给退出码。
//   我们从 payload 的 transcript_path 读取会话 JSONL 里的 toolUseResult.exitCode,
//   仅用来判定 hasErrorPath;kvba.event.v1 还没有执行结果字段,测试状态仍是 Unknown。
// - PostToolUseFailure 表示工具失败,同样按失败证据上报。
import {
  closeSync,
  openSync,
  readFileSync,
  readSync,
  statSync,
} from "node:fs";

const SCHEMA = "kvba.event.v1";
// 与服务端 events::sanitize 的截断口径保持一致(超出的服务端会再截一次)
const MAX_DIFF_CHARS = 6000;
const MAX_INTENT_CHARS = 500;
// 只扫转录尾部:一次工具调用的结果总在文件末尾附近,避免整份会话读入内存
const TRANSCRIPT_TAIL_BYTES = 512 * 1024;

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

function dbg(msg) {
  if (process.env.KVBA_HOOK_DEBUG === "1") console.error(`[kvba-qoder] ${msg}`);
}

function scanFlags(text) {
  const t = String(text).toLowerCase();
  return {
    hasApiCall: /http|curl|fetch\(|axios|npm |pip |go get|bun add/.test(t),
    hasErrorPath: /panic!|throw|unwrap\(\)/.test(t),
    todoOrFixme: /todo|fixme/.test(t),
  };
}

function str(v) {
  return typeof v === "string" ? v : "";
}

/** 取 src 里 keys 中**最长**的一段(内容/差异往往分散在多个字段,最长的那个最接近正文) */
function longestOf(src, keys) {
  let best = "";
  if (!src || typeof src !== "object") return best;
  for (const k of keys) {
    const v = src[k];
    if (typeof v === "string" && v.length > best.length) best = v;
  }
  return best;
}

function extractFiles(input) {
  const out = [];
  for (const k of ["file_path", "filePath", "path", "target_file", "notebook_path"]) {
    const v = input?.[k];
    if (typeof v === "string" && v) out.push(v);
  }
  for (const k of ["file_paths", "filePaths"]) {
    const v = input?.[k];
    if (Array.isArray(v)) for (const p of v) if (typeof p === "string") out.push(p);
  }
  if (Array.isArray(input?.edits)) {
    for (const e of input.edits) {
      for (const k of ["file_path", "filePath"]) {
        const v = e?.[k];
        if (typeof v === "string" && v) out.push(v);
      }
    }
  }
  return [...new Set(out)];
}

// Qoder 的工具名与 Claude Code 兼容(Bash ≡ run_in_terminal),同时接受原生别名。
// 门控的 is_write_event 只认规范小写名,不归一化就等于这一路永远产不出审查卡。
const TOOL_MAP = {
  Write: "write",
  write_file: "write",
  Edit: "edit",
  MultiEdit: "edit",
  NotebookEdit: "edit",
  edit_file: "edit",
  apply_patch: "patch",
  Bash: "bash",
  run_in_terminal: "bash",
  Read: "read",
  read_file: "read",
  Grep: "grep",
  grep_search: "grep",
  Glob: "glob",
  list_dir: "glob",
  Task: "task",
  WebFetch: "webfetch",
  WebSearch: "websearch",
};

function mapTool(name) {
  const n = String(name || "");
  if (TOOL_MAP[n]) return TOOL_MAP[n];
  return n.toLowerCase();
}

/**
 * 从会话转录尾部读取本次工具调用的结构化结果。
 * 匹配链:user.toolUseResult ← tool_result.tool_use_id ← assistant.tool_use.name。
 * 找不到或名称对不上时返回 null,绝不猜。
 */
function readExecution(transcriptPath, rawToolName) {
  if (!transcriptPath) return null;
  let fd;
  try {
    const size = statSync(transcriptPath).size;
    if (size <= 0) return null;
    const start = Math.max(0, size - TRANSCRIPT_TAIL_BYTES);
    const buf = Buffer.alloc(size - start);
    fd = openSync(transcriptPath, "r");
    readSync(fd, buf, 0, buf.length, start);
    const lines = buf.toString("utf8").split("\n");
    if (start > 0 && lines.length) lines.shift(); // 首行可能被截断

    const records = [];
    for (const line of lines) {
      if (!line.trim()) continue;
      try {
        records.push(JSON.parse(line));
      } catch {
        /* 半行/损坏行跳过 */
      }
    }

    const useNames = new Map();
    for (const r of records) {
      if (r.type !== "assistant" || !Array.isArray(r.message?.content)) continue;
      for (const b of r.message.content) {
        if (b?.type === "tool_use") useNames.set(b.id, b.name);
      }
    }

    const wanted = String(rawToolName || "").toLowerCase();
    for (let i = records.length - 1; i >= 0; i--) {
      const r = records[i];
      const result = r.toolUseResult;
      if (!result || typeof result !== "object") continue;
      const content = r.message?.content;
      if (!Array.isArray(content)) continue;
      for (const block of content) {
        if (block?.type !== "tool_result") continue;
        const name = useNames.get(block.tool_use_id);
        if (!name || String(name).toLowerCase() !== wanted) continue;
        return {
          exitCode: typeof result.exitCode === "number" ? result.exitCode : null,
          interrupted: result.interrupted === true,
          signal: typeof result.signal === "string" ? result.signal : null,
        };
      }
    }
  } catch {
    return null;
  } finally {
    if (fd !== undefined) {
      try {
        closeSync(fd);
      } catch {
        /* 忽略 */
      }
    }
  }
  return null;
}

function toolIntent(input, files, rawTool) {
  const command = longestOf(input, ["command", "cmd"]);
  if (command) return `$ ${command}`;
  if (files[0]) return `编辑 ${files[0]}`;
  return `执行 ${rawTool}`;
}

function toolDiff(input, extra, response) {
  const diff =
    str(extra.full_diff_text) ||
    longestOf(input, ["diff", "patch", "new_string", "newString", "replacement"]) ||
    (Array.isArray(input.edits)
      ? input.edits
          .map((e) => longestOf(e, ["new_string", "newString", "replacement"]))
          .join("\n")
      : "") ||
    longestOf(input, ["content"]) ||
    longestOf(response, ["diff", "patch"]);
  return diff;
}

function responseText(response) {
  if (typeof response === "string") return response;
  if (response == null) return "";
  try {
    return JSON.stringify(response);
  } catch {
    return "";
  }
}

/** Qoder hook JSON → KVBA NormalizedEvent(不识别的事件返回 null) */
function normalize(hook) {
  const event = str(hook.hook_event_name);
  const projectPath =
    str(hook.cwd) || process.env.QODER_PROJECT_DIR || process.cwd() || "";
  if (!projectPath) return null;

  const base = {
    schema: SCHEMA,
    emitter: "qoder-hook",
    ts: Date.now(),
    projectPath,
    sessionId: str(hook.session_id),
  };

  switch (event) {
    case "UserPromptSubmit": {
      const prompt = str(hook.prompt);
      const flags = scanFlags(prompt);
      return {
        ...base,
        id: fnv16(`qoder-up|${base.ts}|${prompt.slice(0, 40)}`),
        eventType: "message.updated",
        toolName: "prompt",
        excerpt: {
          files: [],
          diffHead: prompt.slice(0, MAX_DIFF_CHARS),
          intent: `用户提示:${prompt.slice(0, 200)}`,
          hasErrorPath: flags.hasErrorPath,
          hasApiCall: flags.hasApiCall,
          todoOrFixme: flags.todoOrFixme,
          lineDelta: 0,
        },
      };
    }

    case "PostToolUse":
    case "PostToolUseFailure": {
      const rawTool = str(hook.tool_name);
      // Task 是内联子任务,与主 Agent 语义重复,且会把子会话事件成倍放大 → 丢弃
      if (mapTool(rawTool) === "task") return null;
      const tool = mapTool(rawTool);
      const input = hook.tool_input || {};
      const extra = hook.extra || {};
      const failed = event === "PostToolUseFailure";
      const files = extractFiles(input);
      const intent = toolIntent(input, files, rawTool);
      const diff = failed ? "" : toolDiff(input, extra, hook.tool_response);
      const respText = failed
        ? responseText(hook.error)
        : responseText(hook.tool_response);
      const flags = scanFlags(`${intent} ${diff}`);
      const isShell = tool === "bash";

      let hasError = flags.hasErrorPath;
      if (failed) {
        hasError = true;
      } else {
        // 有结构化退出码就以它为准;拿不到才退回文本关键词(只是风险信号,不是退出状态)
        const exec = readExecution(hook.transcript_path, rawTool);
        if (exec && typeof exec.exitCode === "number") {
          hasError = exec.exitCode !== 0 || exec.interrupted === true;
        } else {
          hasError = hasError || /error|failed|exception|panic/i.test(respText);
        }
      }

      return {
        ...base,
        id: fnv16(
          `${failed ? "qoder-tf" : "qoder-tc"}|${tool}|${base.ts}|${(
            hook.tool_use_id || intent
          ).slice(0, 40)}`,
        ),
        eventType: "tool.completed",
        toolName: tool,
        excerpt: {
          files,
          diffHead: (failed ? respText : diff || respText).slice(0, MAX_DIFF_CHARS),
          intent: intent.slice(0, MAX_INTENT_CHARS),
          hasErrorPath: hasError,
          hasApiCall: flags.hasApiCall,
          todoOrFixme: flags.todoOrFixme,
          // bash 的"行数"是输出行数,不是代码变更量 —— 伪造它会污染 G5-A 打分
          lineDelta: isShell || !diff ? 0 : diff.split("\n").length,
        },
      };
    }

    case "Stop":
    case "SubagentStop": {
      const active = hook.stop_hook_active === true;
      return {
        ...base,
        id: fnv16(`qoder-te|${base.ts}|${event}|${active ? 1 : 0}`),
        eventType: "turn.ended",
        toolName: "",
        excerpt: {
          files: [],
          diffHead: "",
          intent:
            event === "SubagentStop"
              ? "子代理回合结束"
              : active
                ? "回合结束(停止钩子应答)"
                : "回合结束",
          hasErrorPath: false,
          hasApiCall: false,
          todoOrFixme: false,
          lineDelta: 0,
        },
      };
    }

    case "SessionEnd": {
      const reason = str(hook.reason) || "other";
      return {
        ...base,
        id: fnv16(`qoder-se|${base.ts}|${reason}`),
        eventType: "session.ended",
        toolName: "",
        excerpt: {
          files: [],
          diffHead: "",
          intent: `会话结束(${reason})`,
          hasErrorPath: false,
          hasApiCall: false,
          todoOrFixme: false,
          lineDelta: 0,
        },
      };
    }

    default:
      return null;
  }
}

async function main() {
  let raw = "";
  try {
    raw = readFileSync(0, "utf8");
  } catch {
    /* stdin 不可读 */
  }
  if (!raw.trim()) {
    dbg("skip/no-stdin");
    return;
  }

  let hook;
  try {
    hook = JSON.parse(raw);
  } catch {
    dbg("skip/bad-json");
    return;
  }

  const ev = normalize(hook);
  if (!ev) {
    dbg(`skip/unrecognized(event=${hook.hook_event_name ?? "?"})`);
    return;
  }
  dbg(`normalized ${JSON.stringify(ev)}`);

  // 手工验证用:只打印归一化结果,不依赖 KVBA 是否在运行
  if (process.env.KVBA_HOOK_DRY_RUN === "1") {
    process.stdout.write(`${JSON.stringify(ev)}\n`);
    return;
  }

  const ep = readEndpoint();
  if (!ep) {
    dbg("skip/no-endpoint(KVBA 未运行或 endpoint.json 不对)");
    return;
  }
  const ok = await postEvent(ep, ev);
  dbg(`${hook.hook_event_name} → ${ok ? "ok" : "fail/post"}`);
}

main().catch(() => process.exit(0));
