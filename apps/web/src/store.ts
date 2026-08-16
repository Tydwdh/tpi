/**
 * 前端状态投影（web_desktop.md §十二：前端不是 source of truth）。
 *
 * 所有 Run/Tool/Session truth 来自 RuntimeEvent 流；本文件只维护
 * ViewState：会话列表、消息列表、工具卡片、挂起问题、连接状态。
 */

import type {
  ChatMessageDto,
  EventEnvelope,
  SessionView,
} from "../../../packages/tpi-client/src";

export interface ToolCardView {
  callId: string;
  name: string;
  arguments: string;
  status: "running" | "success" | "failed" | "cancelled" | "skipped";
  durationMs?: number;
  exitCode?: number | null;
  output: string;
  diff?: string | null;
  expanded: boolean;
}

export interface MessageView {
  id: string;
  kind: "user" | "assistant" | "system" | "tool" | "pending_question";
  role?: "user" | "assistant";
  content: string;
  reasoning?: string;
  streaming?: boolean;
  tool?: ToolCardView;
  plan?: unknown;
  error?: boolean;
}

export interface QuestionView {
  requestId: string;
  text: string;
  questions: {
    question: string;
    header?: string;
    options: { label: string; description?: string }[];
    multiple: boolean;
    custom: boolean;
  }[];
}

export interface SessionViewState {
  id: string;
  view: SessionView;
  messages: MessageView[];
  /** 待回答问题（request_input 挂起）。 */
  pendingQuestion: QuestionView | null;
  pendingAnswer: string;
}

export interface AppViewState {
  sessions: SessionView[];
  activeSessionId: string | null;
  sessionMap: Map<string, SessionViewState>;
  connection: "connecting" | "connected" | "disconnected" | "error";
  connectionDetail?: string;
  lastSeq: number;
  workspace: string;
  model: string;
  pendingInput: string;
}

export function createInitialState(): AppViewState {
  return {
    sessions: [],
    activeSessionId: null,
    sessionMap: new Map(),
    connection: "connecting",
    lastSeq: 0,
    workspace: "",
    model: "",
    pendingInput: "",
  };
}

function ensureSession(state: AppViewState, sessionId: string, view?: SessionView): SessionViewState {
  let s = state.sessionMap.get(sessionId);
  if (!s) {
    s = {
      id: sessionId,
      view: view ?? {
        id: sessionId,
        title: "",
        workspace: state.workspace,
        status: "idle",
        created_at_ms: 0,
        updated_at_ms: 0,
      },
      messages: [],
      pendingQuestion: null,
      pendingAnswer: "",
    };
    state.sessionMap.set(sessionId, s);
  }
  if (view) s.view = view;
  return s;
}

function pushMessage(
  state: AppViewState,
  sessionId: string,
  msg: MessageView,
): MessageView {
  const s = ensureSession(state, sessionId);
  s.messages.push(msg);
  return msg;
}

function findToolCard(
  state: AppViewState,
  sessionId: string,
  callId: string,
): ToolCardView | null {
  const s = state.sessionMap.get(sessionId);
  if (!s) return null;
  for (const m of s.messages) {
    if (m.kind === "tool" && m.tool?.callId === callId) return m.tool;
  }
  return null;
}

/** 从历史快照构造一条消息视图。 */
function historyMessage(m: ChatMessageDto, seq: number): MessageView {
  switch (m.role) {
    case "user":
      return {
        id: `hist-user-${seq}-${m.content.slice(0, 8)}`,
        kind: "user",
        role: "user",
        content: m.content,
      };
    case "assistant":
      return {
        id: `hist-asst-${seq}-${m.content.slice(0, 8)}`,
        kind: "assistant",
        role: "assistant",
        content: m.content,
      };
    case "system":
      return { id: `hist-sys-${seq}`, kind: "system", content: m.content };
    case "tool":
      return {
        id: `hist-tool-${seq}-${m.content.slice(0, 8)}`,
        kind: "tool",
        content: m.content,
        tool: {
          callId: `hist-${seq}`,
          name: "tool",
          arguments: "",
          status: "success",
          output: m.content,
          expanded: false,
        },
      };
  }
}

/** 处理一个事件，原地更新 state（返回是否影响当前活跃会话的渲染）。 */
export function applyEvent(state: AppViewState, envelope: EventEnvelope): void {
  const ev = envelope.event;
  state.lastSeq = envelope.seq;
  // session 归属：事件自带 session_id（run/tool 等）；
  // session_created/resumed 用 session.id。
  const sid =
    ev.type === "session_created" || ev.type === "session_resumed"
      ? ev.session.id
      : (ev as { session_id?: string }).session_id ?? envelope.session_id;

  switch (ev.type) {
    case "session_created":
    case "session_resumed": {
      const view = ev.session;
      if (state.activeSessionId === null) {
        state.activeSessionId = view.id;
      }
      ensureSession(state, view.id, view);
      if (ev.type === "session_created") {
        pushMessage(state, view.id, {
          id: `sys-${envelope.seq}`,
          kind: "system",
          content: `已创建会话${view.title ? `：${view.title}` : ""}`,
        });
      }
      break;
    }
    case "session_history": {
      // 断线重连 / 页面刷新后重建 transcript。
      const s = ensureSession(state, ev.session_id);
      s.messages = [];
      for (const m of ev.messages) {
        pushMessage(state, ev.session_id, historyMessage(m, envelope.seq));
      }
      break;
    }
    case "session_list": {
      state.sessions = ev.sessions;
      for (const s of ev.sessions) {
        ensureSession(state, s.id, s);
      }
      break;
    }
    case "session_status_changed": {
      const s = state.sessionMap.get(ev.session_id);
      if (s) s.view.status = ev.status;
      break;
    }
    case "run_started": {
      pushMessage(state, sid!, {
        id: `sys-run-${envelope.seq}`,
        kind: "system",
        content: "▶ run 开始",
      });
      break;
    }
    case "run_completed": {
      const s = state.sessionMap.get(ev.session_id);
      if (s) s.view.status = "idle";
      if (ev.reason === "awaiting_user_input") {
        // 挂起由 input_requested 事件单独处理。
        break;
      }
      pushMessage(state, ev.session_id, {
        id: `sys-end-${envelope.seq}`,
        kind: "system",
        content: `✓ run 结束（${ev.reason}）`,
      });
      break;
    }
    case "run_failed": {
      const s = state.sessionMap.get(ev.session_id);
      if (s) s.view.status = "idle";
      pushMessage(state, ev.session_id, {
        id: `err-${envelope.seq}`,
        kind: "system",
        content: `✗ run 失败：${ev.error.message}`,
        error: true,
      });
      break;
    }
    case "user_message_added": {
      pushMessage(state, ev.session_id, {
        id: `user-${envelope.seq}`,
        kind: "user",
        role: "user",
        content: ev.content,
      });
      break;
    }
    case "assistant_delta": {
      const s = ensureSession(state, ev.session_id);
      let last = s.messages[s.messages.length - 1];
      if (!last || last.kind !== "assistant" || !last.streaming) {
        last = pushMessage(state, ev.session_id, {
          id: `asst-${ev.request_id}`,
          kind: "assistant",
          role: "assistant",
          content: "",
          streaming: true,
        });
      }
      if (ev.kind === "reasoning") {
        last.reasoning = (last.reasoning ?? "") + ev.text;
      } else {
        last.content += ev.text;
      }
      break;
    }
    case "tool_started": {
      pushMessage(state, ev.session_id, {
        id: `tool-${ev.call_id}`,
        kind: "tool",
        content: "",
        tool: {
          callId: ev.call_id,
          name: ev.name,
          arguments: ev.arguments,
          status: "running",
          output: "",
          expanded: false,
        },
      });
      break;
    }
    case "tool_completed": {
      const card = findToolCard(state, ev.session_id, ev.call_id);
      if (card) {
        card.status = ev.status;
        card.durationMs = ev.duration_ms;
        card.exitCode = ev.exit_code;
        card.output = ev.output;
        card.diff = ev.diff;
      }
      break;
    }
    case "tool_output_delta": {
      const card = findToolCard(state, ev.session_id, ev.call_id);
      if (card) {
        card.output += ev.text;
      }
      break;
    }
    case "input_requested": {
      const s = ensureSession(state, ev.session_id);
      s.view.status = "awaiting_input";
      s.pendingQuestion = {
        requestId: ev.request_id,
        text: ev.text,
        questions: ev.questions,
      };
      s.pendingAnswer = "";
      pushMessage(state, ev.session_id, {
        id: `q-${ev.request_id}`,
        kind: "pending_question",
        content: ev.text,
      });
      break;
    }
    case "input_answered": {
      const s = state.sessionMap.get(ev.session_id);
      if (s) {
        s.pendingQuestion = null;
        pushMessage(state, ev.session_id, {
          id: `ans-${envelope.seq}`,
          kind: "system",
          content: "已提交回答",
        });
      }
      break;
    }
    case "input_rejected": {
      const s = state.sessionMap.get(ev.session_id);
      if (s) {
        s.pendingQuestion = null;
        pushMessage(state, ev.session_id, {
          id: `rej-${envelope.seq}`,
          kind: "system",
          content: "已拒绝该问题",
        });
      }
      break;
    }
    case "plan_updated": {
      pushMessage(state, ev.session_id, {
        id: `plan-${envelope.seq}`,
        kind: "assistant",
        content: "📋 计划已更新",
        plan: ev.plan,
      });
      break;
    }
    case "usage_updated":
    case "context_usage":
    case "budget_warning":
    case "stream_recovering":
    case "turn_restarting":
    case "compaction_notice":
    case "mutation_recorded":
    case "undo_completed":
    case "redo_completed":
    case "subagent_reported":
      // 派生/低频事件：暂不投影（保持简单；关键状态已在上方覆盖）。
      break;
    default: {
      const _exhaustive: never = ev;
      void _exhaustive;
    }
  }
}
