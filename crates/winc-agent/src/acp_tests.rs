use super::*;

#[test]
fn prompt_response_finishes_the_turn_after_streamed_updates() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client
        .receive(json!({"id":0,"result":{"protocolVersion":1}}), &mut events)
        .unwrap();
    assert!(matches!(
        events.pop(),
        Some(Event::Ready {
            login_required: false
        })
    ));
    client.command(2, Command::NewSession, &mut events).unwrap();
    assert_eq!(outbound.recv().unwrap()["method"], "session/new");
    client
        .receive(json!({"id":2,"result":{"sessionId":"s"}}), &mut events)
        .unwrap();
    client
        .command(
            3,
            Command::Send {
                session: "s".into(),
                text: "hello".into(),
                image: None,
                model: None,
                effort: None,
                permissions: PermissionMode::Ask,
            },
            &mut events,
        )
        .unwrap();
    assert_eq!(outbound.recv().unwrap()["method"], "session/prompt");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnStarted(Some(3), _)))
    );
    client
        .receive(
            json!({"method":"session/update","params":{"sessionId":"s","update":{
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer"}
            }}}),
            &mut events,
        )
        .unwrap();
    assert!(
        matches!(events.last(), Some(Event::Delta(_, _, DeltaKind::Message, text)) if text == "answer")
    );
    client
        .receive(
            json!({"id":3,"result":{"stopReason":"end_turn"}}),
            &mut events,
        )
        .unwrap();
    assert!(matches!(
        events.last(),
        Some(Event::TurnEnded(_, TurnStatus::Completed))
    ));
}

#[test]
fn approval_preserves_wire_option_and_replies_to_original_request() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::new(), &wire);
    let turn = TurnKey {
        session: "s".into(),
        turn: "1".into(),
    };
    client.turns.insert("s".into(), ActiveTurn::new(turn));
    let mut events = Vec::new();
    client
        .receive(
            json!({"id":"permission","method":"session/request_permission","params":{
                "sessionId":"s","toolCall":{"toolCallId":"tool","title":"Read file"},
                "options":[{"optionId":"persist","name":"Always allow","kind":"allow_always"},
                           {"optionId":"one","name":"Allow once","kind":"allow_once"}]
            }}),
            &mut events,
        )
        .unwrap();
    let Some(Event::Approval(approval)) = events.pop() else {
        panic!("missing approval")
    };
    assert!(
        approval
            .decisions
            .contains(&ApprovalDecision::AcceptForSession)
    );
    client
        .command(
            4,
            Command::Approve {
                approval: approval.id,
                decision: ApprovalDecision::AcceptForSession,
            },
            &mut events,
        )
        .unwrap();
    let response = outbound.recv().unwrap();
    assert_eq!(response["id"], "permission");
    assert_eq!(response["result"]["outcome"]["optionId"], "persist");
}

#[test]
fn cancelling_a_turn_resolves_permissions_and_waits_for_terminal_reply() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::new(), &wire);
    let turn = TurnKey {
        session: "s".into(),
        turn: "3".into(),
    };
    client
        .turns
        .insert("s".into(), ActiveTurn::new(turn.clone()));
    client
        .requests
        .insert(3, (Instant::now(), Request::Prompt("s".into())));
    let mut events = Vec::new();
    client
        .receive(
            json!({"id":10,"method":"session/request_permission","params":{
                "sessionId":"s","toolCall":{"toolCallId":"tool","title":"Read file"},"options":[]
            }}),
            &mut events,
        )
        .unwrap();

    client
        .command(4, Command::Stop(turn.clone()), &mut events)
        .unwrap();
    assert_eq!(outbound.recv().unwrap()["method"], "session/cancel");
    let permission = outbound.recv().unwrap();
    assert_eq!(permission["id"], 10);
    assert_eq!(permission["result"]["outcome"]["outcome"], "cancelled");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ApprovalResolved(0)))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TurnEnded(..)))
    );
    client
        .receive(
            json!({"id":3,"result":{"stopReason":"cancelled"}}),
            &mut events,
        )
        .unwrap();
    assert!(
        matches!(events.last(), Some(Event::TurnEnded(key, TurnStatus::Interrupted)) if *key == turn)
    );
}

#[test]
fn sparse_tool_updates_preserve_title_kind_and_output() {
    let (wire, _) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::new(), &wire);
    let key = TurnKey {
        session: "s".into(),
        turn: "1".into(),
    };
    client.turns.insert("s".into(), ActiveTurn::new(key));
    let mut events = Vec::new();
    for update in [
        json!({"sessionUpdate":"tool_call","toolCallId":"one","title":"List files","kind":"execute",
               "content":[{"type":"content","content":{"type":"text","text":"winc"}}]}),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"one","status":"completed"}),
    ] {
        client.update(&json!({"sessionId":"s","update":update}), &mut events);
    }
    assert!(matches!(events.last(), Some(Event::Item(_, item, true))
        if item.text == "List files" && item.kind == ItemKind::Command && item.output == "winc"));
}

#[test]
fn session_config_options_tier_and_idle_usage_update() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client.initialize().unwrap();
    let init = outbound.recv().unwrap();
    assert_eq!(init["params"]["clientCapabilities"]["elicitation"]["form"], json!({}));

    client
        .receive(json!({"id":0,"result":{"protocolVersion":1}}), &mut events)
        .unwrap();
    client.command(2, Command::NewSession, &mut events).unwrap();
    let _ = outbound.recv().unwrap();
    events.clear();
    client
        .receive(
            json!({
                "id": 2,
                "result": {
                    "sessionId": "s1",
                    "configOptions": [
                        {
                            "id": "model",
                            "name": "Model",
                            "category": "model",
                            "type": "select",
                            "currentValue": "gemini-4-neon",
                            "options": [
                                {"value": "gemini-4-neon", "name": "Gemini 4 Neon"},
                                {"value": "gpt-oss-120b", "name": "GPT-OSS 120B"}
                            ]
                        },
                        {
                            "id": "thought_level",
                            "name": "Thinking Level",
                            "category": "thought_level",
                            "type": "select",
                            "currentValue": "high",
                            "options": [
                                {"value": "low", "name": "Low"},
                                {"value": "high", "name": "High"}
                            ]
                        },
                        {
                            "id": "mode",
                            "name": "Mode",
                            "category": "mode",
                            "type": "select",
                            "currentValue": "default",
                            "options": [
                                {"value": "default", "name": "Default", "description": "Ask first"},
                                {"value": "auto", "name": "Auto", "description": "Auto approve"}
                            ]
                        }
                    ],
                    "_meta": {"antigravity": {"tier": {"name": "Google AI Pro"}}}
                }
            }),
            &mut events,
        )
        .unwrap();

    assert!(events.iter().any(|e| matches!(e, Event::AccountTier(Some(t)) if t == "Google AI Pro")));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::SessionModes(s, modes, Some(cur))
            if s == "s1" && modes.len() == 2 && cur == "default"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::SessionModels { session, models, current_model: Some(m), current_effort: Some(ef) }
            if session == "s1" && models.len() == 2 && m == "gemini-4-neon" && ef == "high"
                && models[0].efforts == vec!["low", "high"]
    )));

    // Usage update outside an active turn is still delivered.
    events.clear();
    client
        .receive(
            json!({"method":"session/update","params":{"sessionId":"s1","update":{
                "sessionUpdate":"usage_update","used":0,"size":1048576,
                "cost":{"amount":0.12,"currency":"USD"}
            }}}),
            &mut events,
        )
        .unwrap();
    assert!(events.iter().any(|e| matches!(e, Event::Tokens(k, 0, 0, Some(1048576)) if k.session == "s1")));
    assert!(events.iter().any(|e| matches!(e, Event::SessionCost(s, a, c) if s == "s1" && (*a - 0.12).abs() < 1e-6 && c == "USD")));

    // Switching mode & model issues session/set_config_option.
    client
        .command(3, Command::SetMode { session: "s1".into(), mode: "auto".into() }, &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/set_config_option");
    assert_eq!(req["params"]["configId"], "mode");
    assert_eq!(req["params"]["value"], "auto");

    client
        .command(4, Command::SetModel { session: "s1".into(), model: "gpt-oss-120b".into() }, &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["params"]["configId"], "model");
    assert_eq!(req["params"]["value"], "gpt-oss-120b");
    events.clear();
    client
        .receive(
            json!({
                "id": 4,
                "result": {
                    "configOptions": [
                        {
                            "id": "model",
                            "category": "model",
                            "type": "select",
                            "currentValue": "gpt-oss-120b",
                            "options": [
                                {"value": "gemini-4-neon", "name": "Gemini 4 Neon"},
                                {"value": "gpt-oss-120b", "name": "GPT-OSS 120B"}
                            ]
                        }
                    ]
                }
            }),
            &mut events,
        )
        .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::SessionModels { current_model: Some(m), current_effort: None, .. } if m == "gpt-oss-120b"
    )));
}

#[test]
fn elicitation_form_parses_options_and_submits_response() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::new(), &wire);
    let mut events = Vec::new();
    client
        .receive(
            json!({
                "id": "elicit-1",
                "method": "elicitation/create",
                "params": {
                    "sessionId": "s1",
                    "mode": "form",
                    "message": "The agent has a question.",
                    "requestedSchema": {
                        "type": "object",
                        "properties": {
                            "q10": {"type": "string", "title": "Tenth"},
                            "q2": {"type": "string", "title": "Second"},
                            "q1_other": {"type": "string", "title": "Other"},
                            "q1": {
                                "type": "string",
                                "title": "Pick a color",
                                "oneOf": [
                                    {"const": "1", "title": "(Recommended) Red"},
                                    {"const": "2", "title": "Blue"}
                                ]
                            }
                        }
                    }
                }
            }),
            &mut events,
        )
        .unwrap();
    let Some(Event::Elicitation(elicit)) = events.pop() else {
        panic!("expected elicitation event");
    };
    let names: Vec<_> = elicit.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["q1", "q1_other", "q2", "q10"]);
    assert!(matches!(
        &elicit.fields[0].kind,
        ElicitationFieldKind::SingleSelect(opts) if opts.len() == 2 && opts[0].value == "1"
    ));

    client
        .command(
            5,
            Command::AnswerElicitation {
                elicitation: elicit.id,
                response: ElicitationResponse::Accept(vec![(
                    "q1".into(),
                    ElicitationValue::String("2".into()),
                )]),
            },
            &mut events,
        )
        .unwrap();
    let reply = outbound.recv().unwrap();
    assert_eq!(reply["id"], "elicit-1");
    assert_eq!(reply["result"]["action"], "accept");
    assert_eq!(reply["result"]["content"]["q1"], "2");
    assert!(events.iter().any(|e| matches!(e, Event::ElicitationResolved(id) if *id == elicit.id)));
}

#[test]
fn history_list_and_session_load_replay_past_turns() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client
        .receive(
            json!({
                "id": 0,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": true,
                        "sessionCapabilities": {
                            "list": {},
                            "load": {},
                            "resume": {}
                        }
                    }
                }
            }),
            &mut events,
        )
        .unwrap();
    events.clear();

    client
        .command(2, Command::History(None), &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/list");
    assert_eq!(req["params"]["cwd"], "workspace");
    client
        .receive(
            json!({
                "id": 2,
                "result": {
                    "sessions": [
                        {"sessionId": "hist-1", "cwd": "workspace", "title": "Fix history panel"},
                        {"sessionId": "other-proj", "cwd": "elsewhere", "title": "Other"}
                    ],
                    "nextCursor": "page-2"
                }
            }),
            &mut events,
        )
        .unwrap();
    let Some(Event::History(2, sessions, Some(cursor))) = events.pop() else {
        panic!("expected history event");
    };
    assert_eq!(cursor, "page-2");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "hist-1");
    assert_eq!(sessions[0].title, "Fix history panel");

    // Load historical session; server streams updates before LoadSessionResponse.
    client
        .command(3, Command::Resume("hist-1".into()), &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/load");
    assert_eq!(req["params"]["sessionId"], "hist-1");
    assert_eq!(req["params"]["cwd"], "workspace");

    for update in [
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u1", "content": {"type": "text", "text": "Hello "}}),
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u1", "content": {"type": "text", "text": "agent"}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "tc1", "title": "Read README", "kind": "read", "status": "in_progress"}),
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc1", "status": "completed",
               "content": [{"type": "content", "content": {"type": "text", "text": "# Winc"}}]}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "a1", "content": {"type": "text", "text": "First reply"}}),
        json!({"sessionUpdate": "user_message_chunk", "messageId": "u2", "content": {"type": "text", "text": "Second question"}}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "a2", "content": {"type": "text", "text": "Second reply"}}),
        json!({"sessionUpdate": "usage_update", "used": 420, "size": 1048576}),
    ] {
        client
            .receive(
                json!({"method": "session/update", "params": {"sessionId": "hist-1", "update": update}}),
                &mut events,
            )
            .unwrap();
    }
    assert!(events.is_empty());

    client
        .receive(
            json!({
                "id": 3,
                "result": {
                    "modes": {
                        "currentModeId": "auto",
                        "availableModes": [{"id": "auto", "name": "Auto"}]
                    },
                    "_meta": {"antigravity": {"tier": {"name": "Google AI Pro"}}}
                }
            }),
            &mut events,
        )
        .unwrap();

    let Some(Event::SessionOpened(3, opened, turns, None)) = events.first() else {
        panic!("expected SessionOpened first, got {events:?}");
    };
    assert_eq!(opened.id, "hist-1");
    assert_eq!(opened.title, "Fix history panel");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].items.len(), 3);
    assert_eq!(turns[0].items[0].kind, ItemKind::User);
    assert_eq!(turns[0].items[0].text, "Hello agent");
    assert_eq!(turns[0].items[1].kind, ItemKind::Read);
    assert_eq!(turns[0].items[1].output, "# Winc");
    assert_eq!(turns[0].items[2].kind, ItemKind::Message);
    assert_eq!(turns[0].items[2].text, "First reply");
    assert_eq!(turns[1].items.len(), 2);
    assert_eq!(turns[1].items[0].text, "Second question");
    assert_eq!(turns[1].items[1].text, "Second reply");
    assert!(events.iter().any(|e| matches!(e, Event::Tokens(k, 420, 420, Some(1048576)) if k.session == "hist-1")));
    assert!(events.iter().any(|e| matches!(e, Event::SessionModes(s, _, Some(cur)) if s == "hist-1" && cur == "auto")));
    assert!(events.iter().any(|e| matches!(e, Event::AccountTier(Some(t)) if t == "Google AI Pro")));
}

#[test]
fn resume_falls_back_when_load_is_unsupported_and_surfaces_auth_required() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client
        .receive(
            json!({
                "id": 0,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": false,
                        "sessionCapabilities": {
                            "resume": {}
                        }
                    }
                }
            }),
            &mut events,
        )
        .unwrap();
    events.clear();

    // Without list capability, History returns empty immediately without wire request.
    client
        .command(2, Command::History(None), &mut events)
        .unwrap();
    assert!(outbound.try_recv().is_err());
    assert!(matches!(
        events.pop(),
        Some(Event::History(2, list, None)) if list.is_empty()
    ));

    // Resume uses session/resume when loadSession is false.
    client
        .command(3, Command::Resume("s-prev".into()), &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/resume");
    client
        .receive(
            json!({"id": 3, "error": {"code": -32000, "message": "Authentication required"}}),
            &mut events,
        )
        .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Ready {
            login_required: true
        }
    )));
}

#[test]
fn history_matches_windows_uri_cwd_sorts_by_updated_at_and_falls_back_to_resume() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from(r"C:\Users\hundo\code\Winc"), &wire);
    let mut events = Vec::new();
    client
        .receive(
            json!({
                "id": 0,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": true,
                        "load_session": true,
                        "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                        "sessionCapabilities": {"resume": {}, "close": {}, "load": {}, "list": {}, "additionalDirectories": {}},
                        "auth": {"logout": {}},
                        "_meta": {"antigravity": {"tier": true}}
                    },
                    "agentInfo": {"name": "Antigravity ACP Server", "version": "1.5.18"}
                }
            }),
            &mut events,
        )
        .unwrap();
    events.clear();

    client
        .command(2, Command::History(None), &mut events)
        .unwrap();
    let _ = outbound.recv().unwrap();
    client
        .receive(
            json!({
                "id": 2,
                "result": {
                    "sessions": [
                        {"sessionId": "old-win", "cwd": r"C:\Users\hundo\code\Winc", "title": "Old", "updatedAt": "2026-11-28T04:58:15Z"},
                        {"sessionId": "uri-slash", "cwd": "/C:/Users/hundo/code/Winc", "title": "Newest", "updatedAt": "2026-11-28T08:03:43Z"},
                        {"sessionId": "file-uri", "cwd": "file:///c:/Users/hundo/code/Winc/", "title": "Middle", "updatedAt": "2026-11-28T07:00:00Z"},
                        {"sessionId": "other", "cwd": "/C:/Users/hundo/code/Other", "title": "Other", "updatedAt": "2026-11-28T09:00:00Z"}
                    ]
                }
            }),
            &mut events,
        )
        .unwrap();
    let Some(Event::History(2, sessions, None)) = events.pop() else {
        panic!("expected history event");
    };
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["uri-slash", "file-uri", "old-win"]);

    // If session/load returns -32002 (session not found for replay), fall back to session/resume.
    client
        .command(3, Command::Resume("old-win".into()), &mut events)
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/load");
    client
        .receive(
            json!({"id": 3, "error": {"code": -32002, "message": "Session not found"}}),
            &mut events,
        )
        .unwrap();
    assert!(events.is_empty());
    let fallback = outbound.recv().unwrap();
    assert_eq!(fallback["method"], "session/resume");
    assert_eq!(fallback["id"], 3);
    assert_eq!(fallback["params"]["sessionId"], "old-win");
    client
        .receive(
            json!({"id": 3, "result": {"sessionId": "old-win"}}),
            &mut events,
        )
        .unwrap();
    assert!(matches!(
        events.first(),
        Some(Event::SessionOpened(3, session, turns, None))
            if session.id == "old-win" && session.title == "Old" && turns.is_empty()
    ));

    // Replayed tool_call with name fallback and rawOutput.output preserves title and output.
    events.clear();
    client
        .command(4, Command::Resume("uri-slash".into()), &mut events)
        .unwrap();
    let _ = outbound.recv().unwrap();
    for update in [
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Check files"}}),
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc1",
            "name": "run_command",
            "kind": "execute",
            "status": "completed",
            "rawOutput": {"output": "Command exited with code 0."}
        }),
    ] {
        client
            .receive(
                json!({"method": "session/update", "params": {"sessionId": "uri-slash", "update": update}}),
                &mut events,
            )
            .unwrap();
    }
    client
        .receive(json!({"id": 4, "result": {"sessionId": "uri-slash"}}), &mut events)
        .unwrap();
    let Some(Event::SessionOpened(4, _, turns, None)) = events.first() else {
        panic!("expected SessionOpened");
    };
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items.len(), 2);
    assert_eq!(turns[0].items[1].text, "run_command");
    assert_eq!(turns[0].items[1].output, "Command exited with code 0.");
}

#[test]
fn partial_config_option_updates_preserve_effort_config_id_and_emit_effort_changes() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client
        .receive(json!({"id":0,"result":{"protocolVersion":1}}), &mut events)
        .unwrap();
    client.command(2, Command::NewSession, &mut events).unwrap();
    let _ = outbound.recv().unwrap();
    client
        .receive(
            json!({
                "id": 2,
                "result": {
                    "sessionId": "s1",
                    "configOptions": [
                        {
                            "id": "model",
                            "category": "model",
                            "currentValue": "gemini-4-neon",
                            "options": [{"value": "gemini-4-neon", "name": "Gemini 4 Neon"}]
                        },
                        {
                            "id": "thought_level",
                            "category": "thought_level",
                            "currentValue": "high",
                            "options": [
                                {"value": "low", "name": "Low"},
                                {"value": "high", "name": "High"}
                            ]
                        },
                        {
                            "id": "mode",
                            "category": "mode",
                            "currentValue": "default",
                            "options": [
                                {"value": "default", "name": "Default"},
                                {"value": "auto", "name": "Auto"}
                            ]
                        }
                    ]
                }
            }),
            &mut events,
        )
        .unwrap();

    // A mode-only config_option_update must not wipe effort_config_id.
    events.clear();
    client
        .receive(
            json!({
                "method": "session/update",
                "params": {
                    "sessionId": "s1",
                    "update": {
                        "sessionUpdate": "config_option_update",
                        "configOptions": [
                            {
                                "id": "mode",
                                "category": "mode",
                                "currentValue": "auto",
                                "options": [
                                    {"value": "default", "name": "Default"},
                                    {"value": "auto", "name": "Auto"}
                                ]
                            }
                        ]
                    }
                }
            }),
            &mut events,
        )
        .unwrap();

    client
        .command(
            3,
            Command::SetEffort {
                session: "s1".into(),
                effort: "low".into(),
            },
            &mut events,
        )
        .unwrap();
    let req = outbound.recv().unwrap();
    assert_eq!(req["method"], "session/set_config_option");
    assert_eq!(req["params"]["configId"], "thought_level");
    assert_eq!(req["params"]["value"], "low");

    // A thought_level-only configOptions response updates current_effort and emits SessionModels.
    events.clear();
    client
        .receive(
            json!({
                "id": 3,
                "result": {
                    "configOptions": [
                        {
                            "id": "thought_level",
                            "category": "thought_level",
                            "currentValue": "low",
                            "options": [
                                {"value": "low", "name": "Low"},
                                {"value": "high", "name": "High"}
                            ]
                        }
                    ]
                }
            }),
            &mut events,
        )
        .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::SessionModels {
            session,
            current_model: Some(model),
            current_effort: Some(effort),
            ..
        } if session == "s1" && model == "gemini-4-neon" && effort == "low"
    )));
}

#[test]
fn available_commands_update_parses_commands_and_defers_during_load() {
    let (wire, outbound) = std::sync::mpsc::sync_channel(16);
    let mut client = AcpClient::new(PathBuf::from("workspace"), &wire);
    let mut events = Vec::new();
    client
        .receive(
            json!({
                "id": 0,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true}
                }
            }),
            &mut events,
        )
        .unwrap();
    events.clear();

    client.command(2, Command::NewSession, &mut events).unwrap();
    let _ = outbound.recv().unwrap();
    client
        .receive(json!({"id": 2, "result": {"sessionId": "s1"}}), &mut events)
        .unwrap();
    events.clear();

    client
        .receive(
            json!({
                "method": "session/update",
                "params": {
                    "sessionId": "s1",
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": [
                            {"name": "/plan", "description": "Create a plan", "input": {"hint": "task"}},
                            {"name": "grill-me", "description": "Design interview"},
                            {"name": "PLAN", "description": "Duplicate ignored"},
                            {"name": "invalid name", "description": "Whitespace ignored"}
                        ]
                    }
                }
            }),
            &mut events,
        )
        .unwrap();
    let Some(Event::SessionCommands(session, cmds)) = events.pop() else {
        panic!("expected SessionCommands event");
    };
    assert_eq!(session, "s1");
    assert_eq!(cmds.len(), 2);
    assert_eq!(cmds[0].name, "plan");
    assert_eq!(cmds[0].description, "Create a plan");
    assert_eq!(cmds[0].input_hint.as_deref(), Some("task"));
    assert_eq!(cmds[1].name, "grill-me");
    assert_eq!(cmds[1].input_hint, None);

    // During session/load, available_commands_update is deferred until SessionOpened.
    client
        .command(3, Command::Resume("hist-1".into()), &mut events)
        .unwrap();
    let _ = outbound.recv().unwrap();
    events.clear();
    client
        .receive(
            json!({
                "method": "session/update",
                "params": {
                    "sessionId": "hist-1",
                    "update": {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": [{"name": "compact", "description": "Compact context"}]
                    }
                }
            }),
            &mut events,
        )
        .unwrap();
    assert!(events.is_empty());
    client
        .receive(json!({"id": 3, "result": {"sessionId": "hist-1"}}), &mut events)
        .unwrap();
    assert!(matches!(events.first(), Some(Event::SessionOpened(3, ..))));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::SessionCommands(session, list)
            if session == "hist-1" && list.len() == 1 && list[0].name == "compact"
    )));
}
