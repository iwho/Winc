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
        |event| matches!(event, Event::SessionOpened(found, session, _) if found == id && session.id == "s"),
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
    wait(&mut provider, timeout, |event| {
        match event {
            Event::Delta(_, _, _, text) => streamed.push_str(&text),
            Event::TurnStarted(Some(_), key) => {
                running = Some(key);
                correlated = true;
            }
            Event::Approval(request) => {
                assert!(request.reviewable);
                approval = Some(request.id);
            }
            _ => {}
        }
        correlated && approval.is_some() && streamed == "你好"
    });
    provider
        .submit(Command::Approve {
            approval: approval.unwrap(),
            accept: false,
        })
        .unwrap();
    provider.submit(Command::Stop(running.unwrap())).unwrap();
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
    std::fs::create_dir(&root).unwrap();
    let executable = std::env::var_os("WINC_CODEX_TEST_BIN").map(PathBuf::from);
    let mut provider = CodexProvider::connect(root.clone(), executable, || {}).unwrap();
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
        Event::SessionOpened(found, opened, _) if id == found => {
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
        Event::SessionOpened(id, restored, history) if id == resume => {
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
