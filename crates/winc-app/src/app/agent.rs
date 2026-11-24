//! Window-owned state and event handling for the native Codex panel.

use std::collections::{BTreeMap, HashMap, VecDeque};

mod editor;
mod sessions;
pub(crate) use editor::Link;
use sessions::Catalog;
pub(crate) use sessions::Tab;

use winc_agent::{
    AgentProvider, Approval, CodexProvider, Command, DeltaKind, Event, Failure, Item, ItemKind,
    Session, SubmitError, TurnKey, TurnStatus,
};
use winc_i18n::t;

use super::*;

const MAX_CARDS: usize = 600;
const MAX_BODY: usize = winc_agent::MAX_PROMPT;
const MAX_OUTPUT: usize = 24 * 1024;

pub(crate) struct Card {
    pub(crate) turn: TurnKey,
    pub(crate) item: Item,
    pub(crate) done: bool,
    pub(crate) expanded: bool,
    pub(crate) markdown: Option<winc_preview::markdown::Markdown>,
    pub(crate) layout: Option<crate::view::agent::CardLayout>,
}

impl Card {
    fn new(turn: TurnKey, item: Item, done: bool) -> Self {
        Self {
            turn,
            item,
            done,
            expanded: false,
            markdown: None,
            layout: None,
        }
    }
}

struct Submission {
    id: u64,
    text: String,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
}

struct Conversation {
    session: Session,
    cards: VecDeque<Card>,
    running: Option<TurnKey>,
    send_request: Option<Submission>,
    stop_request: Option<u64>,
    status: Option<TurnStatus>,
    tokens: (u64, u64),
    draft: TextField,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
    model: Option<String>,
    effort: Option<String>,
    loaded: bool,
    finished: Option<TurnKey>,
    changes: BTreeMap<PathBuf, bool>,
    change_turn: Option<TurnKey>,
}

impl Conversation {
    fn new(session: Session) -> Self {
        Self {
            session,
            cards: VecDeque::new(),
            running: None,
            send_request: None,
            stop_request: None,
            status: None,
            tokens: (0, 0),
            draft: TextField::multiline(),
            attachment: None,
            model: None,
            effort: None,
            loaded: true,
            finished: None,
            changes: BTreeMap::new(),
            change_turn: None,
        }
    }

    fn card(&mut self, turn: TurnKey, id: String, kind: ItemKind) -> &mut Card {
        let index = self
            .cards
            .iter()
            .rposition(|card| card.turn == turn && card.item.id == id);
        if let Some(index) = index {
            return &mut self.cards[index];
        }
        if self.cards.len() == MAX_CARDS {
            self.cards.pop_front();
        }
        self.cards.push_back(Card::new(
            turn,
            Item {
                id,
                kind,
                text: String::new(),
                output: String::new(),
                status: String::new(),
                files: Vec::new(),
            },
            false,
        ));
        self.cards.back_mut().expect("card just inserted")
    }
}

#[derive(Clone)]
pub(crate) enum Hit {
    New,
    Send,
    Stop,
    Retry,
    CopyMessage(usize),
    CopyCode(usize, usize),
    CopyApproval,
    Expand(usize),
    Decide(u64, bool),
    Attach(bool),
    Detach,
    Show(Tab),
    ChooseSession(String),
    ChooseModel(String),
    ChooseEffort(String),
    More,
    SwitchProject,
    Open(Link, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connection {
    Connecting,
    Ready,
    Login,
    Unavailable,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct PanelRects {
    pub(crate) body: Rect,
    pub(crate) divider: Rect,
    pub(crate) button: Rect,
}

pub(crate) struct Panel {
    pub(crate) shown: bool,
    pub(crate) rects: PanelRects,
    pub(crate) width: f32,
    pub(crate) field: TextField,
    pub(crate) connection: Connection,
    pub(crate) error: Option<String>,
    pub(crate) root: Option<PathBuf>,
    pub(crate) hits: Vec<(Rect, Hit)>,
    pub(crate) viewport: Rect,
    pub(crate) scroll: f32,
    pub(crate) max_scroll: f32,
    pub(crate) follow: bool,
    pub(crate) workspace_parallel: bool,
    pub(crate) attachment: Option<std::rc::Rc<editor::Attachment>>,
    pub(crate) tab: Tab,
    catalog: Catalog,
    default_model: Option<String>,
    default_effort: Option<String>,
    provider: Option<CodexProvider>,
    conversations: HashMap<String, Conversation>,
    selected: Option<String>,
    opening: Option<(u64, bool)>,
    approvals: Vec<Approval>,
}

impl Panel {
    fn new() -> Self {
        Self {
            shown: true,
            rects: PanelRects::default(),
            width: 390.0,
            field: TextField::multiline(),
            connection: Connection::Unavailable,
            error: None,
            root: None,
            hits: Vec::new(),
            viewport: Rect::default(),
            scroll: 0.0,
            max_scroll: 0.0,
            follow: true,
            workspace_parallel: false,
            attachment: None,
            tab: Tab::Chat,
            catalog: Catalog::default(),
            default_model: None,
            default_effort: None,
            provider: None,
            conversations: HashMap::new(),
            selected: None,
            opening: None,
            approvals: Vec::new(),
        }
    }

    fn current(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected.as_deref()?)
    }

    fn current_mut(&mut self) -> Option<&mut Conversation> {
        self.conversations.get_mut(self.selected.as_deref()?)
    }

    pub(crate) fn cards(&mut self) -> Option<&mut VecDeque<Card>> {
        self.current_mut().map(|session| &mut session.cards)
    }

    pub(crate) fn changes(&self) -> Option<&BTreeMap<PathBuf, bool>> {
        self.current().map(|session| &session.changes)
    }

    pub(crate) fn busy(&self) -> bool {
        self.current()
            .is_some_and(|s| s.running.is_some() || s.send_request.is_some())
    }

    pub(crate) fn any_active(&self) -> bool {
        self.conversations
            .values()
            .any(|s| s.running.is_some() || s.send_request.is_some())
            || self.opening.is_some()
            || !self.approvals.is_empty()
    }

    pub(crate) fn can_send(&self) -> bool {
        self.connection == Connection::Ready
            && self.catalog_ready()
            && self.opening.is_none()
            && !self.busy()
            && self.current().is_none_or(|session| session.loaded)
            && (!self.field.text().trim().is_empty() || self.attachment.is_some())
    }

    pub(crate) fn can_stop(&self) -> bool {
        self.current()
            .is_some_and(|s| s.running.is_some() && s.stop_request.is_none())
    }

    pub(crate) fn approval(&self) -> Option<&Approval> {
        self.approvals
            .iter()
            .find(|approval| Some(&approval.turn.session) == self.selected.as_ref())
    }

    pub(crate) fn status(&self) -> &str {
        if let Some(error) = self.error.as_deref() {
            return error;
        }
        match self.connection {
            Connection::Connecting => t!("agent-connecting"),
            Connection::Login => t!("agent-login"),
            Connection::Unavailable => t!("agent-unavailable"),
            Connection::Ready if self.approval().is_some() => t!("agent-awaiting"),
            Connection::Ready if self.opening.is_some() => t!("agent-opening"),
            Connection::Ready if self.busy() => t!("agent-running"),
            Connection::Ready => match self.current().and_then(|s| s.status) {
                Some(TurnStatus::Interrupted) => t!("agent-interrupted"),
                Some(TurnStatus::Completed) => t!("agent-completed"),
                Some(TurnStatus::Failed) => t!("agent-failed"),
                Some(TurnStatus::Running) => t!("agent-running"),
                None => t!("agent-ready"),
            },
        }
    }

    fn limit_transcripts(&mut self) {
        const CONVERSATION_BYTES: usize = 8 * 1024 * 1024;
        const PANEL_BYTES: usize = 32 * 1024 * 1024;
        let bytes = |card: &Card| card.item.text.len() + card.item.output.len();
        let mut total = 0;
        for session in self.conversations.values_mut() {
            let mut size: usize = session.cards.iter().map(bytes).sum();
            while size > CONVERSATION_BYTES {
                if let Some(card) = session.cards.pop_front() {
                    size -= bytes(&card);
                }
            }
            total += size;
        }
        if total > PANEL_BYTES {
            for session in self.conversations.values_mut().filter(|session| {
                self.selected.as_ref() != Some(&session.session.id) && session.running.is_none()
            }) {
                while total > PANEL_BYTES {
                    let Some(card) = session.cards.pop_front() else {
                        break;
                    };
                    total -= bytes(&card);
                }
            }
        }
        while total > PANEL_BYTES {
            let Some(session) = self
                .conversations
                .values_mut()
                .max_by_key(|s| s.cards.len())
            else {
                break;
            };
            let Some(card) = session.cards.pop_front() else {
                break;
            };
            total -= bytes(&card);
        }
    }

    fn submit(&mut self, command: Command) -> Option<u64> {
        let Some(provider) = &mut self.provider else {
            return None;
        };
        match provider.submit(command) {
            Ok(id) => {
                self.error = None;
                Some(id)
            }
            Err(error) => {
                self.error = Some(
                    match error {
                        SubmitError::Busy => t!("agent-queue-full"),
                        SubmitError::Disconnected => t!("agent-disconnected"),
                        SubmitError::PromptTooLarge => t!("agent-prompt-too-large"),
                    }
                    .into(),
                );
                None
            }
        }
    }

    fn new_conversation(&mut self, send: bool) {
        if self.connection == Connection::Ready
            && self.opening.is_none()
            && let Some(id) = self.submit(Command::NewSession)
        {
            self.opening = Some((id, send));
        }
    }

    fn send(&mut self) {
        if !self.can_send() {
            return;
        }
        let Some(session) = self.current() else {
            self.new_conversation(true);
            return;
        };
        let text = self.field.text().to_owned();
        let mut prompt = text.clone();
        if let Some(attachment) = &self.attachment {
            attachment.append_to(&mut prompt);
        }
        if !session.loaded {
            return;
        }
        let attachment = self.attachment.clone();
        let command = Command::Send {
            session: session.session.id.clone(),
            text: prompt,
            model: self.model().map(|model| model.id.clone()),
            effort: self.effort().map(str::to_owned),
        };
        if let Some(id) = self.submit(command) {
            self.current_mut().expect("selected session").send_request = Some(Submission {
                id,
                text,
                attachment,
            });
            self.show_tab(Tab::Chat);
        }
    }

    fn stop(&mut self) {
        let Some(turn) = self.current().and_then(|s| s.running.clone()) else {
            return;
        };
        if self.can_stop()
            && let Some(id) = self.submit(Command::Stop(turn))
        {
            self.current_mut().expect("selected session").stop_request = Some(id);
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Ready { login_required } => {
                self.connection = if login_required {
                    Connection::Login
                } else {
                    Connection::Ready
                };
                if !login_required {
                    self.discover();
                    if let Some(selected) = self.selected.clone() {
                        self.choose(selected);
                    }
                }
            }
            Event::Disconnected(error) => {
                self.provider = None;
                self.connection = Connection::Unavailable;
                self.error = Some(failure(error));
                self.opening = None;
                self.approvals.clear();
                for session in self.conversations.values_mut() {
                    session.loaded = false;
                    if session.running.take().is_some() || session.send_request.take().is_some() {
                        session.status = Some(TurnStatus::Failed);
                    }
                    session.send_request = None;
                    session.stop_request = None;
                }
            }
            Event::RequestFailed(id, error) => {
                self.error = Some(failure(error));
                self.catalog_failed(id);
                if self.opening.is_some_and(|(request, _)| request == id) {
                    self.opening = None;
                }
                for session in self.conversations.values_mut() {
                    if session
                        .send_request
                        .as_ref()
                        .is_some_and(|sent| sent.id == id)
                    {
                        session.send_request = None;
                        session.status = Some(TurnStatus::Failed);
                    }
                    if session.stop_request == Some(id) {
                        session.stop_request = None;
                    }
                }
            }
            Event::SessionOpened(id, session, history) => {
                if self.opening.is_none_or(|(request, _)| request != id) {
                    return;
                }
                let send = self.opening.take().is_some_and(|(_, send)| send);
                let key = session.id.clone();
                self.remember_session(&session);
                let conversation = self
                    .conversations
                    .entry(key.clone())
                    .or_insert_with(|| Conversation::new(session.clone()));
                conversation.loaded = true;
                conversation.session = session;
                if conversation.model.is_none() && !history.is_empty() {
                    conversation.model = conversation.session.model.clone();
                    conversation.effort = conversation.session.effort.clone();
                }
                if !history.is_empty() && conversation.running.is_none() {
                    conversation.cards.clear();
                    for turn in history {
                        for item in turn.items {
                            if conversation.cards.len() == MAX_CARDS {
                                conversation.cards.pop_front();
                            }
                            conversation
                                .cards
                                .push_back(Card::new(turn.key.clone(), item, true));
                        }
                        conversation.status = Some(turn.status);
                        if turn.status == TurnStatus::Running {
                            conversation.running = Some(turn.key);
                        } else {
                            conversation.finished = Some(turn.key);
                        }
                    }
                }
                self.select(key);
                if send {
                    self.send();
                }
            }
            Event::TurnStarted(request, turn) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    if session
                        .send_request
                        .as_ref()
                        .is_some_and(|sent| request == Some(sent.id))
                        && let Some(sent) = session.send_request.take()
                    {
                        let (field, attachment) = if Some(&turn.session) == self.selected.as_ref() {
                            (&mut self.field, &mut self.attachment)
                        } else {
                            (&mut session.draft, &mut session.attachment)
                        };
                        if field.text() == sent.text {
                            field.set_text("");
                        }
                        if matches!((&sent.attachment, &*attachment),
                            (Some(sent), Some(current)) if std::rc::Rc::ptr_eq(sent, current))
                        {
                            *attachment = None;
                        }
                    }
                    if session.finished.as_ref() == Some(&turn) {
                        return;
                    }
                    if session.change_turn.as_ref() != Some(&turn) {
                        session.change_turn = Some(turn.clone());
                        session.changes.clear();
                    }
                    session.running = Some(turn);
                    session.status = Some(TurnStatus::Running);
                }
            }
            Event::TurnEnded(turn, status) => {
                if let Some(session) = self.conversations.get_mut(&turn.session)
                    && session
                        .running
                        .as_ref()
                        .is_none_or(|running| running == &turn)
                {
                    session.running = None;
                    session.stop_request = None;
                    session.status = Some(status);
                    session.finished = Some(turn);
                }
            }
            Event::Item(turn, item, done) => {
                let files =
                    if done && item.kind == ItemKind::FileChange && item.status == "completed" {
                        editor::reported_files(self.root.as_deref(), &item.files)
                    } else {
                        Vec::new()
                    };
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    if session
                        .change_turn
                        .as_ref()
                        .is_none_or(|active| active == &turn)
                    {
                        for file in files {
                            session.note_change(&turn, file, true);
                        }
                    }
                    let card = session.card(turn, item.id.clone(), item.kind);
                    let mut item = item;
                    if item.output.is_empty() {
                        item.output = std::mem::take(&mut card.item.output);
                    }
                    if item.text.is_empty() {
                        item.text = std::mem::take(&mut card.item.text);
                    }
                    if item.files.is_empty() {
                        item.files = std::mem::take(&mut card.item.files);
                    }
                    card.item = item;
                    card.done = done;
                    card.markdown = None;
                    card.layout = None;
                }
            }
            Event::Delta(turn, id, kind, text) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let card = session.card(
                        turn,
                        id,
                        match kind {
                            DeltaKind::Message => ItemKind::Message,
                            DeltaKind::Output => ItemKind::Tool,
                        },
                    );
                    card.layout = None;
                    match kind {
                        DeltaKind::Message => {
                            append(&mut card.item.text, &text, MAX_BODY);
                            card.markdown = None;
                        }
                        DeltaKind::Output => append(&mut card.item.output, &text, MAX_OUTPUT),
                    }
                }
            }
            Event::Files(turn, id, files, patch) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let card = session.card(turn, id, ItemKind::FileChange);
                    card.item.files = files;
                    card.item.output = patch;
                    card.layout = None;
                }
            }
            Event::Approval(approval) => {
                if let Some(known) = self
                    .approvals
                    .iter_mut()
                    .find(|known| known.id == approval.id)
                {
                    *known = approval;
                    return;
                }
                if self.selected.as_ref() == Some(&approval.turn.session) {
                    self.show_tab(Tab::Chat);
                }
                if self.approvals.is_empty() {
                    self.scroll = 0.0;
                }
                self.approvals.push(approval);
            }
            Event::ApprovalResolved(id) => self.approvals.retain(|approval| approval.id != id),
            Event::Tokens(turn, total, last) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    session.tokens = (total, last);
                }
            }
            Event::History(id, sessions, next) => self.history_received(id, sessions, next),
            Event::Models(id, models, next) => self.models_received(id, models, next),
            Event::Acknowledged(_) => {}
        }
    }
}

fn append(target: &mut String, text: &str, limit: usize) {
    target.push_str(winc_agent::prefix(text, limit.saturating_sub(target.len())));
}

fn failure(error: Failure) -> String {
    match error {
        Failure::ExecutableNotFound => t!("agent-no-executable").into(),
        Failure::Workspace => t!("agent-no-workspace").into(),
        Failure::Start | Failure::Exited => t!("agent-disconnected").into(),
        Failure::Protocol => t!("agent-protocol").into(),
        Failure::Timeout => t!("agent-timeout").into(),
        Failure::Rejected(code) => t!("agent-rejected", code = code),
    }
}

impl App {
    pub(crate) fn agent_enabled(&self) -> bool {
        self.config.agent_enabled.unwrap_or(true)
    }

    pub(crate) fn agent_writers(&self) -> Option<(&Path, usize)> {
        let panel = self.agent.as_ref()?;
        let root = panel.root.as_deref()?;
        Some((root, panel.writer_count()))
    }

    pub(crate) fn set_agent_parallel(&mut self, parallel: bool) {
        if let Some(panel) = &mut self.agent
            && panel.workspace_parallel != parallel
        {
            panel.workspace_parallel = parallel;
            if panel.shown {
                self.request_redraw();
            }
        }
    }

    pub(super) fn close_agent(&mut self) {
        self.agent.take();
        if self.focus == Focus::Agent {
            self.focus = Focus::Editor;
            self.ime.field_preedit.clear();
        }
    }

    pub(super) fn toggle_agent(&mut self) {
        if !self.agent_enabled() {
            self.flash(t!("agent-disabled"));
            return;
        }
        if let Some(panel) = &mut self.agent
            && panel.shown
        {
            panel.shown = false;
            panel.hits.clear();
            if self.focus == Focus::Agent {
                self.focus = Focus::Editor;
                self.ime.field_preedit.clear();
            }
            if panel.any_active() {
                self.flash(t!("agent-continues"));
            } else {
                panel.provider = None;
            }
        } else {
            let panel = self.agent.get_or_insert_with(|| Box::new(Panel::new()));
            panel.shown = true;
            if panel.provider.is_none() {
                self.connect_agent();
            }
            self.focus = Focus::Agent;
            self.reset_blink();
        }
        self.relayout();
        self.request_redraw();
    }

    fn connect_agent(&mut self) {
        let root = self
            .editor
            .workspace
            .as_ref()
            .map(|workspace| workspace.root.clone());
        let executable = self.config.agent_executable.as_deref().map(PathBuf::from);
        let Some(panel) = &mut self.agent else { return };
        panel.provider = None;
        panel.connection = Connection::Unavailable;
        let Some(root) = root else {
            panel.root = None;
            panel.error = Some(t!("agent-no-workspace").into());
            return;
        };
        if winc_backend::is_remote(&root) {
            panel.root = None;
            panel.error = Some(t!("agent-remote").into());
            return;
        }
        if panel.root.as_ref() != Some(&root) {
            panel.catalog = Catalog::default();
            panel.conversations.clear();
            panel.selected = None;
            panel.attachment = None;
            panel.default_model = None;
            panel.default_effort = None;
        }
        panel.root = Some(root.clone());
        let Some(proxy) = self.link.proxy.clone() else {
            return;
        };
        match CodexProvider::connect(root.clone(), executable, move || {
            let _ = proxy.send_event(UserEvent::Agent);
        }) {
            Ok(provider) => {
                panel.root = Some(root);
                panel.provider = Some(provider);
                panel.connection = Connection::Connecting;
                panel.error = None;
                for session in panel.conversations.values_mut() {
                    session.loaded = false;
                }
                panel.catalog = Catalog::default();
            }
            Err(_) => panel.error = Some(failure(Failure::Start)),
        }
    }

    pub(super) fn poll_agent(&mut self) {
        let Some(panel) = &mut self.agent else { return };
        let Some(provider) = &mut panel.provider else {
            return;
        };
        let mut hidden_approval = false;
        let mut touched = Vec::new();
        for event in provider.drain() {
            hidden_approval |= !panel.shown && matches!(event, Event::Approval(_));
            if let Event::Item(_, item, true) = &event
                && item.kind == ItemKind::FileChange
            {
                touched.extend(editor::reported_files(panel.root.as_deref(), &item.files));
            }
            panel.event(event);
        }
        panel.limit_transcripts();
        let shown = panel.shown;
        if !shown && !panel.any_active() {
            panel.provider = None;
        }
        if !touched.is_empty() {
            self.sync_documents(|path| touched.iter().any(|changed| changed == path));
        }
        if shown {
            self.request_redraw();
        } else if hidden_approval {
            self.flash(t!("agent-hidden-approval"));
        }
    }

    pub(super) fn agent_send(&mut self) {
        if self.ime.field_preedit.is_empty()
            && self.agent_project_matches()
            && let Some(panel) = &mut self.agent
        {
            panel.send();
            self.request_redraw();
        }
    }

    fn agent_project_matches(&mut self) -> bool {
        let matches = self.agent.as_ref().and_then(|p| p.root.as_ref())
            == self.editor.workspace.as_ref().map(|w| &w.root);
        if !matches {
            self.flash(t!("agent-project-changed"));
        }
        matches
    }

    pub(crate) fn agent_rects(&self) -> PanelRects {
        self.agent
            .as_ref()
            .filter(|panel| panel.shown)
            .map_or(PanelRects::default(), |panel| panel.rects)
    }

    pub(super) fn layout_agent(&mut self) {
        let Some(panel) = self.agent.as_mut().filter(|panel| panel.shown) else {
            return;
        };
        panel.rects = PanelRects::default();
        if !self.config.agent_enabled.unwrap_or(true) {
            return;
        }
        let layout = &mut self.layout;
        let right = layout.editor.right();
        let w = panel
            .width
            .min((layout.editor.w - 160.0).max(layout.editor.w * 0.45))
            .round();
        let left = right - w;
        panel.rects.body = Rect::new(left, layout.editor.y, w, layout.editor.h);
        panel.rects.divider = Rect::new(left - 6.0, layout.editor.y, 8.0, layout.editor.h);
        let end = left - 6.0;
        layout.editor.w = (end - layout.editor.x).max(0.0);
        layout.top_bar.w = (end - layout.top_bar.x).max(0.0);
        layout.tabs.w = layout.tabs.w.min((end - 8.0 - layout.tabs.x).max(0.0));
        layout.text.w = (end - layout.text.x).max(0.0);
        layout.search.w = layout.search.w.min((layout.editor.w - 48.0).max(0.0));
        if layout.search.h > 0.0 {
            layout.search.x = (end - layout.search.w - 18.0).max(layout.editor.x);
        }
        layout.picker.w = layout.picker.w.min((layout.editor.w - 48.0).max(0.0));
        if layout.picker.h > 0.0 {
            layout.picker.x = (layout.editor.x + (layout.editor.w - layout.picker.w) * 0.5).round();
        }
    }

    pub(super) fn layout_agent_button(&mut self) {
        let Some(panel) = self
            .agent
            .as_mut()
            .filter(|panel| panel.shown && panel.rects.body.w > 0.0)
        else {
            return;
        };
        let l = &mut self.layout;
        let edge = if l.controls.w > 0.0 {
            l.controls.x - 6.0
        } else {
            l.window.right() - 14.0
        };
        panel.rects.button = Rect::new(edge - 72.0, l.sidebar_button.y, 72.0, BUTTON);
        l.tabs.w = l
            .tabs
            .w
            .min((panel.rects.button.x - 6.0 - l.tabs.x).max(0.0));
    }

    pub(in crate::app) fn agent_click(&mut self, x: f32, y: f32) -> bool {
        let rects = self.agent_rects();
        if rects.button.contains(x, y) {
            self.toggle_agent();
            return true;
        }
        if rects.divider.contains(x, y) {
            self.pointer.drag = Drag::AgentDivider {
                grab: x - rects.body.x,
            };
            return true;
        }
        if !self.agent_rects().body.contains(x, y) {
            return false;
        }
        let Some(panel) = &mut self.agent else {
            return false;
        };
        if panel.field.view.area.contains(x, y) {
            self.focus = Focus::Agent;
            self.click_field(x, y);
        } else if let Some(hit) = panel
            .hits
            .iter()
            .find_map(|(rect, hit)| rect.contains(x, y).then(|| hit.clone()))
        {
            match hit {
                Hit::New => {
                    if self.agent_project_matches() {
                        self.agent
                            .as_mut()
                            .expect("panel in hit area")
                            .new_conversation(false);
                    }
                }
                Hit::Send => self.agent_send(),
                Hit::Stop => panel.stop(),
                Hit::Retry => {
                    if !panel.any_active() {
                        self.connect_agent();
                    }
                }
                Hit::CopyMessage(index) => {
                    if let Some(card) = panel.cards().and_then(|cards| cards.get(index)) {
                        let text = card.item.text.clone();
                        self.set_clipboard(text, false);
                    }
                }
                Hit::CopyCode(index, block) => {
                    if let Some(winc_preview::markdown::Block::Code { text, .. }) = panel
                        .cards()
                        .and_then(|cards| cards.get(index))
                        .and_then(|card| card.markdown.as_ref())
                        .and_then(|md| md.blocks.get(block))
                    {
                        let text = text.clone();
                        self.set_clipboard(text, false);
                    }
                }
                Hit::CopyApproval => {
                    if let Some(approval) = panel.approval() {
                        let text = approval.details.clone();
                        self.set_clipboard(text, false);
                    }
                }
                Hit::Expand(index) => {
                    if let Some(card) = panel.cards().and_then(|cards| cards.get_mut(index)) {
                        card.expanded = !card.expanded;
                    }
                }
                Hit::Attach(selection) => self.agent_attach(selection),
                Hit::Detach => panel.attachment = None,
                Hit::Show(tab) => panel.show_tab(if panel.tab == tab { Tab::Chat } else { tab }),
                Hit::ChooseSession(id) => panel.choose(id),
                Hit::ChooseModel(id) => panel.choose_model(id),
                Hit::ChooseEffort(effort) => panel.choose_effort(effort),
                Hit::More => panel.next_page(),
                Hit::SwitchProject => {
                    if !panel.any_active() {
                        self.connect_agent();
                    }
                }
                Hit::Open(link, diff) => self.agent_open_link(link, diff),
                Hit::Decide(approval, accept) => {
                    panel.submit(Command::Approve { approval, accept });
                }
            }
        }
        self.request_redraw();
        true
    }

    pub(in crate::app) fn resize_agent(&mut self, x: f32, grab: f32) {
        if let Some(panel) = &mut self.agent {
            panel.width = (panel.rects.body.right() - x + grab).clamp(240.0, 750.0);
            self.relayout();
            self.request_redraw();
        }
    }

    pub(in crate::app) fn scroll_agent(&mut self, x: f32, y: f32, pixels: f32) -> bool {
        if !self.agent_rects().body.contains(x, y) {
            return false;
        }
        if let Some(panel) = &mut self.agent {
            if panel.field.view.area.contains(x, y) {
                panel.field.wheel_scroll(pixels);
            } else if panel.viewport.contains(x, y) {
                panel.scroll = (panel.scroll + pixels).clamp(0.0, panel.max_scroll);
                panel.follow = panel.scroll >= panel.max_scroll - 1.0;
            }
            self.request_redraw();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::snapshot::{blank_app, paint};

    #[test]
    fn renders_streamed_code_and_exposes_approval_actions() {
        let mut app = blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.opening = Some((4, false));
        panel.event(Event::SessionOpened(
            4,
            Session {
                id: "chat".into(),
                cwd: PathBuf::new(),
                title: "Sample".into(),
                model: None,
                effort: None,
            },
            Vec::new(),
        ));
        let turn = TurnKey {
            session: "chat".into(),
            turn: "turn".into(),
        };
        panel.event(Event::TurnStarted(None, turn.clone()));
        panel.event(Event::Delta(
            turn.clone(),
            "m".into(),
            DeltaKind::Message,
            concat!(
                "### Heading\n\n",
                "\u{60}\u{60}\u{60}rust\nfn main() {}\n\u{60}\u{60}\u{60}"
            )
            .into(),
        ));
        paint(&mut app);
        let panel = app.agent.as_mut().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyMessage(0)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyCode(0, _)))
        );
        panel.event(Event::Approval(Approval {
            id: 55,
            turn: turn.clone(),
            item: "command".into(),
            kind: winc_agent::ApprovalKind::Command,
            details: "echo hello".into(),
            reviewable: true,
        }));
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Decide(55, true)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Decide(55, false)))
        );
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::ApprovalResolved(55));
        panel.event(Event::Delta(
            turn,
            "long".into(),
            DeltaKind::Message,
            "- A line to scroll\n".repeat(300),
        ));
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        let before = panel.scroll;
        assert!(before > 0.0);
        let area = panel.viewport;
        assert!(app.scroll_agent(area.x + 5.0, area.y + 5.0, -120.0));
        assert!(app.agent.as_ref().unwrap().scroll < before);
    }
}
