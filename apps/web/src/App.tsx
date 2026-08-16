/**
 * TPI Web 主组件（web_desktop.md §十三/§十四）。
 *
 * 布局：
 * ```text
 * ┌─────────────────────────────────────────────┐
 * │ TPI                       Connected / Model │
 * ├─────────────┬───────────────────────────────┤
 * │ Sessions    │        Conversation           │
 * │ session A   │    User / Assistant / Tool    │
 * │ ...         │    [pending question dialog]  │
 * ├─────────────┴───────────────────────────────┤
 * │ Message input                    Send       │
 * └─────────────────────────────────────────────┘
 * ```
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { TpiClient, type EventEnvelope } from "../../../packages/tpi-client/src";
import {
  AppViewState,
  MessageView,
  applyEvent,
  createInitialState,
} from "./store";

export function App() {
  const clientRef = useRef<TpiClient | null>(null);
  const [state, setState] = useState<AppViewState>(createInitialState);
  const stateRef = useRef(state);
  stateRef.current = state;

  const [connected, setConnected] = useState(false);
  const [input, setInput] = useState("");

  // 连接生命周期。
  useEffect(() => {
    const client = new TpiClient({
      url: undefined, // 默认当前 host + /ws（dev 经 Vite 代理；prod 同源）。
      clientName: "tpi-web",
      autoReconnect: true,
      onStateChange: (s, detail) => {
        setConnected(s === "connected");
        setState((prev) => ({ ...prev, connection: s, connectionDetail: detail }));
      },
    });
    client.onEvent = (envelope: EventEnvelope) => {
      // 事件驱动状态投影：UI 不持有 run/tool truth。
      setState((prev) => {
        const next = { ...prev, sessionMap: new Map(prev.sessionMap) };
        applyEvent(next, envelope);
        return next;
      });
    };
    clientRef.current = client;
    client.connect().catch((e) => console.error("connect failed", e));

    // 连接后加载会话列表。
    const timer = setTimeout(async () => {
      await client.listSessions();
    }, 300);

    return () => {
      clearTimeout(timer);
      client.disconnect();
    };
  }, []);

  const submit = useCallback(async () => {
    const text = input.trim();
    if (!text) return;
    const client = clientRef.current;
    if (!client || !connected) return;
    setInput("");
    const s = stateRef.current;
    let sessionId = s.activeSessionId;
    if (!sessionId) {
      // 无活跃会话：先创建（首条消息将作为新会话）。
      await client.createSession();
      // 等 session_created 事件更新 activeSessionId 后重试一次。
      await new Promise((r) => setTimeout(r, 200));
      sessionId = stateRef.current.activeSessionId;
    }
    if (!sessionId) return;
    await client.submitMessage(sessionId, text);
  }, [input, connected]);

  const sendAnswer = useCallback(async () => {
    const client = clientRef.current;
    const s = stateRef.current;
    const sess = s.activeSessionId ? s.sessionMap.get(s.activeSessionId) : null;
    if (!client || !sess?.pendingQuestion) return;
    const answer = sess.pendingAnswer;
    if (!answer.trim()) return;
    await client.answerInput(sess.id, sess.pendingQuestion.requestId, answer.trim());
  }, []);

  const cancelRun = useCallback(async () => {
    const client = clientRef.current;
    const s = stateRef.current;
    if (!client || !s.activeSessionId) return;
    await client.cancelRun(s.activeSessionId);
  }, []);

  const retryRun = useCallback(async () => {
    const client = clientRef.current;
    const s = stateRef.current;
    if (!client || !s.activeSessionId) return;
    await client.retryRun(s.activeSessionId);
  }, []);

  const undo = useCallback(async () => {
    const client = clientRef.current;
    const s = stateRef.current;
    if (!client || !s.activeSessionId) return;
    await client.undo(s.activeSessionId);
  }, []);

  const redo = useCallback(async () => {
    const client = clientRef.current;
    const s = stateRef.current;
    if (!client || !s.activeSessionId) return;
    await client.redo(s.activeSessionId);
  }, []);

  const active = state.activeSessionId ? state.sessionMap.get(state.activeSessionId) : null;

  return (
    <div className="app">
      <header className="topbar">
        <span className="logo">TPI</span>
        <span className="conn">
          <span className={`dot ${connected ? "ok" : ""}`} />
          {state.connection === "connected" ? "Connected" : state.connection}
        </span>
      </header>

      <div className="body">
        <aside className="sidebar">
          <div className="sidebar-title">Sessions</div>
          <button
            className="new-session"
            onClick={async () => {
              await clientRef.current?.createSession();
            }}
          >
            + 新建会话
          </button>
          {state.sessions.map((s) => (
            <button
              key={s.id}
              className={`session-item ${s.id === state.activeSessionId ? "active" : ""}`}
              onClick={async () => {
                const client = clientRef.current;
                if (!client) return;
                setState((prev) => ({ ...prev, activeSessionId: s.id }));
                await client.resumeSession(s.id);
              }}
            >
              <div className="session-title">{s.title || "(无标题)"}</div>
              <div className={`session-status ${s.status}`}>{s.status}</div>
            </button>
          ))}
        </aside>

        <main className="conversation">
          <div className="messages" ref={(el) => el?.scrollTo({ top: el.scrollHeight })}>
            {!active && <div className="empty">选择或创建一个会话开始</div>}
            {active?.messages.map((m) => <MessageRow key={m.id} message={m} />)}
          </div>

          {active?.pendingQuestion && (
            <div className="question-dialog">
              <div className="q-title">⏸ 等待你的输入</div>
              <div className="q-text">{active.pendingQuestion.text}</div>
              <input
                autoFocus
                value={active.pendingAnswer}
                onChange={(e) =>
                  setState((prev) => {
                    const next = { ...prev, sessionMap: new Map(prev.sessionMap) };
                    const s = next.sessionMap.get(active.id);
                    if (s) s.pendingAnswer = e.target.value;
                    return next;
                  })
                }
                onKeyDown={(e) => e.key === "Enter" && sendAnswer()}
                placeholder="输入回答后回车…"
              />
              <div className="q-actions">
                <button onClick={sendAnswer}>提交回答</button>
                <button
                  onClick={async () => {
                    const client = clientRef.current;
                    if (client && active.pendingQuestion) {
                      // 拒绝：发送空回答由 runtime 裁决（当前协议无独立 reject，用特殊标记）。
                      await client.answerInput(active.id, active.pendingQuestion.requestId, "");
                    }
                  }}
                >
                  拒绝
                </button>
              </div>
            </div>
          )}

          <div className="input-bar">
            <textarea
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  submit();
                }
              }}
              placeholder="输入消息（Enter 发送，Shift+Enter 换行）…"
              rows={2}
            />
            <div className="input-actions">
              <button onClick={cancelRun} title="取消当前 run">取消</button>
              <button onClick={retryRun} title="重试上次 run">重试</button>
              <button onClick={undo} title="撤销最近文件变更">撤销</button>
              <button onClick={redo} title="重做">重做</button>
              <button className="primary" onClick={submit}>发送</button>
            </div>
          </div>
        </main>
      </div>
    </div>
  );
}

function MessageRow({ message }: { message: MessageView }) {
  switch (message.kind) {
    case "user":
      return (
        <div className="msg user">
          <div className="msg-label">你</div>
          <pre className="msg-content">{message.content}</pre>
        </div>
      );
    case "assistant":
      return (
        <div className="msg assistant">
          <div className="msg-label">TPI</div>
          <div className="msg-content">
            {message.reasoning && (
              <details className="reasoning">
                <summary>思考过程</summary>
                <pre>{message.reasoning}</pre>
              </details>
            )}
            <pre className="text">{message.content}</pre>
            {message.streaming && <span className="cursor">▋</span>}
          </div>
        </div>
      );
    case "tool": {
      const t = message.tool;
      if (!t) return null;
      return (
        <div className={`msg tool ${t.status}`}>
          <div className="tool-head">
            <span className={`tool-status ${t.status}`}>
              {t.status === "running" ? "⏳" : t.status === "success" ? "✓" : t.status === "failed" ? "✗" : "·"}
            </span>
            <span className="tool-name">{t.name}</span>
            {t.durationMs !== undefined && <span className="tool-dur">{t.durationMs}ms</span>}
            {t.exitCode !== null && t.exitCode !== undefined && (
              <span className="tool-exit">exit {t.exitCode}</span>
            )}
          </div>
          {t.output && <pre className="tool-output">{t.output}</pre>}
          {t.diff && <pre className="tool-diff">{t.diff}</pre>}
        </div>
      );
    }
    case "pending_question":
      return (
        <div className="msg system question">
          <div className="msg-label">❓</div>
          <pre className="msg-content">{message.content}</pre>
        </div>
      );
    case "system":
      return (
        <div className={`msg system ${message.error ? "error" : ""}`}>
          <pre className="msg-content">{message.content}</pre>
        </div>
      );
    default:
      return null;
  }
}
