//! Stable v2 messages used by the Codex integration.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::*;

pub(super) const MAX_MESSAGE: usize = 256 * 1024;
pub(super) const MAX_OUTPUT: usize = 24 * 1024;
const MAX_REVIEW: usize = 24 * 1024;
const MAX_FILES: usize = 256;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn append(text: &mut String, value: &str, limit: usize) {
    text.push_str(crate::prefix(value, limit.saturating_sub(text.len())));
}

fn turn_input(text: String, image: Option<&[u8]>) -> Vec<Value> {
    let mut input = Vec::new();
    if !text.is_empty() || image.is_none() {
        input.push(json!({"type": "text", "text": text, "text_elements": []}));
    }
    if let Some(image) = image {
        const BASE64: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut url = String::with_capacity(22 + image.len().div_ceil(3) * 4);
        url.push_str("data:image/png;base64,");
        for chunk in image.chunks(3) {
            let bits = chunk
                .iter()
                .fold(0_u32, |bits, &byte| bits << 8 | u32::from(byte))
                << (8 * (3 - chunk.len()));
            for index in 0..4 {
                url.push(if index <= chunk.len() {
                    char::from(BASE64[((bits >> (18 - index * 6)) & 63) as usize])
                } else {
                    '='
                });
            }
        }
        input.push(json!({"type": "image", "url": url}));
    }
    input
}

fn bounded(value: &str, limit: usize) -> String {
    crate::prefix(value, limit).to_owned()
}

fn string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn key(params: &Value) -> Option<TurnKey> {
    Some(TurnKey {
        session: string(&params["threadId"])?,
        turn: string(&params["turnId"])?,
    })
}

fn session(thread: &Value) -> Option<Session> {
    Some(Session {
        id: string(&thread["id"])?,
        cwd: PathBuf::from(thread["cwd"].as_str()?),
        title: bounded(
            thread["name"]
                .as_str()
                .or(thread["preview"].as_str())
                .unwrap_or_default(),
            512,
        ),
        model: string(&thread["model"]),
        effort: string(&thread["reasoningEffort"]),
    })
}

fn model(value: &Value) -> Option<Model> {
    let id = string(&value["model"])?;
    Some(Model {
        name: string(&value["displayName"]).unwrap_or_else(|| id.clone()),
        id,
        efforts: array(&value["supportedReasoningEfforts"])
            .iter()
            .filter_map(|effort| string(&effort["reasoningEffort"]))
            .collect(),
        default_effort: string(&value["defaultReasoningEffort"]),
        is_default: value["isDefault"] == true,
    })
}

fn changes(value: &Value) -> (Vec<PathBuf>, String, Vec<Option<LineChanges>>) {
    let mut paths = Vec::new();
    let mut patch = String::new();
    let mut stats = Vec::new();
    for change in array(value).iter().take(MAX_FILES) {
        if let Some(path) = change["path"].as_str() {
            paths.push(PathBuf::from(path));
            stats.push(line_changes(change));
            append(&mut patch, path, MAX_OUTPUT);
            append(&mut patch, "\n", MAX_OUTPUT);
        }
        append(
            &mut patch,
            change["diff"].as_str().unwrap_or_default(),
            MAX_OUTPUT,
        );
        append(&mut patch, "\n", MAX_OUTPUT);
    }
    (paths, patch, stats)
}

fn line_changes(change: &Value) -> Option<LineChanges> {
    let patch = change["diff"].as_str()?;
    let mut stat = LineChanges::default();
    match change["kind"]["type"].as_str()? {
        "add" => stat.added = patch.lines().count(),
        "delete" => stat.removed = patch.lines().count(),
        "update" => {
            let mut in_hunk = false;
            for line in patch.lines() {
                match line.as_bytes().first() {
                    Some(b'@') if line.starts_with("@@ ") => in_hunk = true,
                    Some(b'+') if in_hunk => stat.added += 1,
                    Some(b'-') if in_hunk => stat.removed += 1,
                    Some(b' ' | b'\\') if in_hunk => {}
                    _ => in_hunk = false,
                }
            }
        }
        _ => return None,
    }
    Some(stat)
}

fn item(value: &Value) -> Option<Item> {
    let id = string(&value["id"])?;
    let mut text = String::new();
    let mut output = String::new();
    let mut files = Vec::new();
    let mut file_stats = Vec::new();
    let kind = match value["type"].as_str()? {
        "userMessage" => {
            for part in array(&value["content"]) {
                if let Some(part) = part["text"].as_str() {
                    append(&mut text, part, MAX_MESSAGE);
                }
            }
            ItemKind::User
        }
        "agentMessage" | "plan" => {
            text = bounded(value["text"].as_str().unwrap_or_default(), MAX_MESSAGE);
            if value["type"] == "plan" {
                ItemKind::Plan
            } else {
                ItemKind::Message
            }
        }
        "commandExecution" => {
            text = bounded(value["command"].as_str().unwrap_or_default(), MAX_OUTPUT);
            output = bounded(
                value["aggregatedOutput"].as_str().unwrap_or_default(),
                MAX_OUTPUT,
            );
            command_kind(&value["commandActions"], &text)
        }
        "fileChange" => {
            (files, output, file_stats) = changes(&value["changes"]);
            for path in &files {
                append(&mut text, &path.to_string_lossy(), MAX_OUTPUT);
                append(&mut text, "\n", MAX_OUTPUT);
            }
            ItemKind::FileChange
        }
        "mcpToolCall" | "dynamicToolCall" | "collabAgentToolCall" | "webSearch" => {
            text = bounded(
                value["tool"]
                    .as_str()
                    .or(value["query"].as_str())
                    .unwrap_or_default(),
                MAX_OUTPUT,
            );
            if let Some(server) = value["server"].as_str() {
                text = bounded(&format!("{server} / {text}"), MAX_OUTPUT);
            }
            if let Some(result) = [
                "error",
                "result",
                "agentsStates",
                "agentStatuses",
                "contentItems",
            ]
            .into_iter()
            .find_map(|name| value.get(name).filter(|v| !v.is_null()))
            {
                output = bounded(
                    &serde_json::to_string_pretty(result).unwrap_or_default(),
                    MAX_OUTPUT,
                );
            }
            if value["type"] == "collabAgentToolCall" {
                ItemKind::Subagent
            } else {
                ItemKind::Tool
            }
        }
        _ => return None,
    };
    Some(Item {
        id,
        kind,
        text,
        output,
        status: bounded(value["status"].as_str().unwrap_or_default(), 64),
        files,
        file_stats,
    })
}

fn command_kind(actions: &Value, command: &str) -> ItemKind {
    let actions = array(actions);
    if !actions.is_empty() && actions.iter().all(|action| action["type"] == "read") {
        return ItemKind::Read;
    }
    if !actions.is_empty()
        && actions
            .iter()
            .all(|action| matches!(action["type"].as_str(), Some("search" | "listFiles")))
    {
        return ItemKind::Search;
    }
    let mut words = command.split_whitespace();
    match (words.next(), words.next()) {
        (Some("cargo" | "go" | "npm" | "pnpm" | "yarn" | "mvn"), Some("test"))
        | (Some("pytest" | "vitest" | "jest"), _) => ItemKind::Test,
        _ => ItemKind::Command,
    }
}

fn turn(session: &str, value: &Value) -> Option<Turn> {
    let status = match value["status"].as_str()? {
        "inProgress" => TurnStatus::Running,
        "completed" => TurnStatus::Completed,
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed,
        _ => return None,
    };
    Some(Turn {
        key: TurnKey {
            session: session.to_owned(),
            turn: string(&value["id"])?,
        },
        status,
        items: array(&value["items"]).iter().filter_map(item).collect(),
    })
}

fn turns_page(session: &str, result: &Value) -> Option<(Vec<Turn>, Option<String>)> {
    let turns = result["data"]
        .as_array()?
        .iter()
        .rev()
        .filter_map(|value| turn(session, value))
        .collect();
    Some((turns, string(&result["nextCursor"])))
}

#[derive(Debug)]
pub(super) enum Reply {
    Initialize,
    Account,
    Session,
    VerifySession(String),
    Resume,
    Recent(Session),
    Older(String),
    History,
    Models,
    Send(String),
    Steer(TurnKey),
    Stop,
}

struct PendingApproval {
    wire_id: Value,
    expires: Instant,
    turn: TurnKey,
    kind: ApprovalKind,
    permissions: Value,
    reviewable: bool,
    decisions: Vec<ApprovalDecision>,
    file: Option<(String, String, String)>,
}

#[derive(Default)]
pub(super) struct Protocol {
    approvals: HashMap<u64, PendingApproval>,
    loaded: HashSet<String>,
    // Some servers send the file item before its approval request, others publish a patch update.
    file_reviews: HashMap<(TurnKey, String), String>,
}

impl Protocol {
    pub(super) fn command(
        &mut self,
        id: u64,
        cmd: Command,
        root: &Path,
    ) -> Result<(Value, Option<Reply>), Failure> {
        let (method, params, reply) = match cmd {
            Command::NewSession => (
                "thread/start",
                json!({"cwd": root, "approvalPolicy": "on-request", "approvalsReviewer": "user", "sandbox": "workspace-write"}),
                Reply::Session,
            ),
            Command::Resume(session) => (
                "thread/read",
                json!({"threadId": session, "includeTurns": false}),
                Reply::VerifySession(session),
            ),
            Command::Older { session, cursor } if self.loaded.contains(&session) => (
                "thread/turns/list",
                json!({"threadId": session, "cursor": cursor, "limit": 16, "sortDirection": "desc", "itemsView": "full"}),
                Reply::Older(session),
            ),
            Command::History(cursor) => (
                "thread/list",
                json!({"cwd": root, "limit": 25, "cursor": cursor}),
                Reply::History,
            ),
            Command::Models(cursor) => (
                "model/list",
                json!({"cursor": cursor, "includeHidden": false}),
                Reply::Models,
            ),
            Command::Send {
                session,
                text,
                image,
                model,
                effort,
                permissions,
            } if self.loaded.contains(&session) => {
                use crate::PermissionMode;
                let (approval, reviewer) = match permissions {
                    PermissionMode::Ask => ("on-request", "user"),
                    PermissionMode::AutoReview => ("on-request", "auto_review"),
                    PermissionMode::FullAccess => ("never", "user"),
                };
                let sandbox = if permissions == PermissionMode::FullAccess {
                    json!({"type": "dangerFullAccess"})
                } else {
                    json!({"type": "workspaceWrite", "networkAccess": false})
                };
                (
                    "turn/start",
                    json!({
                        "threadId": session, "cwd": root, "model": model, "effort": effort,
                        "approvalPolicy": approval, "approvalsReviewer": reviewer,
                        "sandboxPolicy": sandbox,
                        "input": turn_input(text, image.as_deref()),
                    }),
                    Reply::Send(session),
                )
            }
            Command::Steer { turn, text, image } if self.loaded.contains(&turn.session) => (
                "turn/steer",
                json!({
                    "threadId": turn.session, "expectedTurnId": turn.turn,
                    "input": turn_input(text, image.as_deref()),
                }),
                Reply::Steer(turn),
            ),
            Command::Stop(turn) if self.loaded.contains(&turn.session) => (
                "turn/interrupt",
                json!({"threadId": turn.session, "turnId": turn.turn}),
                Reply::Stop,
            ),
            Command::Approve { approval, decision } => {
                let pending = self
                    .approvals
                    .get(&approval)
                    .ok_or(Failure::Rejected(-32602))?;
                if Instant::now() >= pending.expires
                    || !pending.decisions.contains(&decision)
                    || decision.grants() && !pending.reviewable
                {
                    return Err(Failure::Rejected(-32602));
                }
                let answer = match pending.kind {
                    ApprovalKind::Command | ApprovalKind::Files => {
                        json!({"decision": decision.wire()})
                    }
                    ApprovalKind::Permissions => {
                        let permissions = if decision.grants() {
                            pending.permissions.clone()
                        } else {
                            json!({})
                        };
                        let scope = if decision == ApprovalDecision::AcceptForSession {
                            "session"
                        } else {
                            "turn"
                        };
                        json!({"permissions": permissions, "scope": scope})
                    }
                };
                return Ok((json!({"id": pending.wire_id, "result": answer}), None));
            }
            _ => return Err(Failure::Rejected(-32602)),
        };
        Ok((request(id, method, params), Some(reply)))
    }

    pub(super) fn reply(
        &mut self,
        id: u64,
        reply: Reply,
        result: &Value,
        root: &Path,
    ) -> Option<Event> {
        Some(match reply {
            Reply::Initialize => return None,
            Reply::Account => Event::Ready {
                login_required: result["requiresOpenaiAuth"] == true && result["account"].is_null(),
            },
            Reply::Session => {
                let session = self.opened(result, root)?;
                Event::SessionOpened(id, session, Vec::new(), None)
            }
            Reply::Recent(session) => {
                let (turns, next) = turns_page(&session.id, result)?;
                Event::SessionOpened(id, session, turns, next)
            }
            Reply::Older(session) => {
                let (turns, next) = turns_page(&session, result)?;
                Event::Older(id, session, turns, next)
            }
            Reply::VerifySession(_) | Reply::Resume => return None,
            Reply::History => {
                let sessions: Vec<_> = array(&result["data"])
                    .iter()
                    .filter_map(session)
                    .filter(|session| same_root(&session.cwd, root))
                    .collect();
                Event::History(id, sessions, string(&result["nextCursor"]))
            }
            Reply::Models => Event::Models(
                id,
                array(&result["data"]).iter().filter_map(model).collect(),
                string(&result["nextCursor"]),
            ),
            Reply::Send(session) => {
                let turn = turn(&session, &result["turn"])?;
                Event::TurnStarted(Some(id), turn.key)
            }
            Reply::Steer(turn) => {
                if result["turnId"].as_str() != Some(&turn.turn) {
                    return None;
                }
                Event::Steered(id, turn.session)
            }
            Reply::Stop => Event::Acknowledged(id),
        })
    }

    pub(super) fn opened(&mut self, result: &Value, root: &Path) -> Option<Session> {
        let mut session = session(&result["thread"])?;
        if !same_root(&session.cwd, root) {
            return None;
        }
        session.model = string(&result["model"]).or(session.model);
        session.effort = string(&result["reasoningEffort"]).or(session.effort);
        self.loaded.insert(session.id.clone());
        Some(session)
    }

    pub(super) fn verify_resume(
        &self,
        id: u64,
        thread: String,
        result: &Value,
        root: &Path,
    ) -> Result<Value, Failure> {
        let current = session(&result["thread"]).ok_or(Failure::Protocol)?;
        if current.id != thread || !same_root(&current.cwd, root) {
            return Err(Failure::Workspace);
        }
        Ok(request(
            id,
            "thread/resume",
            json!({
                "threadId": thread, "cwd": root, "excludeTurns": true,
                "approvalPolicy": "on-request", "approvalsReviewer": "user", "sandbox": "workspace-write",
            }),
        ))
    }

    pub(super) fn notification(&mut self, method: &str, params: &Value, events: &mut Vec<Event>) {
        if method == "serverRequest/resolved" {
            if let Some(id) = self
                .approvals
                .iter()
                .find_map(|(id, pending)| (pending.wire_id == params["requestId"]).then_some(*id))
            {
                self.approvals.remove(&id);
                events.push(Event::ApprovalResolved(id));
            }
            return;
        }
        let Some(session) = params["threadId"].as_str() else {
            return;
        };
        // Notifications may precede a start/resume response. Session IDs are still
        // carried on every event, and the editor only displays its loaded sessions.
        let event = match method {
            "thread/name/updated" => Event::SessionTitle(
                session.to_owned(),
                params["threadName"]
                    .as_str()
                    .map(|name| bounded(name, 1024)),
            ),
            "turn/started" | "turn/completed" => {
                let Some(turn) = turn(session, &params["turn"]) else {
                    return;
                };
                if method == "turn/started" {
                    Event::TurnStarted(None, turn.key)
                } else {
                    self.file_reviews.retain(|(key, _), _| key != &turn.key);
                    self.approvals.retain(|id, pending| {
                        if pending.turn == turn.key {
                            events.push(Event::ApprovalResolved(*id));
                            false
                        } else {
                            true
                        }
                    });
                    if let Some(message) = params["turn"]["error"]["message"].as_str() {
                        events.push(Event::Item(
                            turn.key.clone(),
                            Item {
                                id: "\0turn-error".into(),
                                kind: ItemKind::Error,
                                text: bounded(message, MAX_OUTPUT),
                                output: String::new(),
                                status: "failed".into(),
                                files: Vec::new(),
                                file_stats: Vec::new(),
                            },
                            true,
                        ));
                    }
                    Event::TurnEnded(turn.key, turn.status)
                }
            }
            "item/started" | "item/completed" => {
                let (Some(key), Some(item)) = (key(params), item(&params["item"])) else {
                    return;
                };
                if method == "item/started" && item.kind == ItemKind::FileChange {
                    self.remember_file_review(
                        &key,
                        &item.id,
                        &params["item"]["changes"],
                        &item.output,
                        events,
                    );
                }
                Event::Item(key, item, method == "item/completed")
            }
            "item/fileChange/patchUpdated" => {
                let (Some(key), Some(id)) = (key(params), string(&params["itemId"])) else {
                    return;
                };
                let (files, patch, _) = changes(&params["changes"]);
                self.remember_file_review(&key, &id, &params["changes"], &patch, events);
                Event::Files(key, id, files, patch)
            }
            "turn/plan/updated" => {
                let Some(turn) = key(params) else { return };
                let mut text = bounded(
                    params["explanation"].as_str().unwrap_or_default(),
                    MAX_OUTPUT,
                );
                for step in array(&params["plan"]) {
                    let marker = match step["status"].as_str() {
                        Some("completed") => "✓ ",
                        Some("inProgress") => "▸ ",
                        _ => "○ ",
                    };
                    if let Some(step) = step["step"].as_str() {
                        if !text.is_empty() {
                            append(&mut text, "\n", MAX_OUTPUT);
                        }
                        append(&mut text, marker, MAX_OUTPUT);
                        append(&mut text, step, MAX_OUTPUT);
                    }
                }
                Event::Item(
                    turn,
                    Item {
                        id: "\0turn-plan".into(),
                        kind: ItemKind::Plan,
                        text,
                        output: String::new(),
                        status: String::new(),
                        files: Vec::new(),
                        file_stats: Vec::new(),
                    },
                    false,
                )
            }
            "item/mcpToolCall/progress" => {
                let (Some(turn), Some(id), Some(message)) = (
                    key(params),
                    string(&params["itemId"]),
                    params["message"].as_str(),
                ) else {
                    return;
                };
                let mut output = bounded(message, MAX_OUTPUT);
                append(&mut output, "\n", MAX_OUTPUT);
                Event::Delta(turn, id, DeltaKind::Output, output)
            }
            "item/plan/delta" => {
                let (Some(turn), Some(id), Some(delta)) = (
                    key(params),
                    string(&params["itemId"]),
                    params["delta"].as_str(),
                ) else {
                    return;
                };
                Event::Delta(turn, id, DeltaKind::Plan, bounded(delta, MAX_MESSAGE))
            }
            "item/agentMessage/delta"
            | "item/commandExecution/outputDelta"
            | "item/fileChange/outputDelta" => {
                let (Some(key), Some(id), Some(delta)) = (
                    key(params),
                    string(&params["itemId"]),
                    params["delta"].as_str(),
                ) else {
                    return;
                };
                let (kind, limit) = if method == "item/agentMessage/delta" {
                    (DeltaKind::Message, MAX_MESSAGE)
                } else {
                    (DeltaKind::Output, MAX_OUTPUT)
                };
                if let Some(Event::Delta(previous, item, target, text)) = events.last_mut()
                    && *previous == key
                    && *item == id
                    && *target == kind
                    && text.len() + delta.len() <= limit
                {
                    text.push_str(delta);
                    return;
                }
                Event::Delta(key, id, kind, bounded(delta, limit))
            }
            "thread/tokenUsage/updated" => {
                let Some(key) = key(params) else { return };
                let usage = &params["tokenUsage"];
                Event::Tokens(
                    key,
                    usage["total"]["totalTokens"].as_u64().unwrap_or_default(),
                    usage["last"]["totalTokens"].as_u64().unwrap_or_default(),
                    usage["modelContextWindow"].as_u64(),
                )
            }
            _ => return,
        };
        events.push(event);
    }

    fn remember_file_review(
        &mut self,
        turn: &TurnKey,
        item: &str,
        changes: &Value,
        patch: &str,
        events: &mut Vec<Event>,
    ) {
        let file_updates = array(changes);
        let complete = !file_updates.is_empty()
            && file_updates.len() <= MAX_FILES
            && file_bytes(file_updates).is_some_and(|bytes| bytes <= MAX_OUTPUT)
            && !patch.trim().is_empty();
        let key = (turn.clone(), item.to_owned());
        if !complete {
            self.file_reviews.remove(&key);
        } else if self.file_reviews.len() < 256 || self.file_reviews.contains_key(&key) {
            self.file_reviews.insert(key, patch.to_owned());
        }
        for (&id, pending) in &mut self.approvals {
            let Some((target, root, reason)) = &pending.file else {
                continue;
            };
            if pending.turn != *turn || *target != item {
                continue;
            }
            let (details, reviewable) = file_approval(root, reason, complete.then_some(patch));
            pending.reviewable = reviewable;
            events.push(Event::Approval(Approval {
                id,
                turn: turn.clone(),
                item: item.to_owned(),
                kind: ApprovalKind::Files,
                details,
                reviewable,
                decisions: pending.decisions.clone(),
            }));
        }
    }

    pub(super) fn server_request(
        &mut self,
        local_id: u64,
        id: Value,
        method: &str,
        params: &Value,
        events: &mut Vec<Event>,
    ) -> Option<Value> {
        let kind = match method {
            "item/commandExecution/requestApproval" => ApprovalKind::Command,
            "item/fileChange/requestApproval" => ApprovalKind::Files,
            "item/permissions/requestApproval" => ApprovalKind::Permissions,
            _ => {
                return Some(
                    json!({"id": id, "error": {"code": -32601, "message": "Request unsupported by Winc"}}),
                );
            }
        };
        let (Some(turn), Some(item)) = (key(params), string(&params["itemId"])) else {
            return Some(
                json!({"id": id, "error": {"code": -32602, "message": "Invalid approval request"}}),
            );
        };
        if !self.loaded.contains(&turn.session) || self.approvals.len() >= 64 {
            return Some(
                json!({"id": id, "error": {"code": -32000, "message": "Approval unavailable"}}),
            );
        }
        let permission = &params["permissions"];
        let (details, reviewable) = match kind {
            ApprovalKind::Command => {
                let display = json!({
                    "command": params["command"],
                    "kind": params["kind"],
                    "cwd": params["cwd"],
                    "networkApprovalContext": params["networkApprovalContext"],
                    "reason": params["reason"],
                    "additionalPermissions": params["additionalPermissions"],
                });
                let reviewable =
                    params["command"].is_string() || params["networkApprovalContext"].is_object();
                (
                    serde_json::to_string_pretty(&display).unwrap_or_default(),
                    reviewable,
                )
            }
            ApprovalKind::Files => {
                let patch = self.file_reviews.get(&(turn.clone(), item.clone()));
                let grant_root = params["grantRoot"].as_str().unwrap_or_default();
                let reason = params["reason"].as_str().unwrap_or_default();
                file_approval(grant_root, reason, patch.map(String::as_str))
            }
            ApprovalKind::Permissions => (
                serde_json::to_string_pretty(permission).unwrap_or_default(),
                permission.is_object(),
            ),
        };
        let reviewable = reviewable && details.len() <= MAX_REVIEW;
        use ApprovalDecision::{Accept, AcceptForSession, Cancel, Decline};
        let decisions = match (kind, params["availableDecisions"].as_array()) {
            (ApprovalKind::Command, Some(values)) => values
                .iter()
                .filter_map(|value| {
                    [Accept, AcceptForSession, Decline, Cancel]
                        .into_iter()
                        .find(|decision| value.as_str() == Some(decision.wire()))
                })
                .collect::<Vec<_>>(),
            (ApprovalKind::Permissions, _) => vec![Accept, AcceptForSession, Decline],
            _ => vec![Accept, AcceptForSession, Decline, Cancel],
        };
        if decisions.is_empty()
            || !reviewable && !decisions.iter().any(|decision| !decision.grants())
        {
            return Some(
                json!({"id": id, "error": {"code": -32602, "message": "Approval cannot be reviewed"}}),
            );
        }
        self.approvals.insert(
            local_id,
            PendingApproval {
                wire_id: id,
                expires: Instant::now() + APPROVAL_TIMEOUT,
                turn: turn.clone(),
                kind,
                permissions: permission.clone(),
                reviewable,
                decisions: decisions.clone(),
                file: if kind == ApprovalKind::Files {
                    let root = params["grantRoot"].as_str().unwrap_or_default();
                    let reason = params["reason"].as_str().unwrap_or_default();
                    (root.len().saturating_add(reason.len()).saturating_add(2) <= MAX_REVIEW)
                        .then(|| (item.clone(), root.to_owned(), reason.to_owned()))
                } else {
                    None
                },
            },
        );
        events.push(Event::Approval(Approval {
            id: local_id,
            turn,
            item,
            kind,
            details: bounded(&details, MAX_REVIEW),
            reviewable,
            decisions,
        }));
        None
    }

    pub(super) fn expire_approvals(&mut self, now: Instant, events: &mut Vec<Event>) -> Vec<Value> {
        let mut replies = Vec::new();
        self.approvals.retain(|id, pending| {
            if now < pending.expires {
                return true;
            }
            replies.push(json!({
                "id": pending.wire_id,
                "error": {"code": -32000, "message": "Winc approval expired"},
            }));
            events.push(Event::ApprovalExpired(*id));
            false
        });
        replies
    }

    pub(super) fn remove_approval(&mut self, id: u64) {
        self.approvals.remove(&id);
    }
}

fn file_bytes(changes: &[Value]) -> Option<usize> {
    changes.iter().try_fold(0usize, |total, change| {
        let path = change["path"].as_str()?;
        let diff = change["diff"].as_str()?;
        Some(
            total
                .saturating_add(path.len())
                .saturating_add(diff.len())
                .saturating_add(2),
        )
    })
}

fn file_approval(root: &str, reason: &str, patch: Option<&str>) -> (String, bool) {
    let details = format!("{root}\n{reason}\n{}", patch.unwrap_or_default());
    let complete = patch.is_some() || !root.is_empty();
    let reviewable = complete && details.len() <= MAX_REVIEW;
    (bounded(&details, MAX_REVIEW), reviewable)
}

fn same_root(a: &Path, b: &Path) -> bool {
    a == b || (cfg!(windows) && winc_config::files::canonicalize(a).is_ok_and(|real| real == b))
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"id": id, "method": method, "params": params})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_start_applies_the_selected_permissions() {
        use crate::PermissionMode;

        let mut protocol = Protocol::default();
        protocol.loaded.insert("thread".into());
        for (permissions, approval, reviewer, sandbox) in [
            (PermissionMode::Ask, "on-request", "user", "workspaceWrite"),
            (
                PermissionMode::AutoReview,
                "on-request",
                "auto_review",
                "workspaceWrite",
            ),
            (
                PermissionMode::FullAccess,
                "never",
                "user",
                "dangerFullAccess",
            ),
        ] {
            let (request, _) = protocol
                .command(
                    1,
                    Command::Send {
                        session: "thread".into(),
                        text: "Hello".into(),
                        image: None,
                        model: None,
                        effort: None,
                        permissions,
                    },
                    Path::new("/workspace"),
                )
                .unwrap();
            let params = &request["params"];
            assert_eq!(params["approvalPolicy"], approval);
            assert_eq!(params["approvalsReviewer"], reviewer);
            assert_eq!(params["sandboxPolicy"]["type"], sandbox);
            if sandbox == "workspaceWrite" {
                assert_eq!(params["sandboxPolicy"]["networkAccess"], false);
            }
        }
    }

    #[test]
    fn expired_approvals_invalidate_the_original_wire_request() {
        let mut protocol = Protocol::default();
        protocol.loaded.insert("thread".into());
        let mut events = Vec::new();
        let params = json!({
            "threadId": "thread", "turnId": "turn", "itemId": "command", "command": "echo example",
        });
        assert!(
            protocol
                .server_request(
                    1,
                    json!("server-id"),
                    "item/commandExecution/requestApproval",
                    &params,
                    &mut events,
                )
                .is_none()
        );
        let replies = protocol.expire_approvals(Instant::now() + APPROVAL_TIMEOUT, &mut events);
        assert_eq!(replies[0]["id"], "server-id");
        assert!(replies[0].get("result").is_none());
        assert!(matches!(events.last(), Some(Event::ApprovalExpired(1))));
        assert!(
            protocol
                .command(
                    2,
                    Command::Approve {
                        approval: 1,
                        decision: ApprovalDecision::Accept,
                    },
                    Path::new("/project")
                )
                .is_err()
        );
    }

    #[test]
    fn failed_turn_keeps_the_diagnostic_and_resolves_pending_review() {
        let mut protocol = Protocol::default();
        protocol.loaded.insert("thread".into());
        let mut events = Vec::new();
        let approval = json!({
            "threadId": "thread", "turnId": "turn", "itemId": "command", "command": "echo example",
        });
        protocol.server_request(
            1,
            json!(7),
            "item/commandExecution/requestApproval",
            &approval,
            &mut events,
        );
        events.clear();
        protocol.notification("turn/completed", &json!({
            "threadId": "thread",
            "turn": {"id":"turn", "status":"failed", "error":{"message":"tool connection lost"}, "items":[]},
        }), &mut events);
        assert!(matches!(events.as_slice(), [
            Event::ApprovalResolved(1),
            Event::Item(_, item, true),
            Event::TurnEnded(_, TurnStatus::Failed),
        ] if item.kind == ItemKind::Error && item.text == "tool connection lost"));
    }

    #[test]
    fn line_counts_read_the_agent_patch_and_ignore_its_headers() {
        let change = json!({
            "path": "src/lib.rs", "kind": {"type": "update"},
            "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+++ prefix\n context\n\nMoved to: src/main.rs",
        });
        assert_eq!(
            line_changes(&change),
            Some(LineChanges {
                added: 2,
                removed: 1
            })
        );
        let change = json!({"path":"new.txt","kind":{"type":"add"},"diff":"one\ntwo\n"});
        assert_eq!(
            line_changes(&change),
            Some(LineChanges {
                added: 2,
                removed: 0
            })
        );
    }

    #[test]
    fn turn_input_includes_png_as_an_image() {
        let input = turn_input("Describe this".into(), Some(&[0x89, b'P', b'N', b'G']));
        assert_eq!(input[0]["text"], "Describe this");
        assert_eq!(input[1]["type"], "image");
        assert_eq!(input[1]["url"], "data:image/png;base64,iVBORw==");
        assert_eq!(turn_input(String::new(), Some(&[0])).len(), 1);
    }
}
