/**
 * TPI 协议客户端 SDK（web_desktop.md §二十四）。
 *
 * 职责：connect / disconnect / reconnect / sendCommand / subscribe /
 * protocol decode / request correlation / event sequencing。
 *
 * React 组件不直接 `new WebSocket(...)` + `JSON.parse(...)`--
 * 一切经 TpiClient。
 */

// ---- 协议类型（与 crates/tpi-protocol 的 serde wire 格式一一对应） ----

export const PROTOCOL_VERSION = 1;

export interface SessionView {
  id: string;
  title: string;
  workspace: string;
  status: "idle" | "running" | "awaiting_input";
  created_at_ms: number;
  updated_at_ms: number;
}

export interface ChatMessageDto {
  role: "user" | "assistant" | "system" | "tool";
  content: string;
}

export interface QuestionOptionDto {
  label: string;
  description?: string;
}

export interface QuestionDto {
  question: string;
  header?: string;
  options: QuestionOptionDto[];
  multiple: boolean;
  custom: boolean;
}

export type ClientCommand =
  | { type: "create_session"; title?: string }
  | { type: "list_sessions" }
  | { type: "resume_session"; session_id: string }
  | { type: "submit_message"; session_id: string; content: string }
  | { type: "cancel_run"; session_id: string }
  | { type: "retry_run"; session_id: string }
  | { type: "answer_input"; session_id: string; request_id: string; answer: string }
  | { type: "undo"; session_id: string; all?: boolean; force?: boolean }
  | { type: "redo"; session_id: string; all?: boolean; force?: boolean }
  | { type: "shutdown" };

export interface AppError {
  code: string;
  message: string;
  retryable: boolean;
  details?: Record<string, unknown>;
}

export type CommandAck =
  | { status: "accepted" }
  | { status: "rejected"; code: string; message: string; retryable: boolean; details?: Record<string, unknown> };

export type RuntimeEvent =
  | { type: "session_created"; session: SessionView }
  | { type: "session_list"; sessions: SessionView[] }
  | { type: "session_resumed"; session: SessionView }
  | { type: "session_history"; session_id: string; messages: ChatMessageDto[] }
  | { type: "session_status_changed"; session_id: string; status: SessionView["status"] }
  | { type: "run_started"; session_id: string; run_id: string }
  | {
      type: "run_completed";
      session_id: string;
      run_id: string;
      reason: string;
      assistant_text: string;
    }
  | { type: "run_failed"; session_id: string; run_id: string; error: AppError }
  | { type: "user_message_added"; session_id: string; content: string }
  | {
      type: "assistant_delta";
      session_id: string;
      run_id: string;
      request_id: string;
      kind: "text" | "reasoning";
      text: string;
    }
  | {
      type: "tool_started";
      session_id: string;
      run_id: string;
      call_id: string;
      name: string;
      arguments: string;
    }
  | {
      type: "tool_completed";
      session_id: string;
      run_id: string;
      call_id: string;
      name: string;
      status: "success" | "failed" | "cancelled" | "skipped";
      duration_ms: number;
      exit_code: number | null;
      output: string;
      diff: string | null;
    }
  | {
      type: "tool_output_delta";
      session_id: string;
      run_id: string;
      call_id: string;
      stream: number;
      text: string;
    }
  | {
      type: "input_requested";
      session_id: string;
      run_id: string;
      request_id: string;
      text: string;
      questions: QuestionDto[];
    }
  | { type: "input_answered"; session_id: string; request_id: string }
  | { type: "input_rejected"; session_id: string; request_id: string }
  | { type: "plan_updated"; session_id: string; plan: unknown }
  | { type: "context_usage"; session_id: string; projected: number; usable: number }
  | {
      type: "usage_updated";
      session_id: string;
      input_tokens: number;
      output_tokens: number;
      cache_read_tokens: number;
    }
  | { type: "budget_warning"; session_id: string }
  | { type: "stream_recovering"; session_id: string; attempt: number }
  | { type: "turn_restarting"; session_id: string; attempt: number }
  | { type: "compaction_notice"; session_id: string; message: string }
  | {
      type: "subagent_reported";
      child_session: string;
      summary: string;
      evidence: string[];
    }
  | { type: "mutation_recorded"; session_id: string; summary: string }
  | { type: "undo_completed"; session_id: string; summary: string }
  | { type: "redo_completed"; session_id: string; summary: string };

export interface EventEnvelope {
  protocol_version: number;
  seq: number;
  timestamp_ms: number;
  session_id?: string;
  run_id?: string;
  event: RuntimeEvent;
}

// ---- WebSocket 消息信封 ----

type WsClientMessage =
  | {
      type: "hello";
      protocol_version: number;
      client_name: string;
      client_version: string;
      token?: string;
    }
  | { type: "command"; payload: ClientCommand }
  | { type: "ping" };

type WsServerMessage =
  | { type: "server_hello"; protocol_version: number; server_version: string; last_seq: number }
  | { type: "ack"; ack: { request_id: string; status: string; code?: string; message?: string } }
  | ({ type: "event" } & EventEnvelope)
  | { type: "pong" }
  | { type: "error"; code: string; message: string };

// ---- 连接状态 ----

export type ConnectionState = "connecting" | "connected" | "disconnected" | "error";

export interface TpiClientOptions {
  url?: string;
  token?: string;
  clientName?: string;
  clientVersion?: string;
  /** 事件监听器。 */
  onEvent?: (envelope: EventEnvelope) => void;
  /** 连接状态变化。 */
  onStateChange?: (state: ConnectionState, detail?: string) => void;
  /** 自动重连（默认 true）。 */
  autoReconnect?: boolean;
  /** 重连退避基数 ms（默认 500，指数退避 x2，上限 15s）。 */
  reconnectBaseMs?: number;
}

/**
 * TPI 协议客户端。
 *
 * ```ts
 * const client = new TpiClient({ url: "ws://127.0.0.1:8765/ws" });
 * client.onEvent = (env) => console.log(env.seq, env.event.type);
 * await client.connect();
 * const ack = await client.createSession();
 * ```
 */
export class TpiClient {
  private ws: WebSocket | null = null;
  private pendingAcks = new Map<string, { resolve: (ack: CommandAck) => void; reject: (err: Error) => void }>();
  private nextLocalId = 1;
  private lastSeq = 0;
  private state: ConnectionState = "disconnected";
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedByUser = false;

  onEvent: (envelope: EventEnvelope) => void = () => {};
  onStateChange: (state: ConnectionState, detail?: string) => void = () => {};

  constructor(private options: TpiClientOptions = {}) {
    // 从 URL 查询参数读取 token（`?token=...`；Desktop 由 Tauri 注入）。
    if (!this.options.token && typeof window !== "undefined") {
      const params = new URLSearchParams(window.location.search);
      const t = params.get("token");
      if (t) this.options.token = t;
    }
  }

  get currentState(): ConnectionState {
    return this.state;
  }

  get lastEventSeq(): number {
    return this.lastSeq;
  }

  /** 连接（hello 握手完成即 connected）。 */
  async connect(): Promise<void> {
    this.closedByUser = false;
    await this.openSocket();
  }

  /** 用户主动断开（不自动重连）。 */
  disconnect(): void {
    this.closedByUser = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.close();
    this.ws = null;
    this.setState("disconnected");
  }

  // ---- 命令 API（返回 ack） ----

  async createSession(title?: string): Promise<CommandAck> {
    return this.sendCommand({ type: "create_session", title });
  }

  async listSessions(): Promise<CommandAck> {
    return this.sendCommand({ type: "list_sessions" });
  }

  async resumeSession(sessionId: string): Promise<CommandAck> {
    return this.sendCommand({ type: "resume_session", session_id: sessionId });
  }

  async submitMessage(sessionId: string, content: string): Promise<CommandAck> {
    return this.sendCommand({ type: "submit_message", session_id: sessionId, content });
  }

  async cancelRun(sessionId: string): Promise<CommandAck> {
    return this.sendCommand({ type: "cancel_run", session_id: sessionId });
  }

  async retryRun(sessionId: string): Promise<CommandAck> {
    return this.sendCommand({ type: "retry_run", session_id: sessionId });
  }

  async answerInput(sessionId: string, requestId: string, answer: string): Promise<CommandAck> {
    return this.sendCommand({ type: "answer_input", session_id: sessionId, request_id: requestId, answer });
  }

  async undo(sessionId: string, all = false, force = false): Promise<CommandAck> {
    return this.sendCommand({ type: "undo", session_id: sessionId, all, force });
  }

  async redo(sessionId: string, all = false, force = false): Promise<CommandAck> {
    return this.sendCommand({ type: "redo", session_id: sessionId, all, force });
  }

  async sendCommand(payload: ClientCommand): Promise<CommandAck> {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return {
        status: "rejected",
        code: "not_connected",
        message: "未连接到服务器",
        retryable: true,
      };
    }
    // request correlation：本地生成 request id，server ack 带回同 id。
    // （协议层 request_id 由 server 生成；客户端按 FIFO 关联 ack--
    //   单连接上命令串行处理，FIFO 足够且无需 server 端回显 id。）
    const requestId = `c${this.nextLocalId++}`;
    const promise = new Promise<CommandAck>((resolve, reject) => {
      this.pendingAcks.set(requestId, { resolve, reject });
    });
    ws.send(JSON.stringify({ type: "command", payload } satisfies WsClientMessage));
    // 30s 超时保护。
    setTimeout(() => {
      const pending = this.pendingAcks.get(requestId);
      if (pending) {
        this.pendingAcks.delete(requestId);
        pending.resolve({
          status: "rejected",
          code: "timeout",
          message: "命令超时未确认",
          retryable: true,
        });
      }
    }, 30_000);
    return promise;
  }

  // ---- 内部 ----

  private setState(state: ConnectionState, detail?: string) {
    if (this.state !== state) {
      this.state = state;
      this.onStateChange(state, detail);
      this.options.onStateChange?.(state, detail);
    }
  }

  private openSocket(): Promise<void> {
    return new Promise((resolve, reject) => {
      const url = this.options.url ?? defaultWsUrl();
      this.setState("connecting");
      const ws = new WebSocket(url);
      this.ws = ws;

      ws.onopen = () => {
        // 发送 hello。
        const hello: WsClientMessage = {
          type: "hello",
          protocol_version: PROTOCOL_VERSION,
          client_name: this.options.clientName ?? "tpi-web",
          client_version: this.options.clientVersion ?? "0.1.0",
          ...(this.options.token ? { token: this.options.token } : {}),
        };
        ws.send(JSON.stringify(hello));
      };

      ws.onmessage = (ev) => {
        let msg: WsServerMessage;
        try {
          msg = JSON.parse(ev.data as string);
        } catch {
          return;
        }
        if (msg.type === "server_hello") {
          if (msg.protocol_version !== PROTOCOL_VERSION) {
            this.setState("error", `协议版本不匹配: client=${PROTOCOL_VERSION}, server=${msg.protocol_version}`);
            ws.close();
            reject(new Error("protocol_version_mismatch"));
            return;
          }
          this.lastSeq = msg.last_seq;
          this.reconnectAttempt = 0;
          this.setState("connected");
          resolve();
          return;
        }
        this.handleMessage(msg);
      };

      ws.onerror = () => {
        if (this.state === "connecting") {
          reject(new Error("connection_failed"));
        }
      };

      ws.onclose = () => {
        this.failAllPending("connection_closed");
        this.setState("disconnected");
        if (!this.closedByUser && this.options.autoReconnect !== false) {
          this.scheduleReconnect();
        }
      };
    });
  }

  private handleMessage(msg: WsServerMessage) {
    switch (msg.type) {
      case "ack": {
        // FIFO 关联：串行命令场景下最早的 pending 即本 ack。
        const firstKey = this.pendingAcks.keys().next().value;
        if (firstKey !== undefined) {
          const pending = this.pendingAcks.get(firstKey)!;
          this.pendingAcks.delete(firstKey);
          if (msg.ack.status === "accepted") {
            pending.resolve({ status: "accepted" });
          } else {
            pending.resolve({
              status: "rejected",
              code: msg.ack.code ?? "unknown",
              message: msg.ack.message ?? "",
              retryable: false,
            });
          }
        }
        break;
      }
      case "event": {
        const { seq } = msg;
        if (seq > this.lastSeq) this.lastSeq = seq;
        const envelope: EventEnvelope = {
          protocol_version: msg.protocol_version,
          seq: msg.seq,
          timestamp_ms: msg.timestamp_ms,
          session_id: msg.session_id,
          run_id: msg.run_id,
          event: msg.event as RuntimeEvent,
        };
        this.onEvent(envelope);
        this.options.onEvent?.(envelope);
        break;
      }
      case "error": {
        this.setState("error", `${msg.code}: ${msg.message}`);
        break;
      }
      default:
        break;
    }
  }

  private failAllPending(reason: string) {
    for (const [, pending] of this.pendingAcks) {
      pending.resolve({
        status: "rejected",
        code: reason,
        message: "连接断开",
        retryable: true,
      });
    }
    this.pendingAcks.clear();
  }

  private scheduleReconnect() {
    const base = this.options.reconnectBaseMs ?? 500;
    const delay = Math.min(base * 2 ** this.reconnectAttempt, 15_000);
    this.reconnectAttempt += 1;
    this.reconnectTimer = setTimeout(() => {
      this.openSocket().catch(() => {
        // 失败由 onclose -> scheduleReconnect 继续退避。
      });
    }, delay);
  }
}

/** 默认 WS URL：当前页面 host + /ws。 */
export function defaultWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}
