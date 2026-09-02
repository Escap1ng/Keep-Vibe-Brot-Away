// KVBA opencode 插件:订阅 tool.execute.before/after + session.idle
// 归一化事件上报到本地 KVBA(127.0.0.1 随机端口 + 随机 token 鉴权)。
// 安装:复制到 ~/.config/opencode/plugins/ 或 .opencode/plugins/,重启会话生效
import { readFileSync } from "node:fs";
import type { Plugin } from "@opencode-ai/plugin";

interface Endpoint {
  port: number;
  helloToken: string;
  eventToken: string;
}

const SCHEMA = "kvba.event.v1";

/** 读取 KVBA 端点信息(KVBA 数据目录) */
function readEndpoint(): Endpoint | null {
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

async function postEvent(
  ep: Endpoint,
  payload: Record<string, unknown>,
): Promise<boolean> {
  const base = `http://127.0.0.1:${ep.port}`;
  try {
    // hello 换取上报 token
    const hello = await fetch(`${base}/kvba/hello`, {
      method: "POST",
      headers: { "X-KVBA-Token": ep.helloToken },
    });
    let token = ep.eventToken;
    if (hello.ok) {
      const v = (await hello.json()) as { token?: string };
      if (v.token) token = v.token;
    }
    // 上报(失败重试 3 次,最后丢弃,绝不影响 Agent)
    let lastErr: unknown;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const r = await fetch(`${base}/kvba/events`, {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-KVBA-Token": token },
          body: JSON.stringify(payload),
        });
        if (r.ok) return true;
        lastErr = new Error(`HTTP ${r.status}`);
      } catch (e) {
        lastErr = e;
      }
      await new Promise((res) => setTimeout(res, 200 * (attempt + 1)));
    }
    console.warn("[kvba] 事件上报失败(已重试):", lastErr);
    return false;
  } catch (e) {
    console.warn("[kvba] 上报异常:", e);
    return false;
  }
}

function nowMs(): number {
  return Date.now();
}

function fnv16(s: string): string {
  // 轻量确定性散列(无依赖):Fowler–Noll–Vo 1a 64bit + hex
  // (仅用于事件 id 去重指纹;主程序不校验哈希算法)
  let h = 0xcbf29ce484222325n;
  for (let i = 0; i < s.length; i++) {
    h ^= BigInt(s.charCodeAt(i));
    h = (h * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return h.toString(16).padStart(16, "0");
}

function scanFlags(text: string) {
  const t = text.toLowerCase();
  return {
    hasApiCall: /http|curl|fetch\(|axios|npm |pip |go get|bun add/.test(t),
    hasErrorPath: /panic!|throw/.test(t),
    todoOrFixme: /todo|fixme/.test(t),
  };
}

function pickContent(
  src: Record<string, unknown> | undefined,
  keys: string[],
): string {
  if (!src) return "";
  for (const k of keys) {
    const v = src[k];
    if (typeof v === "string" && v.length > 0) return v;
  }
  return "";
}

function extractFiles(args: Record<string, unknown> | undefined): string[] {
  if (!args) return [];
  const out: string[] = [];
  for (const k of ["filePath", "file_path", "path"]) {
    const v = args[k];
    if (typeof v === "string") out.push(v);
  }
  for (const k of ["filePaths", "file_paths"]) {
    const v = args[k];
    if (Array.isArray(v)) for (const p of v) if (typeof p === "string") out.push(p);
  }
  return [...new Set(out)];
}

export const KvbaPlugin: Plugin = async ({ directory, client }) => {
  // before 阶段捕获 args(edit/write 的内容在 after 阶段也能取到,这里主要用于命令/文件)
  const lastArgs = new Map<string, unknown>();

  // 端点惰性重读:插件加载时 KVBA 可能尚未启动;此后每 ≥30s 重试一次,避免整会话静默失效
  let endpoint: Endpoint | null = readEndpoint();
  let lastEndpointAttempt = Date.now();
  function ensureEndpoint(): Endpoint | null {
    if (endpoint) return endpoint;
    const now = Date.now();
    if (now - lastEndpointAttempt < 30_000) return null;
    lastEndpointAttempt = now;
    endpoint = readEndpoint();
    return endpoint;
  }

  await client.app.log({
    body: {
      service: "kvba-plugin",
      level: "info",
      message: endpoint
        ? `KVBA 插件已加载,上报端点 127.0.0.1:${endpoint.port}`
        : "KVBA 未运行(未找到 endpoint.json),插件静默待命",
    },
  });

  const sessionProject = directory || process.env.PWD || "";

  function emit(
    eventType: string,
    tool: string,
    files: string[],
    diffHead: string,
    intent: string,
    isError: boolean,
    isApi: boolean,
    todo: boolean,
    lineDelta: number,
  ) {
    if (!ensureEndpoint()) return;
    const excerpt = {
      files,
      diffHead: diffHead.slice(0, 6000),
      intent: intent.slice(0, 500),
      hasErrorPath: isError,
      hasApiCall: isApi,
      todoOrFixme: todo,
      lineDelta,
    };
    void postEvent(endpoint!, {
      schema: SCHEMA,
      id: fnv16(`${eventType}|${tool}|${nowMs()}|${intent.slice(0, 40)}`),
      emitter: "opencode-plugin",
      eventType,
      toolName: tool,
      ts: nowMs(),
      projectPath: sessionProject,
      sessionId: "",
      excerpt,
    });
  }

  return {
    "tool.execute.before": async (input, output) => {
      if (!ensureEndpoint()) return;
      lastArgs.set(input.tool, output?.args ?? null);
    },
    "tool.execute.after": async (input, output) => {
      if (!ensureEndpoint()) return;
      const tool = input.tool;
      const args = output?.args ?? lastArgs.get(tool);
      const files = extractFiles(args as Record<string, unknown>);
      const command = pickContent(args as Record<string, unknown>, ["command", "commandText"]);
      const content = pickContent(args as Record<string, unknown>, ["content", "option"]);
      const diff = pickContent(args as Record<string, unknown>, ["diff", "patch"]);
      const src = content || diff || command;

      const intent = command
        ? `$ ${command}`
        : files[0]
          ? `编辑 ${files[0]}`
          : tool === "think"
            ? content.slice(0, 300)
            : `执行 ${tool}`;

      const flags = scanFlags(intent + " " + src);
      // 空内容不计行数(避免 "".split("\n") 得出虚高 1)
      const lineDelta = diff || content ? (diff || content).split("\n").length : 0;
      emit("tool.completed", tool, files, diff || content.slice(0, 6000), intent, flags.hasErrorPath, flags.hasApiCall, flags.todoOrFixme, tool === "bash" ? 0 : lineDelta);

      // opencode 的 bash 输出错误(excerpt 摘录)
      if (tool === "bash") {
        const resText = JSON.stringify(output?.parts ?? output ?? "").slice(0, 6000);
        if (/error|failed|exception|panic/i.test(resText)) {
          emit("tool.completed", "bash", files, resText, `${intent} [输出含错误]`, true, flags.hasApiCall, false, 0);
        }
      }
    },
    event: async ({ event }) => {
      if (!ensureEndpoint()) return;
      if (event.type === "session.idle") {
        emit("turn.ended", "", [], "", "本轮处理结束", false, false, false, 0);
      }
    },
  };
};
