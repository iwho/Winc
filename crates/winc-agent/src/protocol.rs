//! Stable v2 messages used by the Codex integration.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::*;

pub(super) const MAX_MESSAGE: usize = 256 * 1024;
pub(super) const MAX_OUTPUT: usize = 24 * 1024;
const MAX_REVIEW: usize = 24 * 1024;
const MAX_FILES: usize = 256;

pub(super) fn append(text: &mut String, value: &str, limit: usize) {
    let available = limit.saturating_sub(text.len());
    text.push_str(&value[..value.floor_char_boundary(available.min(value.len()))]);
}

fn bounded(value: &str, limit: usize) -> String {
    value[..value.floor_char_boundary(limit.min(value.len()))].to_owned()
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

fn changes(value: &Value) -> (Vec<PathBuf>, String) {
    let mut paths = Vec::new();
    let mut patch = String::new();
    for change in array(value).iter().take(MAX_FILES) {
        if let Some(path) = change["path"].as_str() {
            paths.push(PathBuf::from(path));
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
    (paths, patch)
}

fn item(value: &Value) -> Option<Item> {
    let id = string(&value["id"])?;
    let mut text = String::new();
    let mut output = String::new();
    let mut files = Vec::new();
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
            ItemKind::Message
        }
        "commandExecution" => {
            text = bounded(value["command"].as_str().unwrap_or_default(), MAX_OUTPUT);
            output = bounded(
                value["aggregatedOutput"].as_str().unwrap_or_default(),
                MAX_OUTPUT,
            );
            ItemKind::Command
        }
        "fileChange" => {
            (files, output) = changes(&value["changes"]);
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
            if let Some(result) = value.get("result").filter(|v| !v.is_null()) {
                output = bounded(&result.to_string(), MAX_OUTPUT);
            }
            ItemKind::Tool
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
    })
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

#[derive(Debug)]
pub(super) enum Reply {
    Initialize,
    Account,
    Session,
    VerifySession(String),
    Resume,
    Recent(Session),
    History,
    Models,
    Send(String),
    Stop,
}

struct PendingApproval {
    wire_id: Value,
    turn: TurnKey,
    kind: ApprovalKind,
    permissions: Value,
    reviewable: bool,
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
                model,
                effort,
            } if self.loaded.contains(&session) => (
                "turn/start",
                json!({
                    "threadId": session, "cwd": root, "model": model, "effort": effort,
                    "approvalPolicy": "on-request", "approvalsReviewer": "user",
                    "sandboxPolicy": {"type": "workspaceWrite", "networkAccess": false},
                    "input": [{"type": "text", "text": text, "text_elements": []}],
                }),
                Reply::Send(session),
            ),
            Command::Stop(turn) if self.loaded.contains(&turn.session) => (
                "turn/interrupt",
                json!({"threadId": turn.session, "turnId": turn.turn}),
                Reply::Stop,
            ),
            Command::Approve { approval, accept } => {
                let pending = self
                    .approvals
                    .get(&approval)
                    .ok_or(Failure::Rejected(-32602))?;
                if accept && !pending.reviewable {
                    return Err(Failure::Rejected(-32602));
                }
                let answer = match pending.kind {
                    ApprovalKind::Command | ApprovalKind::Files => {
                        json!({"decision": if accept {"accept"} else {"decline"}})
                    }
                    ApprovalKind::Permissions => {
                        let permissions = if accept {
                            pending.permissions.clone()
                        } else {
                            json!({})
                        };
                        json!({"permissions": permissions, "scope": "turn"})
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
                Event::SessionOpened(id, session, Vec::new())
            }
            Reply::Recent(session) => {
                let turns = array(&result["data"])
                    .iter()
                    .rev()
                    .filter_map(|value| turn(&session.id, value))
                    .collect();
                Event::SessionOpened(id, session, turns)
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
                    Event::TurnEnded(turn.key, turn.status)
                }
            }
            "item/started" | "item/completed" => {
                let (Some(key), Some(item)) = (key(params), item(&params["item"])) else {
                    return;
                };
                if method == "item/started" && item.kind == ItemKind::FileChange {
                    self.remember_file_review(&key, &item.id, &item.output);
                }
                Event::Item(key, item, method == "item/completed")
            }
            "item/fileChange/patchUpdated" => {
                let (Some(key), Some(id)) = (key(params), string(&params["itemId"])) else {
                    return;
                };
                let (files, patch) = changes(&params["changes"]);
                self.remember_file_review(&key, &id, &patch);
                Event::Files(key, id, files, patch)
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
                )
            }
            _ => return,
        };
        events.push(event);
    }

    fn remember_file_review(&mut self, turn: &TurnKey, item: &str, patch: &str) {
        if self.file_reviews.len() < 256 {
            self.file_reviews
                .insert((turn.clone(), item.to_owned()), patch.to_owned());
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
                let details = format!(
                    "{grant_root}\n{reason}\n{}",
                    patch.map_or("", String::as_str)
                );
                (
                    details,
                    patch.is_some_and(|patch| !patch.trim().is_empty() && patch.len() < MAX_OUTPUT)
                        || !grant_root.is_empty(),
                )
            }
            ApprovalKind::Permissions => (
                serde_json::to_string_pretty(permission).unwrap_or_default(),
                permission.is_object(),
            ),
        };
        let reviewable = reviewable && details.len() <= MAX_REVIEW;
        self.approvals.insert(
            local_id,
            PendingApproval {
                wire_id: id,
                turn: turn.clone(),
                kind,
                permissions: permission.clone(),
                reviewable,
            },
        );
        events.push(Event::Approval(Approval {
            id: local_id,
            turn,
            item,
            kind,
            details: bounded(&details, MAX_REVIEW),
            reviewable,
        }));
        None
    }

    pub(super) fn remove_approval(&mut self, id: u64) {
        self.approvals.remove(&id);
    }
}

fn same_root(a: &Path, b: &Path) -> bool {
    a == b || (cfg!(windows) && a.canonicalize().is_ok_and(|real| real == b))
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"id": id, "method": method, "params": params})
}
