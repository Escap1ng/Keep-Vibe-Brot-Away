// KVBA Codex notify 适配器:把 OpenAI Codex CLI 的 notify 事件归一化后上报本地 KVBA。
// 接入:~/.codex/config.toml 里加一行顶层键 notify = ["node", "<本文件绝对路径>"],见同目录 config.sample.toml。
// 无需编译,装好 Node 即可用。
//
// Codex 的 hook 面与 Trae/Claude Code 有三点结构性差异,决定了本脚本的形状:
// 1. **payload 走 argv 而不是 stdin** —— Codex 把 JSON 作为最后一个参数追加到命令上(所以读 process.argv[2])。
//    为保留"不经宿主包装层自证"的排查路径,argv 为空时回退读 stdin。
// 2. **只有 agent-turn-complete 一个事件**,没有工具级 hook,也没有"开始"事件。
//    所以 Codex 这一路**永远只能喂出 turn.ended → 知识分支**,审查分支需要写事件,拿不到。
//    这是 Codex 的能力边界,不是本脚本的缺陷,不要试图用文本猜测"刚才改了哪些文件"。
// 3. **不经 shell 直接 exec**:没有管道/&&/变量展开,也**不需要**给带空格的路径加引号
//    (Trae 那条坑在这里不存在)。
//
// 铁律:任何情况都以 exit 0 结束,绝不阻断 Codex(notify 本就是 fire-and-forget,stdout/stderr 被丢弃)。
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
    h = (BigInt.asUintN(64, h) ^ BigInt(s.charCodeAt(i))) & 0xffffffffffffffffn;
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

/** Codex notify JSON → KVBA NormalizedEvent(不识别的事件返回 null) */
function normalize(payload) {
  // 目前 Codex 只发这一种 notify;将来若新增类型,在这里加分支而不是改核心
  if (!payload || payload.type !== "agent-turn-complete") return null;

  const projectPath = payload.cwd || process.cwd() || "";
  if (!projectPath) return null;

  const summary = String(payload["last-assistant-message"] || "").trim();
  const asks = Array.isArray(payload["input-messages"]) ? payload["input-messages"] : [];
  const context = summary || String(asks[asks.length - 1] || "").trim();
  const flags = scanFlags(context);
  const ts = Date.now();
  const threadId = String(payload["thread-id"] || payload.thread_id || "");
  const turnId = String(payload["turn-id"] || payload.turn_id || "");

  return {
    schema: SCHEMA,
    // 同一时间戳同时用于 id 哈希与事件本身,保证二者一致
    id: fnv16(`te|${threadId}|${turnId}|${ts}|${context.slice(0, 40)}`),
    emitter: "codex-notify",
    eventType: "turn.ended",
    toolName: "",
    ts,
    projectPath,
    // 破冰按 sessionId 记账,所以这里必须给真值;thread-id 就是 Codex 的会话身份
    sessionId: threadId,
    excerpt: {
      // 诚实给空:Codex 没有工具级 hook,拿不到真实改动清单。
      // 从收尾文本里正则猜路径会污染两处判据 —— G6 的主题级指纹,以及
      // 「引用文件被再次修改 → 卡片过时退场」。假路径比空列表坏得多。
      files: [],
      diffHead: context.slice(0, MAX_DIFF_CHARS),
      intent: (summary ? `Codex 收尾:${summary.slice(0, 200)}` : "Codex 回合结束").slice(
        0,
        MAX_INTENT_CHARS,
      ),
      hasErrorPath: flags.hasErrorPath,
      hasApiCall: flags.hasApiCall,
      todoOrFixme: flags.todoOrFixme,
      // 拿不到真实行数,诚实给 0(伪造 lineDelta 会污染 G5-A 的审查打分)
      lineDelta: 0,
    },
  };
}

function readPayload() {
  // 首选 argv(Codex 的真实传法),argv 缺失时回退 stdin 以便手工管道自证
  const arg = process.argv[2];
  if (typeof arg === "string" && arg.trim().length > 0) return arg;
  try {
    return readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

async function main() {
  const dbg = process.env.KVBA_HOOK_DEBUG ? console.error : () => {};
  const raw = readPayload();
  if (!raw) {
    dbg("[kvba-hook] codex skip/no-payload");
    process.exit(0);
  }
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    dbg("[kvba-hook] codex skip/bad-json"); // 非 JSON 输入,忽略
    process.exit(0);
  }
  const ev = normalize(payload);
  if (!ev) {
    // 与"KVBA 没在跑"分开陈述:两者的修法完全相反,压成一个词等于没输出
    dbg(`[kvba-hook] codex skip/unrecognized(type=${payload.type ?? "?"})`);
    process.exit(0);
  }
  dbg(`[kvba-hook] codex normalized ${JSON.stringify(ev)}`);
  const ep = readEndpoint();
  if (!ep) {
    dbg("[kvba-hook] codex skip/no-endpoint(KVBA 未运行或 endpoint.json 不对)");
    process.exit(0); // KVBA 未运行 → 静默待命
  }
  const ok = await postEvent(ep, ev);
  dbg(`[kvba-hook] codex ${payload.type} → ${ok ? "ok" : "fail/post"}`);
  process.exit(0); // 永远 0:绝不阻断 Codex
}

// 铁律兜底:任何未预期异常都以 exit 0 结束(未处理的 Promise 拒绝默认以 1 退出)
main().catch(() => process.exit(0));
