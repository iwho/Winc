//! Window-owned state and event handling for the native Codex panel.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

mod composer;
mod editor;
mod sessions;
pub(crate) use editor::Link;
use sessions::Catalog;
pub(crate) use sessions::Tab;

use winc_agent::{
    AgentProvider, Approval, ApprovalDecision, CodexProvider, Command, DeltaKind, Event, Failure,
    Item, ItemKind, LineChanges, Session, SubmitError, TurnKey, TurnStatus,
};
use winc_i18n::t;

use super::*;

const MIN_WIDTH: f32 = 240.0;
const MAX_WIDTH: f32 = 800.0;
const MAX_CARDS: usize = 10_000;
const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
const ESTIMATED_HEIGHT: f32 = 52.0;
const MAX_BODY: usize = winc_agent::MAX_PROMPT;
const MAX_OUTPUT: usize = 24 * 1024;

pub(crate) struct Card {
    pub(crate) turn: TurnKey,
    pub(crate) item: Item,
    pub(crate) done: bool,
    pub(crate) message_time: Option<(i32, i32)>,
    pub(crate) expanded: bool,
    pub(crate) height: f32,
    pub(crate) dirty: bool,
    pub(crate) layout: Option<crate::view::agent::CardLayout>,
}

impl Card {
    fn new(turn: TurnKey, item: Item, done: bool) -> Self {
        Self {
            turn,
            item,
            done,
            message_time: None,
            expanded: false,
            height: ESTIMATED_HEIGHT,
            dirty: true,
            layout: None,
        }
    }
}

struct Submission {
    id: u64,
    steer: bool,
    text: String,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextPoint {
    pub(crate) section: usize,
    pub(crate) offset: usize,
}

pub(crate) struct TextHit {
    pub(crate) card: usize,
    pub(crate) point: TextPoint,
    pub(crate) rect: Rect,
    pub(crate) x: f32,
    pub(crate) advances: std::sync::Arc<[f32]>,
}

impl TextHit {
    fn position(&self, x: f32) -> TextPoint {
        let mut edge = self.x;
        let mut offset = self.point.offset;
        for width in self.advances.iter().copied() {
            if x < edge + width * 0.5 {
                break;
            }
            edge += width;
            offset += 1;
        }
        TextPoint {
            offset,
            ..self.point
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TextSelection {
    pub(crate) card: usize,
    pub(crate) anchor: TextPoint,
    pub(crate) head: TextPoint,
}

impl TextSelection {
    pub(crate) fn ordered(self) -> (TextPoint, TextPoint) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

pub(crate) struct Transcript {
    pub(crate) cards: VecDeque<Card>,
    indices: HashMap<(TurnKey, String), usize>,
    bytes: usize,
    pub(crate) first: usize,
    pub(crate) anchor: Option<(usize, f32)>,
    pub(crate) unread: bool,
    pub(crate) laid: std::ops::Range<usize>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            cards: VecDeque::new(),
            indices: HashMap::new(),
            bytes: 0,
            first: MAX_CARDS,
            anchor: None,
            unread: false,
            laid: 0..0,
        }
    }
}

impl Transcript {
    fn pop_front(&mut self) -> Option<Card> {
        let card = self.cards.pop_front()?;
        self.first += 1;
        self.bytes -= card.item.text.len() + card.item.output.len();
        self.indices
            .remove(&(card.turn.clone(), card.item.id.clone()));
        Some(card)
    }

    fn clear(&mut self) {
        self.first += self.cards.len() + MAX_CARDS;
        self.cards.clear();
        self.indices.clear();
        self.bytes = 0;
        self.anchor = None;
        self.unread = false;
        self.laid = 0..0;
    }

    fn push(&mut self, card: Card) {
        if self.cards.len() == MAX_CARDS {
            self.pop_front();
        }
        self.bytes += card.item.text.len() + card.item.output.len();
        self.indices.insert(
            (card.turn.clone(), card.item.id.clone()),
            self.first + self.cards.len(),
        );
        self.cards.push_back(card);
    }

    fn prepend(&mut self, turns: Vec<winc_agent::Turn>) -> bool {
        for turn in turns.into_iter().rev() {
            for item in turn.items.into_iter().rev() {
                let key = (turn.key.clone(), item.id.clone());
                if self.indices.contains_key(&key) {
                    continue;
                }
                let bytes = item.text.len() + item.output.len();
                if self.cards.len() >= MAX_CARDS || self.bytes + bytes > MAX_TRANSCRIPT_BYTES {
                    return false;
                }
                self.first -= 1;
                self.bytes += bytes;
                self.indices.insert(key, self.first);
                self.cards
                    .push_front(Card::new(turn.key.clone(), item, true));
            }
        }
        true
    }

    fn update(
        &mut self,
        turn: TurnKey,
        id: String,
        kind: ItemKind,
        update: impl FnOnce(&mut Card),
    ) {
        let key = (turn, id);
        let ordinal = match self.indices.get(&key) {
            Some(&ordinal) => ordinal,
            None => {
                self.unread |= self.anchor.is_some();
                self.push(Card::new(
                    key.0,
                    Item {
                        id: key.1,
                        kind,
                        text: String::new(),
                        output: String::new(),
                        status: String::new(),
                        files: Vec::new(),
                        file_stats: Vec::new(),
                    },
                    false,
                ));
                self.first + self.cards.len() - 1
            }
        };
        let card = &mut self.cards[ordinal - self.first];
        if card.message_time.is_none() && matches!(kind, ItemKind::User | ItemKind::Message) {
            let (_, _, _, hour, minute, _) = crate::system::local_time();
            card.message_time = Some((hour, minute));
        }
        self.bytes -= card.item.text.len() + card.item.output.len();
        update(card);
        self.bytes += card.item.text.len() + card.item.output.len();
    }

    fn card_mut(&mut self, ordinal: usize) -> Option<&mut Card> {
        self.cards.get_mut(ordinal.checked_sub(self.first)?)
    }

    pub(crate) fn scroll(&mut self, pixels: f32, viewport: f32) {
        let Some(last) = self.cards.len().checked_sub(1) else {
            return;
        };
        let (bottom, bottom_offset) = self.bottom(viewport);
        let (mut row, mut offset) = self
            .anchor
            .map_or((bottom, bottom_offset), |(row, offset)| {
                (row.saturating_sub(self.first).min(last), offset)
            });
        offset += pixels;
        while offset < 0.0 && row > 0 {
            row -= 1;
            offset += self.cards[row].height;
        }
        while row < last && offset >= self.cards[row].height {
            offset -= self.cards[row].height;
            row += 1;
        }
        if (bottom == 0 && bottom_offset == 0.0)
            || pixels > 0.0 && (row > bottom || (row == bottom && offset >= bottom_offset - 1.0))
        {
            self.follow();
        } else {
            self.anchor = Some((self.first + row, offset.max(0.0)));
        }
    }

    pub(crate) fn bottom(&self, viewport: f32) -> (usize, f32) {
        let mut remaining = viewport;
        for (i, card) in self.cards.iter().enumerate().rev() {
            remaining -= card.height;
            if remaining <= 0.0 {
                return (i, -remaining);
            }
        }
        (0, 0.0)
    }

    pub(crate) fn follow(&mut self) {
        self.anchor = None;
        self.unread = false;
    }
}

enum TurnTiming {
    Running(Instant),
    Finished(Duration),
}

struct Conversation {
    session: Session,
    transcript: Transcript,
    running: Option<TurnKey>,
    timing: Option<TurnTiming>,
    send_request: Option<Submission>,
    stop_request: Option<u64>,
    status: Option<TurnStatus>,
    tokens: (u64, u64, Option<u64>),
    draft: TextField,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
    model: Option<String>,
    effort: Option<String>,
    loaded: bool,
    older_cursor: Option<String>,
    older_request: Option<u64>,
    finished: Option<TurnKey>,
    changes: BTreeMap<PathBuf, editor::Change>,
    change_turn: Option<TurnKey>,
}

impl Conversation {
    fn new(session: Session) -> Self {
        Self {
            session,
            transcript: Transcript::default(),
            running: None,
            timing: None,
            send_request: None,
            stop_request: None,
            status: None,
            tokens: (0, 0, None),
            draft: TextField::multiline(),
            attachment: None,
            model: None,
            effort: None,
            loaded: true,
            older_cursor: None,
            older_request: None,
            finished: None,
            changes: BTreeMap::new(),
            change_turn: None,
        }
    }

    fn finish_timing(&mut self) {
        if let Some(TurnTiming::Running(start)) = self.timing {
            self.timing = Some(TurnTiming::Finished(start.elapsed()));
        }
    }
}

#[derive(Clone)]
pub(crate) enum Hit {
    New,
    Close,
    Send,
    Stop,
    Retry,
    CopyMessage(usize),
    CopyCode(std::sync::Arc<str>),
    Follow,
    Complete(usize),
    CopyApproval,
    Expand(usize),
    ScrollTable(usize, usize, usize, f32),
    Decide(u64, ApprovalDecision),
    Attach(bool),
    Detach,
    Show(Tab),
    ChooseSession(String),
    ChooseModel(String),
    ChooseEffort(String),
    More,
    Older,
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
}

pub(crate) struct Panel {
    pub(crate) shown: bool,
    pub(crate) rects: PanelRects,
    pub(crate) width: f32,
    pub(crate) field: TextField,
    pub(crate) completion: composer::Completion,
    pub(crate) connection: Connection,
    pub(crate) error: Option<String>,
    pub(crate) root: Option<PathBuf>,
    pub(crate) standalone: bool,
    pub(crate) hits: Vec<(Rect, Hit)>,
    pub(crate) viewport: Rect,
    pub(crate) text_hits: Vec<TextHit>,
    pub(crate) selection: Option<TextSelection>,
    pub(crate) scroll: f32,
    pub(crate) max_scroll: f32,
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
    timer_wake: Option<Instant>,
}

impl Panel {
    fn new() -> Self {
        Self {
            shown: true,
            rects: PanelRects::default(),
            width: MAX_WIDTH,
            field: TextField::multiline(),
            completion: composer::Completion::default(),
            connection: Connection::Unavailable,
            error: None,
            root: None,
            standalone: false,
            hits: Vec::new(),
            viewport: Rect::default(),
            text_hits: Vec::new(),
            selection: None,
            scroll: 0.0,
            max_scroll: 0.0,
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
            timer_wake: None,
        }
    }

    fn current(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected.as_deref()?)
    }

    fn current_mut(&mut self) -> Option<&mut Conversation> {
        self.conversations.get_mut(self.selected.as_deref()?)
    }

    pub(crate) fn transcript(&mut self) -> Option<&mut Transcript> {
        self.current_mut().map(|session| &mut session.transcript)
    }

    pub(crate) fn session_title(&self) -> Option<&str> {
        self.current()
            .map(|session| session.session.title.as_str())
            .filter(|title| !title.is_empty())
    }

    pub(crate) fn changes(&self) -> Option<&BTreeMap<PathBuf, editor::Change>> {
        self.current().map(|session| &session.changes)
    }

    pub(crate) fn busy(&self) -> bool {
        self.current()
            .is_some_and(|s| s.running.is_some() || s.send_request.is_some())
    }

    pub(crate) fn turn_elapsed(&self) -> Option<Duration> {
        match self.current()?.timing.as_ref()? {
            TurnTiming::Running(start) => Some(start.elapsed()),
            TurnTiming::Finished(elapsed) => Some(*elapsed),
        }
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
            && self.approval().is_none()
            && self.current().is_none_or(|session| {
                session.send_request.is_none() && session.stop_request.is_none()
            })
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
        const PANEL_BYTES: usize = 32 * 1024 * 1024;
        let mut total = 0;
        for session in self.conversations.values_mut() {
            let chat = &mut session.transcript;
            while chat.bytes > MAX_TRANSCRIPT_BYTES {
                chat.pop_front();
            }
            total += chat.bytes;
        }
        if total > PANEL_BYTES {
            for session in self.conversations.values_mut().filter(|session| {
                self.selected.as_ref() != Some(&session.session.id) && session.running.is_none()
            }) {
                while total > PANEL_BYTES {
                    let Some(card) = session.transcript.pop_front() else {
                        break;
                    };
                    total -= card.item.text.len() + card.item.output.len();
                }
            }
        }
        while total > PANEL_BYTES {
            let Some(session) = self
                .conversations
                .values_mut()
                .max_by_key(|s| s.transcript.cards.len())
            else {
                break;
            };
            let Some(card) = session.transcript.pop_front() else {
                break;
            };
            total -= card.item.text.len() + card.item.output.len();
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

    pub(super) fn scroll_viewport(&mut self, pixels: f32) {
        if self.tab == Tab::Chat && self.approval().is_none() {
            let viewport = self.viewport.h;
            if let Some(chat) = self.transcript() {
                chat.scroll(pixels, viewport);
            }
        } else {
            self.scroll = (self.scroll + pixels).clamp(0.0, self.max_scroll);
        }
    }

    pub(crate) fn older(&self) -> Option<bool> {
        let session = self.current()?;
        session.older_cursor.as_ref()?;
        (session.loaded && session.transcript.cards.len() < MAX_CARDS)
            .then_some(session.older_request.is_none())
    }

    fn load_older(&mut self) {
        if self.older() != Some(true) {
            return;
        }
        let Some(session) = self.current() else {
            return;
        };
        let Some(cursor) = session.older_cursor.clone() else {
            return;
        };
        let command = Command::Older {
            session: session.session.id.clone(),
            cursor,
        };
        if let Some(id) = self.submit(command) {
            let viewport = self.viewport.h;
            let session = self.current_mut().expect("selected session");
            session.older_request = Some(id);
            if session.transcript.anchor.is_none() {
                let (row, offset) = session.transcript.bottom(viewport);
                session.transcript.anchor = Some((session.transcript.first + row, offset));
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
        let steer = session.running.is_some();
        let command = if let Some(turn) = session.running.clone() {
            Command::Steer { turn, text: prompt }
        } else {
            Command::Send {
                session: session.session.id.clone(),
                text: prompt,
                model: self.model().map(|model| model.id.clone()),
                effort: self.effort().map(str::to_owned),
            }
        };
        if let Some(id) = self.submit(command) {
            let session = self.current_mut().expect("selected session");
            if !steer {
                session.timing = None;
            }
            session.send_request = Some(Submission {
                id,
                steer,
                text,
                attachment,
            });
            self.show_tab(Tab::Chat);
        }
    }

    fn acknowledge_submission(&mut self, session_id: &str, request: u64, steer: bool) {
        let Some(session) = self.conversations.get_mut(session_id) else {
            return;
        };
        if !session
            .send_request
            .as_ref()
            .is_some_and(|sent| sent.id == request && sent.steer == steer)
        {
            return;
        }
        let sent = session.send_request.take().expect("matching submission");
        let (field, attachment) = if self.selected.as_deref() == Some(session_id) {
            (&mut self.field, &mut self.attachment)
        } else {
            (&mut session.draft, &mut session.attachment)
        };
        field.remember_text(&sent.text);
        if field.text() == sent.text {
            field.set_text("");
        }
        if matches!((&sent.attachment, &*attachment),
            (Some(sent), Some(current)) if std::rc::Rc::ptr_eq(sent, current))
        {
            *attachment = None;
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
                    session.older_request = None;
                    if session.running.take().is_some() || session.send_request.take().is_some() {
                        session.status = Some(TurnStatus::Failed);
                    }
                    session.finish_timing();
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
                        && session.send_request.take().is_some_and(|sent| !sent.steer)
                    {
                        session.status = Some(TurnStatus::Failed);
                    }
                    if session.older_request == Some(id) {
                        session.older_request = None;
                    }
                    if session.stop_request == Some(id) {
                        session.stop_request = None;
                    }
                }
            }
            Event::SessionOpened(id, session, history, older_cursor) => {
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
                conversation.older_cursor = older_cursor;
                conversation.session = session;
                if conversation.model.is_none() && !history.is_empty() {
                    conversation.model = conversation.session.model.clone();
                    conversation.effort = conversation.session.effort.clone();
                }
                if !history.is_empty() && conversation.running.is_none() {
                    conversation.transcript.clear();
                    conversation.changes.clear();
                    conversation.change_turn = None;
                    let root = self.root.as_deref();
                    let last_turn = history.last().map(|turn| turn.key.clone());
                    if last_turn != conversation.finished {
                        conversation.timing = None;
                    }
                    for turn in history {
                        for item in turn.items {
                            if Some(&turn.key) == last_turn.as_ref()
                                && item.kind == ItemKind::FileChange
                                && item.status == "completed"
                                && let Some(root) = root
                            {
                                for (path, stat) in editor::reported_changes(root, &item) {
                                    conversation.note_change(&turn.key, path, true, stat);
                                }
                            }
                            conversation.transcript.push(Card::new(
                                turn.key.clone(),
                                item,
                                turn.status != TurnStatus::Running,
                            ));
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
            Event::Older(id, session, history, next) => {
                if let Some(session) = self.conversations.get_mut(&session)
                    && session.older_request == Some(id)
                {
                    session.older_request = None;
                    let has_room = session.transcript.prepend(history);
                    session.older_cursor = next
                        .filter(|cursor| has_room && Some(cursor) != session.older_cursor.as_ref());
                }
            }
            Event::TurnStarted(request, turn) => {
                if let Some(request) = request {
                    self.acknowledge_submission(&turn.session, request, false);
                }
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    if session.finished.as_ref() == Some(&turn) {
                        return;
                    }
                    if session.change_turn.as_ref() != Some(&turn) {
                        session.change_turn = Some(turn.clone());
                        session.changes.clear();
                    }
                    if session.running.as_ref() != Some(&turn)
                        || !matches!(session.timing, Some(TurnTiming::Running(_)))
                    {
                        session.timing = Some(TurnTiming::Running(Instant::now()));
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
                    if session.running.as_ref() == Some(&turn) {
                        session.finish_timing();
                    }
                    session.running = None;
                    let terminal = match status {
                        TurnStatus::Completed => "completed",
                        TurnStatus::Failed => "failed",
                        TurnStatus::Interrupted => "interrupted",
                        TurnStatus::Running => "",
                    };
                    for card in session
                        .transcript
                        .cards
                        .iter_mut()
                        .rev()
                        .take_while(|card| card.turn == turn)
                    {
                        if !card.done {
                            card.done = true;
                            card.item.status = terminal.into();
                        }
                    }
                    session.stop_request = None;
                    session.status = Some(status);
                    session.finished = Some(turn);
                }
            }
            Event::Item(turn, item, done) => {
                let files = self
                    .root
                    .as_deref()
                    .filter(|_| {
                        done && item.kind == ItemKind::FileChange && item.status == "completed"
                    })
                    .map(|root| editor::reported_changes(root, &item))
                    .unwrap_or_default();
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let first_completion = session
                        .transcript
                        .indices
                        .get(&(turn.clone(), item.id.clone()))
                        .and_then(|ordinal| {
                            session
                                .transcript
                                .cards
                                .get(ordinal - session.transcript.first)
                        })
                        .is_none_or(|card| !card.done);
                    if first_completion
                        && session
                            .change_turn
                            .as_ref()
                            .is_none_or(|active| active == &turn)
                    {
                        for (file, stat) in files {
                            session.note_change(&turn, file, true, stat);
                        }
                    }
                    session
                        .transcript
                        .update(turn, item.id.clone(), item.kind, |card| {
                            let mut item = item;
                            if item.output.is_empty() {
                                item.output = std::mem::take(&mut card.item.output);
                            }
                            let text_changed = !item.text.is_empty() && card.item.text != item.text;
                            if item.text.is_empty() {
                                item.text = std::mem::take(&mut card.item.text);
                            }
                            if item.files.is_empty() {
                                item.files = std::mem::take(&mut card.item.files);
                            }
                            card.dirty |= card.item.kind != item.kind
                                || text_changed
                                || card.expanded && card.item.output != item.output;
                            card.item = item;
                            card.done |= done;
                        });
                }
            }
            Event::Delta(turn, id, kind, text) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let item_kind = match kind {
                        DeltaKind::Message => ItemKind::Message,
                        DeltaKind::Plan => ItemKind::Plan,
                        DeltaKind::Output => ItemKind::Tool,
                    };
                    session
                        .transcript
                        .update(turn, id, item_kind, |card| match kind {
                            DeltaKind::Message | DeltaKind::Plan => {
                                let before = card.item.text.len();
                                append(&mut card.item.text, &text, MAX_BODY);
                                card.dirty |= card.item.text.len() != before;
                            }
                            DeltaKind::Output => {
                                let before = card.item.output.len();
                                append(&mut card.item.output, &text, MAX_OUTPUT);
                                card.dirty |= card.expanded && card.item.output.len() != before;
                            }
                        });
                }
            }
            Event::Files(turn, id, files, patch) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    session
                        .transcript
                        .update(turn, id, ItemKind::FileChange, |card| {
                            card.item.files = files;
                            card.item.output = patch;
                            card.dirty |= card.expanded;
                        });
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
            Event::ApprovalExpired(id) => {
                if self.approval().is_some_and(|approval| approval.id == id) {
                    self.error = Some(t!("agent-approval-expired").into());
                }
                self.approvals.retain(|approval| approval.id != id);
            }
            Event::SessionTitle(id, title) => self.title_received(&id, title),
            Event::Steered(id, session) => self.acknowledge_submission(&session, id, true),
            Event::Tokens(turn, total, last, window) => {
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    session.tokens = (total, last, window);
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
        let standalone = root.is_none();
        let root = root
            .or_else(|| winc_config::config_path().map(|path| path.with_file_name("codex-chat")));
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
        if panel.root.as_ref() != Some(&root) || panel.standalone != standalone {
            panel.catalog = Catalog::default();
            panel.conversations.clear();
            panel.selected = None;
            panel.attachment = None;
            panel.default_model = None;
            panel.default_effort = None;
        }
        panel.standalone = standalone;
        panel.root = Some(root.clone());
        panel.error = None;
        let Some(proxy) = self.link.proxy.clone() else {
            return;
        };
        let wake = move || {
            let _ = proxy.send_event(UserEvent::Agent);
        };
        let connection = if standalone {
            CodexProvider::connect_standalone(root, executable, wake)
        } else {
            CodexProvider::connect(root, executable, wake)
        };
        match connection {
            Ok(provider) => {
                panel.provider = Some(provider);
                panel.connection = Connection::Connecting;
                panel.error = None;
                for session in panel.conversations.values_mut() {
                    session.loaded = false;
                    session.older_request = None;
                }
                panel.catalog = Catalog::default();
            }
            Err(_) => panel.error = Some(failure(Failure::Start)),
        }
    }

    pub(super) fn agent_timer_tick(&mut self, now: Instant) -> Option<Instant> {
        let panel = self.agent.as_mut()?;
        if !panel.shown || !panel.busy() {
            panel.timer_wake = None;
            return None;
        }
        if let Some(wake) = panel.timer_wake
            && now < wake
        {
            return Some(wake);
        }
        let next = now + Duration::from_millis(33);
        let redraw = panel.timer_wake.is_some();
        panel.timer_wake = Some(next);
        if redraw {
            self.request_redraw();
        }
        Some(next)
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
        let matches = self
            .agent
            .as_ref()
            .is_some_and(|panel| self.agent_matches_workspace(panel));
        if !matches {
            self.flash(t!("agent-project-changed"));
        }
        matches
    }

    pub(crate) fn agent_matches_workspace(&self, panel: &Panel) -> bool {
        match &self.editor.workspace {
            None => panel.standalone,
            Some(workspace) => !panel.standalone && panel.root.as_ref() == Some(&workspace.root),
        }
    }

    pub(crate) fn agent_rects(&self) -> PanelRects {
        self.agent
            .as_ref()
            .filter(|panel| panel.shown)
            .map_or(PanelRects::default(), |panel| panel.rects)
    }

    pub(super) fn agent_caption_controls(&self) -> impl Iterator<Item = Rect> + '_ {
        self.agent
            .iter()
            .filter(|panel| panel.shown && panel.rects.body.w > 0.0)
            .flat_map(|panel| {
                std::iter::once(panel.rects.divider).chain(panel.hits.iter().map(|(rect, _)| *rect))
            })
            .filter(|rect| rect.y < TOP_BAR_END)
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
        let end = left - ISLAND_GAP;
        let top = layout.window.y;
        let height = layout.editor.bottom() - top;
        panel.rects.body = Rect::new(left, top, w, height);
        let divider_top = layout.top_bar.y;
        panel.rects.divider = Rect::new(
            end,
            divider_top,
            ISLAND_GAP,
            layout.editor.bottom() - divider_top,
        );
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

    pub(in crate::app) fn agent_click(&mut self, x: f32, y: f32) -> bool {
        let rects = self.agent_rects();
        if rects.divider.contains(x, y) {
            self.pointer.drag = Drag::AgentDivider {
                grab: x - rects.body.x,
            };
            return true;
        }
        if !rects.body.contains(x, y)
            || self.window_control_at(x, y).is_some()
            || self.resize_edge_at(x, y).is_some()
        {
            return false;
        }
        let Some(panel) = &mut self.agent else {
            return false;
        };
        if panel.field.view.area.contains(x, y) {
            panel.selection = None;
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
                Hit::Close => self.toggle_agent(),
                Hit::Send => self.agent_send(),
                Hit::Complete(index) => self.agent_complete(index),
                Hit::Stop => panel.stop(),
                Hit::Retry => {
                    if panel.connection != Connection::Connecting && !panel.any_active() {
                        self.connect_agent();
                    }
                }
                Hit::CopyMessage(index) => {
                    if let Some(card) = panel.transcript().and_then(|chat| chat.card_mut(index)) {
                        let text = card.item.text.clone();
                        self.set_clipboard(text, false);
                    }
                }
                Hit::CopyCode(text) => self.set_clipboard(text.to_string(), false),
                Hit::Follow => {
                    if let Some(chat) = panel.transcript() {
                        chat.follow();
                    }
                }
                Hit::CopyApproval => {
                    if let Some(approval) = panel.approval() {
                        let text = approval.details.clone();
                        self.set_clipboard(text, false);
                    }
                }
                Hit::Expand(index) => {
                    if let Some(card) = panel.transcript().and_then(|chat| chat.card_mut(index)) {
                        card.expanded = !card.expanded;
                    }
                }
                Hit::ScrollTable(card, section, table, pixels) => {
                    if let Some(layout) = panel
                        .transcript()
                        .and_then(|chat| chat.card_mut(card))
                        .and_then(|card| card.layout.as_mut())
                    {
                        layout.scroll_table(section, table, pixels);
                    }
                }
                Hit::Attach(selection) => self.agent_attach(selection),
                Hit::Detach => panel.attachment = None,
                Hit::Show(tab) => panel.show_tab(if panel.tab == tab { Tab::Chat } else { tab }),
                Hit::ChooseSession(id) => panel.choose(id),
                Hit::ChooseModel(id) => panel.choose_model(id),
                Hit::ChooseEffort(effort) => panel.choose_effort(effort),
                Hit::More => panel.next_page(),
                Hit::Older => panel.load_older(),
                Hit::SwitchProject => {
                    if !panel.any_active() {
                        self.connect_agent();
                    }
                }
                Hit::Open(link, diff) => self.agent_open_link(link, diff),
                Hit::Decide(approval, decision) => {
                    panel.submit(Command::Approve { approval, decision });
                }
            }
        } else if y < TOP_BAR_END {
            return false;
        } else if let Some(hit) = panel.text_hits.iter().find(|hit| hit.rect.contains(x, y)) {
            let point = hit.position(x);
            panel.selection = Some(TextSelection {
                card: hit.card,
                anchor: point,
                head: point,
            });
            self.focus = Focus::Agent;
            self.pointer.drag = Drag::AgentText;
        } else {
            panel.selection = None;
        }
        self.request_redraw();
        true
    }

    pub(in crate::app) fn drag_agent_text(&mut self, x: f32, y: f32) {
        let Some(panel) = &mut self.agent else { return };
        let Some(selection) = &mut panel.selection else {
            return;
        };
        let distance = |value: f32, start: f32, end: f32| (start - value).max(value - end).max(0.0);
        if let Some(hit) = panel
            .text_hits
            .iter()
            .filter(|hit| hit.card == selection.card)
            .min_by(|a, b| {
                let score = |hit: &TextHit| {
                    (
                        (y - hit.rect.y - hit.rect.h * 0.5).abs(),
                        distance(x, hit.rect.x, hit.rect.right()),
                    )
                };
                let a = score(a);
                let b = score(b);
                a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
            })
        {
            selection.head = hit.position(x);
            self.request_redraw();
        }
    }

    pub(super) fn copy_agent_text(&mut self) -> bool {
        let Some(panel) = &mut self.agent else {
            return false;
        };
        let Some(selection) = panel.selection.filter(|s| s.anchor != s.head) else {
            return false;
        };
        let Some(layout) = panel
            .transcript()
            .and_then(|chat| chat.card_mut(selection.card))
            .and_then(|card| card.layout.as_ref())
        else {
            return false;
        };
        let text = layout.selected_text(selection);
        if text.is_empty() {
            return false;
        }
        self.set_clipboard(text, false);
        true
    }

    pub(in crate::app) fn resize_agent(&mut self, x: f32, grab: f32) {
        if let Some(panel) = &mut self.agent {
            panel.width = (panel.rects.body.right() - x + grab).clamp(MIN_WIDTH, MAX_WIDTH);
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
                panel.scroll_viewport(pixels);
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
            None,
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
        let panel = app.agent.as_ref().unwrap();
        assert!(
            !panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyMessage(_)))
        );
        app.pointer.cursor_inside = true;
        app.pointer.mouse = (panel.viewport.x + 10.0, panel.viewport.y + 10.0);
        paint(&mut app);
        let panel = app.agent.as_mut().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyMessage(MAX_CARDS)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyCode(_)))
        );
        panel.event(Event::Approval(Approval {
            id: 55,
            turn: turn.clone(),
            item: "command".into(),
            kind: winc_agent::ApprovalKind::Command,
            details: "echo hello".into(),
            reviewable: true,
            decisions: vec![ApprovalDecision::Accept, ApprovalDecision::Decline],
        }));
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Decide(55, ApprovalDecision::Accept)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Decide(55, ApprovalDecision::Decline)))
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
        let area = panel.viewport;
        assert!(app.scroll_agent(area.x + 5.0, area.y + 5.0, -120.0));
        assert!(
            app.agent
                .as_mut()
                .unwrap()
                .transcript()
                .unwrap()
                .anchor
                .is_some()
        );
    }
    const RICH: &str = "# 项目概览\n\n中英文 **mixed** with `inline code` and [src/main.rs:12](src/main.rs#L12).\n\n> 原生 Markdown 排版\n\n1. 读取文件\n2. Review changes\n\n| Category | Verification | Notes |\n| --- | --- | --- |\n| Markdown | 通过 | Progress and references |\n| Tool output | In progress | Folded by default |\n\n```rust\nfn main() { println!(\"你好\"); }\n```";

    fn test_panel(size: usize) -> (App, TurnKey) {
        let mut app = blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.opening = Some((1, false));
        let key = TurnKey {
            session: "sample".into(),
            turn: "turn".into(),
        };
        panel.event(Event::SessionOpened(
            1,
            Session {
                id: key.session.clone(),
                cwd: PathBuf::new(),
                title: "Winc · Agent transcript".into(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.connection = Connection::Ready;
        panel.error = None;
        for index in 0..size {
            panel.event(Event::Delta(
                key.clone(),
                index.to_string(),
                DeltaKind::Message,
                format!("消息 {index} · reply with `code`."),
            ));
        }
        (app, key)
    }

    #[test]
    fn turn_timer_ticks_until_completion_without_restarting_on_duplicate_start() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        assert!(panel.turn_elapsed().is_none());
        panel.event(Event::TurnStarted(None, turn.clone()));
        let start = Instant::now() - Duration::from_secs(7);
        panel.current_mut().unwrap().timing = Some(TurnTiming::Running(start));
        panel.event(Event::TurnStarted(Some(42), turn.clone()));
        assert!(panel.turn_elapsed().unwrap() >= Duration::from_secs(7));
        assert_eq!(
            app.agent_timer_tick(start + Duration::from_secs(7)),
            Some(start + Duration::from_millis(7033))
        );
        assert_eq!(
            app.agent_timer_tick(start + Duration::from_secs(8)),
            Some(start + Duration::from_millis(8033))
        );

        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::TurnEnded(turn, TurnStatus::Completed));
        assert!(matches!(
            panel.current().unwrap().timing,
            Some(TurnTiming::Finished(elapsed)) if elapsed >= Duration::from_secs(7)
        ));
        assert_eq!(app.agent_timer_tick(Instant::now()), None);
    }

    #[test]
    fn agent_running_layout_keeps_stop_above_the_composer() {
        let (mut app, turn) = test_panel(0);
        app.palette = Palette::for_dark(true);
        let panel = app.agent.as_mut().unwrap();
        panel.catalog.models.push(winc_agent::Model {
            id: "gpt-6.2-sol".into(),
            name: "GPT-6.2-Sol".into(),
            efforts: vec!["low".into()],
            default_effort: Some("low".into()),
            is_default: true,
        });
        panel.event(Event::TurnStarted(None, turn.clone()));
        panel.event(Event::Tokens(turn.clone(), 311_887, 82_938, Some(475_000)));
        let (_, fraction) = panel.token_summary().unwrap();
        assert_eq!(fraction, Some(82_938.0 / 475_000.0));
        for (id, kind, message, done) in [
            ("prompt", ItemKind::User, "整理 Codex 侧边栏的布局", true),
            (
                "response",
                ItemKind::Message,
                "我会调整模型选择、工具卡片和输入框的位置。",
                true,
            ),
            (
                "command",
                ItemKind::Command,
                "cargo test -p winc-app --lib agent",
                true,
            ),
            (
                "file",
                ItemKind::FileChange,
                "crates/winc-app/src/view/agent.rs",
                false,
            ),
        ] {
            let is_file = kind == ItemKind::FileChange;
            panel.event(Event::Item(
                turn.clone(),
                Item {
                    id: id.into(),
                    kind,
                    text: message.into(),
                    output: String::new(),
                    status: if done { "completed" } else { "inProgress" }.into(),
                    files: is_file
                        .then(|| PathBuf::from(message))
                        .into_iter()
                        .collect(),
                    file_stats: is_file
                        .then_some(Some(LineChanges {
                            added: 42,
                            removed: 18,
                        }))
                        .into_iter()
                        .collect(),
                },
                done,
            ));
        }
        panel.field.set_text("再确认一下窄侧栏的布局");
        for width in [240.0, 520.0] {
            app.agent.as_mut().unwrap().width = width;
            if std::env::var_os("WINC_SNAPSHOT_DIR").is_some() {
                crate::app::snapshot::shoot(&mut app, &format!("codex-running-{width}"));
            } else {
                paint(&mut app);
            }
            let panel = app.agent.as_ref().unwrap();
            let stop = panel
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Stop).then_some(rect))
                .unwrap();
            let send = panel
                .hits
                .iter()
                .find_map(|(rect, hit)| matches!(hit, Hit::Send).then_some(rect))
                .unwrap();
            assert!(panel.viewport.bottom() <= stop.y);
            assert!(stop.bottom() < panel.field.view.area.y);
            assert!(send.y >= panel.field.view.area.bottom());
        }
        app.agent
            .as_mut()
            .unwrap()
            .event(Event::TurnEnded(turn, TurnStatus::Completed));
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        assert!(panel.turn_elapsed().is_some());
        assert!(!panel.hits.iter().any(|(_, hit)| matches!(hit, Hit::Stop)));
    }

    #[test]
    fn return_to_bottom_uses_the_rendered_end_of_a_long_message() {
        let (mut app, key) = test_panel(0);
        app.agent.as_mut().unwrap().event(Event::Delta(
            key,
            "long".into(),
            DeltaKind::Message,
            "- A line to scroll\n".repeat(300),
        ));
        paint(&mut app);
        let panel = app.agent.as_mut().unwrap();
        let height = panel.viewport.h;
        let chat = panel.transcript().unwrap();
        let (row, offset) = chat.bottom(height);
        assert_eq!(row, 0);
        assert!(offset > 120.0);
        chat.anchor = Some((chat.first, offset));
        chat.unread = true;
        paint(&mut app);
        let chat = app.agent.as_mut().unwrap().transcript().unwrap();
        assert!(chat.anchor.is_none());
        assert!(!chat.unread);
        let has_button = |app: &App| {
            app.agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Follow))
        };
        assert!(!has_button(&app));

        let area = app.agent.as_ref().unwrap().viewport;
        assert!(app.scroll_agent(area.x + 5.0, area.y + 5.0, -120.0));
        paint(&mut app);
        assert!(has_button(&app));
        assert!(app.scroll_agent(area.x + 5.0, area.y + 5.0, 120.0));
        paint(&mut app);
        assert!(!has_button(&app));
    }

    #[test]
    fn virtual_transcript_keeps_anchors_at_100_1000_and_10000_cards() {
        for size in [100, 1000, 10_000] {
            let (mut app, key) = test_panel(size);
            paint(&mut app);
            let panel = app.agent.as_mut().unwrap();
            let chat = panel.transcript().unwrap();
            assert_eq!(chat.cards.len(), size);
            assert!(
                chat.cards
                    .iter()
                    .filter(|card| card.layout.is_some())
                    .count()
                    < 40
            );
            panel.scroll_viewport(-210.0);
            paint(&mut app);
            let anchor = app
                .agent
                .as_mut()
                .unwrap()
                .transcript()
                .unwrap()
                .anchor
                .unwrap();
            let panel = app.agent.as_mut().unwrap();
            panel.event(Event::Delta(
                key,
                "new".into(),
                DeltaKind::Message,
                RICH.into(),
            ));
            paint(&mut app);
            let panel = app.agent.as_mut().unwrap();
            let chat = panel.transcript().unwrap();
            assert_eq!(chat.anchor, Some(anchor));
            assert!(chat.unread);
            for width in [280.0, 400.0, 600.0, 700.0] {
                app.agent.as_mut().unwrap().width = width;
                paint(&mut app);
                let panel = app.agent.as_mut().unwrap();
                assert_eq!(panel.transcript().unwrap().anchor.unwrap().0, anchor.0);
            }
            app.agent.as_mut().unwrap().transcript().unwrap().follow();
            paint(&mut app);
            let panel = app.agent.as_mut().unwrap();
            assert!(!panel.transcript().unwrap().unread);
            assert!(
                panel
                    .hits
                    .iter()
                    .any(|(_, hit)| matches!(hit, Hit::CopyCode(_)))
            );
        }
    }

    #[test]
    fn agent_rich_snapshot_and_selection() {
        let (mut app, key) = test_panel(0);
        app.agent.as_mut().unwrap().event(Event::Delta(
            key,
            "rich".into(),
            DeltaKind::Message,
            RICH.into(),
        ));
        for width in [280.0, 400.0, 600.0, 700.0] {
            app.agent.as_mut().unwrap().width = width;
            let panel = app.agent.as_mut().unwrap();
            panel.transcript().unwrap().anchor = Some((MAX_CARDS, 0.0));
            if std::env::var_os("WINC_SNAPSHOT_DIR").is_some() {
                crate::app::snapshot::shoot(&mut app, &format!("agent-{width}"));
            } else {
                paint(&mut app);
            }
            let panel = app.agent.as_mut().unwrap();
            let text_hit = panel
                .text_hits
                .iter()
                .find(|hit| hit.point.section == 0)
                .unwrap();
            let point = text_hit.point;
            let point_next = TextPoint {
                offset: point.offset + 2,
                ..point
            };
            let card = panel.transcript().unwrap().cards.front().unwrap();
            let selection = TextSelection {
                card: MAX_CARDS,
                anchor: point,
                head: point_next,
            };
            assert_eq!(
                card.layout.as_ref().unwrap().selected_text(selection),
                "项目"
            );
        }
    }

    #[test]
    #[ignore = "Release performance measurement"]
    fn agent_transcript_release_benchmark() {
        fn rss() -> u64 {
            let output = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap_or_default()
        }
        fn cpu_seconds() -> f64 {
            let output = std::process::Command::new("ps")
                .args(["-o", "time=", "-p", &std::process::id().to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .split(':')
                .fold(0.0, |elapsed, field| {
                    elapsed * 60.0 + field.parse().unwrap_or(0.0)
                })
        }
        fn median_ms(mut times: Vec<std::time::Duration>) -> f64 {
            times.sort_unstable();
            times[times.len() / 2].as_secs_f64() * 1000.0
        }
        let sizes = std::env::var("WINC_AGENT_BENCH_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .map_or_else(|| vec![100, 1000, 10_000], |size| vec![size]);
        for size in sizes {
            let idle = rss();
            let start = std::time::Instant::now();
            let (mut app, key) = test_panel(size);
            paint(&mut app);
            let setup = start.elapsed().as_secs_f64() * 1000.0;
            let visible_rss = rss();
            let mut scroll = Vec::new();
            let mut resize = Vec::new();
            let mut stream = Vec::new();
            for i in 0..60 {
                let start = std::time::Instant::now();
                app.agent.as_mut().unwrap().scroll_viewport(-32.0);
                paint(&mut app);
                scroll.push(start.elapsed());
                let start = std::time::Instant::now();
                app.agent.as_mut().unwrap().width = [280.0, 400.0, 600.0][i % 3];
                paint(&mut app);
                resize.push(start.elapsed());
            }
            app.agent.as_mut().unwrap().transcript().unwrap().follow();
            let cpu_start = cpu_seconds();
            let stream_start = std::time::Instant::now();
            for _ in 0..60 {
                let start = std::time::Instant::now();
                app.agent.as_mut().unwrap().event(Event::Delta(
                    key.clone(),
                    "stream".into(),
                    DeltaKind::Message,
                    format!("{RICH}\n\n"),
                ));
                paint(&mut app);
                stream.push(start.elapsed());
            }
            let cpu_ms = (cpu_seconds() - cpu_start) * 1000.0;
            let stream_wall_ms = stream_start.elapsed().as_secs_f64() * 1000.0;
            let retained = app
                .agent
                .as_mut()
                .unwrap()
                .transcript()
                .unwrap()
                .cards
                .len();
            println!(
                "agent size={size} retained={retained} setup_ms={setup:.3} idle_kib={idle} visible_kib={visible_rss} scroll_ms={:.3} resize_ms={:.3} stream_ms={:.3} stream_cpu_ms={cpu_ms:.3} stream_wall_ms={stream_wall_ms:.3} end_kib={}",
                median_ms(scroll),
                median_ms(resize),
                median_ms(stream),
                rss()
            );
        }
    }
}
