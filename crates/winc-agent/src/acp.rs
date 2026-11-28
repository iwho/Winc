use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    CancelNotification, ContentBlock, ImageContent, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    PromptResponse, ResumeSessionRequest, StopReason, TextContent,
};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::codex::WireClient;
use crate::*;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TEXT: usize = 256 * 1024;
const MAX_TOOL_OUTPUT: usize = 24 * 1024;
const ACP_AUTH_REQUIRED: i64 = -32000;

enum Request {
    Session,
    Load(String),
    Resume(String),
    History,
    Prompt(String),
    Config(String),
    SetMode(String, String),
    SetModel(String, String),
}

#[derive(Default)]
struct SessionConfigState {
    model_config_id: Option<String>,
    effort_config_id: Option<String>,
    mode_config_id: Option<String>,
    has_legacy_modes: bool,
    has_legacy_models: bool,
    modes: Vec<ModeOption>,
    current_mode: Option<String>,
    models: Vec<Model>,
    current_model: Option<String>,
    current_effort: Option<String>,
}

struct PendingElicitation {
    session: String,
    wire_id: Value,
    deadline: Instant,
}

struct ActiveTurn {
    key: TurnKey,
    message: Option<String>,
    next_message: usize,
    tools: HashMap<String, Item>,
    cancel_deadline: Option<Instant>,
}

impl ActiveTurn {
    fn new(key: TurnKey) -> Self {
        Self {
            key,
            message: None,
            next_message: 0,
            tools: HashMap::new(),
            cancel_deadline: None,
        }
    }
}

struct PendingApproval {
    session: String,
    wire_id: Value,
    choices: Vec<(ApprovalDecision, String)>,
    deadline: Instant,
    reviewable: bool,
}

struct LoadingSession {
    request_id: u64,
    turns: Vec<Turn>,
    in_user_phase: bool,
    user_message_id: Option<String>,
    agent_message_id: Option<String>,
    next_local_id: usize,
    tool_indices: HashMap<String, (usize, usize)>,
    deferred_events: Vec<Event>,
}

impl LoadingSession {
    fn new(request_id: u64) -> Self {
        Self {
            request_id,
            turns: Vec::new(),
            in_user_phase: false,
            user_message_id: None,
            agent_message_id: None,
            next_local_id: 0,
            tool_indices: HashMap::new(),
            deferred_events: Vec::new(),
        }
    }

    fn ensure_turn(&mut self, session: &str) -> usize {
        if self.turns.is_empty() {
            self.turns.push(Turn {
                key: TurnKey {
                    session: session.to_owned(),
                    turn: "history-1".into(),
                },
                status: TurnStatus::Completed,
                items: Vec::new(),
            });
        }
        self.turns.len() - 1
    }

    fn record_update(&mut self, session: &str, update: &Value) {
        match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => {
                let Some(text) = content_text(&update["content"]) else {
                    return;
                };
                let msg_id = update["messageId"].as_str().filter(|id| !id.is_empty());
                let append_existing = self.in_user_phase
                    && !self.turns.is_empty()
                    && match (self.user_message_id.as_deref(), msg_id) {
                        (Some(prev), Some(curr)) => prev == curr,
                        (None, None) => true,
                        _ => false,
                    };
                if append_existing {
                    if let Some(turn) = self.turns.last_mut()
                        && let Some(item) = turn.items.last_mut()
                    {
                        item.text
                            .push_str(prefix(text, MAX_TEXT.saturating_sub(item.text.len())));
                    }
                } else {
                    let turn_num = self.turns.len() + 1;
                    let item_id = match msg_id {
                        Some(id) => format!("user:{id}"),
                        None => {
                            self.next_local_id += 1;
                            format!("user-local:{}", self.next_local_id)
                        }
                    };
                    self.turns.push(Turn {
                        key: TurnKey {
                            session: session.to_owned(),
                            turn: format!("history-{turn_num}"),
                        },
                        status: TurnStatus::Completed,
                        items: vec![make_item(
                            item_id,
                            ItemKind::User,
                            prefix(text, MAX_TEXT).into(),
                        )],
                    });
                    self.in_user_phase = true;
                    self.user_message_id = msg_id.map(str::to_owned);
                    self.agent_message_id = None;
                }
            }
            Some("agent_thought_chunk") => {
                self.in_user_phase = false;
            }
            Some("agent_message_chunk") => {
                let Some(text) = content_text(&update["content"]) else {
                    return;
                };
                self.in_user_phase = false;
                let msg_id = update["messageId"].as_str().filter(|id| !id.is_empty());
                let turn_idx = self.ensure_turn(session);
                let append_existing = self.turns[turn_idx]
                    .items
                    .last()
                    .is_some_and(|item| item.kind == ItemKind::Message)
                    && match (self.agent_message_id.as_deref(), msg_id) {
                        (Some(prev), Some(curr)) => prev == curr,
                        (None, None) => true,
                        _ => false,
                    };
                if append_existing {
                    let item = self.turns[turn_idx].items.last_mut().expect("message item");
                    item.text
                        .push_str(prefix(text, MAX_TEXT.saturating_sub(item.text.len())));
                } else {
                    let item_id = match msg_id {
                        Some(id) => format!("message:{id}"),
                        None => {
                            self.next_local_id += 1;
                            format!("message-local:{}", self.next_local_id)
                        }
                    };
                    self.agent_message_id = msg_id.map(str::to_owned);
                    self.turns[turn_idx].items.push(make_item(
                        item_id,
                        ItemKind::Message,
                        prefix(text, MAX_TEXT).into(),
                    ));
                }
            }
            Some("tool_call" | "tool_call_update") => {
                let Some(tool_id) = update["toolCallId"].as_str().filter(|id| !id.is_empty())
                else {
                    return;
                };
                self.in_user_phase = false;
                self.agent_message_id = None;
                let item = if let Some(&(t_idx, i_idx)) = self.tool_indices.get(tool_id) {
                    &mut self.turns[t_idx].items[i_idx]
                } else {
                    let t_idx = self.ensure_turn(session);
                    if self.turns[t_idx].items.len() >= 256 {
                        return;
                    }
                    let i_idx = self.turns[t_idx].items.len();
                    self.turns[t_idx].items.push(make_item(
                        tool_key(tool_id),
                        ItemKind::Tool,
                        String::new(),
                    ));
                    self.tool_indices.insert(tool_id.to_owned(), (t_idx, i_idx));
                    &mut self.turns[t_idx].items[i_idx]
                };
                apply_tool_update(item, update);
            }
            Some("plan") => {
                if let Some(text) = format_plan_entries(update) {
                    self.in_user_phase = false;
                    self.agent_message_id = None;
                    let t_idx = self.ensure_turn(session);
                    if let Some(existing) = self.turns[t_idx]
                        .items
                        .iter_mut()
                        .find(|item| item.id == "plan")
                    {
                        existing.text = text;
                    } else {
                        self.turns[t_idx]
                            .items
                            .push(make_item("plan".into(), ItemKind::Plan, text));
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> (Vec<Turn>, Vec<Event>) {
        for turn in &mut self.turns {
            for item in &mut turn.items {
                if matches!(item.status.as_str(), "pending" | "inProgress") {
                    item.status.clear();
                }
            }
        }
        (self.turns, self.deferred_events)
    }
}

pub(crate) struct AcpClient<'a> {
    root: PathBuf,
    wire: &'a SyncSender<Value>,
    initialize_by: Instant,
    ready: bool,
    images: bool,
    can_list: bool,
    can_load: bool,
    can_resume: bool,
    requests: HashMap<u64, (Instant, Request)>,
    sessions: HashSet<String>,
    titles: HashMap<String, String>,
    loading: HashMap<String, LoadingSession>,
    configs: HashMap<String, SessionConfigState>,
    turns: HashMap<String, ActiveTurn>,
    approvals: HashMap<u64, PendingApproval>,
    next_approval: u64,
    elicitations: HashMap<u64, PendingElicitation>,
    next_elicitation: u64,
}

impl<'a> AcpClient<'a> {
    pub(crate) fn new(root: PathBuf, wire: &'a SyncSender<Value>) -> Self {
        Self {
            root,
            wire,
            initialize_by: Instant::now() + CONTROL_TIMEOUT,
            ready: false,
            images: false,
            can_list: false,
            can_load: false,
            can_resume: false,
            requests: HashMap::new(),
            sessions: HashSet::new(),
            titles: HashMap::new(),
            loading: HashMap::new(),
            configs: HashMap::new(),
            turns: HashMap::new(),
            approvals: HashMap::new(),
            next_approval: 0,
            elicitations: HashMap::new(),
            next_elicitation: 0,
        }
    }

    pub(crate) fn initialize(&self) -> Result<(), Failure> {
        let mut params = InitializeRequest::new(ProtocolVersion::V1);
        params.client_info = Some(Implementation::new("winc", env!("CARGO_PKG_VERSION")));
        let mut params = serde_json::to_value(params).map_err(|_| Failure::Protocol)?;
        params["clientCapabilities"] = json!({"elicitation": {"form": {}}});
        self.write(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":params}))
    }

    fn write(&self, value: Value) -> Result<(), Failure> {
        self.wire.try_send(value).map_err(|_| Failure::Exited)
    }

    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        request: Request,
    ) -> Result<(), Failure> {
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        self.requests
            .insert(id, (Instant::now() + CONTROL_TIMEOUT, request));
        Ok(())
    }

    fn permission_response(&self, id: &Value, outcome: Value) -> Result<(), Failure> {
        self.write(json!({"jsonrpc":"2.0","id":id,"result":{"outcome":outcome}}))
    }

    fn cancel_approvals(&mut self, session: &str, events: &mut Vec<Event>) -> Result<(), Failure> {
        let keys: Vec<_> = self
            .approvals
            .iter()
            .filter_map(|(&id, approval)| (approval.session == session).then_some(id))
            .collect();
        for id in keys {
            let pending = self.approvals.remove(&id).expect("pending approval");
            self.permission_response(&pending.wire_id, json!({"outcome":"cancelled"}))?;
            events.push(Event::ApprovalResolved(id));
        }
        Ok(())
    }

    fn cancel_elicitations(
        &mut self,
        session: &str,
        events: &mut Vec<Event>,
    ) -> Result<(), Failure> {
        let keys: Vec<_> = self
            .elicitations
            .iter()
            .filter_map(|(&id, item)| (item.session == session).then_some(id))
            .collect();
        for id in keys {
            let pending = self.elicitations.remove(&id).expect("pending elicitation");
            self.elicitation_response(&pending.wire_id, json!({"action": "cancel"}))?;
            events.push(Event::ElicitationResolved(id));
        }
        Ok(())
    }

    fn elicitation_response(&self, id: &Value, result: Value) -> Result<(), Failure> {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn elicitation(
        &mut self,
        wire_id: Value,
        params: &Value,
        events: &mut Vec<Event>,
    ) -> Result<(), Failure> {
        let Some(session) = params["sessionId"].as_str().filter(|id| !id.is_empty()) else {
            return self.elicitation_response(&wire_id, json!({"action": "decline"}));
        };
        if params["mode"].as_str() != Some("form") {
            return self.elicitation_response(&wire_id, json!({"action": "decline"}));
        }
        let fields = parse_elicitation_fields(&params["requestedSchema"]);
        if fields.is_empty() {
            return self.elicitation_response(&wire_id, json!({"action": "decline"}));
        }
        let message = prefix(params["message"].as_str().unwrap_or_default(), MAX_TEXT).to_owned();
        let id = self.next_elicitation;
        self.next_elicitation += 1;
        self.elicitations.insert(
            id,
            PendingElicitation {
                session: session.into(),
                wire_id,
                deadline: Instant::now() + Duration::from_secs(300),
            },
        );
        events.push(Event::Elicitation(Elicitation {
            id,
            session: session.into(),
            message,
            fields,
        }));
        Ok(())
    }

    fn apply_session_config(
        &mut self,
        session: &str,
        payload: &Value,
        events: &mut Vec<Event>,
    ) {
        let cfg = self.configs.entry(session.into()).or_default();
        let mut saw_config_mode = false;
        let mut saw_config_model = false;

        if let Some(options) = payload["configOptions"].as_array() {
            let mut mode_opt = None;
            let mut model_opt = None;
            let mut effort_opt = None;
            for option in options {
                let Some(id) = option["id"].as_str() else {
                    continue;
                };
                let category = option["category"].as_str();
                match category {
                    Some("mode") => mode_opt = Some((id, option)),
                    Some("model") => model_opt = Some((id, option)),
                    Some("thought_level") => effort_opt = Some((id, option)),
                    None if id == "mode" => mode_opt = Some((id, option)),
                    None if id == "model" => model_opt = Some((id, option)),
                    None if id == "thought_level" => effort_opt = Some((id, option)),
                    _ => {}
                }
            }

            if let Some((id, option)) = mode_opt {
                saw_config_mode = true;
                cfg.mode_config_id = Some(id.to_owned());
                cfg.modes = parse_select_options(option)
                    .into_iter()
                    .map(|(value, name, description)| ModeOption {
                        id: value,
                        name,
                        description,
                    })
                    .collect();
                cfg.current_mode = option["currentValue"].as_str().map(str::to_owned);
                events.push(Event::SessionModes(
                    session.into(),
                    cfg.modes.clone(),
                    cfg.current_mode.clone(),
                ));
            }

            let effort_state = if let Some((id, option)) = effort_opt {
                cfg.effort_config_id = Some(id.to_owned());
                let list = parse_select_options(option)
                    .into_iter()
                    .map(|(value, _, _)| value)
                    .collect::<Vec<_>>();
                let current = option["currentValue"].as_str().map(str::to_owned);
                Some((list, current))
            } else {
                if model_opt.is_some() {
                    cfg.effort_config_id = None;
                }
                None
            };

            if let Some((id, option)) = model_opt {
                saw_config_model = true;
                cfg.model_config_id = Some(id.to_owned());
                let current_model = option["currentValue"].as_str().map(str::to_owned);
                let (efforts, current_effort) = effort_state.unwrap_or_default();
                cfg.current_model.clone_from(&current_model);
                cfg.current_effort.clone_from(&current_effort);
                cfg.models = parse_select_options(option)
                    .into_iter()
                    .map(|(value, name, _)| {
                        let active = current_model.as_deref() == Some(value.as_str());
                        Model {
                            id: value,
                            name,
                            efforts: if active { efforts.clone() } else { Vec::new() },
                            default_effort: if active { current_effort.clone() } else { None },
                            is_default: false,
                        }
                    })
                    .collect();
                events.push(Event::SessionModels {
                    session: session.into(),
                    models: cfg.models.clone(),
                    current_model,
                    current_effort,
                });
            } else if let Some((efforts, current_effort)) = effort_state {
                saw_config_model = true;
                cfg.current_effort.clone_from(&current_effort);
                for model in &mut cfg.models {
                    let active = cfg.current_model.as_deref() == Some(model.id.as_str());
                    if active {
                        model.efforts.clone_from(&efforts);
                        model.default_effort.clone_from(&current_effort);
                    }
                }
                if !cfg.models.is_empty() {
                    events.push(Event::SessionModels {
                        session: session.into(),
                        models: cfg.models.clone(),
                        current_model: cfg.current_model.clone(),
                        current_effort,
                    });
                }
            }
        }

        if !saw_config_mode && let Some(modes) = payload["modes"]["availableModes"].as_array() {
            cfg.has_legacy_modes = true;
            cfg.modes = modes
                .iter()
                .filter_map(|mode| {
                    let id = mode["id"].as_str()?;
                    let name = mode["name"].as_str().unwrap_or(id);
                    let description = mode["description"].as_str().unwrap_or_default();
                    Some(ModeOption {
                        id: prefix(id, 128).into(),
                        name: prefix(name, 128).into(),
                        description: prefix(description, 512).into(),
                    })
                })
                .collect();
            cfg.current_mode = payload["modes"]["currentModeId"]
                .as_str()
                .map(str::to_owned);
            if !cfg.modes.is_empty() {
                events.push(Event::SessionModes(
                    session.into(),
                    cfg.modes.clone(),
                    cfg.current_mode.clone(),
                ));
            }
        }

        if !saw_config_model && let Some(models) = payload["models"]["availableModels"].as_array() {
            cfg.has_legacy_models = true;
            cfg.current_model = payload["models"]["currentModelId"]
                .as_str()
                .map(str::to_owned);
            cfg.current_effort = None;
            cfg.models = models
                .iter()
                .filter_map(|model| {
                    let id = model["modelId"].as_str()?;
                    let name = model["name"].as_str().unwrap_or(id);
                    Some(Model {
                        id: prefix(id, 128).into(),
                        name: prefix(name, 256).into(),
                        efforts: Vec::new(),
                        default_effort: None,
                        is_default: false,
                    })
                })
                .collect();
            if !cfg.models.is_empty() {
                events.push(Event::SessionModels {
                    session: session.into(),
                    models: cfg.models.clone(),
                    current_model: cfg.current_model.clone(),
                    current_effort: None,
                });
            }
        }
    }

    fn permission(
        &mut self,
        wire_id: Value,
        params: &Value,
        events: &mut Vec<Event>,
    ) -> Result<(), Failure> {
        let Some(session) = params["sessionId"].as_str() else {
            return self.permission_response(&wire_id, json!({"outcome":"cancelled"}));
        };
        let Some(turn) = self
            .turns
            .get(session)
            .filter(|turn| turn.cancel_deadline.is_none())
        else {
            return self.permission_response(&wire_id, json!({"outcome":"cancelled"}));
        };
        let Some(tool_id) = params["toolCall"]["toolCallId"].as_str() else {
            return self.permission_response(&wire_id, json!({"outcome":"cancelled"}));
        };
        let mut choices = Vec::new();
        if let Some(options) = params["options"].as_array() {
            for option in options {
                let decision = match option["kind"].as_str() {
                    Some("allow_once") => ApprovalDecision::Accept,
                    Some("allow_always") => ApprovalDecision::AcceptForSession,
                    Some("reject_once") => ApprovalDecision::Decline,
                    _ => continue,
                };
                if let Some(id) = option["optionId"].as_str()
                    && !choices.iter().any(|(kind, _)| *kind == decision)
                {
                    choices.push((decision, id.to_owned()));
                }
            }
        }
        let tool = &params["toolCall"];
        let name = tool["title"]
            .as_str()
            .or_else(|| turn.tools.get(tool_id).map(|item| item.text.as_str()))
            .unwrap_or_default();
        let detail = ["rawInput", "content", "locations"]
            .into_iter()
            .filter_map(|field| {
                (!tool[field].is_null())
                    .then(|| serde_json::to_string_pretty(&tool[field]).unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join("\n");
        let details = format!("{name}\n{detail}");
        let reviewable = !name.trim().is_empty() && details.len() <= MAX_TEXT;
        let id = self.next_approval;
        self.next_approval += 1;
        let decisions = choices
            .iter()
            .map(|(decision, _)| *decision)
            .chain(std::iter::once(ApprovalDecision::Cancel))
            .collect();
        self.approvals.insert(
            id,
            PendingApproval {
                session: session.into(),
                wire_id,
                choices,
                deadline: Instant::now() + Duration::from_secs(300),
                reviewable,
            },
        );
        events.push(Event::Approval(Approval {
            id,
            turn: turn.key.clone(),
            item: tool_key(tool_id),
            kind: ApprovalKind::Permissions,
            details: prefix(&details, MAX_TEXT).into(),
            reviewable,
            decisions,
        }));
        Ok(())
    }

    fn update(&mut self, params: &Value, events: &mut Vec<Event>) {
        let Some(session) = params["sessionId"].as_str() else {
            return;
        };
        if let Some(loading) = self.loading.get(session)
            && let Some((deadline, _)) = self.requests.get_mut(&loading.request_id)
        {
            *deadline = Instant::now() + CONTROL_TIMEOUT;
        }
        let update = &params["update"];
        let is_loading = self.loading.contains_key(session);
        match update["sessionUpdate"].as_str() {
            Some("session_info_update") => {
                if let Some(title) = update["title"].as_str() {
                    let trimmed = title.trim();
                    let bounded: String = prefix(trimmed, 1024).into();
                    if !bounded.is_empty() {
                        self.titles.insert(session.into(), bounded.clone());
                    }
                    if !is_loading {
                        events.push(Event::SessionTitle(session.into(), Some(bounded)));
                    }
                }
                return;
            }
            Some("config_option_update") => {
                let mut config_events = Vec::new();
                self.apply_session_config(session, update, &mut config_events);
                if let Some(loading) = self.loading.get_mut(session) {
                    loading.deferred_events.extend(config_events);
                } else {
                    events.extend(config_events);
                }
                return;
            }
            Some("current_mode_update") => {
                if let Some(mode_id) = update["currentModeId"].as_str() {
                    let cfg = self.configs.entry(session.into()).or_default();
                    cfg.current_mode = Some(mode_id.to_owned());
                    let event = Event::SessionModes(
                        session.into(),
                        cfg.modes.clone(),
                        cfg.current_mode.clone(),
                    );
                    if let Some(loading) = self.loading.get_mut(session) {
                        loading.deferred_events.push(event);
                    } else {
                        events.push(event);
                    }
                }
                return;
            }
            Some("available_commands_update") => {
                if let Some(entries) = update["availableCommands"].as_array() {
                    let commands = parse_available_commands(entries);
                    let event = Event::SessionCommands(session.into(), commands);
                    if let Some(loading) = self.loading.get_mut(session) {
                        loading.deferred_events.push(event);
                    } else {
                        events.push(event);
                    }
                }
                return;
            }
            Some("usage_update") => {
                let mut usage_events = Vec::new();
                if let (Some(used), Some(size)) = (update["used"].as_u64(), update["size"].as_u64())
                {
                    let key = self.turns.get(session).map_or_else(
                        || TurnKey {
                            session: session.into(),
                            turn: String::new(),
                        },
                        |turn| turn.key.clone(),
                    );
                    usage_events.push(Event::Tokens(key, used, used, Some(size)));
                }
                if let (Some(amount), Some(currency)) = (
                    update["cost"]["amount"].as_f64(),
                    update["cost"]["currency"].as_str(),
                ) {
                    usage_events.push(Event::SessionCost(
                        session.into(),
                        amount,
                        prefix(currency, 32).into(),
                    ));
                }
                if let Some(loading) = self.loading.get_mut(session) {
                    loading.deferred_events.extend(usage_events);
                } else {
                    events.extend(usage_events);
                }
                return;
            }
            _ => {}
        }
        if let Some(loading) = self.loading.get_mut(session) {
            loading.record_update(session, update);
            return;
        }
        let Some(turn) = self.turns.get_mut(session) else {
            return;
        };
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                let Some(text) = content_text(&update["content"]) else {
                    return;
                };
                let id = if let Some(id) = update["messageId"].as_str() {
                    format!("message:{id}")
                } else {
                    turn.message
                        .get_or_insert_with(|| {
                            turn.next_message += 1;
                            format!("message-local:{}", turn.next_message)
                        })
                        .clone()
                };
                events.push(Event::Delta(
                    turn.key.clone(),
                    id,
                    DeltaKind::Message,
                    prefix(text, MAX_TEXT).into(),
                ));
            }
            Some("tool_call" | "tool_call_update") => {
                let Some(id) = update["toolCallId"].as_str() else {
                    return;
                };
                turn.message = None;
                if !turn.tools.contains_key(id) && turn.tools.len() >= 128 {
                    return;
                }
                let item = turn
                    .tools
                    .entry(id.into())
                    .or_insert_with(|| make_item(tool_key(id), ItemKind::Tool, String::new()));
                apply_tool_update(item, update);
                let done = matches!(item.status.as_str(), "completed" | "failed");
                events.push(Event::Item(turn.key.clone(), item.clone(), done));
            }
            Some("plan") => {
                if let Some(text) = format_plan_entries(update) {
                    events.push(Event::Item(
                        turn.key.clone(),
                        make_item("plan".into(), ItemKind::Plan, text),
                        false,
                    ));
                }
            }
            _ => {}
        }
    }
}

impl WireClient for AcpClient<'_> {
    fn ready(&self) -> bool {
        self.ready
    }

    fn command(
        &mut self,
        id: u64,
        command: Command,
        events: &mut Vec<Event>,
    ) -> Result<(), Failure> {
        match command {
            Command::NewSession if self.requests.len() < 64 => {
                self.request(
                    id,
                    "session/new",
                    json!(NewSessionRequest::new(self.root.clone())),
                    Request::Session,
                )?;
            }
            Command::Send {
                session,
                text,
                image,
                ..
            } if self.sessions.contains(&session)
                && !self.turns.contains_key(&session)
                && self.requests.len() < 64
                && (image.is_none() || self.images) =>
            {
                let mut prompt = vec![ContentBlock::Text(TextContent::new(text.clone()))];
                if let Some(image) = image {
                    prompt.push(ContentBlock::Image(ImageContent::new(
                        base64::engine::general_purpose::STANDARD.encode(image),
                        "image/png",
                    )));
                }
                self.request(
                    id,
                    "session/prompt",
                    json!(PromptRequest::new(session.clone(), prompt)),
                    Request::Prompt(session.clone()),
                )?;
                let key = TurnKey {
                    session: session.clone(),
                    turn: id.to_string(),
                };
                self.turns.insert(session, ActiveTurn::new(key.clone()));
                events.push(Event::TurnStarted(Some(id), key.clone()));
                events.push(Event::Item(
                    key,
                    make_item("user".into(), ItemKind::User, text),
                    true,
                ));
            }
            Command::Stop(key)
                if self
                    .turns
                    .get(&key.session)
                    .is_some_and(|turn| turn.key == key) =>
            {
                self.write(json!({"jsonrpc":"2.0","method":"session/cancel","params":CancelNotification::new(key.session.clone())}))?;
                self.turns
                    .get_mut(&key.session)
                    .expect("active turn")
                    .cancel_deadline = Some(Instant::now() + CONTROL_TIMEOUT);
                self.cancel_approvals(&key.session, events)?;
                self.cancel_elicitations(&key.session, events)?;
                events.push(Event::Acknowledged(id));
            }
            Command::Approve { approval, decision } => {
                let Some(pending) = self.approvals.get(&approval) else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32602)));
                    return Ok(());
                };
                if decision == ApprovalDecision::Cancel {
                    let session = pending.session.clone();
                    if let Some(turn) = self.turns.get(&session) {
                        return self.command(id, Command::Stop(turn.key.clone()), events);
                    }
                }
                let option = pending
                    .choices
                    .iter()
                    .find(|(choice, _)| *choice == decision)
                    .filter(|_| !decision.grants() || pending.reviewable);
                if let Some((_, option)) = option {
                    self.permission_response(
                        &pending.wire_id,
                        json!({"outcome":"selected","optionId":option}),
                    )?;
                    self.approvals.remove(&approval);
                    events.push(Event::ApprovalResolved(approval));
                    events.push(Event::Acknowledged(id));
                } else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32602)));
                }
            }
            Command::SetMode { session, mode }
                if self.sessions.contains(&session) && self.requests.len() < 64 =>
            {
                let cfg = self.configs.get(&session);
                if let Some(config_id) = cfg.and_then(|c| c.mode_config_id.clone()) {
                    self.request(
                        id,
                        "session/set_config_option",
                        json!({"sessionId": session, "configId": config_id, "value": mode}),
                        Request::Config(session),
                    )?;
                } else if cfg.is_some_and(|c| c.has_legacy_modes) {
                    self.request(
                        id,
                        "session/set_mode",
                        json!({"sessionId": session, "modeId": mode}),
                        Request::SetMode(session, mode),
                    )?;
                } else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32601)));
                }
            }
            Command::SetModel { session, model }
                if self.sessions.contains(&session) && self.requests.len() < 64 =>
            {
                let cfg = self.configs.get(&session);
                if let Some(config_id) = cfg.and_then(|c| c.model_config_id.clone()) {
                    self.request(
                        id,
                        "session/set_config_option",
                        json!({"sessionId": session, "configId": config_id, "value": model}),
                        Request::Config(session),
                    )?;
                } else if cfg.is_some_and(|c| c.has_legacy_models) {
                    self.request(
                        id,
                        "session/set_model",
                        json!({"sessionId": session, "modelId": model}),
                        Request::SetModel(session, model),
                    )?;
                } else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32601)));
                }
            }
            Command::SetEffort { session, effort }
                if self.sessions.contains(&session) && self.requests.len() < 64 =>
            {
                if let Some(config_id) = self
                    .configs
                    .get(&session)
                    .and_then(|c| c.effort_config_id.clone())
                {
                    self.request(
                        id,
                        "session/set_config_option",
                        json!({"sessionId": session, "configId": config_id, "value": effort}),
                        Request::Config(session),
                    )?;
                } else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32601)));
                }
            }
            Command::AnswerElicitation {
                elicitation,
                response,
            } => {
                let Some(pending) = self.elicitations.remove(&elicitation) else {
                    events.push(Event::RequestFailed(id, Failure::Rejected(-32602)));
                    return Ok(());
                };
                let wire_result = match response {
                    ElicitationResponse::Accept(entries) => {
                        let mut content = serde_json::Map::new();
                        for (key, value) in entries {
                            let val = match value {
                                ElicitationValue::String(s) => Value::String(s),
                                ElicitationValue::Strings(list) => Value::Array(
                                    list.into_iter().map(Value::String).collect(),
                                ),
                                ElicitationValue::Bool(b) => Value::Bool(b),
                                ElicitationValue::Integer(n) => json!(n),
                                ElicitationValue::Number(n) => json!(n),
                            };
                            content.insert(key, val);
                        }
                        json!({"action": "accept", "content": content})
                    }
                    ElicitationResponse::Decline => json!({"action": "decline"}),
                    ElicitationResponse::Cancel => json!({"action": "cancel"}),
                };
                self.elicitation_response(&pending.wire_id, wire_result)?;
                events.push(Event::ElicitationResolved(elicitation));
                events.push(Event::Acknowledged(id));
            }
            Command::Resume(session) if self.sessions.contains(&session) => {
                events.push(Event::SessionOpened(
                    id,
                    self.session(session),
                    vec![],
                    None,
                ));
            }
            Command::Resume(session)
                if self.can_load
                    && !self.loading.contains_key(&session)
                    && self.requests.len() < 64 =>
            {
                self.request(
                    id,
                    "session/load",
                    json!(LoadSessionRequest::new(session.clone(), self.root.clone())),
                    Request::Load(session.clone()),
                )?;
                self.loading.insert(session, LoadingSession::new(id));
            }
            Command::Resume(session) if self.can_resume && self.requests.len() < 64 => {
                self.request(
                    id,
                    "session/resume",
                    json!(ResumeSessionRequest::new(session.clone(), self.root.clone())),
                    Request::Resume(session),
                )?;
            }
            Command::History(cursor) if self.can_list && self.requests.len() < 64 => {
                self.request(
                    id,
                    "session/list",
                    json!(ListSessionsRequest::new().cwd(self.root.clone()).cursor(cursor)),
                    Request::History,
                )?;
            }
            Command::History(_) if !self.can_list => {
                events.push(Event::History(id, vec![], None));
            }
            Command::Models(_) => events.push(Event::Models(id, vec![], None)),
            Command::Skills => events.push(Event::Skills(id, vec![])),
            Command::RateLimits => events.push(Event::RateLimits(vec![])),
            _ => events.push(Event::RequestFailed(id, Failure::Rejected(-32601))),
        }
        Ok(())
    }

    fn receive(&mut self, value: Value, events: &mut Vec<Event>) -> Result<(), Failure> {
        if let Some(method) = value["method"].as_str() {
            if let Some(id) = value
                .get("id")
                .filter(|id| id.is_number() || id.is_string())
            {
                if method == "session/request_permission" {
                    self.permission(id.clone(), &value["params"], events)?;
                } else if method == "elicitation/create" {
                    self.elicitation(id.clone(), &value["params"], events)?;
                } else {
                    self.write(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}))?;
                }
            } else if method == "session/update" {
                self.update(&value["params"], events);
            }
            return Ok(());
        }
        let Some(id) = value["id"].as_u64() else {
            return Ok(());
        };
        let error = value["error"]["code"].as_i64();
        let result = &value["result"];
        if id == 0 && !self.ready {
            if let Some(code) = error {
                return Err(Failure::Rejected(code));
            }
            let handshake: InitializeResponse =
                serde_json::from_value(result.clone()).map_err(|_| Failure::Protocol)?;
            if handshake.protocol_version != ProtocolVersion::V1 {
                return Err(Failure::Protocol);
            }
            self.images = handshake.agent_capabilities.prompt_capabilities.image;
            self.can_list = handshake.agent_capabilities.session_capabilities.list.is_some();
            self.can_load = handshake.agent_capabilities.load_session
                || result["agentCapabilities"]["sessionCapabilities"]["load"].is_object();
            self.can_resume = handshake
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some();
            self.ready = true;
            events.push(Event::ImageInput(self.images));
            events.push(Event::Ready {
                login_required: false,
            });
            return Ok(());
        }
        let Some((_, request)) = self.requests.remove(&id) else {
            return Ok(());
        };
        match request {
            Request::Session => {
                if let Some(session) = result["sessionId"].as_str().filter(|id| !id.is_empty()) {
                    let session_id = session.to_owned();
                    self.sessions.insert(session_id.clone());
                    events.push(Event::SessionOpened(
                        id,
                        self.session(session_id.clone()),
                        vec![],
                        None,
                    ));
                    self.emit_session_meta(&session_id, result, events);
                } else {
                    self.reject_or_auth(id, error, events);
                }
            }
            Request::Load(session_id) => {
                let loading = self.loading.remove(&session_id);
                if let Some(code) = error {
                    if code != ACP_AUTH_REQUIRED && self.can_resume {
                        self.request(
                            id,
                            "session/resume",
                            json!(ResumeSessionRequest::new(
                                session_id.clone(),
                                self.root.clone()
                            )),
                            Request::Resume(session_id),
                        )?;
                    } else {
                        self.reject_or_auth(id, error, events);
                    }
                } else {
                    self.sessions.insert(session_id.clone());
                    let (turns, deferred) = loading
                        .map_or_else(|| (Vec::new(), Vec::new()), LoadingSession::finish);
                    events.push(Event::SessionOpened(
                        id,
                        self.session(session_id.clone()),
                        turns,
                        None,
                    ));
                    events.extend(deferred);
                    self.emit_session_meta(&session_id, result, events);
                }
            }
            Request::Resume(session_id) => {
                if error.is_some() {
                    self.reject_or_auth(id, error, events);
                } else {
                    self.sessions.insert(session_id.clone());
                    events.push(Event::SessionOpened(
                        id,
                        self.session(session_id.clone()),
                        vec![],
                        None,
                    ));
                    self.emit_session_meta(&session_id, result, events);
                }
            }
            Request::History => {
                if error.is_some() {
                    self.reject_or_auth(id, error, events);
                } else if let Some(entries) = result["sessions"].as_array() {
                    let mut sessions = Vec::new();
                    for entry in entries {
                        let Some(session_id) =
                            entry["sessionId"].as_str().filter(|s| !s.is_empty())
                        else {
                            continue;
                        };
                        let matches_root = match entry["cwd"].as_str().filter(|s| !s.is_empty()) {
                            Some(cwd) => {
                                same_root(&normalize_acp_path(cwd), &self.root)
                                    || entry["additionalDirectories"]
                                        .as_array()
                                        .is_some_and(|dirs| {
                                            dirs.iter().filter_map(Value::as_str).any(|dir| {
                                                same_root(&normalize_acp_path(dir), &self.root)
                                            })
                                        })
                            }
                            None => true,
                        };
                        if !matches_root {
                            continue;
                        }
                        let title = entry["title"]
                            .as_str()
                            .or_else(|| entry["summary"].as_str())
                            .map(str::trim)
                            .filter(|title| !title.is_empty())
                            .map(|title| prefix(title, 512).to_owned())
                            .unwrap_or_default();
                        if !title.is_empty() {
                            self.titles.insert(session_id.to_owned(), title.clone());
                        }
                        let updated_at = entry["updatedAt"]
                            .as_str()
                            .filter(|ts| !ts.is_empty())
                            .map(str::to_owned);
                        sessions.push((
                            updated_at,
                            Session {
                                id: session_id.to_owned(),
                                cwd: self.root.clone(),
                                title,
                                model: None,
                                effort: None,
                            },
                        ));
                    }
                    if sessions.iter().any(|(updated, _)| updated.is_some()) {
                        sessions.sort_by(|(a, _), (b, _)| b.cmp(a));
                    }
                    let sessions = sessions.into_iter().map(|(_, s)| s).collect();
                    let next_cursor = result["nextCursor"]
                        .as_str()
                        .filter(|cursor| !cursor.is_empty())
                        .map(str::to_owned);
                    events.push(Event::History(id, sessions, next_cursor));
                } else {
                    events.push(Event::RequestFailed(id, Failure::Protocol));
                }
            }
            Request::Config(session) => {
                if let Some(code) = error {
                    events.push(Event::RequestFailed(id, Failure::Rejected(code)));
                } else {
                    self.apply_session_config(&session, result, events);
                    events.push(Event::Acknowledged(id));
                }
            }
            Request::SetMode(session, mode) => {
                if let Some(code) = error {
                    events.push(Event::RequestFailed(id, Failure::Rejected(code)));
                } else {
                    let cfg = self.configs.entry(session.clone()).or_default();
                    cfg.current_mode = Some(mode);
                    events.push(Event::SessionModes(
                        session,
                        cfg.modes.clone(),
                        cfg.current_mode.clone(),
                    ));
                    events.push(Event::Acknowledged(id));
                }
            }
            Request::SetModel(session, model) => {
                if let Some(code) = error {
                    events.push(Event::RequestFailed(id, Failure::Rejected(code)));
                } else {
                    let cfg = self.configs.entry(session.clone()).or_default();
                    cfg.current_model = Some(model);
                    events.push(Event::SessionModels {
                        session,
                        models: cfg.models.clone(),
                        current_model: cfg.current_model.clone(),
                        current_effort: cfg.current_effort.clone(),
                    });
                    events.push(Event::Acknowledged(id));
                }
            }
            Request::Prompt(session) => {
                self.cancel_approvals(&session, events)?;
                self.cancel_elicitations(&session, events)?;
                let Some(turn) = self.turns.remove(&session) else {
                    return Ok(());
                };
                let status = match serde_json::from_value::<PromptResponse>(result.clone())
                    .map(|r| r.stop_reason)
                {
                    Ok(StopReason::EndTurn) => TurnStatus::Completed,
                    Ok(StopReason::Cancelled) => TurnStatus::Interrupted,
                    _ => TurnStatus::Failed,
                };
                if status == TurnStatus::Failed {
                    let message = value["error"]["message"]
                        .as_str()
                        .or_else(|| result["stopReason"].as_str())
                        .unwrap_or("Invalid ACP prompt response");
                    events.push(Event::Item(
                        turn.key.clone(),
                        make_item(
                            "turn-error".into(),
                            ItemKind::Error,
                            prefix(message, MAX_TEXT).into(),
                        ),
                        true,
                    ));
                }
                events.push(Event::TurnEnded(turn.key, status));
            }
        }
        Ok(())
    }

    fn expire(&mut self, events: &mut Vec<Event>) -> Result<(), Failure> {
        let now = Instant::now();
        if (!self.ready && now >= self.initialize_by)
            || self
                .turns
                .values()
                .any(|turn| turn.cancel_deadline.is_some_and(|deadline| now >= deadline))
        {
            return Err(Failure::Timeout);
        }
        self.requests.retain(|&id, (deadline, request)| {
            let expired = !matches!(request, Request::Prompt(_)) && now >= *deadline;
            if expired {
                events.push(Event::RequestFailed(id, Failure::Timeout));
            }
            !expired
        });
        self.loading
            .retain(|_, loading| self.requests.contains_key(&loading.request_id));
        let expired: Vec<_> = self
            .approvals
            .iter()
            .filter_map(|(&id, pending)| (now >= pending.deadline).then_some(id))
            .collect();
        for id in expired {
            let pending = self.approvals.remove(&id).expect("pending approval");
            self.permission_response(&pending.wire_id, json!({"outcome":"cancelled"}))?;
            events.push(Event::ApprovalExpired(id));
        }
        let expired: Vec<_> = self
            .elicitations
            .iter()
            .filter_map(|(&id, pending)| (now >= pending.deadline).then_some(id))
            .collect();
        for id in expired {
            let pending = self.elicitations.remove(&id).expect("pending elicitation");
            self.elicitation_response(&pending.wire_id, json!({"action": "cancel"}))?;
            events.push(Event::ElicitationExpired(id));
        }
        Ok(())
    }
}

fn parse_available_commands(entries: &[Value]) -> Vec<SlashCommand> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries.iter().take(256) {
        let Some(raw_name) = entry["name"].as_str() else {
            continue;
        };
        let name = raw_name.trim().trim_start_matches('/');
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            continue;
        }
        let name: String = prefix(name, 128).into();
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let description: String = prefix(
            entry["description"].as_str().unwrap_or_default().trim(),
            512,
        )
        .into();
        let input_hint = entry["input"]["hint"]
            .as_str()
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(|hint| prefix(hint, 256).into());
        commands.push(SlashCommand {
            name,
            description,
            input_hint,
        });
    }
    commands
}

fn parse_select_options(option: &Value) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Some(entries) = option["options"].as_array() else {
        return out;
    };
    let mut push_one = |entry: &Value| {
        if let Some(value) = entry["value"].as_str() {
            let name = entry["name"].as_str().unwrap_or(value);
            let description = entry["description"].as_str().unwrap_or_default();
            out.push((
                prefix(value, 128).into(),
                prefix(name, 256).into(),
                prefix(description, 512).into(),
            ));
        }
    };
    for entry in entries {
        if let Some(grouped) = entry["options"].as_array() {
            for item in grouped {
                push_one(item);
            }
        } else {
            push_one(entry);
        }
    }
    out
}

fn parse_enum_options(schema: &Value) -> Vec<ElicitationOption> {
    if let Some(list) = schema["oneOf"]
        .as_array()
        .or_else(|| schema["anyOf"].as_array())
    {
        return list
            .iter()
            .filter_map(|entry| {
                let value = entry["const"].as_str()?;
                let label = entry["title"].as_str().unwrap_or(value);
                let description = entry["description"].as_str().unwrap_or_default();
                Some(ElicitationOption {
                    value: prefix(value, 512).into(),
                    label: prefix(label, 512).into(),
                    description: prefix(description, 1024).into(),
                })
            })
            .collect();
    }
    if let Some(values) = schema["enum"].as_array() {
        let names = schema["enumNames"].as_array();
        return values
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let value = entry.as_str()?;
                let label = names
                    .and_then(|list| list.get(index))
                    .and_then(Value::as_str)
                    .unwrap_or(value);
                Some(ElicitationOption {
                    value: prefix(value, 512).into(),
                    label: prefix(label, 512).into(),
                    description: String::new(),
                })
            })
            .collect();
    }
    Vec::new()
}

fn natural_field_key(key: &str) -> (String, u64, String) {
    let digits_start = key.find(|c: char| c.is_ascii_digit()).unwrap_or(key.len());
    let rest = &key[digits_start..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let number = rest[..digits_end].parse::<u64>().unwrap_or(u64::MAX);
    (
        key[..digits_start].to_owned(),
        number,
        rest[digits_end..].to_owned(),
    )
}

fn parse_elicitation_fields(schema: &Value) -> Vec<ElicitationField> {
    let Some(properties) = schema["properties"].as_object() else {
        return Vec::new();
    };
    let required: HashSet<&str> = schema["required"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort_by(|a, b| natural_field_key(a).cmp(&natural_field_key(b)).then_with(|| a.cmp(b)));

    let mut fields = Vec::new();
    for key in keys.into_iter().take(64) {
        let prop = &properties[key];
        let title = prop["title"].as_str().unwrap_or(key.as_str());
        let description = prop["description"].as_str().unwrap_or_default();
        let is_required = required.contains(key.as_str());
        let field_type = prop["type"].as_str().unwrap_or("string");
        let (kind, default_text, default_values, default_bool) = match field_type {
            "array" => {
                let options = parse_enum_options(&prop["items"]);
                let defaults = prop["default"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                (ElicitationFieldKind::MultiSelect(options), None, defaults, None)
            }
            "boolean" => (
                ElicitationFieldKind::Boolean,
                None,
                Vec::new(),
                prop["default"].as_bool(),
            ),
            "integer" | "number" => {
                let default = prop["default"]
                    .as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| prop["default"].as_f64().map(|n| n.to_string()));
                (
                    ElicitationFieldKind::Number {
                        integer: field_type == "integer",
                    },
                    default,
                    Vec::new(),
                    None,
                )
            }
            _ => {
                let options = parse_enum_options(prop);
                let default = prop["default"].as_str().map(str::to_owned);
                if options.is_empty() {
                    (ElicitationFieldKind::Text, default, Vec::new(), None)
                } else {
                    (
                        ElicitationFieldKind::SingleSelect(options),
                        default,
                        Vec::new(),
                        None,
                    )
                }
            }
        };
        fields.push(ElicitationField {
            name: prefix(key, 128).into(),
            title: prefix(title, 512).into(),
            description: prefix(description, 1024).into(),
            required: is_required,
            kind,
            default_text,
            default_values,
            default_bool,
        });
    }
    fields
}

impl AcpClient<'_> {
    fn session(&self, id: String) -> Session {
        let title = self.titles.get(&id).cloned().unwrap_or_default();
        Session {
            id,
            cwd: self.root.clone(),
            title,
            model: None,
            effort: None,
        }
    }

    fn emit_session_meta(&mut self, session_id: &str, result: &Value, events: &mut Vec<Event>) {
        if let Some(tier) = result["_meta"]["antigravity"]["tier"]["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
        {
            events.push(Event::AccountTier(Some(prefix(tier, 128).into())));
        }
        self.apply_session_config(session_id, result, events);
    }

    fn reject_or_auth(&self, id: u64, error: Option<i64>, events: &mut Vec<Event>) {
        events.push(Event::RequestFailed(
            id,
            error.map_or(Failure::Protocol, Failure::Rejected),
        ));
        if error == Some(ACP_AUTH_REQUIRED) {
            events.push(Event::Ready {
                login_required: true,
            });
        }
    }
}

fn content_text(content: &Value) -> Option<&str> {
    content["text"]
        .as_str()
        .or_else(|| content["resource"]["text"].as_str())
        .filter(|text| !text.is_empty())
}

fn apply_tool_update(item: &mut Item, update: &Value) {
    if let Some(title) = update["title"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| update["name"].as_str().filter(|s| !s.is_empty()))
    {
        item.text = prefix(title, 1024).into();
    }
    if let Some(kind) = update["kind"].as_str() {
        item.kind = match kind {
            "read" => ItemKind::Read,
            "search" => ItemKind::Search,
            "execute" => ItemKind::Command,
            _ => ItemKind::Tool,
        };
    }
    if let Some(status) = update["status"].as_str() {
        item.status = match status {
            "pending" => "pending",
            "in_progress" => "inProgress",
            "completed" => "completed",
            "failed" => "failed",
            _ => "",
        }
        .into();
    }
    if let Some(content) = update["content"].as_array() {
        item.output.clear();
        for block in content {
            if let Some(text) = block["content"]["text"]
                .as_str()
                .or_else(|| block["newText"].as_str())
            {
                if !item.output.is_empty() && item.output.len() < MAX_TOOL_OUTPUT {
                    item.output.push('\n');
                }
                item.output
                    .push_str(prefix(text, MAX_TOOL_OUTPUT - item.output.len()));
            }
        }
    } else if item.output.is_empty()
        && let Some(output) = update["rawOutput"]["output"]
            .as_str()
            .or_else(|| update["rawOutput"].as_str())
            .filter(|s| !s.is_empty())
    {
        item.output = prefix(output, MAX_TOOL_OUTPUT).into();
    }
}

fn format_plan_entries(update: &Value) -> Option<String> {
    let entries = update["entries"].as_array()?;
    let mut text = String::new();
    for entry in entries {
        if let Some(content) = entry["content"].as_str() {
            let row = format!(
                "- {}: {content}\n",
                entry["status"].as_str().unwrap_or("pending")
            );
            text.push_str(prefix(&row, MAX_TEXT - text.len()));
        }
    }
    Some(text)
}

fn strip_windows_drive_leading_slash(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && matches!(bytes[0], b'/' | b'\\')
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && (bytes.len() == 3 || matches!(bytes[3], b'/' | b'\\'))
    {
        &path[1..]
    } else {
        path
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn normalize_acp_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let (from_uri, stripped) = if let Some(rest) = trimmed
        .strip_prefix("file://localhost")
        .filter(|rest| rest.starts_with('/'))
    {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix("file://") {
        (true, rest)
    } else {
        (false, trimmed)
    };
    let decoded;
    let path_str = if from_uri {
        decoded = percent_decode(stripped);
        decoded.as_str()
    } else {
        stripped
    };
    PathBuf::from(strip_windows_drive_leading_slash(path_str))
}

fn lexical_root_key(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let mut normalized = strip_windows_drive_leading_slash(text).replace('\\', "/");
    let is_windows_drive = normalized.len() >= 2
        && normalized.as_bytes()[0].is_ascii_alphabetic()
        && normalized.as_bytes()[1] == b':';
    while normalized.len() > 1 && normalized.ends_with('/') {
        if is_windows_drive && normalized.len() == 3 {
            break;
        }
        normalized.pop();
    }
    if is_windows_drive {
        normalized.make_ascii_lowercase();
    }
    Some(normalized)
}

fn same_root(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let Ok(real_a) = winc_config::files::canonicalize(a) {
        if real_a == b {
            return true;
        }
        if let Ok(real_b) = winc_config::files::canonicalize(b)
            && real_a == real_b
        {
            return true;
        }
    }
    matches!((lexical_root_key(a), lexical_root_key(b)), (Some(ka), Some(kb)) if ka == kb)
}

fn tool_key(id: &str) -> String {
    format!("tool:{id}")
}

fn make_item(id: String, kind: ItemKind, text: String) -> Item {
    Item {
        id,
        kind,
        text,
        output: String::new(),
        status: String::new(),
        files: vec![],
        file_stats: vec![],
    }
}

#[cfg(test)]
#[path = "acp_tests.rs"]
mod tests;
