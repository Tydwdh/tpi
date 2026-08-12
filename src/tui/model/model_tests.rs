//! tui/model.rs 的测试（从 `#[cfg(test)] mod` 内联块迁出；子模块经 `super::*`
//! 访问父模块私有项，行为与内联等价）。
#[cfg(test)]
mod tests {
    use super::super::*;

    /// §用户诉求：TUI 卡片只显示用户可见正文——面向模型的 envelope 元数据头
    /// （status/revision/path/lines/cursor 等）剥掉；错误诊断保留。
    #[test]
    fn user_visible_output_strips_ai_metadata() {
        // bash：`output:` 头之后是实际输出；无行号。
        let bash = "status: succeeded\nprogram: bash\nexit_code: 0\nduration_ms: 42\noutput: 11 bytes\n\nhello world";
        assert_eq!(
            user_visible_output("bash", bash),
            ("hello world".into(), None)
        );
        // bash：尾部 `artifact: @artifact/...` 是给模型的 opaque 引用，必须剥离。
        let bash_artifact = "status: succeeded\nprogram: bash\nexit_code: 0\nduration_ms: 9\noutput: 4 bytes\n\nhi\n\nartifact: @artifact/019fec4e-fdd0-7ab3-aa13-2aaf7233e02b/019fed0a-b3c8-7092-b470-50aba264f763";
        assert_eq!(
            user_visible_output("bash", bash_artifact),
            ("hi".into(), None),
            "artifact 引用不能进用户视野"
        );
        // read：`[revision]/path/lines` 头之后是文件内容；起始行号解析自 lines: 头。
        let read = "[revision=b3:abc]\npath: src/main.rs\nlines: 1-10 of 20\n\nfn main() {}";
        assert_eq!(
            user_visible_output("read", read),
            ("fn main() {}".into(), Some(1))
        );
        // read：非 1 起始行号（分段读取）也必须精确。
        let read2 =
            "[revision=b3:abc]\npath: src/main.rs\nlines: 201-240 of 400\n\nline 201\nline 202";
        assert_eq!(
            user_visible_output("read", read2),
            ("line 201\nline 202".into(), Some(201))
        );
        // read：真实工具输出正文每行带 `{n}: ` 行号前缀（files.rs number_lines
        // 给模型精确定位用）；TUI 自己渲染行号，必须剥掉避免双行号。
        let read_numbered = "[revision=b3:abc]\npath: src/main.rs\nlines: 10-11 of 400\n\n10: fn a() {}\n11: fn b() {}";
        assert_eq!(
            user_visible_output("read", read_numbered),
            ("fn a() {}\nfn b() {}".into(), Some(10)),
            "read 正文的行号前缀必须剥掉（防双行号）"
        );
        // read：截断续读指引（§10.2 是给模型的，非文件内容）必须剥掉。
        let read_truncated = "[revision=b3:abc]\npath: src/main.rs\nlines: 1-200 of 400\n\nline 1\nline 2\n\n续读: read src/main.rs start_line=201 line_count=200";
        assert_eq!(
            user_visible_output("read", read_truncated),
            ("line 1\nline 2".into(), Some(1)),
            "续读指引不能进用户视野"
        );
        // read：字节截断提示同样剥离。
        let read_bytes = "[revision=b3:abc]\npath: src/main.rs\nlines: 1-2 of 400\n\nline 1\nline 2\n\n内容超过 32 KiB 预算被截断，请用 start_line/line_count 分段读取。";
        assert_eq!(
            user_visible_output("read", read_bytes),
            ("line 1\nline 2".into(), Some(1)),
            "字节截断提示不能进用户视野"
        );
        // edit：剥 revision 元数据，保留 applied 摘要（path 已在卡片 target）；无行号。
        let edit = "status: succeeded\ntool: edit\npath: src/a.rs\napplied: replaced 2 of 2\nprevious_revision: b3:aaa\ncurrent_revision: b3:bbb";
        assert_eq!(
            user_visible_output("edit", edit),
            ("applied: replaced 2 of 2".into(), None)
        );
        // search：剥 status，保留扫描统计与命中正文。
        let search = "status: succeeded\nscanned_files: 42\nscanned_bytes: 12345\nelapsed_ms: 3\nstop_reason: complete\nitems: 2 shown of 2\n\na.txt:1: hit";
        let (out, _) = user_visible_output("search", search);
        assert!(
            out.contains("scanned_files: 42") && out.contains("hit"),
            "扫描统计与命中保留: {out:?}"
        );
        assert!(!out.contains("status:"), "status 头必须剥掉: {out:?}");
        // 失败诊断保留（error 行不剥）。
        let err = "status: failed\ntool: read\nerror: artifact_not_found";
        let (out, _) = user_visible_output("read", err);
        assert!(out.contains("error: artifact_not_found"), "{out:?}");
        assert!(!out.contains("status:"));
        // 无元数据的纯文本原样返回（只归一化末尾换行）。
        assert_eq!(
            user_visible_output("bash", "line-1\nline-2\n"),
            ("line-1\nline-2".into(), None)
        );
    }

    #[test]
    fn streaming_chunks_form_one_logical_message() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "你");
        view.push_stream_delta(LineKind::Assistant, "好");
        assert_eq!(
            view.transcript.len(),
            0,
            "finalize 前不进 transcript（§7.2）"
        );
        let msg = view.live.assistant.as_ref().expect("live 区必须有消息");
        assert_eq!(msg.text, "你好");
        // finalize 后进入 transcript。
        view.finalize_streaming();
        assert_eq!(view.transcript.len(), 1);
        let Entry::Message { line, .. } = &view.transcript[0] else {
            panic!("finalize 后必须是消息条目");
        };
        assert_eq!(line.text, "你好");
    }

    #[test]
    fn stream_delta_bumps_version_for_render_cache() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "a");
        let v1 = view.live.assistant.as_ref().unwrap().version;
        view.push_stream_delta(LineKind::Assistant, "b");
        let v2 = view.live.assistant.as_ref().unwrap().version;
        assert_ne!(v1, v2);
    }

    /// §4.3 第三阶段：partial tool-call restart 时丢弃 live 区 partial
    /// （不进 transcript）——已显示的流式内容被清空，等待整个 turn 重新生成。
    #[test]
    fn discard_live_turn_drops_partial() {
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Reasoning, "思考中…");
        view.push_stream_delta(LineKind::Assistant, "部分回答");
        assert!(view.live.assistant.is_some() && view.live.reasoning.is_some());
        assert_eq!(view.transcript.len(), 0, "finalize 前不进 transcript");

        view.discard_live_turn();

        assert!(
            view.live.assistant.is_none() && view.live.reasoning.is_none(),
            "restart 必须丢弃 partial 流式内容（整个 turn 重新生成）"
        );
        assert_eq!(
            view.transcript.len(),
            0,
            "丢弃的 partial 不得进入 transcript（durable 事实在 session）"
        );
    }

    #[test]
    fn new_output_returns_to_follow_mode() {
        // 整改 C + TUI v2：scroll lock 期间新输出不再强制拉回底部，只计数。
        let mut view = ViewModel::default();
        // 模拟已布局（layout_top + 高度表；scroll_up 基于它移动锚点）。
        view.push_line(LineKind::Assistant, "a");
        view.push_line(LineKind::Assistant, "b");
        view.layout_top = Some((EntryId(1), 0));
        view.entry_heights.insert(EntryId(1), 1);
        view.entry_heights.insert(EntryId(2), 1);
        view.scroll_up(20);
        assert_eq!(
            view.scroll_mode,
            ScrollMode::Locked(ScrollAnchor {
                entry_id: EntryId(1),
                row_in_entry: 0,
            }),
            "scroll lock 保持（锚定最早行）"
        );
        view.push_line(LineKind::Assistant, "new");
        assert_eq!(view.pending_below, 1, "新条目计数");
        // Ctrl+End 恢复跟随并清空计数。
        view.follow_tail();
        assert_eq!(view.scroll_mode, ScrollMode::Follow);
        assert_eq!(view.pending_below, 0);
    }

    #[test]
    fn tool_card_lifecycle_matches_by_call_id() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", Some("bash: cargo test".into()), None);
        view.begin_tool("call-2", "read", Some("read src/main.rs".into()), None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Failed,
            1234,
            Some(2),
            "exit_code: 1\n错误详情",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!("call-1 必须是卡片");
        };
        assert_eq!(card.name, "bash");
        assert_eq!(
            card.target.as_deref(),
            Some("bash: cargo test"),
            "detail 保留实际命令"
        );
        assert_eq!(
            card.state,
            ToolCardState::Done {
                status: ToolStatus::Failed,
                duration_ms: 1234,
                exit_code: Some(2)
            }
        );
        assert!(card.tail.as_deref().unwrap_or("").contains("错误详情"));
        // 未完成的 call-2 仍在 live 区（§7.2），保持 Running。
        let card2 = view.live.tools.get("call-2").expect("call-2 必须在 live");
        assert_eq!(card2.card.state, ToolCardState::Running);
        assert_eq!(
            view.transcript.len(),
            1,
            "只有 call-1 finalize 进 transcript"
        );
    }

    #[test]
    fn success_card_has_no_tail() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", None, None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            42,
            Some(0),
            "大量输出",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert_eq!(
            card.state,
            ToolCardState::Done {
                status: ToolStatus::Succeeded,
                duration_ms: 42,
                exit_code: Some(0)
            }
        );
        assert!(card.tail.is_none());
    }

    #[test]
    fn tail_is_bounded() {
        let mut view = ViewModel::default();
        view.begin_tool("c", "bash", None, None);
        view.finish_tool(
            ("c", "bash"),
            ToolStatus::Failed,
            0,
            None,
            "x".repeat(10_000),
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(card.tail.as_ref().unwrap().chars().count() <= 241);
    }

    #[test]
    fn usage_accumulates() {
        let mut view = ViewModel::default();
        view.add_usage(&Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
        });
        view.add_usage(&Usage {
            input_tokens: 200,
            output_tokens: 60,
            cache_read_tokens: 20,
        });
        assert_eq!(view.input_tokens, 300);
        assert_eq!(view.output_tokens, 110);
        assert_eq!(view.cache_read_tokens, 30, "缓存命中 token 必须累积");
    }

    /// §16.2：配置单价后按 token 累计花费（每百万 token 美元）。
    #[test]
    fn usage_accumulates_cost_with_pricing() {
        let mut view = ViewModel {
            price_input: Some(1.0),  // $1/1M input
            price_output: Some(2.0), // $2/1M output
            ..Default::default()
        };
        // 1M input tokens + 1M output tokens → 1.0 + 2.0 = 3.0。
        view.add_usage(&Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 100,
        });
        assert!(
            (view.cost_usd - 3.0).abs() < 1e-9,
            "cost = {:.4}",
            view.cost_usd
        );
        // 缓存命中不影响花费（只展示命中量）。
        assert_eq!(view.cache_read_tokens, 100);

        // 无 pricing → cost 恒 0（不显示花费）。
        let mut view2 = ViewModel::default();
        view2.add_usage(&Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
        });
        assert_eq!(view2.cost_usd, 0.0, "未配置单价不计算花费");
    }

    #[test]
    fn command_menu_filters_and_preserves_selection() {
        let mut view = ViewModel {
            input: "/set".into(),
            ..Default::default()
        };
        view.refresh_command_menu();
        let menu = view.menu.as_ref().expect("菜单应打开");
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].0, "settings");

        // 完全匹配时保留一条。
        view.input = "/help".into();
        view.refresh_command_menu();
        assert_eq!(view.menu.as_ref().unwrap().items[0].0, "help");

        // 无前缀 '/' 时关闭。
        view.input = "hello".into();
        view.refresh_command_menu();
        assert!(view.menu.is_none());

        // 前缀无匹配时关闭（未知命令走普通消息）。
        view.input = "/xyz".into();
        view.refresh_command_menu();
        assert!(view.menu.is_none());
    }

    #[test]
    fn menu_completion_replaces_input() {
        let mut view = ViewModel {
            input: "/set".into(),
            ..Default::default()
        };
        view.refresh_command_menu();
        view.complete_menu_command();
        assert_eq!(view.input, "/settings");
        assert_eq!(view.input_cursor, "/settings".len());
    }

    #[test]
    fn reasoning_fold_defaults_collapsed() {
        // 整改 A1：reasoning 默认折叠（不形成正文墙）。
        let view = ViewModel::default();
        assert!(view.reasoning_expanded.is_empty());
        assert!(!view.is_reasoning_expanded(EntryId(1)));
    }

    #[test]
    fn finish_tool_keeps_success_output_expandable() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", None, None);
        // 运行中实时输出累积。
        view.append_tool_output("call-1", "line-1\n");
        view.append_tool_output("call-1", "line-2\n");
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            42,
            Some(0),
            "line-1\nline-2\n",
            None,
        );
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        // 成功也保留完整输出（此前被丢弃）；净化只剥元数据头，正文原样
        //（lines() 归一化会去掉末尾换行）。
        assert_eq!(card.output.as_deref(), Some("line-1\nline-2"));
        assert_eq!(card.tail, None, "成功卡片折叠态不显示红色 tail");
        assert!(!card.expanded);
        // 展开切换。
        view.toggle_expand("call-1");
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(card.expanded);
        view.toggle_last_tool_expanded();
        let Entry::Tool { card, .. } = &view.transcript[0] else {
            panic!();
        };
        assert!(!card.expanded, "Alt+E 再次切换回折叠");
    }

    #[test]
    fn append_tool_output_is_bounded() {
        let mut view = ViewModel::default();
        view.begin_tool("c", "bash", None, None);
        let big = "x".repeat(40 * 1024);
        view.append_tool_output("c", &big);
        let tool = view.live.tools.get("c").expect("live 区必须有卡片");
        let card = &tool.card;
        assert!(card.output_truncated, "超过预算必须标记截断");
        assert!(card.output.as_ref().unwrap().len() <= MAX_CARD_OUTPUT);
        // 尾部保留（错误相关输出通常在末尾）。
        assert!(card.output.as_ref().unwrap().ends_with('x'));
    }

    #[test]
    fn at_menu_filters_file_index() {
        let mut view = ViewModel {
            file_index: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "Cargo.toml".into(),
            ],
            ..Default::default()
        };
        view.input = "看看 @src/".into();
        assert!(view.has_at_token());
        view.refresh_at_menu();
        let menu = view.menu.as_ref().expect("@ 菜单应打开");
        assert_eq!(menu.kind, crate::tui::model::MenuKind::File);
        assert_eq!(menu.items.len(), 2, "只显示前缀匹配的文件");
        // Enter 补全：@token 被替换为选中路径。
        view.menu.as_mut().unwrap().selected = 0;
        view.complete_menu_command();
        assert_eq!(view.input, "看看 src/main.rs");
        assert_eq!(view.menu, None, "补全后菜单关闭");
    }

    #[test]
    fn at_menu_closes_without_match_or_token() {
        let mut view = ViewModel {
            file_index: vec!["src/main.rs".into()],
            ..Default::default()
        };
        // 无 @ token：不弹菜单。
        view.input = "hello".into();
        view.refresh_at_menu();
        assert!(view.menu.is_none());
        // 有 token 但无匹配：关闭。
        view.input = "@no-such-file".into();
        view.refresh_at_menu();
        assert!(view.menu.is_none());
    }
}

#[cfg(test)]
mod p1_message_cap_tests {
    use super::super::*;

    /// P1-9：assistant 流式消息超过 MAX_MESSAGE_CHARS 时必须截断并标记，
    /// 不能无限膨胀（此前工具输出有上限而消息没有）。
    #[test]
    fn stream_delta_is_bounded_for_messages() {
        let mut view = ViewModel::default();
        let chunk = "a".repeat(1024);
        let total_chunks = (MAX_MESSAGE_CHARS / 1024) + 10;
        for _ in 0..total_chunks {
            view.push_stream_delta(LineKind::Assistant, &chunk);
        }
        let msg = view.live.assistant.as_ref().expect("live 区必须有消息");
        assert!(
            msg.text.len() <= MAX_MESSAGE_CHARS + 32,
            "消息必须被截断: {} > {}",
            msg.text.len(),
            MAX_MESSAGE_CHARS
        );
        assert!(msg.text.contains("truncated"), "截断必须带标记");
    }

    /// P1-9：reasoning 同样有界。
    #[test]
    fn stream_reasoning_is_bounded() {
        let mut view = ViewModel::default();
        for _ in 0..(MAX_MESSAGE_CHARS / 1024 + 10) {
            view.push_stream_delta(LineKind::Reasoning, &"r".repeat(1024));
        }
        let msg = view.live.reasoning.as_ref().expect("live 区必须有消息");
        assert!(msg.text.len() <= MAX_MESSAGE_CHARS + 32);
    }
}

#[cfg(test)]
mod p2_card_nav_tests {
    use super::super::*;

    fn view_with_two_cards() -> ViewModel {
        let mut view = ViewModel::default();
        // 卡 1：成功。
        view.begin_tool(
            String::from("call-1"),
            String::from("read"),
            Some(String::from("a.rs")),
            None,
        );
        view.finish_tool(
            (String::from("call-1"), String::from("read")),
            ToolStatus::Succeeded,
            10,
            None,
            String::from("ok"),
            None,
        );
        // 卡 2：失败。
        view.begin_tool(
            String::from("call-2"),
            String::from("bash"),
            Some(String::from("ls")),
            Some(String::from("ls")),
        );
        view.finish_tool(
            (String::from("call-2"), String::from("bash")),
            ToolStatus::Failed,
            20,
            Some(1),
            String::from("boom"),
            None,
        );
        view
    }

    /// P2：Alt+O 打开最近一张失败卡片（跳过成功卡片）。
    #[test]
    fn failed_tool_overlay_skips_success_cards() {
        let mut view = view_with_two_cards();
        view.open_failed_tool_overlay();
        let overlay = view.overlay.expect("失败卡片必须可打开");
        assert_eq!(overlay.tool_id.as_deref(), Some("call-2"));
        assert!(overlay.title.contains("failed"), "{overlay:?}");
    }

    /// P2：Alt+[ / Alt+] 在卡片间循环切换。
    #[test]
    fn cycle_tool_overlay_wraps_around() {
        let mut view = view_with_two_cards();
        view.open_tool_overlay("call-1");
        view.cycle_tool_overlay(1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-2")
        );
        view.cycle_tool_overlay(1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-1"),
            "循环回绕"
        );
        view.cycle_tool_overlay(-1);
        assert_eq!(
            view.overlay
                .as_ref()
                .and_then(|o| o.tool_id.clone())
                .as_deref(),
            Some("call-2"),
            "反向"
        );
    }

    /// BUG-006：/new 后屏幕投影必须全部清空（模型上下文已清空）。
    #[test]
    fn reset_for_new_session_clears_all_projection() {
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "旧会话问题");
        view.push_line(LineKind::Assistant, "旧会话回答");
        view.push_stream_delta(LineKind::Assistant, "流式中");
        view.plan = Some(crate::tool::plan::Plan {
            explanation: Some("旧计划".into()),
            items: Vec::new(),
        });
        view.input_tokens = 123;
        view.output_tokens = 456;
        view.context_usage = Some((10, 100));
        view.open_modal("/old", "旧 modal");
        view.lock_to(crate::tui::scroll::EntryId(1), 0);
        view.reset_for_new_session();
        assert!(view.transcript.is_empty(), "transcript 必须清空");
        assert!(view.live.assistant.is_none() && view.live.reasoning.is_none());
        assert!(view.plan.is_none(), "旧计划必须清空");
        assert_eq!(view.input_tokens, 0);
        assert_eq!(view.output_tokens, 0);
        assert!(view.context_usage.is_none());
        assert!(view.modal.is_none());
        assert_eq!(view.scroll_mode, ScrollMode::Follow, "滚动必须回到底部");
    }
    /// BUG-006：恢复 session 后必须把 history 重建到屏幕（User/Assistant/工具摘要）。
    #[test]
    fn load_history_rebuilds_transcript_from_context() {
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "旧内容"); // 先有旧屏幕
        let history = vec![
            crate::provider::ChatMessage::User("新会话问题".into()),
            crate::provider::ChatMessage::Assistant {
                content: "新会话回答".into(),
                tool_calls: Vec::new(),
            },
            crate::provider::ChatMessage::Tool {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                content: "status: failed\\nerror: x".into(),
            },
        ];
        view.load_history(&history);
        assert_eq!(view.transcript.len(), 3, "旧屏幕被替换为重建历史");
        let kinds: Vec<LineKind> = view
            .transcript
            .iter()
            .map(|e| match e {
                Entry::Message { line, .. } => line.kind,
                Entry::Tool { .. } => LineKind::Tool,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![LineKind::User, LineKind::Assistant, LineKind::Tool]
        );
    }

    /// §用户诉求：恢复会话后工具卡片净化——不再残留 `bash: status: succeeded`
    /// 或 `[revision=...]`；read 正文保留并带起始行号。
    #[test]
    fn load_history_rebuilds_sanitized_tool_cards() {
        let mut view = ViewModel::default();
        let history = vec![
            crate::provider::ChatMessage::Tool {
                tool_call_id: "call-1".into(),
                name: "read".into(),
                content:
                    "[revision=b3:abc]\npath: src/a.rs\nlines: 10-12 of 50\n\nline 10\nline 11"
                        .into(),
            },
            crate::provider::ChatMessage::Tool {
                tool_call_id: "call-2".into(),
                name: "bash".into(),
                content: "status: succeeded\noutput: 4 bytes\n\nhi".into(),
            },
        ];
        view.load_history(&history);
        assert_eq!(view.transcript.len(), 2, "两个工具卡片");
        for entry in &view.transcript {
            let Entry::Tool { card, .. } = entry else {
                panic!("恢复后工具必须是卡片");
            };
            let body = card.output.as_deref().unwrap_or("");
            assert!(
                !body.contains("[revision=") && !body.contains("status: succeeded"),
                "模型元数据必须剥掉: {body:?}"
            );
            if card.name == "read" {
                assert_eq!(card.line_number_start, Some(10), "read 起始行号");
                assert!(body.contains("line 10"), "read 正文保留: {body:?}");
            } else {
                assert_eq!(body, "hi", "bash 只显示输出: {body:?}");
            }
        }
    }

    /// §24：点击工具卡片设置 active_hit（高亮反馈），关闭 Overlay 清除。
    #[test]
    fn tool_overlay_sets_and_clears_active_hit() {
        let mut view = ViewModel::default();
        view.begin_tool("call-1", "bash", Some("run".into()), None);
        view.finish_tool(
            ("call-1", "bash"),
            ToolStatus::Succeeded,
            10,
            Some(0),
            "",
            None,
        );
        assert!(view.active_hit.is_none(), "初始无高亮");

        view.open_tool_overlay("call-1");
        assert!(
            matches!(&view.active_hit, Some(crate::tui::HitTarget::Tool(id)) if id == "call-1"),
            "点击后必须设置 active_hit: {:?}",
            view.active_hit
        );
        assert!(view.overlay.is_some(), "Overlay 打开");

        view.close_overlay();
        assert!(view.active_hit.is_none(), "关闭 Overlay 清除高亮");
        assert!(view.overlay.is_none());
    }

    /// §用户诉求：应用内选择——语义位置（entry + 偏移），开始/更新/清除。
    /// 语义选区指向内容而非屏幕坐标；反向拖动由 normalized() 处理。
    #[test]
    fn selection_state_lifecycle() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        assert!(view.selection.is_none());

        let p2 = TextPosition {
            entry_id: EntryId(1),
            offset: 2,
        };
        let p3 = TextPosition {
            entry_id: EntryId(1),
            offset: 3,
        };
        let p7 = TextPosition {
            entry_id: EntryId(1),
            offset: 7,
        };
        view.selection_start(p2);
        let sel = view.selection.as_ref().expect("开始选择");
        assert_eq!(sel.anchor, p2);
        assert_eq!(sel.focus, p2);

        view.selection_update(p7);
        let sel = view.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, p2, "anchor 保持按下点");
        assert_eq!(sel.focus, p7);
        assert_eq!(sel.normalized(), (p2, p7));

        // 反向拖动：anchor 不变，focus 更新。
        view.selection_update(p3);
        let sel = view.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, p2);
        assert_eq!(sel.focus, p3);
        // 值排序：offset 2 < offset 3 → normalized 为 (p2, p3)。
        assert_eq!(sel.normalized(), (p2, p3));

        view.selection_end();
        assert!(view.selection.is_some(), "释放后选区保留");

        view.selection_clear();
        assert!(view.selection.is_none());
    }

    /// §PointerHit：selected_text 从 ViewModel 语义文本提取（不依赖 viewport）。
    /// 选区指向 (entry, offset)，resize/滚动后仍精确；无选区返回空。
    #[test]
    fn selected_text_extracts_from_transcript_entries() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        view.push_line(LineKind::User, "hello world");
        view.push_line(LineKind::Assistant, "second line");

        // 无选区 → 空。
        assert_eq!(view.selected_text(), "");

        // 选区：entry 1 的 offset 0..5 → "hello"。
        view.selection_start(TextPosition {
            entry_id: EntryId(1),
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: EntryId(1),
            offset: 5,
        });
        assert_eq!(view.selected_text(), "hello");

        // 跨 entry：entry 1 offset 6 → entry 2 offset 6 → "world\nsecond"。
        view.selection_start(TextPosition {
            entry_id: EntryId(1),
            offset: 6,
        });
        view.selection_update(TextPosition {
            entry_id: EntryId(2),
            offset: 6,
        });
        assert_eq!(view.selected_text(), "world\nsecond");
    }

    /// §PointerHit：streaming 期间选中的稳定 id 在 finalize 后仍有效——
    /// 运行中拖选 → Agent 输出结束 → Ctrl+C 复制不悬空。
    #[test]
    fn streaming_selection_survives_finalize() {
        use crate::tui::interaction::TextPosition;
        let mut view = ViewModel::default();
        view.push_stream_delta(LineKind::Assistant, "streaming content");
        // streaming 期间选中（entry_id 已分配）。
        let live_id = view
            .live
            .assistant
            .as_ref()
            .expect("streaming 消息必须有 entry_id")
            .entry_id;
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 9,
        });
        assert_eq!(view.selected_text(), "streaming");
        // finalize：沿用同一 id，选区不悬空。
        view.finalize_streaming();
        let finalized = view.transcript.last().expect("finalize 后必须提交").id();
        assert_eq!(finalized, live_id, "finalize 必须沿用 streaming 的稳定 id");
        assert_eq!(
            view.selected_text(),
            "streaming",
            "finalize 后选区仍指向同一内容"
        );
    }

    /// §PointerHit ⑤：运行中工具卡片有独立稳定 id，finalize 后选区不悬空，
    /// selected_text 能覆盖 tool 输出（情况 A：仅 tool 运行时）。
    #[test]
    fn live_tool_selection_survives_finalize() {
        use crate::tui::interaction::TextPosition;
        use crate::tui::scroll::EntryId;
        let mut view = ViewModel::default();
        view.begin_tool("c1", "bash", Some("cargo test".into()), None);
        view.append_tool_output("c1", "running tests...");
        // 运行中 tool 有独立 id（不是 assistant/reasoning 的，也不是哨兵）。
        let live_id = view.live.tools.get("c1").expect("c1 必须在 live").entry_id;
        assert_ne!(live_id, EntryId(u64::MAX), "live tool 不得用哨兵 id");
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 0,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 7,
        });
        // tool 语义文本 = "bash cargo test\nrunning tests..."；offset 0..7 = "bash ca"。
        assert_eq!(view.selected_text(), "bash ca");
        // 选 body 部分：offset 跳过 "bash cargo test\n"（16 chars）后到 "running"。
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 16,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 23,
        });
        assert_eq!(view.selected_text(), "running");
        // finish_tool：沿用同一 id，选区不悬空。
        view.finish_tool(("c1", "bash"), ToolStatus::Succeeded, 10, None, "", None);
        let finalized = view.transcript.last().expect("finish 后必须提交").id();
        assert_eq!(finalized, live_id, "finish_tool 必须沿用 begin 的稳定 id");
        // §修复：header 语义含 meta（与渲染 card_semantic_header 一致）。
        // Running（无 meta）→ Done（+ " 10ms"）：header 从 16 chars 变 21，
        // body 起始偏移后移——这是内容真实变化（meta 加入），选区 offset
        // 不再指向旧位置。验证新 offset 正确落到 body："bash cargo test 10ms"
        // (21 chars) + "running"。
        assert_eq!(
            view.selected_text(),
            "10ms\nru",
            "finalize 后 header 语义加入 meta，偏移指向新内容（预期变化）"
        );
        // 用新 offset 重新选中 body 的 "running"：内容正确定位。
        view.selection_start(TextPosition {
            entry_id: live_id,
            offset: 21,
        });
        view.selection_update(TextPosition {
            entry_id: live_id,
            offset: 28,
        });
        assert_eq!(view.selected_text(), "running", "body 内容不受 meta 影响");
    }
}
