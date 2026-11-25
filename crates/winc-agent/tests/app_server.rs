//! Transport integration tests and an opt-in live Codex smoke test.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use winc_agent::{AgentProvider, CodexProvider, Command, Event, TurnStatus};

fn wait(provider: &mut CodexProvider, timeout: Duration, mut inspect: impl FnMut(Event) -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        for event in provider.drain() {
            assert!(
                !matches!(event, Event::Disconnected(_) | Event::RequestFailed(..)),
                "{event:?}"
            );
            if inspect(event) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for app-server"
        );
        thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(unix)]
#[test]
fn handles_approval_and_interrupt_over_stdio() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("winc-agent-mock-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let script = root.join("codex");
    fs::write(&script, include_str!("mock-app-server.sh")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut provider = CodexProvider::connect(root.clone(), Some(script), || {}).unwrap();
    let timeout = Duration::from_secs(5);
    wait(&mut provider, timeout, |event| {
        matches!(
            event,
            Event::Ready {
                login_required: false
            }
        )
    });
    let id = provider.submit(Command::NewSession).unwrap();
    wait(
        &mut provider,
        timeout,
        |event| matches!(event, Event::SessionOpened(found, session, _, _) if found == id && session.id == "s"),
    );
    provider
        .submit(Command::Send {
            session: "s".into(),
            text: "你好".into(),
            model: None,
            effort: None,
        })
        .unwrap();
    let mut streamed = String::new();
    let mut running = None;
    let mut correlated = false;
    let mut approval = None;
    let mut output = String::new();
    let mut tools = Vec::new();
    wait(&mut provider, timeout, |event| {
        match event {
            Event::Delta(_, id, winc_agent::DeltaKind::Output, text) if id == "read" => {
                output.push_str(&text);
            }
            Event::Delta(_, _, winc_agent::DeltaKind::Message, text) => streamed.push_str(&text),
            Event::Item(_, item, _) => tools.push(item.kind),
            Event::TurnStarted(Some(_), key) => {
                running = Some(key);
                correlated = true;
            }
            Event::Approval(request) => {
                assert!(request.reviewable);
                assert_eq!(
                    request.decisions,
                    [
                        winc_agent::ApprovalDecision::Decline,
                        winc_agent::ApprovalDecision::Cancel
                    ]
                );
                approval = Some(request.id);
            }
            _ => {}
        }
        correlated && approval.is_some() && streamed == "你好"
    });
    assert_eq!(output, "one\nthree\n");
    assert_eq!(
        tools,
        [
            winc_agent::ItemKind::Read,
            winc_agent::ItemKind::Search,
            winc_agent::ItemKind::Plan
        ]
    );
    provider
        .submit(Command::Approve {
            approval: approval.unwrap(),
            decision: winc_agent::ApprovalDecision::Decline,
        })
        .unwrap();
    let mut file_approval = None;
    wait(&mut provider, timeout, |event| match event {
        Event::Approval(request)
            if request.kind == winc_agent::ApprovalKind::Files && request.reviewable =>
        {
            assert!(request.details.contains("result.txt"));
            file_approval = Some(request.id);
            true
        }
        _ => false,
    });
    provider
        .submit(Command::Approve {
            approval: file_approval.unwrap(),
            decision: winc_agent::ApprovalDecision::Accept,
        })
        .unwrap();
    let turn = running.unwrap();
    let steer = provider
        .submit(Command::Steer {
            turn: turn.clone(),
            text: "Continue".into(),
        })
        .unwrap();
    wait(
        &mut provider,
        timeout,
        |event| matches!(event, Event::Steered(id, session) if id == steer && session == "s"),
    );
    provider.submit(Command::Stop(turn)).unwrap();
    wait(&mut provider, timeout, |event| {
        matches!(event, Event::TurnEnded(_, TurnStatus::Interrupted))
    });
    drop(provider);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "uses the locally signed-in Codex account and a real model turn"]
fn live_codex_turn() {
    let root = std::env::temp_dir().join(format!("winc-agent-live-{}", std::process::id()));
    let executable = std::env::var_os("WINC_CODEX_TEST_BIN").map(PathBuf::from);
    let mut provider = CodexProvider::connect_standalone(root.clone(), executable, || {}).unwrap();
    let timeout = Duration::from_secs(40);
    wait(&mut provider, timeout, |event| match event {
        Event::Ready { login_required } => {
            assert!(!login_required, "run codex login first");
            true
        }
        _ => false,
    });
    let mut model = None;
    provider.submit(Command::Models(None)).unwrap();
    wait(&mut provider, timeout, |event| match event {
        Event::Models(_, models, _) => {
            let entry = models
                .iter()
                .find(|m| m.is_default)
                .or(models.first())
                .expect("model list empty");
            model = Some(entry.id.clone());
            true
        }
        _ => false,
    });
    let mut session = None;
    let id = provider.submit(Command::NewSession).unwrap();
    wait(&mut provider, timeout, |event| match event {
        Event::SessionOpened(found, opened, _, _) if id == found => {
            session = Some(opened.id);
            true
        }
        _ => false,
    });
    let session = session.unwrap();
    provider
        .submit(Command::Send {
            session: session.clone(),
            text: "Create result.txt in the current directory containing WINC_OK on its own line, and then reply WINC_OK. Only modify that file.".into(),
            model,
            effort: None,
        })
        .unwrap();
    let mut streamed = String::new();
    wait(&mut provider, timeout, |event| match event {
        Event::Delta(_, _, winc_agent::DeltaKind::Message, text) => {
            streamed.push_str(&text);
            false
        }
        Event::TurnEnded(_, status) => {
            assert_eq!(status, TurnStatus::Completed);
            assert!(
                streamed.contains("WINC_OK"),
                "expected the streamed smoke-test response"
            );
            true
        }
        _ => false,
    });
    assert_eq!(
        std::fs::read_to_string(root.join("result.txt"))
            .unwrap()
            .trim(),
        "WINC_OK"
    );
    provider.submit(Command::History(None)).unwrap();
    wait(&mut provider, timeout, |event| match event {
        Event::History(_, sessions, _) => {
            assert!(sessions.iter().any(|entry| entry.id == session));
            true
        }
        _ => false,
    });
    let resume = provider.submit(Command::Resume(session.clone())).unwrap();
    wait(&mut provider, timeout, |event| match event {
        Event::SessionOpened(id, restored, history, _) if id == resume => {
            assert_eq!(restored.id, session);
            assert!(
                history
                    .iter()
                    .any(|turn| turn.status == TurnStatus::Completed)
            );
            true
        }
        _ => false,
    });
    drop(provider);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn an_exiting_server_reports_disconnect_and_can_be_replaced() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("winc-agent-exit-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let script = root.join("codex");
    fs::write(&script, "#!/bin/sh\nread -r request\nexit 1\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    for _ in 0..2 {
        let mut provider =
            CodexProvider::connect(root.clone(), Some(script.clone()), || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if provider
                .drain()
                .into_iter()
                .any(|event| matches!(event, Event::Disconnected(winc_agent::Failure::Exited)))
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(15));
        }
    }
    fs::remove_dir_all(root).unwrap();
}
