//! Supervised Codex stdio transport. No pipe or process operation runs on the UI thread.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod shutdown;
use shutdown::Shutdown;

use crate::protocol::{Protocol, Reply, request};
use crate::*;

const MAX_FRAME: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BATCH_INTERVAL: Duration = Duration::from_millis(12);
const MAX_PENDING: usize = 64;

type Job = (u64, Command);

/// A lazily launched connection to the locally installed Codex App Server.
pub struct CodexProvider {
    commands: SyncSender<Job>,
    events: Receiver<Vec<Event>>,
    wake_pending: Arc<AtomicBool>,
    shutdown: Arc<Shutdown>,
    next_id: u64,
}

impl CodexProvider {
    /// Starts the supervisor. Path discovery, workspace validation, and launch are
    /// performed in the background. Dropping the handle requests child teardown.
    ///
    /// # Errors
    /// The supervisor thread could not be created.
    pub fn connect(
        root: PathBuf,
        executable: Option<PathBuf>,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let (commands, jobs) = mpsc::sync_channel(32);
        let (out, events) = mpsc::sync_channel(4);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let wake_flag = Arc::clone(&wake_pending);
        let shutdown = Arc::new(Shutdown::default());
        let process_shutdown = Arc::clone(&shutdown);
        thread::Builder::new()
            .name("winc-codex".into())
            .spawn(move || {
                let publish = |events: Vec<Event>| {
                    if events.is_empty() {
                        return Ok(());
                    }
                    // Notify before waiting for capacity too: a sleeping UI cannot
                    // drain a full queue until it has received a wakeup.
                    match out.try_send(events) {
                        Ok(()) => {}
                        Err(TrySendError::Full(events)) => {
                            out.send(events).map_err(|_| ())?;
                        }
                        Err(TrySendError::Disconnected(_)) => return Err(()),
                    }
                    if !wake_flag.swap(true, Ordering::AcqRel) {
                        wake();
                    }
                    Ok(())
                };
                if let Err(failure) =
                    supervise(&root, executable, jobs, &process_shutdown, &publish)
                {
                    let _ = publish(vec![Event::Disconnected(failure)]);
                }
            })?;
        Ok(Self {
            commands,
            events,
            wake_pending,
            shutdown,
            next_id: 2,
        })
    }
}

impl Drop for CodexProvider {
    fn drop(&mut self) {
        self.shutdown.stop();
    }
}

impl AgentProvider for CodexProvider {
    fn submit(&mut self, command: Command) -> Result<u64, SubmitError> {
        if matches!(&command, Command::Send { text, .. } if text.len() > MAX_PROMPT) {
            return Err(SubmitError::PromptTooLarge);
        }
        let id = self.next_id;
        self.commands
            .try_send((id, command))
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Busy,
                TrySendError::Disconnected(_) => SubmitError::Disconnected,
            })?;
        self.next_id += 1;
        Ok(id)
    }

    fn drain(&mut self) -> Vec<Event> {
        self.wake_pending.store(false, Ordering::Release);
        self.events.try_iter().take(4).flatten().collect()
    }
}

struct ProcessGuard<'a>(Child, &'a Shutdown);

impl Drop for ProcessGuard<'_> {
    fn drop(&mut self) {
        self.1.release();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn supervise(
    root: &Path,
    executable: Option<PathBuf>,
    jobs: Receiver<Job>,
    shutdown: &Shutdown,
    publish: &impl Fn(Vec<Event>) -> Result<(), ()>,
) -> Result<(), Failure> {
    let root = winc_config::files::canonicalize(root).map_err(|_| Failure::Workspace)?;
    if root.to_str().is_none() || !root.is_dir() {
        return Err(Failure::Workspace);
    }
    let mut process = Process::new(codex_path(executable)?);
    process
        .arg("app-server")
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let spawned = process.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Failure::ExecutableNotFound,
        _ => Failure::Start,
    })?;
    let mut child = ProcessGuard(spawned, shutdown);
    shutdown.register(&child.0).map_err(|_| Failure::Start)?;

    let stdin = child.0.stdin.take().ok_or(Failure::Start)?;
    let stdout = child.0.stdout.take().ok_or(Failure::Start)?;
    let mut stderr = child.0.stderr.take().ok_or(Failure::Start)?;
    let (wire_out, outgoing) = mpsc::sync_channel::<Value>(32);
    let (incoming, wire_in) = mpsc::sync_channel(8);
    let incoming_writer = incoming.clone();
    let mut workers = Vec::new();
    let result = (|| {
        workers.push(
            thread::Builder::new()
                .name("winc-codex-stdin".into())
                .spawn(move || {
                    let mut pipe = BufWriter::new(stdin);
                    for value in outgoing {
                        if serde_json::to_writer(&mut pipe, &value).is_err()
                            || pipe.write_all(b"\n").and_then(|()| pipe.flush()).is_err()
                        {
                            let _ = incoming_writer.send(Err(Failure::Exited));
                            break;
                        }
                    }
                })
                .map_err(|_| Failure::Start)?,
        );
        workers.push(
            thread::Builder::new()
                .name("winc-codex-stdout".into())
                .spawn(move || {
                    let mut pipe = BufReader::new(stdout);
                    let mut line = Vec::new();
                    loop {
                        let frame = read_frame(&mut pipe, &mut line);
                        let done = frame.is_err();
                        if incoming.send(frame).is_err() || done {
                            break;
                        }
                    }
                })
                .map_err(|_| Failure::Start)?,
        );
        workers.push(
            thread::Builder::new()
                .name("winc-codex-stderr".into())
                .spawn(move || {
                    // Drain diagnostics separately. Raw stderr can contain local data
                    // or secrets, so it is neither persisted nor sent to the UI.
                    let _ = io::copy(&mut stderr, &mut io::sink());
                })
                .map_err(|_| Failure::Start)?,
        );

        let mut client = Client::new(root, &wire_out);
        client.send_request(
            0,
            request(
                0,
                "initialize",
                json!({
                    "clientInfo": {"name": "winc", "title": "Winc", "version": "1"},
                }),
            ),
            Reply::Initialize,
        )?;
        loop {
            let mut events = Vec::new();
            let deadline = Instant::now() + BATCH_INTERVAL;
            for _ in 0..32 {
                match jobs.try_recv() {
                    Ok((id, cmd)) if client.ready => client.command(id, cmd, &mut events)?,
                    Ok((id, _)) => events.push(Event::RequestFailed(id, Failure::Rejected(-32000))),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            let mut failure = None;
            for _ in 0..64 {
                match wire_in.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Ok(value)) => client.receive(value, &mut events)?,
                    Ok(Err(error)) => {
                        failure = Some(error);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        failure = Some(Failure::Exited);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            client.expire(&mut events)?;
            if publish(events).is_err() {
                return Ok(());
            }
            if let Some(error) = failure {
                return Err(error);
            }
        }
    })();
    drop(child);
    drop(wire_out);
    drop(wire_in);
    // The supervisor reaps the process and joins the pipe workers outside the event loop.
    for worker in workers {
        if cfg!(any(unix, windows)) || worker.is_finished() {
            let _ = worker.join();
        }
    }
    result
}

fn codex_path(configured: Option<PathBuf>) -> Result<PathBuf, Failure> {
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    if let Some(configured) = configured {
        if !configured.is_absolute() {
            return Err(Failure::ExecutableNotFound);
        }
        return winc_config::files::canonicalize(configured)
            .map_err(|_| Failure::ExecutableNotFound);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let locations = std::env::split_paths(&path).filter(|path| path.is_absolute());
    #[cfg(target_os = "macos")]
    let locations = locations.chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    locations
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .ok_or(Failure::ExecutableNotFound)
}

fn read_frame(pipe: &mut impl BufRead, line: &mut Vec<u8>) -> Result<Value, Failure> {
    line.clear();
    loop {
        let input = pipe.fill_buf().map_err(|_| Failure::Exited)?;
        if input.is_empty() {
            return Err(if line.is_empty() {
                Failure::Exited
            } else {
                Failure::Protocol
            });
        }
        let end = input.iter().position(|&b| b == b'\n');
        let count = end.map_or(input.len(), |pos| pos + 1);
        if line.len() + count > MAX_FRAME {
            return Err(Failure::Protocol);
        }
        line.extend_from_slice(&input[..count]);
        pipe.consume(count);
        if end.is_some() {
            return serde_json::from_slice(line).map_err(|_| Failure::Protocol);
        }
    }
}

struct Client<'a> {
    root: PathBuf,
    wire: &'a SyncSender<Value>,
    requests: HashMap<u64, (Instant, Reply)>,
    protocol: Protocol,
    next_approval: u64,
    ready: bool,
}

impl<'a> Client<'a> {
    fn new(root: PathBuf, wire: &'a SyncSender<Value>) -> Self {
        Self {
            root,
            wire,
            requests: HashMap::new(),
            protocol: Protocol::default(),
            next_approval: 0,
            ready: false,
        }
    }

    fn write(&self, value: Value) -> Result<(), Failure> {
        self.wire.try_send(value).map_err(|_| Failure::Exited)
    }

    fn send_request(&mut self, id: u64, value: Value, reply: Reply) -> Result<(), Failure> {
        self.write(value)?;
        self.requests
            .insert(id, (Instant::now() + REQUEST_TIMEOUT, reply));
        Ok(())
    }

    fn command(&mut self, id: u64, cmd: Command, events: &mut Vec<Event>) -> Result<(), Failure> {
        if self.requests.len() >= MAX_PENDING {
            events.push(Event::RequestFailed(id, Failure::Rejected(-32000)));
            return Ok(());
        }
        let approval = match &cmd {
            Command::Approve { approval, .. } => Some(*approval),
            _ => None,
        };
        match self.protocol.command(id, cmd, &self.root) {
            Ok((message, Some(reply))) => self.send_request(id, message, reply)?,
            Ok((message, None)) => {
                self.write(message)?;
                if let Some(approval) = approval {
                    self.protocol.remove_approval(approval);
                    events.push(Event::ApprovalResolved(approval));
                }
                events.push(Event::Acknowledged(id));
            }
            Err(error) => events.push(Event::RequestFailed(id, error)),
        }
        Ok(())
    }

    fn receive(&mut self, mut value: Value, events: &mut Vec<Event>) -> Result<(), Failure> {
        if let Some(method) = value["method"].as_str() {
            if value["id"].is_string() || value["id"].as_i64().is_some() {
                let id = value["id"].clone();
                let local = self.next_approval;
                self.next_approval += 1;
                if let Some(reply) =
                    self.protocol
                        .server_request(local, id, method, &value["params"], events)
                {
                    self.write(reply)?;
                }
            } else {
                self.protocol.notification(method, &value["params"], events);
            }
            return Ok(());
        }

        let Some(id) = value["id"].as_u64() else {
            return Ok(());
        };
        let Some((_, reply)) = self.requests.remove(&id) else {
            return Ok(());
        };
        if let Some(code) = value["error"]["code"].as_i64() {
            if id < 2 {
                return Err(Failure::Rejected(code));
            }
            events.push(Event::RequestFailed(id, Failure::Rejected(code)));
            return Ok(());
        }
        let result = value["result"].take();
        match reply {
            Reply::Initialize if result.is_object() => {
                self.write(json!({"method": "initialized"}))?;
                self.send_request(
                    1,
                    request(1, "account/read", json!({"refreshToken": false})),
                    Reply::Account,
                )?;
            }
            Reply::Initialize => return Err(Failure::Protocol),
            Reply::VerifySession(thread) => {
                match self.protocol.verify_resume(id, thread, &result, &self.root) {
                    Ok(resume) => self.send_request(id, resume, Reply::Resume)?,
                    Err(error) => events.push(Event::RequestFailed(id, error)),
                }
            }
            Reply::Resume => {
                if let Some(session) = self.protocol.opened(&result, &self.root) {
                    self.send_request(id, request(id, "thread/turns/list", json!({
                        "threadId": session.id, "limit": 16, "sortDirection": "desc", "itemsView": "full",
                    })), Reply::Recent(session))?;
                } else {
                    events.push(Event::RequestFailed(id, Failure::Workspace));
                }
            }
            Reply::Account if !result["requiresOpenaiAuth"].is_boolean() => {
                return Err(Failure::Protocol);
            }
            reply => {
                if matches!(reply, Reply::Account) {
                    self.ready = true;
                }
                events.push(
                    self.protocol
                        .reply(id, reply, &result, &self.root)
                        .unwrap_or(Event::RequestFailed(id, Failure::Protocol)),
                );
            }
        }
        Ok(())
    }

    fn expire(&mut self, events: &mut Vec<Event>) -> Result<(), Failure> {
        let now = Instant::now();
        let mut fatal = false;
        self.requests.retain(|&id, (deadline, reply)| {
            if now < *deadline {
                return true;
            }
            if id < 2 || matches!(reply, Reply::Send(_)) {
                // A lost start acknowledgement can leave an active writing
                // turn. Tear down this connection instead of resending.
                fatal = true;
            }
            events.push(Event::RequestFailed(id, Failure::Timeout));
            false
        });
        if fatal { Err(Failure::Timeout) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_utf8_and_refuses_truncated_or_oversized_frames() {
        let mut scratch = Vec::new();
        let mut input = BufReader::with_capacity(5, "{\"text\":\"你好\"}\r\n{}".as_bytes());
        assert_eq!(
            read_frame(&mut input, &mut scratch).unwrap()["text"],
            "你好"
        );
        assert_eq!(read_frame(&mut input, &mut scratch), Err(Failure::Protocol));
        let oversized = vec![b' '; MAX_FRAME + 1];
        assert_eq!(
            read_frame(&mut &oversized[..], &mut scratch),
            Err(Failure::Protocol)
        );
    }
}
