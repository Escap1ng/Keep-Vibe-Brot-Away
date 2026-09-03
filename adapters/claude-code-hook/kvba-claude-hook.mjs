// KVBA Claude Code Hook 适配器(零编译版):stdin 收 hook JSON → 归一化 → 上报本地 KVBA。
// 由 ~/.claude/settings.json 的 hooks 在事件触发时以 bash 命令执行;配置见同目录 hooks.sample.json。
// 与 adapters/claude-code 里的 Rust bridge 功能等价,区别是**不需要 cargo build**:
// 装好 Node 就能用,代价是多一个 node 运行时依赖。两者同开会双报,
// 服务端按 emitter 无关的内容指纹在 5s 窗口内判重,但**只选一种接**。
//
// 铁律:任何情况都以 exit 0 结束。exit 2 在 Claude Code 里是**阻断语义**(stderr 会回喂给模型),
// 任何非 0 退出都可能把 hook 的失败变成用户看得见的报错 —— 观察者绝不允许干预主流程。
import { readFileSync } from "node:fs";

const SCHEMA = "kvba.event.v1";
// 与服务端 events::sanitize 的截断口径保持一致(超出的服务端会再截一次)
const MAX_DIFF_CHARS = 6000;
const MAX_INTENT_CHARS = 500;

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
  for (const k of ["file_path", "filePath", "path", "notebook_path"]) {
    const v = input?.[k];
    if (typeof v === "string" && v) out.push(v);
  }
  for (const k of ["file_paths", "filePaths"]) {
    const v = input?.[k];
    if (Array.isArray(v)) for (const p of v) if (typeof p === "string") out.push(p);
  }
  // MultiEdit 的文件藏在 edits 数组里,逐个 edit 也要看
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

// Claude Code 的工具名是首字母大写的,而门控的 is_write_event 只认规范小写名
// (`edit` / `write` / `patch`)。**不归一化就等于这一路永远产不出审查卡。**
// 这正是旧 Rust bridge 的缺陷:它把 `Edit` 原样上报,零写事件命中。
const TOOL_MAP = {
  Write: "write",
  Edit: "edit",
  MultiEdit: "edit",
  NotebookEdit: "edit",
  Bash: "bash",
  Read: "read",
  Glob: "glob",
  Grep: "grep",
  Task: "task",
  WebFetch: "webfetch",
  WebSearch: "websearch",
  SlashCommand: "slash",
};
function mapTool(name) {
  const n = String(name || "");
  if (TOOL_MAP[n]) return TOOL_MAP[n];
  // MCP 工具形如 mcp__github__create_issue:整串小写保留,主题性与文件类判据都不依赖它
  return n.toLowerCase();
}

/** Claude Code hook JSON → KVBA NormalizedEvent(不识别的事件返回 null) */
function normalize(hook) {
  const event = hook.hook_event_name;
  // Stop / SessionEnd / UserPromptSubmit 不带 cwd 字段(文档 schema 里没有),
  // 回退到 CLAUDE_PROJECT_DIR(hook 进程可见)或本进程 cwd。
  const projectPath = hook.cwd || process.env.CLAUDE_PROJECT_DIR || process.cwd() || "";
  if (!projectPath) return null;

  const base = {
    schema: SCHEMA,
    emitter: "claude-code-hook",
    ts: Date.now(),
    projectPath,
    sessionId: hook.session_id || "",
  };

  switch (event) {
    // 用户刚提交提示 = 一整轮的开始,也是"对话一开始就能递话"的那个自然断点。
    // 它让破冰卡不必等到 PostToolUse 之后才出现。
    case "UserPromptSubmit": {
      const prompt = str(hook.prompt);
      const flags = scanFlags(prompt);
      return {
        ...base,
        id: fnv16(`up|${base.ts}|${prompt.slice(0, 40)}`),
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

    case "PostToolUse": {
      const rawTool = str(hook.tool_name);
      // Task 是内联子任务,与主 Agent 语义重复,且会把子会话的事件成倍放大 → 丢弃
      if (mapTool(rawTool) === "task") return null;
      const tool = mapTool(rawTool);
      const input = hook.tool_input || {};
      const response = hook.tool_response || {};
      const files = extractFiles(input);
      const command = longestOf(input, ["command", "cmd"]);
      // 内容/差异字段:Claude Code 用 snake_case(old_string/new_string/content),
      // Trae 用 camelCase(newString)。两边都列上,别拿一侧的序列化名去猜另一侧。
      let diff = longestOf(input, [
        "diff",
        "patch",
        "content",
        "new_string",
        "newString",
        "replacement",
      ]);
      if (!diff && Array.isArray(input.edits)) {
        diff = input.edits
          .map((e) => longestOf(e, ["new_string", "newString", "replacement"]))
          .join("\n");
      }
      if (!diff) diff = longestOf(response, ["diff", "patch"]);
      const intent = command
        ? `$ ${command}`
        : files[0]
          ? `编辑 ${files[0]}`
          : `执行 ${rawTool}`;
      const respText =
        typeof hook.tool_response === "string"
          ? hook.tool_response
          : JSON.stringify(hook.tool_response ?? "");
      const flags = scanFlags(`${intent} ${diff}`);
      const isShell = tool === "bash";
      return {
        ...base,
        id: fnv16(`tc|${tool}|${base.ts}|${intent.slice(0, 40)}`),
        eventType: "tool.completed",
        toolName: tool,
        excerpt: {
          files,
          diffHead: (diff || respText).slice(0, MAX_DIFF_CHARS),
          intent: intent.slice(0, MAX_INTENT_CHARS),
          hasErrorPath: flags.hasErrorPath || /error|failed|exception|panic/i.test(respText),
          hasApiCall: flags.hasApiCall,
          todoOrFixme: flags.todoOrFixme,
          // bash 的"行数"没有意义(输出行数会被当成代码变更量),诚实给 0
          lineDelta: isShell || !diff ? 0 : diff.split("\n").length,
        },
      };
    }

    case "Stop":
    case "SubagentStop": {
      const active = hook.stop_hook_active === true;
      return {
        ...base,
        id: fnv16(`te|${base.ts}|${event}|${active ? 1 : 0}`),
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

    // 会话结束 → 服务端会做一次增量记忆合并
    case "SessionEnd": {
      const reason = str(hook.reason) || "other";
      return {
        ...base,
        id: fnv16(`se|${base.sessionId}|${reason}`),
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

    // Notification 有意不接:它覆盖"Claude 需要审批"与"输入框闲置 60s"两种场景,
    // 前者用户正被阻塞(此时递卡是干扰),后者没有可递的内容(空转烧额度)。
    default:
      return null;
  }
}

async function main() {
  const dbg = process.env.KVBA_HOOK_DEBUG ? console.error : () => {};
  let raw = "";
  try {
    raw = readFileSync(0, "utf8"); // Claude Code 把 hook JSON 写在 stdin
  } catch {
    dbg("[kvba-hook] claude skip/no-stdin");
    process.exit(0);
  }
  let hook;
  try {
    hook = JSON.parse(raw);
  } catch {
    dbg("[kvba-hook] claude skip/bad-json"); // 非 JSON 输入,忽略
    process.exit(0);
  }
  const ev = normalize(hook);
  if (!ev) {
    // 与"KVBA 没在跑"分开陈述:两者的修法完全相反,压成一个词等于没输出
    dbg(`[kvba-hook] claude skip/unrecognized(event=${hook.hook_event_name ?? "?"})`);
    process.exit(0);
  }
  dbg(`[kvba-hook] claude normalized ${JSON.stringify(ev)}`);
  const ep = readEndpoint();
  if (!ep) {
    dbg("[kvba-hook] claude skip/no-endpoint(KVBA 未运行或 endpoint.json 不对)");
    process.exit(0); // KVBA 未运行 → 静默待命
  }
  const ok = await postEvent(ep, ev);
  dbg(`[kvba-hook] claude ${hook.hook_event_name} → ${ok ? "ok" : "fail/post"}`);
  process.exit(0); // 永远 0:exit 2 在 Claude Code 里是阻断语义,绝不能从这里发生
}

// 铁律兜底:任何未预期异常都以 exit 0 结束(未处理的 Promise 拒绝默认以 1 退出)
main().catch(() => process.exit(0));
