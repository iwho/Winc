//! Window-owned state and event handling for the native Codex panel.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

mod composer;
mod editor;
mod sessions;
mod stream;
pub(crate) use editor::Link;
use sessions::Catalog;
pub(crate) use sessions::Tab;

use winc_agent::{
    AgentProvider, Approval, ApprovalDecision, Backend, Command, DeltaKind, Elicitation,
    ElicitationFieldKind, ElicitationResponse, ElicitationValue, Event, Failure, Item, ItemKind,
    LineChanges, ModeOption, PermissionMode, Session, SlashCommand, SubmitError, TurnKey,
    TurnStatus,
};
use winc_i18n::t;

use super::*;

const MIN_WIDTH: f32 = 240.0;
const MAX_CARDS: usize = 10_000;
const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
const ESTIMATED_HEIGHT: f32 = 52.0;
const MAX_BODY: usize = winc_agent::MAX_PROMPT;
const MAX_OUTPUT: usize = 24 * 1024;

pub(crate) const PROCESS_ROW: f32 = 28.0;
const PROCESS_COLLAPSE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy)]
pub(crate) struct ProcessSummary {
    pub(crate) tools: usize,
    pub(crate) expanded: bool,
}

pub(crate) struct Card {
    pub(crate) turn: TurnKey,
    pub(crate) item: Item,
    pub(crate) done: bool,
    pub(crate) message_time: Option<(i32, i32)>,
    pub(crate) expanded: bool,
    pub(crate) height: f32,
    pub(crate) process_summary: Option<ProcessSummary>,
    pub(crate) process_hidden: bool,
    pub(crate) process_share: f32,
    process_collapse: Option<super::animation::Clock>,
    process_expanded: Option<bool>,
    turn_finished: bool,
    pub(crate) turn_elapsed: Option<Duration>,
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
            process_summary: None,
            process_hidden: false,
            process_share: 1.0,
            process_collapse: None,
            process_expanded: None,
            turn_finished: false,
            turn_elapsed: None,
            dirty: true,
            layout: None,
        }
    }

    fn is_process(&self) -> bool {
        !matches!(
            self.item.kind,
            ItemKind::User | ItemKind::Message | ItemKind::Error
        )
    }

    pub(crate) fn display_height(&self) -> f32 {
        let summary = if self.process_summary.is_some() {
            PROCESS_ROW
        } else {
            0.0
        };
        summary
            + if self.process_hidden {
                0.0
            } else {
                self.height * self.process_share
            }
    }
}

pub(crate) struct DraftImage {
    png: std::sync::Arc<[u8]>,
    pub(crate) preview: winc_preview::image::Pixels,
}

struct Submission {
    queued: bool,
    id: u64,
    steer: bool,
    text: String,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
    image: Option<std::rc::Rc<DraftImage>>,
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

struct BufferedDelta {
    turn: TurnKey,
    id: String,
    kind: DeltaKind,
    text: String,
    offset: usize,
}

impl BufferedDelta {
    fn remaining(&self) -> &str {
        &self.text[self.offset..]
    }

    fn remaining_len(&self) -> usize {
        self.text.len().saturating_sub(self.offset)
    }
}

pub(crate) struct Transcript {
    pub(crate) cards: VecDeque<Card>,
    indices: HashMap<(TurnKey, String), usize>,
    bytes: usize,
    pub(crate) first: usize,
    pub(crate) anchor: Option<(usize, f32)>,
    pub(crate) follow_pos: Option<(usize, f32)>,
    pub(crate) follow_layout: Option<(f32, f32)>,
    pub(crate) follow_last_tick: Option<Instant>,
    pub(crate) follow_smooth: bool,
    pub(crate) follow_animating: bool,
    pub(crate) unread: bool,
    pub(crate) laid: std::ops::Range<usize>,
    process_animating: bool,
    stream_queue: VecDeque<BufferedDelta>,
    stream_bytes: usize,
    stream_last_tick: Option<Instant>,
    stream_budget: f32,
    stream_hold: Option<stream::HoldState>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            cards: VecDeque::new(),
            indices: HashMap::new(),
            bytes: 0,
            first: MAX_CARDS,
            anchor: None,
            follow_pos: None,
            follow_layout: None,
            follow_last_tick: None,
            follow_smooth: false,
            follow_animating: false,
            unread: false,
            laid: 0..0,
            process_animating: false,
            stream_queue: VecDeque::new(),
            stream_bytes: 0,
            stream_last_tick: None,
            stream_budget: 0.0,
            stream_hold: None,
        }
    }
}

impl Transcript {
    pub(crate) fn layout_processes(&mut self, expanded_by_default: bool, animate: bool) {
        self.process_animating = false;
        let mut first = 0;
        while first < self.cards.len() {
            let turn = &self.cards[first].turn;
            let mut end = first;
            let mut leader = None;
            let mut tools = 0;
            while end < self.cards.len() && self.cards[end].turn == *turn {
                let card = &self.cards[end];
                if card.is_process() {
                    leader.get_or_insert(end);
                    tools += usize::from(!matches!(
                        card.item.kind,
                        ItemKind::Commentary | ItemKind::Plan
                    ));
                }
                end += 1;
            }
            let expanded = leader.is_some_and(|index| {
                let lead = &self.cards[index];
                lead.process_expanded
                    .unwrap_or(expanded_by_default && !lead.turn_finished)
            });
            let share = leader.map_or(1.0, |index| {
                let lead = &mut self.cards[index];
                if expanded || !animate {
                    lead.process_collapse = None;
                    return f32::from(expanded);
                }
                if lead.turn_finished
                    && lead.process_expanded.is_none()
                    && lead
                        .process_summary
                        .is_some_and(|previous| previous.expanded)
                {
                    lead.process_collapse = Some(super::animation::Clock::new(PROCESS_COLLAPSE));
                }
                let share = lead.process_collapse.map_or(0.0, |clock| clock.fade_out());
                if share <= 0.0 {
                    lead.process_collapse = None;
                }
                share
            });
            self.process_animating |= !expanded && share > 0.0 && leader.is_some();
            for index in first..end {
                let card = &mut self.cards[index];
                card.process_share = if card.is_process() { share } else { 1.0 };
                card.process_hidden = card.process_share <= 0.0;
                card.process_summary =
                    (Some(index) == leader).then_some(ProcessSummary { tools, expanded });
            }
            first = end;
        }
    }

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
        self.reset_follow_state();
        self.unread = false;
        self.laid = 0..0;
        self.process_animating = false;
        self.stream_queue.clear();
        self.stream_bytes = 0;
        self.stream_last_tick = None;
        self.stream_budget = 0.0;
        self.stream_hold = None;
    }

    pub(crate) fn reset_follow_state(&mut self) {
        self.follow_pos = None;
        self.follow_last_tick = None;
        self.follow_smooth = false;
        self.follow_animating = false;
    }

    pub(crate) fn arm_smooth_follow(&mut self) {
        if self.anchor.is_none() {
            self.follow_smooth = true;
        }
    }

    pub(crate) fn has_stream_pending(&self) -> bool {
        !self.stream_queue.is_empty()
    }

    fn has_buffered_message(&self, turn: &TurnKey) -> bool {
        self.stream_queue.iter().any(|entry| {
            &entry.turn == turn
                && match entry.kind {
                    DeltaKind::Message => !entry.remaining().trim().is_empty(),
                    DeltaKind::Commentary | DeltaKind::Plan | DeltaKind::Output => true,
                }
        })
    }

    fn full_card_text(&self, card: &Card) -> String {
        let mut text = card.item.text.clone();
        for entry in &self.stream_queue {
            if entry.turn == card.turn
                && entry.id == card.item.id
                && matches!(
                    entry.kind,
                    DeltaKind::Message | DeltaKind::Commentary | DeltaKind::Plan
                )
            {
                append(&mut text, entry.remaining(), MAX_BODY);
            }
        }
        text
    }

    fn apply_delta(&mut self, turn: TurnKey, id: String, kind: DeltaKind, text: &str, finished: bool) {
        if text.is_empty() {
            return;
        }
        let item_kind = match kind {
            DeltaKind::Message => ItemKind::Message,
            DeltaKind::Commentary => ItemKind::Commentary,
            DeltaKind::Plan => ItemKind::Plan,
            DeltaKind::Output => ItemKind::Tool,
        };
        self.update(turn, id, item_kind, |card| {
            if finished {
                card.done = true;
                card.turn_finished = true;
            }
            match kind {
                DeltaKind::Message | DeltaKind::Commentary | DeltaKind::Plan => {
                    let before = card.item.text.len();
                    append(&mut card.item.text, text, MAX_BODY);
                    card.dirty |= card.item.text.len() != before;
                }
                DeltaKind::Output => {
                    let before = card.item.output.len();
                    append(&mut card.item.output, text, MAX_OUTPUT);
                    card.dirty |= card.expanded && card.item.output.len() != before;
                }
            }
        });
    }

    fn enqueue_delta(&mut self, turn: TurnKey, id: String, kind: DeltaKind, text: String) {
        if text.is_empty() {
            return;
        }
        let card_len = self
            .indices
            .get(&(turn.clone(), id.clone()))
            .and_then(|&ordinal| self.cards.get(ordinal.saturating_sub(self.first)))
            .map_or(0, |card| card.item.text.len());
        let queued_for_card: usize = self
            .stream_queue
            .iter()
            .filter(|entry| entry.turn == turn && entry.id == id && entry.kind == kind)
            .map(BufferedDelta::remaining_len)
            .sum();
        let allowed = MAX_BODY.saturating_sub(card_len.saturating_add(queued_for_card));
        let slice = winc_agent::prefix(&text, allowed);
        if slice.is_empty() {
            return;
        }
        let added = slice.len();
        if let Some(back) = self
            .stream_queue
            .back_mut()
            .filter(|entry| entry.turn == turn && entry.id == id && entry.kind == kind)
        {
            back.text.push_str(slice);
        } else {
            self.stream_queue.push_back(BufferedDelta {
                turn,
                id,
                kind,
                text: slice.to_owned(),
                offset: 0,
            });
        }
        self.stream_bytes = self.stream_bytes.saturating_add(added);
        while self.stream_bytes > MAX_TRANSCRIPT_BYTES {
            let Some(front) = self.stream_queue.pop_front() else {
                self.stream_bytes = 0;
                break;
            };
            let rest = &front.text[front.offset..];
            self.stream_bytes = self.stream_bytes.saturating_sub(rest.len());
            self.apply_delta(front.turn, front.id, front.kind, rest, false);
        }
    }

    fn stream_backlog_cols(&self) -> f32 {
        let mut cols = 0.0;
        for entry in &self.stream_queue {
            for ch in entry.remaining().chars() {
                cols += stream::char_columns(ch);
                if cols >= 8_000.0 {
                    return cols;
                }
            }
        }
        cols
    }

    fn advance_stream(&mut self, now: Instant, finished_turn: Option<&TurnKey>) -> bool {
        if self.stream_queue.is_empty() {
            self.stream_last_tick = None;
            self.stream_budget = 0.0;
            self.stream_hold = None;
            return false;
        }
        let dt = self
            .stream_last_tick
            .map_or(1.0 / 60.0, |prev| {
                now.saturating_duration_since(prev).as_secs_f32().clamp(0.0, 0.05)
            });
        self.stream_last_tick = Some(now);
        let backlog = self.stream_backlog_cols();
        let extra =
            (backlog - stream::STREAM_RAMP_THRESHOLD).max(0.0) * stream::STREAM_RAMP_GAIN;
        let rate = (stream::STREAM_BASE_COLS_PER_SEC + extra).clamp(
            stream::STREAM_BASE_COLS_PER_SEC,
            stream::STREAM_MAX_COLS_PER_SEC,
        );
        self.stream_budget = (self.stream_budget + rate * dt).min(rate * 0.1);
        let mut released = false;
        while self.stream_budget >= 1.0 {
            let queue_len = self.stream_queue.len();
            let Some((turn, id, kind, rest_len)) = self.stream_queue.front().map(|front| {
                (
                    front.turn.clone(),
                    front.id.clone(),
                    front.kind,
                    front.remaining_len(),
                )
            }) else {
                break;
            };
            if rest_len == 0 {
                self.stream_queue.pop_front();
                self.stream_hold = None;
                continue;
            }
            let is_finished = finished_turn == Some(&turn);
            let allow_wait = !is_finished && queue_len == 1;
            let ctx = if matches!(
                kind,
                DeltaKind::Message | DeltaKind::Commentary | DeltaKind::Plan
            ) {
                let revealed = self
                    .indices
                    .get(&(turn.clone(), id.clone()))
                    .and_then(|&ordinal| self.cards.get(ordinal.saturating_sub(self.first)))
                    .map(|card| card.item.text.as_str())
                    .unwrap_or_default();
                stream::LineContext::from_revealed(revealed)
            } else {
                stream::LineContext::plain_text()
            };
            let can_hold =
                stream::HoldState::can_hold(&mut self.stream_hold, now, rest_len, allow_wait);
            let plan = {
                let front = self.stream_queue.front().expect("checked non-empty front");
                stream::plan_chunk(front.remaining(), &ctx, self.stream_budget, can_hold)
            };
            match plan {
                stream::ChunkPlan::Hold => {
                    stream::HoldState::mark_held(&mut self.stream_hold, now, rest_len);
                    break;
                }
                stream::ChunkPlan::Release { bytes, cost } => {
                    if bytes == 0 {
                        break;
                    }
                    let chunk = {
                        let front = self
                            .stream_queue
                            .front_mut()
                            .expect("checked non-empty front");
                        let chunk = front.remaining()[..bytes].to_owned();
                        front.offset += bytes;
                        if front.offset >= front.text.len() {
                            self.stream_queue.pop_front();
                        }
                        chunk
                    };
                    stream::HoldState::on_release(&mut self.stream_hold, bytes, &chunk);
                    self.stream_bytes = self.stream_bytes.saturating_sub(bytes);
                    self.stream_budget -= cost;
                    self.apply_delta(turn, id, kind, &chunk, is_finished);
                    released = true;
                }
            }
        }
        if released {
            self.arm_smooth_follow();
        }
        if self.stream_queue.is_empty() {
            self.stream_last_tick = None;
            self.stream_budget = 0.0;
            self.stream_hold = None;
        }
        released
    }

    fn flush_stream(&mut self, finished_turn: Option<&TurnKey>) -> bool {
        if self.stream_queue.is_empty() {
            self.stream_last_tick = None;
            self.stream_budget = 0.0;
            self.stream_hold = None;
            return false;
        }
        while let Some(front) = self.stream_queue.pop_front() {
            let rest = front.remaining();
            let is_finished = finished_turn == Some(&front.turn);
            self.apply_delta(front.turn.clone(), front.id.clone(), front.kind, rest, is_finished);
        }
        self.stream_bytes = 0;
        self.stream_last_tick = None;
        self.stream_budget = 0.0;
        self.stream_hold = None;
        true
    }

    fn flush_stream_matching(&mut self, mut predicate: impl FnMut(&BufferedDelta) -> bool, finished_turn: Option<&TurnKey>) {
        if self.stream_queue.is_empty() {
            return;
        }
        let mut flushed_any = false;
        let mut kept = VecDeque::with_capacity(self.stream_queue.len());
        while let Some(entry) = self.stream_queue.pop_front() {
            if predicate(&entry) {
                flushed_any = true;
                let rest = entry.remaining();
                self.stream_bytes = self.stream_bytes.saturating_sub(rest.len());
                let is_finished = finished_turn == Some(&entry.turn);
                self.apply_delta(entry.turn.clone(), entry.id.clone(), entry.kind, rest, is_finished);
            } else {
                kept.push_back(entry);
            }
        }
        self.stream_queue = kept;
        if flushed_any || self.stream_queue.is_empty() {
            self.stream_hold = None;
        }
        if self.stream_queue.is_empty() {
            self.stream_last_tick = None;
            self.stream_budget = 0.0;
        }
    }

    fn reconcile_buffered_item(&mut self, turn: &TurnKey, item: &mut Item) -> bool {
        if !matches!(item.kind, ItemKind::Message | ItemKind::Commentary | ItemKind::Plan) {
            return false;
        }
        let has_buffered = self
            .stream_queue
            .iter()
            .any(|entry| entry.turn == *turn && entry.id == item.id);
        if !has_buffered {
            return false;
        }
        if item.text.is_empty() {
            return true;
        }
        let revealed = self
            .indices
            .get(&(turn.clone(), item.id.clone()))
            .and_then(|&ordinal| self.cards.get(ordinal.saturating_sub(self.first)))
            .map(|card| card.item.text.as_str())
            .unwrap_or_default();
        self.stream_hold = None;
        if let Some(suffix) = item.text.strip_prefix(revealed) {
            let kind = match item.kind {
                ItemKind::Commentary => DeltaKind::Commentary,
                ItemKind::Plan => DeltaKind::Plan,
                _ => DeltaKind::Message,
            };
            let suffix = suffix.to_owned();
            let mut kept = VecDeque::with_capacity(self.stream_queue.len());
            while let Some(entry) = self.stream_queue.pop_front() {
                if entry.turn == *turn && entry.id == item.id {
                    self.stream_bytes = self.stream_bytes.saturating_sub(entry.remaining_len());
                } else {
                    kept.push_back(entry);
                }
            }
            self.stream_queue = kept;
            if !suffix.is_empty() {
                self.enqueue_delta(turn.clone(), item.id.clone(), kind, suffix);
            }
            item.text.clear();
            true
        } else {
            let mut kept = VecDeque::with_capacity(self.stream_queue.len());
            while let Some(entry) = self.stream_queue.pop_front() {
                if entry.turn == *turn && entry.id == item.id {
                    self.stream_bytes = self.stream_bytes.saturating_sub(entry.remaining_len());
                } else {
                    kept.push_back(entry);
                }
            }
            self.stream_queue = kept;
            false
        }
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
                let mut card =
                    Card::new(turn.key.clone(), item, turn.status != TurnStatus::Running);
                card.turn_finished = turn.status != TurnStatus::Running;
                self.cards.push_front(card);
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
            card.dirty = true;
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
            .or(self.follow_pos)
            .map_or((bottom, bottom_offset), |(row, offset)| {
                (row.saturating_sub(self.first).min(last), offset)
            });
        offset += pixels;
        while offset < 0.0 && row > 0 {
            row -= 1;
            offset += self.cards[row].display_height();
        }
        while row < last && offset >= self.cards[row].display_height() {
            offset -= self.cards[row].display_height();
            row += 1;
        }
        if (bottom == 0 && bottom_offset == 0.0)
            || pixels > 0.0 && (row > bottom || (row == bottom && offset >= bottom_offset - 1.0))
        {
            self.follow();
        } else {
            self.reset_follow_state();
            self.flush_stream(None);
            self.anchor = Some((self.first + row, offset.max(0.0)));
        }
    }

    pub(crate) fn bottom(&self, viewport: f32) -> (usize, f32) {
        let mut remaining = viewport;
        for (i, card) in self.cards.iter().enumerate().rev() {
            remaining -= card.display_height();
            if remaining <= 0.0 {
                return (i, -remaining);
            }
        }
        (0, 0.0)
    }

    pub(crate) fn follow(&mut self) {
        self.anchor = None;
        self.unread = false;
        self.reset_follow_state();
        self.flush_stream(None);
    }
}

enum TurnTiming {
    Running(Instant),
    Finished(Duration),
}

#[derive(Clone)]
pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    attachment: Option<std::rc::Rc<editor::Attachment>>,
    image: Option<std::rc::Rc<DraftImage>>,
}

#[derive(Default)]
struct PendingRestore {
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    awaiting_model: Option<String>,
    model_request: Option<u64>,
}

#[derive(Clone, Copy)]
struct Opening {
    request: u64,
    send: bool,
    is_new: bool,
}

struct Conversation {
    queue: VecDeque<QueuedMessage>,
    editing: Option<(usize, QueuedMessage)>,
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
    image: Option<std::rc::Rc<DraftImage>>,
    model: Option<String>,
    effort: Option<String>,
    loaded: bool,
    older_cursor: Option<String>,
    older_request: Option<u64>,
    finished: Option<TurnKey>,
    changes: BTreeMap<PathBuf, editor::Change>,
    change_turn: Option<TurnKey>,
    modes: Vec<ModeOption>,
    mode: Option<String>,
    commands: Vec<SlashCommand>,
    cost: Option<(f64, String)>,
    pending_restore: PendingRestore,
}

impl Conversation {
    fn new(session: Session) -> Self {
        Self {
            session,
            transcript: Transcript::default(),
            queue: VecDeque::new(),
            editing: None,
            running: None,
            timing: None,
            send_request: None,
            stop_request: None,
            status: None,
            tokens: (0, 0, None),
            draft: TextField::multiline(),
            attachment: None,
            image: None,
            model: None,
            effort: None,
            loaded: true,
            older_cursor: None,
            older_request: None,
            finished: None,
            changes: BTreeMap::new(),
            change_turn: None,
            modes: Vec::new(),
            mode: None,
            commands: Vec::new(),
            cost: None,
            pending_restore: PendingRestore::default(),
        }
    }

    fn finish_timing(&mut self) {
        if let Some(TurnTiming::Running(start)) = self.timing {
            self.timing = Some(TurnTiming::Finished(start.elapsed()));
        }
    }

    fn flush_stream(&mut self) -> bool {
        self.transcript.flush_stream(self.finished.as_ref())
    }

    fn advance_stream(&mut self, now: Instant) -> bool {
        self.transcript.advance_stream(now, self.finished.as_ref())
    }
}

pub(crate) struct ElicitationDraft {
    pub(crate) request: Elicitation,
    pub(crate) single: Vec<Option<String>>,
    pub(crate) multi: Vec<Vec<String>>,
    pub(crate) bools: Vec<bool>,
    pub(crate) texts: Vec<TextField>,
    pub(crate) focused_field: Option<usize>,
}

impl ElicitationDraft {
    fn new(request: Elicitation) -> Self {
        let mut single = Vec::with_capacity(request.fields.len());
        let mut multi = Vec::with_capacity(request.fields.len());
        let mut bools = Vec::with_capacity(request.fields.len());
        let mut texts = Vec::with_capacity(request.fields.len());
        for field in &request.fields {
            single.push(match &field.kind {
                ElicitationFieldKind::SingleSelect(_) => field.default_text.clone(),
                _ => None,
            });
            multi.push(field.default_values.clone());
            bools.push(field.default_bool.unwrap_or(false));
            let mut input = TextField::default();
            if matches!(
                field.kind,
                ElicitationFieldKind::Text | ElicitationFieldKind::Number { .. }
            ) && let Some(default) = &field.default_text
            {
                input.set_text(default);
            }
            texts.push(input);
        }
        Self {
            request,
            single,
            multi,
            bools,
            texts,
            focused_field: None,
        }
    }

    fn paired_other_index(&self, field_index: usize) -> Option<usize> {
        let name = &self.request.fields.get(field_index)?.name;
        let other = format!("{name}_other");
        self.request
            .fields
            .iter()
            .position(|field| field.name == other && matches!(field.kind, ElicitationFieldKind::Text))
    }

    fn paired_select_index(&self, other_index: usize) -> Option<usize> {
        let base = self
            .request
            .fields
            .get(other_index)?
            .name
            .strip_suffix("_other")?;
        self.request.fields.iter().position(|field| {
            field.name == base
                && matches!(
                    field.kind,
                    ElicitationFieldKind::SingleSelect(_) | ElicitationFieldKind::MultiSelect(_)
                )
        })
    }

    pub(crate) fn select_single(&mut self, field_index: usize, value: String) {
        if let Some(slot) = self.single.get_mut(field_index) {
            *slot = Some(value);
            if let Some(other) = self.paired_other_index(field_index) {
                self.texts[other].set_text("");
                if self.focused_field == Some(other) {
                    self.focused_field = None;
                }
            }
        }
    }

    pub(crate) fn toggle_multi(&mut self, field_index: usize, value: String) {
        if let Some(list) = self.multi.get_mut(field_index) {
            if let Some(pos) = list.iter().position(|item| item == &value) {
                list.remove(pos);
            } else {
                list.push(value);
            }
        }
    }

    pub(crate) fn toggle_bool(&mut self, field_index: usize) {
        if let Some(flag) = self.bools.get_mut(field_index) {
            *flag = !*flag;
        }
    }

    pub(crate) fn sync_other_text(&mut self) {
        let Some(index) = self.focused_field else {
            return;
        };
        if !self.texts[index].text().trim().is_empty()
            && let Some(select_idx) = self.paired_select_index(index)
            && matches!(
                self.request.fields[select_idx].kind,
                ElicitationFieldKind::SingleSelect(_)
            )
        {
            self.single[select_idx] = None;
        }
    }

    pub(crate) fn can_submit(&self) -> bool {
        self.build_accept().is_some()
    }

    pub(crate) fn build_accept(&self) -> Option<Vec<(String, ElicitationValue)>> {
        let mut out = Vec::new();
        for (index, field) in self.request.fields.iter().enumerate() {
            let has_other = self
                .paired_other_index(index)
                .is_some_and(|other| !self.texts[other].text().trim().is_empty());
            match &field.kind {
                ElicitationFieldKind::SingleSelect(_) => {
                    if let Some(value) = &self.single[index] {
                        out.push((field.name.clone(), ElicitationValue::String(value.clone())));
                    } else if field.required && !has_other {
                        return None;
                    }
                }
                ElicitationFieldKind::MultiSelect(_) => {
                    if !self.multi[index].is_empty() {
                        out.push((
                            field.name.clone(),
                            ElicitationValue::Strings(self.multi[index].clone()),
                        ));
                    } else if field.required && !has_other {
                        return None;
                    }
                }
                ElicitationFieldKind::Boolean => {
                    out.push((field.name.clone(), ElicitationValue::Bool(self.bools[index])));
                }
                ElicitationFieldKind::Text => {
                    let text = self.texts[index].text().trim();
                    if !text.is_empty() {
                        out.push((field.name.clone(), ElicitationValue::String(text.to_owned())));
                    } else if field.required {
                        return None;
                    }
                }
                ElicitationFieldKind::Number { integer } => {
                    let text = self.texts[index].text().trim();
                    if text.is_empty() {
                        if field.required {
                            return None;
                        }
                    } else if *integer {
                        let parsed = text.parse::<i64>().ok()?;
                        out.push((field.name.clone(), ElicitationValue::Integer(parsed)));
                    } else {
                        let parsed = text.parse::<f64>().ok().filter(|n| n.is_finite())?;
                        out.push((field.name.clone(), ElicitationValue::Number(parsed)));
                    }
                }
            }
        }
        Some(out)
    }
}

#[derive(Clone)]
pub(crate) enum Hit {
    ChooseBackend(Backend),
    Split,
    Maximize,
    New,
    Close,
    Send,
    QueueEdit(usize),
    QueueDelete(usize),
    QueueSend(usize),
    Stop,
    Retry,
    CopyMessage(usize),
    CopyCode(std::sync::Arc<str>),
    Follow,
    Complete(usize),
    CopyApproval,
    Expand(usize),
    ToggleProcess(usize),
    ScrollTable(usize, usize, usize, f32),
    Decide(u64, ApprovalDecision),
    Attach(bool),
    Detach,
    RemoveImage,
    TogglePermissions,
    SetPermissions(PermissionMode),
    SetMode(String),
    ToggleWorktrees,
    SelectWorktree(Option<PathBuf>),
    CreateWorktree,
    ElicitSelect(usize, String),
    ElicitToggle(usize, String),
    ElicitBool(usize),
    ElicitFocusField(usize),
    ElicitSubmit,
    ElicitDecline,
    ElicitCancel,
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

pub(crate) enum WorktreeOutcome {
    Listed {
        repo: PathBuf,
        worktrees: Result<Vec<winc_editor::git::Worktree>, ()>,
    },
    Created {
        repo: PathBuf,
        pane_right: bool,
        result: Result<(PathBuf, Vec<winc_editor::git::Worktree>), String>,
    },
}

pub(crate) struct WorktreeChannel {
    sender: mpsc::Sender<WorktreeOutcome>,
    receiver: mpsc::Receiver<WorktreeOutcome>,
}

impl Default for WorktreeChannel {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { sender, receiver }
    }
}

pub(crate) struct Panel {
    pub(crate) rate_limits: Vec<winc_agent::RateLimitWindow>,
    pub(crate) account_tier: Option<String>,
    rate_limits_request: Option<u64>,
    rate_limits_refresh: Option<Instant>,
    pub(crate) shown: bool,
    pub(crate) maximized: bool,
    pub(crate) rects: PanelRects,
    pub(crate) width: f32,
    pub(crate) field: TextField,
    pub(crate) completion: composer::Completion,
    pub(crate) connection: Connection,
    pub(crate) error: Option<String>,
    pub(crate) root: Option<PathBuf>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) git_repo: Option<PathBuf>,
    pub(crate) selected_worktree: Option<PathBuf>,
    pub(crate) worktrees: Vec<winc_editor::git::Worktree>,
    pub(crate) worktree_open: bool,
    pub(crate) worktree_creating: bool,
    pub(crate) standalone: bool,
    pub(crate) hits: Vec<(Rect, Hit)>,
    pub(crate) viewport: Rect,
    pub(crate) queue_viewport: Rect,
    pub(crate) queue_scroll: f32,
    pub(crate) text_hits: Vec<TextHit>,
    pub(crate) selection: Option<TextSelection>,
    pub(crate) scroll: f32,
    pub(crate) max_scroll: f32,
    pub(crate) workspace_parallel: bool,
    pub(crate) attachment: Option<std::rc::Rc<editor::Attachment>>,
    pub(crate) image: Option<std::rc::Rc<DraftImage>>,
    pub(crate) tab: Tab,
    pub(crate) permissions: PermissionMode,
    pub(crate) permissions_open: bool,
    catalog: Catalog,
    default_model: Option<String>,
    default_effort: Option<String>,
    saved_preferences: winc_config::AgentPreferences,
    pub(crate) backend: Backend,
    parked_backends: HashMap<Backend, Box<Panel>>,
    provider: Option<Box<dyn AgentProvider>>,
    accepts_images: bool,
    conversations: HashMap<String, Conversation>,
    selected: Option<String>,
    opening: Option<Opening>,
    approvals: Vec<Approval>,
    elicitations: Vec<ElicitationDraft>,
    timer_wake: Option<Instant>,
}

impl Panel {
    fn new() -> Self {
        Self {
            shown: true,
            rate_limits: Vec::new(),
            account_tier: None,
            rate_limits_request: None,
            rate_limits_refresh: None,
            maximized: false,
            rects: PanelRects::default(),
            width: f32::INFINITY,
            field: TextField::multiline(),
            completion: composer::Completion::default(),
            connection: Connection::Unavailable,
            error: None,
            root: None,
            workspace_root: None,
            git_repo: None,
            selected_worktree: None,
            worktrees: Vec::new(),
            worktree_open: false,
            worktree_creating: false,
            standalone: false,
            hits: Vec::new(),
            viewport: Rect::default(),
            queue_viewport: Rect::default(),
            queue_scroll: 0.0,
            text_hits: Vec::new(),
            selection: None,
            scroll: 0.0,
            max_scroll: 0.0,
            workspace_parallel: false,
            attachment: None,
            image: None,
            tab: Tab::Chat,
            permissions: PermissionMode::Ask,
            permissions_open: false,
            catalog: Catalog::default(),
            default_model: None,
            default_effort: None,
            saved_preferences: winc_config::AgentPreferences::default(),
            backend: Backend::default(),
            parked_backends: HashMap::new(),
            provider: None,
            accepts_images: true,
            conversations: HashMap::new(),
            selected: None,
            opening: None,
            approvals: Vec::new(),
            elicitations: Vec::new(),
            timer_wake: None,
        }
    }

    pub(super) fn apply_saved_preferences(&mut self, state: &winc_config::State) {
        let prefs = state
            .agent_preferences
            .get(self.backend.key())
            .cloned()
            .unwrap_or_default();
        self.saved_preferences = prefs.clone();
        if self.backend == Backend::Codex {
            self.default_model = prefs.model;
            self.default_effort = prefs.effort;
        }
    }

    pub(crate) fn modes(&self) -> &[ModeOption] {
        self.current()
            .map_or(&[], |session| session.modes.as_slice())
    }

    pub(crate) fn commands(&self) -> &[SlashCommand] {
        self.current()
            .map_or(&[], |session| session.commands.as_slice())
    }

    pub(crate) fn current_mode(&self) -> Option<&ModeOption> {
        let session = self.current()?;
        let chosen = session.mode.as_deref();
        session
            .modes
            .iter()
            .find(|mode| Some(mode.id.as_str()) == chosen)
            .or_else(|| session.modes.first())
    }

    pub(crate) fn has_permission_control(&self) -> bool {
        self.backend == Backend::Codex || !self.modes().is_empty()
    }

    pub(crate) fn has_model_control(&self) -> bool {
        self.backend == Backend::Codex || !self.models().is_empty()
    }

    pub(crate) fn has_git_worktree_control(&self) -> bool {
        !self.standalone && self.git_repo.is_some()
    }

    pub(crate) fn worktree_label(&self) -> std::borrow::Cow<'static, str> {
        if self.worktree_creating {
            return std::borrow::Cow::Borrowed(t!("agent-worktree-creating"));
        }
        let Some(selected) = self.selected_worktree.as_deref() else {
            return std::borrow::Cow::Borrowed(t!("agent-worktree-local"));
        };
        if let Some(index) = self.worktrees.iter().position(|w| w.path == selected) {
            if index == 0 {
                return std::borrow::Cow::Borrowed(t!("agent-worktree-main"));
            }
            return std::borrow::Cow::Owned(winc_editor::git::repo_name(
                &self.worktrees[index].path,
            ));
        }
        std::borrow::Cow::Owned(winc_editor::git::repo_name(selected))
    }

    pub(crate) fn worktree_local_detail(&self) -> std::borrow::Cow<'static, str> {
        let local = self.workspace_root.as_deref().or(self.git_repo.as_deref());
        if let Some(local) = local
            && let Some(wt) = self.worktrees.iter().find(|w| w.path == local)
            && let Some(detail) = worktree_head_label(wt)
        {
            return std::borrow::Cow::Owned(detail);
        }
        std::borrow::Cow::Borrowed(t!("agent-worktree-local-detail"))
    }

    pub(crate) fn worktree_entries(&self) -> Vec<(String, String, bool, Option<PathBuf>)> {
        let mut entries = vec![(
            t!("agent-worktree-local").to_owned(),
            self.worktree_local_detail().into_owned(),
            self.selected_worktree.is_none(),
            None,
        )];
        let mut selected_listed = self.selected_worktree.is_none();
        for (index, wt) in self.worktrees.iter().enumerate() {
            if wt.head.is_none() || wt.prunable {
                continue;
            }
            if self.workspace_root.as_deref() == Some(wt.path.as_path())
                || self.git_repo.as_deref() == Some(wt.path.as_path())
            {
                continue;
            }
            let name = if index == 0 {
                t!("agent-worktree-main").to_owned()
            } else {
                winc_editor::git::repo_name(&wt.path)
            };
            let detail = worktree_head_label(wt).unwrap_or_default();
            let selected = self.selected_worktree.as_deref() == Some(wt.path.as_path());
            selected_listed |= selected;
            entries.push((name, detail, selected, Some(wt.path.clone())));
        }
        if !selected_listed
            && let Some(path) = &self.selected_worktree
        {
            entries.push((
                winc_editor::git::repo_name(path),
                String::new(),
                true,
                Some(path.clone()),
            ));
        }
        entries
    }

    pub(crate) fn cost_summary(&self) -> Option<String> {
        let (amount, currency) = self.current()?.cost.as_ref()?;
        if currency.eq_ignore_ascii_case("USD") {
            Some(format!("${amount:.2}"))
        } else {
            Some(format!("{amount:.2} {currency}"))
        }
    }

    pub(crate) fn elicitation(&self) -> Option<&ElicitationDraft> {
        self.elicitations
            .iter()
            .find(|draft| Some(&draft.request.session) == self.selected.as_ref())
    }

    pub(crate) fn elicitation_mut(&mut self) -> Option<&mut ElicitationDraft> {
        let selected = self.selected.as_ref()?;
        self.elicitations
            .iter_mut()
            .find(|draft| &draft.request.session == selected)
    }

    pub(crate) fn elicitation_field_focused(&self) -> bool {
        self.tab == Tab::Chat
            && self.approval().is_none()
            && self
                .elicitation()
                .is_some_and(|draft| draft.focused_field.is_some())
    }

    pub(crate) fn active_field_mut(&mut self) -> &mut TextField {
        if self.tab == Tab::Chat && self.approval().is_none() {
            let selected = self.selected.as_ref();
            if let Some(index) = self.elicitations.iter().position(|draft| {
                Some(&draft.request.session) == selected && draft.focused_field.is_some()
            }) && let Some(field_idx) = self.elicitations[index].focused_field
                && field_idx < self.elicitations[index].texts.len()
            {
                return &mut self.elicitations[index].texts[field_idx];
            }
        }
        &mut self.field
    }

    pub(crate) fn submit_elicitation(&mut self) {
        let Some(draft) = self.elicitation() else {
            return;
        };
        let id = draft.request.id;
        let Some(values) = draft.build_accept() else {
            return;
        };
        self.submit(Command::AnswerElicitation {
            elicitation: id,
            response: ElicitationResponse::Accept(values),
        });
    }

    fn respond_elicitation(&mut self, response: ElicitationResponse) {
        let Some(id) = self.elicitation().map(|draft| draft.request.id) else {
            return;
        };
        self.submit(Command::AnswerElicitation {
            elicitation: id,
            response,
        });
    }

    pub(crate) fn supports_steer(&self) -> bool {
        self.backend == Backend::Codex
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

    pub(crate) fn thinking(&self) -> bool {
        let Some(session) = self.current() else {
            return false;
        };
        let Some(turn) = &session.running else {
            return session
                .send_request
                .as_ref()
                .is_some_and(|request| !request.steer);
        };
        if session.transcript.has_buffered_message(turn) {
            return false;
        }
        !session.transcript.cards.iter().rev().any(|card| {
            &card.turn == turn
                && match card.item.kind {
                    ItemKind::User => false,
                    ItemKind::Message => !card.item.text.trim().is_empty(),
                    _ => true,
                }
        })
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
            || !self.elicitations.is_empty()
    }

    pub(crate) fn can_send(&self) -> bool {
        self.connection == Connection::Ready
            && self.catalog_ready()
            && self.opening.is_none()
            && self.approval().is_none()
            && self.elicitation().is_none()
            && self.current().is_none_or(|session| {
                session.send_request.is_none() && session.stop_request.is_none()
            })
            && self.current().is_none_or(|session| session.loaded)
            && (!self.field.text().trim().is_empty()
                || self.attachment.is_some()
                || self.image.is_some())
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
            Connection::Connecting if self.backend != Backend::Codex => {
                t!("agent-backend-connecting")
            }
            Connection::Connecting => t!("agent-connecting"),
            Connection::Login => match self.backend {
                Backend::Codex => t!("agent-login"),
                Backend::Antigravity => t!("agent-antigravity-login"),
                Backend::Claude | Backend::Gemini => t!("agent-backend-login"),
            },
            Connection::Unavailable if self.backend != Backend::Codex => {
                t!("agent-backend-unavailable")
            }
            Connection::Unavailable => t!("agent-unavailable"),
            Connection::Ready if self.approval().is_some() => t!("agent-awaiting"),
            Connection::Ready if self.elicitation().is_some() => t!("agent-awaiting-input"),
            Connection::Ready if self.opening.is_some() => t!("agent-opening"),
            Connection::Ready if self.busy() => t!("agent-running"),
            Connection::Ready => match self.current().and_then(|s| s.status) {
                Some(TurnStatus::Interrupted) => t!("agent-interrupted"),
                Some(TurnStatus::Completed) => t!("agent-completed"),
                Some(TurnStatus::Failed) if self.backend != Backend::Codex => {
                    t!("agent-backend-failed")
                }
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
        if self.tab == Tab::Chat && self.approval().is_none() && self.elicitation().is_none() {
            let viewport = self.viewport.h;
            if let Some(session) = self.current_mut() {
                session.transcript.scroll(pixels, viewport);
                if session.transcript.anchor.is_some() {
                    session.flush_stream();
                }
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
            self.opening = Some(Opening {
                request: id,
                send,
                is_new: true,
            });
        }
    }

    pub(crate) fn queued(&self) -> impl Iterator<Item = &QueuedMessage> {
        self.current()
            .into_iter()
            .flat_map(|session| session.queue.iter())
    }

    pub(crate) fn editing_queue(&self) -> bool {
        self.current()
            .is_some_and(|session| session.editing.is_some())
    }

    fn draft_message(&self) -> QueuedMessage {
        QueuedMessage {
            text: self.field.text().to_owned(),
            attachment: self.attachment.clone(),
            image: self.image.clone(),
        }
    }

    fn put_draft(&mut self, draft: QueuedMessage) {
        self.field.set_text(&draft.text);
        self.attachment = draft.attachment;
        self.image = draft.image;
    }

    fn enqueue(&mut self) {
        if !self.can_send() {
            return;
        }
        let draft = self.draft_message();
        let Some(session) = self.current_mut() else {
            return;
        };
        if let Some((index, previous)) = session.editing.take() {
            session.queue.insert(index.min(session.queue.len()), draft);
            self.put_draft(previous);
        } else {
            session.queue.push_back(draft);
            self.field.set_text("");
            self.attachment = None;
            self.image = None;
        }
    }

    fn send_queued(&mut self, session_id: &str, index: usize, immediate: bool) {
        let Some(session) = self.conversations.get(session_id) else {
            return;
        };
        if self.connection != Connection::Ready
            || !session.loaded
            || session.send_request.is_some()
            || session.stop_request.is_some()
            || self
                .approvals
                .iter()
                .any(|approval| approval.turn.session == session_id)
            || (session.running.is_some() && (!immediate || !self.supports_steer()))
        {
            return;
        }
        let Some(draft) = session.queue.get(index).cloned() else {
            return;
        };
        let mut prompt = draft.text.clone();
        if let Some(attachment) = &draft.attachment {
            attachment.append_to(&mut prompt);
        }
        let image = draft.image.as_ref().map(|image| image.png.clone());
        let steer = session.running.is_some();
        let command = if let Some(turn) = session.running.clone() {
            Command::Steer {
                turn,
                text: prompt,
                image,
            }
        } else {
            Command::Send {
                session: session_id.to_owned(),
                text: prompt,
                image,
                model: session.model.clone(),
                effort: session.effort.clone(),
                permissions: self.permissions,
            }
        };
        if let Some(id) = self.submit(command) {
            let session = self
                .conversations
                .get_mut(session_id)
                .expect("queued session");
            session.queue.remove(index);
            session.send_request = Some(Submission {
                queued: true,
                id,
                steer,
                text: draft.text,
                attachment: draft.attachment,
                image: draft.image,
            });
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
        let image = self.image.clone();
        let png = image.as_ref().map(|image| image.png.clone());
        let steer = session.running.is_some();
        let command = if let Some(turn) = session.running.clone() {
            Command::Steer {
                turn,
                text: prompt,
                image: png,
            }
        } else {
            Command::Send {
                session: session.session.id.clone(),
                text: prompt,
                image: png,
                model: self.model().map(|model| model.id.clone()),
                effort: self.effort().map(str::to_owned),
                permissions: self.permissions,
            }
        };
        if let Some(id) = self.submit(command) {
            let session = self.current_mut().expect("selected session");
            session.flush_stream();
            session.transcript.reset_follow_state();
            if !steer {
                session.timing = None;
            }
            session.send_request = Some(Submission {
                queued: false,
                id,
                steer,
                text,
                attachment,
                image,
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
        self.seed_title(session_id, &sent.text);
        if sent.queued {
            return;
        }
        let session = self
            .conversations
            .get_mut(session_id)
            .expect("matching session");
        let (field, attachment, image) = if self.selected.as_deref() == Some(session_id) {
            (&mut self.field, &mut self.attachment, &mut self.image)
        } else {
            (
                &mut session.draft,
                &mut session.attachment,
                &mut session.image,
            )
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
        if matches!((&sent.image, &*image),
            (Some(sent), Some(current)) if std::rc::Rc::ptr_eq(sent, current))
        {
            *image = None;
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

    fn flush_all_streams(&mut self) {
        for session in self.conversations.values_mut() {
            session.flush_stream();
            session.transcript.reset_follow_state();
        }
    }

    #[cfg(test)]
    fn event(&mut self, event: Event) {
        self.ingest_event(event, false);
    }

    fn ingest_event(&mut self, event: Event, smooth: bool) {
        match event {
            Event::Ready { login_required } => {
                self.connection = if login_required {
                    Connection::Login
                } else {
                    Connection::Ready
                };
                if login_required {
                    self.error = None;
                    self.opening = None;
                    self.catalog_reset_pending();
                } else {
                    if let Some(selected) = self.selected.clone() {
                        self.choose(selected);
                    } else {
                        self.new_conversation(false);
                    }
                    self.discover();
                    if self.backend == Backend::Codex {
                        self.rate_limits_request = self.submit(Command::RateLimits);
                        self.rate_limits_refresh = Some(Instant::now() + Duration::from_secs(60));
                    }
                }
            }
            Event::ImageInput(supported) => self.accepts_images = supported,
            Event::RateLimits(limits) => {
                self.rate_limits = limits;
                self.rate_limits_request = None;
            }
            Event::Disconnected(error) => {
                self.rate_limits.clear();
                self.account_tier = None;
                self.rate_limits_request = None;
                self.rate_limits_refresh = None;
                self.provider = None;
                self.connection = Connection::Unavailable;
                self.error = Some(failure(self.backend, error));
                self.opening = None;
                self.approvals.clear();
                self.elicitations.clear();
                for session in self.conversations.values_mut() {
                    session.flush_stream();
                    if let Some(sent) = session.send_request.as_ref().filter(|sent| sent.queued) {
                        session.queue.push_front(QueuedMessage {
                            text: sent.text.clone(),
                            attachment: sent.attachment.clone(),
                            image: sent.image.clone(),
                        });
                    }
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
                if self.rate_limits_request == Some(id) {
                    self.rate_limits_request = None;
                    return;
                }
                if self.catalog.skills_request == Some(id) {
                    self.catalog.skills_request = None;
                    return;
                }
                self.error = Some(failure(self.backend, error));
                self.catalog_failed(id);
                if self.opening.is_some_and(|opening| opening.request == id) {
                    self.opening = None;
                }
                for session in self.conversations.values_mut() {
                    if session.pending_restore.model_request == Some(id) {
                        session.pending_restore.model_request = None;
                        session.pending_restore.awaiting_model = None;
                        session.pending_restore.effort = None;
                    }
                    if let Some(sent) = session
                        .send_request
                        .as_ref()
                        .filter(|sent| sent.id == id && sent.queued)
                    {
                        session.queue.push_front(QueuedMessage {
                            text: sent.text.clone(),
                            attachment: sent.attachment.clone(),
                            image: sent.image.clone(),
                        });
                        session.status = Some(TurnStatus::Failed);
                    }
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
            Event::SessionOpened(id, mut session, history, older_cursor) => {
                let Some(opening) = self.opening.filter(|opening| opening.request == id) else {
                    return;
                };
                self.opening = None;
                let send = opening.send;
                let is_new = opening.is_new;
                let key = session.id.clone();
                if session.title.trim().is_empty() {
                    session.title = self
                        .conversations
                        .get(&key)
                        .map(|known| known.session.title.as_str())
                        .filter(|title| !title.trim().is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            self.catalog
                                .history
                                .iter()
                                .find(|entry| entry.id == key && !entry.title.trim().is_empty())
                                .map(|entry| entry.title.clone())
                        })
                        .or_else(|| {
                            history
                                .iter()
                                .flat_map(|turn| &turn.items)
                                .filter(|item| item.kind == ItemKind::User)
                                .find_map(|item| sessions::prompt_title(&item.text))
                        })
                        .unwrap_or_default();
                }
                self.remember_session(&session);
                let conversation = self
                    .conversations
                    .entry(key.clone())
                    .or_insert_with(|| Conversation::new(session.clone()));
                conversation.loaded = true;
                conversation.older_cursor = older_cursor;
                conversation.session = session;
                if is_new && self.backend != Backend::Codex {
                    conversation.pending_restore = PendingRestore {
                        mode: self.saved_preferences.mode.clone(),
                        model: self.saved_preferences.model.clone(),
                        effort: self.saved_preferences.effort.clone(),
                        awaiting_model: None,
                        model_request: None,
                    };
                }
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
                            let mut card = Card::new(
                                turn.key.clone(),
                                item,
                                turn.status != TurnStatus::Running,
                            );
                            card.turn_finished = turn.status != TurnStatus::Running;
                            conversation.transcript.push(card);
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
                    let elapsed = match session.timing {
                        Some(TurnTiming::Finished(elapsed)) => Some(elapsed),
                        _ => None,
                    };
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
                        if !card.turn_finished {
                            card.process_expanded = None;
                            card.turn_finished = true;
                            card.turn_elapsed = elapsed;
                        }
                        if !card.done {
                            card.done = true;
                            card.item.status = terminal.into();
                        }
                    }
                    session.stop_request = None;
                    session.status = Some(status);
                    session.finished = Some(turn.clone());
                    session.transcript.flush_stream_matching(
                        |entry| entry.turn == turn && entry.kind == DeltaKind::Commentary,
                        session.finished.as_ref(),
                    );
                }
            }
            Event::Item(turn, mut item, done) => {
                if item.kind == ItemKind::User {
                    self.seed_title(&turn.session, &item.text);
                }
                let files = self
                    .root
                    .as_deref()
                    .filter(|_| {
                        done && item.kind == ItemKind::FileChange && item.status == "completed"
                    })
                    .map(|root| editor::reported_changes(root, &item))
                    .unwrap_or_default();
                let smooth_session =
                    smooth && self.selected.as_deref() == Some(turn.session.as_str());
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let buffered = session.transcript.reconcile_buffered_item(&turn, &mut item);
                    let turn_finished = session.finished.as_ref() == Some(&turn);
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
                            card.turn_finished |= turn_finished;
                        });
                    if smooth_session && !buffered {
                        session.transcript.arm_smooth_follow();
                    }
                }
            }
            Event::Delta(turn, id, kind, text) => {
                let smooth_session =
                    smooth && self.selected.as_deref() == Some(turn.session.as_str());
                if let Some(session) = self.conversations.get_mut(&turn.session) {
                    let bufferable = matches!(
                        kind,
                        DeltaKind::Message | DeltaKind::Commentary | DeltaKind::Plan
                    );
                    if smooth_session && bufferable && session.transcript.anchor.is_none() {
                        session.transcript.enqueue_delta(turn, id, kind, text);
                        session.transcript.arm_smooth_follow();
                    } else {
                        let is_finished = session.finished.as_ref() == Some(&turn);
                        session
                            .transcript
                            .apply_delta(turn, id, kind, &text, is_finished);
                        if smooth_session {
                            session.transcript.arm_smooth_follow();
                        }
                    }
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
            Event::Skills(id, skills) => self.skills_received(id, skills),
            Event::AccountTier(tier) => self.account_tier = tier,
            Event::SessionCost(session, amount, currency) => {
                if let Some(conv) = self.conversations.get_mut(&session) {
                    conv.cost = Some((amount, currency));
                }
            }
            Event::SessionModes(session, modes, current) => {
                let mut apply_mode = None;
                if let Some(conv) = self.conversations.get_mut(&session) {
                    conv.modes = modes;
                    conv.mode = current;
                    if let Some(target) = conv.pending_restore.mode.take() {
                        if conv.modes.iter().any(|mode| mode.id == target) {
                            if conv.mode.as_deref() != Some(target.as_str()) {
                                conv.mode = Some(target.clone());
                                apply_mode = Some(target);
                            }
                        } else if conv.modes.is_empty() {
                            conv.pending_restore.mode = Some(target);
                        }
                    }
                }
                if let Some(mode) = apply_mode {
                    self.submit(Command::SetMode { session, mode });
                }
            }
            Event::SessionCommands(session, commands) => {
                if let Some(conv) = self.conversations.get_mut(&session) {
                    conv.commands = commands;
                }
            }
            Event::SessionModels {
                session,
                models,
                current_model,
                current_effort,
            } => {
                let is_selected = self.selected.as_deref() == Some(session.as_str())
                    || self.catalog.models.is_empty();
                if is_selected {
                    self.catalog.models = models.clone();
                }
                let mut apply_model = None;
                let mut apply_effort = None;
                let mut effective_model = current_model.clone();
                let mut effective_effort = current_effort.clone();
                if let Some(conv) = self.conversations.get_mut(&session) {
                    conv.model = current_model;
                    conv.effort = current_effort;

                    if let Some(target_model) = conv.pending_restore.model.take() {
                        if models.iter().any(|model| model.id == target_model) {
                            if conv.model.as_deref() != Some(target_model.as_str()) {
                                conv.model = Some(target_model.clone());
                                conv.pending_restore.awaiting_model = Some(target_model.clone());
                                apply_model = Some(target_model);
                            }
                        } else if models.is_empty() {
                            conv.pending_restore.model = Some(target_model);
                        } else {
                            conv.pending_restore.effort = None;
                        }
                    }

                    let waiting_for_model =
                        if let Some(expected) = conv.pending_restore.awaiting_model.clone() {
                            if apply_model.is_none()
                                && conv.model.as_deref() == Some(expected.as_str())
                            {
                                conv.pending_restore.awaiting_model = None;
                                conv.pending_restore.model_request = None;
                                false
                            } else {
                                conv.model = Some(expected);
                                true
                            }
                        } else {
                            false
                        };

                    if apply_model.is_none()
                        && !waiting_for_model
                        && let Some(target_effort) = conv.pending_restore.effort.take()
                    {
                        let active_model = models
                            .iter()
                            .find(|model| Some(model.id.as_str()) == conv.model.as_deref());
                        if let Some(model) = active_model {
                            if model.efforts.iter().any(|effort| effort == &target_effort)
                                && conv.effort.as_deref() != Some(target_effort.as_str())
                            {
                                conv.effort = Some(target_effort.clone());
                                apply_effort = Some(target_effort);
                            }
                        } else if models.is_empty() {
                            conv.pending_restore.effort = Some(target_effort);
                        }
                    }

                    effective_model.clone_from(&conv.model);
                    effective_effort.clone_from(&conv.effort);
                }
                if is_selected {
                    self.default_model = effective_model;
                    self.default_effort = effective_effort;
                }
                if let Some(model) = apply_model {
                    if let Some(req_id) = self.submit(Command::SetModel {
                        session: session.clone(),
                        model,
                    }) {
                        if let Some(conv) = self.conversations.get_mut(&session) {
                            conv.pending_restore.model_request = Some(req_id);
                        }
                    } else if let Some(conv) = self.conversations.get_mut(&session) {
                        conv.pending_restore.awaiting_model = None;
                        conv.pending_restore.effort = None;
                    }
                }
                if let Some(effort) = apply_effort {
                    self.submit(Command::SetEffort { session, effort });
                }
            }
            Event::Elicitation(elicit) => {
                if let Some(known) = self
                    .elicitations
                    .iter_mut()
                    .find(|known| known.request.id == elicit.id)
                {
                    *known = ElicitationDraft::new(elicit);
                    return;
                }
                if self.selected.as_ref() == Some(&elicit.session) {
                    self.show_tab(Tab::Chat);
                }
                if self.elicitations.is_empty() {
                    self.scroll = 0.0;
                }
                self.elicitations.push(ElicitationDraft::new(elicit));
            }
            Event::ElicitationResolved(id) => {
                self.elicitations.retain(|draft| draft.request.id != id);
            }
            Event::ElicitationExpired(id) => {
                if self
                    .elicitation()
                    .is_some_and(|draft| draft.request.id == id)
                {
                    self.error = Some(t!("agent-question-expired").into());
                }
                self.elicitations.retain(|draft| draft.request.id != id);
            }
            Event::Acknowledged(id) => {
                for session in self.conversations.values_mut() {
                    if session.pending_restore.model_request == Some(id) {
                        session.pending_restore.model_request = None;
                    }
                }
            }
        }
    }

    fn completion_notice(&self, event: &Event, pane_focused: bool) -> Option<String> {
        let Event::TurnEnded(turn, TurnStatus::Completed) = event else {
            return None;
        };
        let conversation = self.conversations.get(&turn.session)?;
        if conversation.running.as_ref() != Some(turn)
            || (pane_focused
                && self.shown
                && self.tab == Tab::Chat
                && self.selected.as_deref() == Some(turn.session.as_str()))
        {
            return None;
        }
        let title = conversation.session.title.trim();
        let title = if title.is_empty() {
            winc_agent::prefix(&turn.session, 8)
        } else {
            winc_agent::prefix(title, 160)
        };
        Some(t!("agent-notification-completed-body", session = title))
    }
}

fn append(target: &mut String, text: &str, limit: usize) {
    target.push_str(winc_agent::prefix(text, limit.saturating_sub(target.len())));
}

fn worktree_head_label(wt: &winc_editor::git::Worktree) -> Option<String> {
    match wt.head.as_ref()? {
        winc_editor::git::Head::Branch(name) => Some(name.clone()),
        winc_editor::git::Head::Detached(oid) => {
            Some(t!("agent-worktree-detached", oid = oid.as_str()))
        }
    }
}

fn next_agent_worktree_branch(
    branches: &[winc_editor::git::Branch],
    worktrees: &[winc_editor::git::Worktree],
) -> String {
    let mut index = 1usize;
    loop {
        let candidate = format!("agent-{index}");
        let taken_branch = branches
            .iter()
            .any(|b| !b.remote && b.name == candidate);
        let taken_tree = worktrees
            .iter()
            .any(|w| w.branch() == Some(candidate.as_str()));
        if !taken_branch && !taken_tree {
            return candidate;
        }
        index += 1;
    }
}

fn failure(backend: Backend, error: Failure) -> String {
    match error {
        Failure::ExecutableNotFound => match backend {
            Backend::Codex => t!("agent-no-executable").into(),
            Backend::Claude => t!("agent-no-claude").into(),
            Backend::Gemini => t!("agent-no-gemini").into(),
            Backend::Antigravity => t!("agent-no-antigravity").into(),
        },
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
            panel.permissions_open = false;
            panel.worktree_open = false;
            panel.flush_all_streams();
            panel.hits.clear();
            if self.focus == Focus::Agent {
                self.focus = Focus::Editor;
                self.ime.field_preedit.clear();
            }
            if panel.any_active() {
                self.flash(t!("agent-continues"));
            } else if panel.backend == Backend::Codex {
                panel.provider = None;
            }
        } else {
            let panel = self.agent.get_or_insert_with(|| Box::new(Panel::new()));
            panel.shown = true;
            panel.permissions = self.state.agent_permissions;
            panel.apply_saved_preferences(&self.state);
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
        let workspace_root = self
            .editor
            .workspace
            .as_ref()
            .map(|workspace| workspace.root.clone());
        let standalone = workspace_root.is_none();
        let executable = self.config.agent_executable.as_deref().map(PathBuf::from);
        let known_git_root = self.git.root.clone();
        let known_worktrees = self.git.worktrees.clone();
        let mut refresh_repo = None;
        let Some(panel) = &mut self.agent else { return };
        let standalone_dir = match panel.backend {
            Backend::Codex => "codex-chat",
            Backend::Claude => "claude-chat",
            Backend::Gemini => "gemini-chat",
            Backend::Antigravity => "antigravity-chat",
        };
        panel.permissions_open = false;
        panel.worktree_open = false;
        panel.provider = None;
        panel.accepts_images = panel.backend == Backend::Codex;
        panel.connection = Connection::Unavailable;
        if let Some(ws_root) = &workspace_root {
            if winc_backend::is_remote(ws_root) {
                panel.root = None;
                panel.workspace_root = None;
                panel.git_repo = None;
                panel.selected_worktree = None;
                panel.worktrees.clear();
                panel.error = Some(t!("agent-remote").into());
                return;
            }
            if panel.workspace_root.as_ref() != Some(ws_root) {
                panel.selected_worktree = None;
                panel.worktrees.clear();
                panel.worktree_creating = false;
            }
            let git_repo = winc_editor::git::find_root(ws_root);
            if panel.git_repo != git_repo {
                panel.worktrees.clear();
                if let Some(repo) = &git_repo
                    && known_git_root.as_ref() == Some(repo)
                    && !known_worktrees.is_empty()
                {
                    panel.worktrees = known_worktrees;
                }
            }
            panel.workspace_root = Some(ws_root.clone());
            panel.git_repo = git_repo.clone();
            refresh_repo = git_repo;
        } else {
            panel.workspace_root = None;
            panel.git_repo = None;
            panel.selected_worktree = None;
            panel.worktrees.clear();
            panel.worktree_creating = false;
        }
        let target_root = workspace_root
            .as_ref()
            .map(|ws_root| {
                panel
                    .selected_worktree
                    .clone()
                    .unwrap_or_else(|| ws_root.clone())
            })
            .or_else(|| winc_config::config_path().map(|path| path.with_file_name(standalone_dir)));
        let Some(root) = target_root else {
            panel.root = None;
            panel.error = Some(t!("agent-no-workspace").into());
            return;
        };
        if panel.root.as_ref() != Some(&root) || panel.standalone != standalone {
            panel.catalog = Catalog::default();
            panel.conversations.clear();
            panel.selected = None;
            panel.attachment = None;
            panel.image = None;
            panel.default_model = None;
            panel.default_effort = None;
        }
        panel.apply_saved_preferences(&self.state);
        panel.standalone = standalone;
        panel.root = Some(root.clone());
        panel.error = None;
        if let Some(proxy) = self.link.proxy.clone() {
            let wake = move || {
                let _ = proxy.send_event(UserEvent::Agent);
            };
            let connection = panel.backend.connect(root, standalone, executable, wake);
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
                Err(_) => panel.error = Some(failure(panel.backend, Failure::Start)),
            }
        }
        if refresh_repo.is_some() {
            self.refresh_agent_worktrees();
        }
    }

    pub(super) fn agent_timer_tick(&mut self, now: Instant) -> Option<Instant> {
        let first = self.agent_pane_timer_tick(now);
        let second = if self.agent_peer.is_some() {
            self.swap_agent_panes();
            let wake = self.agent_pane_timer_tick(now);
            self.swap_agent_panes();
            wake
        } else {
            None
        };
        first.into_iter().chain(second).min()
    }

    fn agent_pane_timer_tick(&mut self, now: Instant) -> Option<Instant> {
        let panel = self.agent.as_mut()?;
        let quota_wake = if panel.shown
            && panel.connection == Connection::Ready
            && panel.backend == Backend::Codex
        {
            let due = panel.rate_limits_refresh.unwrap_or(now);
            if now >= due {
                if panel.rate_limits_request.is_none() {
                    panel.rate_limits_request = panel.submit(Command::RateLimits);
                }
                panel.rate_limits_refresh = Some(now + Duration::from_secs(60));
            }
            panel.rate_limits_refresh
        } else {
            None
        };
        let process_animating = panel.tab == Tab::Chat
            && panel.approval().is_none()
            && panel
                .current()
                .is_some_and(|session| session.transcript.process_animating);
        if !panel.shown || !(panel.busy() || process_animating || panel.worktree_creating) {
            panel.timer_wake = None;
            return quota_wake;
        }
        if let Some(wake) = panel.timer_wake
            && now < wake
        {
            return Some(quota_wake.map_or(wake, |quota| quota.min(wake)));
        }
        let next = now + Duration::from_millis(if process_animating { 16 } else { 33 });
        let redraw = panel.timer_wake.is_some();
        panel.timer_wake = Some(next);
        if redraw {
            self.request_redraw();
        }
        Some(quota_wake.map_or(next, |quota| quota.min(next)))
    }

    pub(super) fn advance_agent_stream(&mut self, now: Instant) {
        let smooth = self.animations_on();
        let advance = |panel: &mut Panel| {
            if !panel.shown || panel.tab != Tab::Chat || !smooth {
                panel.flush_all_streams();
                return;
            }
            let selected = panel.selected.as_deref().map(str::to_owned);
            let mut ready_to_dequeue = Vec::new();
            for (id, session) in &mut panel.conversations {
                let had_pending = session.transcript.has_stream_pending();
                if selected.as_deref() == Some(id.as_str()) {
                    session.advance_stream(now);
                } else {
                    session.flush_stream();
                }
                if had_pending
                    && !session.transcript.has_stream_pending()
                    && !session.queue.is_empty()
                    && session.editing.is_none()
                    && session.running.is_none()
                    && session.status == Some(TurnStatus::Completed)
                {
                    ready_to_dequeue.push(id.clone());
                }
            }
            for id in ready_to_dequeue {
                panel.send_queued(&id, 0, false);
            }
        };
        if let Some(panel) = &mut self.agent {
            advance(panel);
        }
        if self.agent_split
            && let Some(peer) = &mut self.agent_peer
        {
            advance(peer);
        }
    }

    pub(super) fn agent_animating(&self) -> bool {
        if !self.animations_on() {
            return false;
        }
        let active = |panel: &Panel| {
            panel.shown
                && panel.tab == Tab::Chat
                && panel.approval().is_none()
                && panel.elicitation().is_none()
                && panel.current().is_some_and(|session| {
                    session.transcript.has_stream_pending()
                        || session.transcript.follow_animating
                        || session.transcript.process_animating
                })
        };
        self.agent.as_deref().is_some_and(active)
            || (self.agent_split && self.agent_peer.as_deref().is_some_and(active))
    }

    pub(super) fn poll_agent(&mut self) {
        let focused = self.window.focused && self.focus == Focus::Agent && !self.settings.open;
        self.poll_agent_pane(focused);
        if self.agent_peer.is_some() {
            self.swap_agent_panes();
            self.poll_agent_pane(false);
            self.swap_agent_panes();
        }
    }

    fn poll_agent_pane(&mut self, focused: bool) {
        let animations_on = self.animations_on();
        let Some(panel) = &mut self.agent else { return };
        let Some(events) = panel.provider.as_mut().map(|provider| provider.drain()) else {
            return;
        };
        let smooth = animations_on
            && panel.shown
            && panel.tab == Tab::Chat
            && panel.approval().is_none()
            && panel.elicitation().is_none();
        let mut hidden_approval = false;
        let mut touched = Vec::new();
        let mut notices = Vec::new();
        for event in events {
            notices.extend(panel.completion_notice(&event, focused));
            hidden_approval |=
                !panel.shown && matches!(event, Event::Approval(_) | Event::Elicitation(_));
            if let Event::Item(_, item, true) = &event
                && item.kind == ItemKind::FileChange
            {
                touched.extend(editor::reported_files(panel.root.as_deref(), &item.files));
            }
            panel.ingest_event(event, smooth);
        }
        if !notices.is_empty() {
            let title = t!("agent-notification-completed").to_owned();
            let _ = std::thread::Builder::new()
                .name("agent-notifications".into())
                .spawn(move || {
                    for body in notices {
                        let _ = notify_rust::Notification::new()
                            .appname("Winc")
                            .summary(&title)
                            .body(&body)
                            .show();
                    }
                });
        }
        let queued_sessions: Vec<_> = panel
            .conversations
            .iter()
            .filter(|(_, session)| {
                !session.queue.is_empty()
                    && session.editing.is_none()
                    && session.running.is_none()
                    && !session.transcript.has_stream_pending()
                    && session.status == Some(TurnStatus::Completed)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in queued_sessions {
            panel.send_queued(&id, 0, false);
        }
        panel.limit_transcripts();
        let shown = panel.shown;
        if !shown && !panel.any_active() && panel.backend == Backend::Codex {
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

    pub(super) fn paste_agent_image(&mut self) -> bool {
        let Some(panel) = &mut self.agent else {
            return false;
        };
        let Some(png) = self.clipboard.png() else {
            return false;
        };
        if !panel.accepts_images {
            panel.error = Some(t!("agent-image-unsupported").into());
        } else if png.len() > 5 * 1024 * 1024 {
            panel.error = Some(t!("agent-image-too-large").into());
        } else {
            match winc_preview::image::decode_bytes(&png) {
                Ok(pixels) => {
                    let scale = (96.0 / pixels.width.max(pixels.height) as f32).min(1.0);
                    let width = ((pixels.width as f32 * scale).round() as u32).max(1);
                    let height = ((pixels.height as f32 * scale).round() as u32).max(1);
                    let preview = winc_preview::image::resize(&pixels, width, height);
                    panel.image = Some(std::rc::Rc::new(DraftImage {
                        png: png.into(),
                        preview,
                    }));
                    if panel.error.as_deref().is_some_and(|error| {
                        error == t!("agent-image-too-large") || error == t!("agent-image-invalid")
                    }) {
                        panel.error = None;
                    }
                }
                Err(_) => panel.error = Some(t!("agent-image-invalid").into()),
            }
        }
        true
    }

    pub(super) fn agent_send(&mut self) {
        if self.ime.field_preedit.is_empty()
            && self.agent_project_matches()
            && let Some(panel) = &mut self.agent
        {
            if panel.can_stop() || panel.editing_queue() {
                panel.enqueue();
            } else {
                panel.send();
            }
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
            Some(workspace) => {
                !panel.standalone
                    && panel.workspace_root.as_ref().or(panel.root.as_ref())
                        == Some(&workspace.root)
            }
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
            .chain(self.agent_peer.iter().filter(|_| self.agent_split))
            .filter(|panel| panel.shown && panel.rects.body.w > 0.0)
            .flat_map(|panel| {
                std::iter::once(panel.rects.divider).chain(panel.hits.iter().map(|(rect, _)| *rect))
            })
            .filter(|rect| rect.y < TOP_BAR_END)
    }

    pub(super) fn layout_agent(&mut self) {
        let available_width = self.agent_available_width();
        let git_full = self.git.panel.full && self.git_panel_shown();
        let Some(panel) = self.agent.as_mut().filter(|panel| panel.shown) else {
            return;
        };
        panel.rects = PanelRects::default();
        if git_full || !self.config.agent_enabled.unwrap_or(true) {
            return;
        }
        let layout = &mut self.layout;
        if panel.maximized {
            panel.rects.body = Rect::new(
                layout.editor.x,
                layout.top_bar.y,
                layout.editor.w,
                layout.editor.bottom() - layout.top_bar.y,
            );
            return;
        }
        let right = layout.editor.right();
        let max_width = (available_width * 0.8).floor();
        let preferred_width = if panel.width.is_finite() {
            panel.width
        } else {
            available_width * 0.5
        };
        let w = preferred_width.min(max_width).round();
        let left = right - w;
        let end = left - ISLAND_GAP;
        let top = layout.top_bar.y;
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
        self.focus_agent_pane_at(x, y);
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
            if let Some(panel) = &mut self.agent
                && (std::mem::take(&mut panel.permissions_open)
                    || std::mem::take(&mut panel.worktree_open))
            {
                self.request_redraw();
            }
            return false;
        }
        let Some(panel) = &mut self.agent else {
            return false;
        };
        if panel.permissions_open {
            panel.permissions_open = false;
            if let Some(hit) = panel.hits.iter().find_map(|(rect, hit)| match hit {
                Hit::SetPermissions(mode) if rect.contains(x, y) => Some(Hit::SetPermissions(*mode)),
                Hit::SetMode(mode) if rect.contains(x, y) => Some(Hit::SetMode(mode.clone())),
                _ => None,
            }) {
                match hit {
                    Hit::SetPermissions(mode) => {
                        panel.permissions = mode;
                        self.update_state(move |state| state.agent_permissions = mode);
                    }
                    Hit::SetMode(mode) => {
                        let backend = panel.backend.key().to_owned();
                        panel.choose_mode(mode.clone());
                        self.update_state(move |state| {
                            state.agent_preferences.entry(backend).or_default().mode = Some(mode);
                        });
                        self.sync_agent_preferences();
                    }
                    _ => {}
                }
            }
            self.request_redraw();
            return true;
        }
        if panel.worktree_open {
            panel.worktree_open = false;
            if let Some(hit) = panel.hits.iter().find_map(|(rect, hit)| match hit {
                Hit::SelectWorktree(target) if rect.contains(x, y) => {
                    Some(Hit::SelectWorktree(target.clone()))
                }
                Hit::CreateWorktree if rect.contains(x, y) => Some(Hit::CreateWorktree),
                _ => None,
            }) {
                match hit {
                    Hit::SelectWorktree(target) => self.choose_agent_worktree(target),
                    Hit::CreateWorktree => self.create_agent_worktree(),
                    _ => {}
                }
            }
            self.request_redraw();
            return true;
        }
        if panel.field.view.area.contains(x, y) {
            if let Some(elicit) = panel.elicitation_mut() {
                elicit.focused_field = None;
            }
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
                Hit::Split => self.toggle_agent_split(),
                Hit::Maximize => {
                    panel.maximized = !panel.maximized;
                    self.relayout();
                    self.request_redraw();
                }
                Hit::Send => self.agent_send(),
                Hit::QueueDelete(index) => {
                    if let Some(session) = panel.current_mut() {
                        session.queue.remove(index);
                    }
                }
                Hit::QueueSend(index) => {
                    if let Some(id) = panel.selected.clone() {
                        panel.send_queued(&id, index, true);
                    }
                }
                Hit::QueueEdit(index) => {
                    if !panel.editing_queue() {
                        let previous = panel.draft_message();
                        if let Some(session) = panel.current_mut()
                            && let Some(draft) = session.queue.remove(index)
                        {
                            session.editing = Some((index, previous));
                            panel.put_draft(draft);
                            self.focus = Focus::Agent;
                        }
                    }
                }
                Hit::Complete(index) => self.agent_complete(index),
                Hit::Stop => panel.stop(),
                Hit::Retry => {
                    if panel.connection != Connection::Connecting && !panel.any_active() {
                        self.connect_agent();
                    }
                }
                Hit::CopyMessage(index) => {
                    if let Some(chat) = panel.transcript()
                        && let Some(local) = index.checked_sub(chat.first)
                        && let Some(card) = chat.cards.get(local)
                    {
                        let text = chat.full_card_text(card);
                        self.set_clipboard(text, false);
                    }
                }
                Hit::CopyCode(text) => self.set_clipboard(text.to_string(), false),
                Hit::Follow => {
                    if let Some(session) = panel.current_mut() {
                        session.flush_stream();
                        session.transcript.follow();
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
                Hit::ToggleProcess(index) => {
                    panel.selection = None;
                    if let Some(card) = panel.transcript().and_then(|chat| chat.card_mut(index))
                        && let Some(summary) = card.process_summary
                    {
                        card.process_expanded = Some(!summary.expanded);
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
                Hit::RemoveImage => panel.image = None,
                Hit::TogglePermissions => {
                    panel.worktree_open = false;
                    panel.permissions_open = true;
                    self.focus = Focus::Agent;
                }
                Hit::SetPermissions(_) | Hit::SetMode(_) => {}
                Hit::ToggleWorktrees => {
                    panel.permissions_open = false;
                    panel.worktree_open = true;
                    self.focus = Focus::Agent;
                    self.refresh_agent_worktrees();
                }
                Hit::SelectWorktree(target) => self.choose_agent_worktree(target),
                Hit::CreateWorktree => self.create_agent_worktree(),
                Hit::ElicitSelect(field, value) => {
                    if let Some(elicit) = panel.elicitation_mut() {
                        elicit.select_single(field, value);
                    }
                }
                Hit::ElicitToggle(field, value) => {
                    if let Some(elicit) = panel.elicitation_mut() {
                        elicit.toggle_multi(field, value);
                    }
                }
                Hit::ElicitBool(field) => {
                    if let Some(elicit) = panel.elicitation_mut() {
                        elicit.toggle_bool(field);
                    }
                }
                Hit::ElicitFocusField(field) => {
                    if let Some(elicit) = panel.elicitation_mut()
                        && field < elicit.texts.len()
                    {
                        elicit.focused_field = Some(field);
                        self.focus = Focus::Agent;
                        self.click_field(x, y);
                    }
                }
                Hit::ElicitSubmit => panel.submit_elicitation(),
                Hit::ElicitDecline => panel.respond_elicitation(ElicitationResponse::Decline),
                Hit::ElicitCancel => panel.respond_elicitation(ElicitationResponse::Cancel),
                Hit::ChooseBackend(backend) => self.choose_agent_backend(backend),
                Hit::Show(tab) => panel.show_tab(if panel.tab == tab { Tab::Chat } else { tab }),
                Hit::ChooseSession(id) => panel.choose(id),
                Hit::ChooseModel(id) => {
                    let backend = panel.backend.key().to_owned();
                    let (changed, effort) = panel.choose_model(id.clone());
                    self.update_state(move |state| {
                        let prefs = state.agent_preferences.entry(backend).or_default();
                        prefs.model = Some(id);
                        if changed {
                            prefs.effort = effort;
                        }
                    });
                    self.sync_agent_preferences();
                }
                Hit::ChooseEffort(effort) => {
                    let backend = panel.backend.key().to_owned();
                    panel.choose_effort(effort.clone());
                    self.update_state(move |state| {
                        state.agent_preferences.entry(backend).or_default().effort = Some(effort);
                    });
                    self.sync_agent_preferences();
                }
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
        let max_width = (self.agent_available_width() * 0.8).floor();
        if let Some(panel) = &mut self.agent {
            panel.width =
                (panel.rects.body.right() - x + grab).clamp(MIN_WIDTH.min(max_width), max_width);
            self.relayout();
            self.request_redraw();
        }
    }

    fn agent_available_width(&self) -> f32 {
        let left = (self.layout.sidebar.right() + ISLAND_GAP).max(ISLAND_MARGIN);
        (self.layout.window.right() - ISLAND_MARGIN - left).max(0.0)
    }

    pub(in crate::app) fn sync_agent_preferences(&mut self) {
        if let Some(panel) = &mut self.agent {
            panel.apply_saved_preferences(&self.state);
            for parked in panel.parked_backends.values_mut() {
                parked.apply_saved_preferences(&self.state);
            }
        }
        if let Some(peer) = &mut self.agent_peer {
            peer.apply_saved_preferences(&self.state);
            for parked in peer.parked_backends.values_mut() {
                parked.apply_saved_preferences(&self.state);
            }
        }
    }

    fn choose_agent_backend(&mut self, backend: Backend) {
        let Some(panel) = self.agent.as_mut() else {
            return;
        };
        if panel.backend == backend {
            panel.show_tab(Tab::Chat);
            return;
        }
        if panel.any_active() {
            return;
        }

        let mut parked = std::mem::take(&mut panel.parked_backends);
        let mut next = parked.remove(&backend).unwrap_or_else(|| {
            let mut next = Box::new(Panel::new());
            next.backend = backend;
            next
        });
        next.permissions = self.state.agent_permissions;
        next.apply_saved_preferences(&self.state);
        next.rects = panel.rects;
        next.shown = panel.shown;
        next.width = panel.width;
        next.maximized = panel.maximized;
        next.workspace_root = panel.workspace_root.clone();
        next.git_repo = panel.git_repo.clone();
        next.selected_worktree = panel.selected_worktree.clone();
        next.worktrees = panel.worktrees.clone();
        next.worktree_creating = panel.worktree_creating;
        next.worktree_open = false;
        next.show_tab(Tab::Chat);
        panel.show_tab(Tab::Chat);
        let old = std::mem::replace(panel, next);
        parked.insert(old.backend, old);
        panel.parked_backends = parked;
        let expected_root = panel
            .selected_worktree
            .as_ref()
            .or(panel.workspace_root.as_ref());
        let reconnect = panel.provider.is_none()
            || (!panel.standalone && panel.root.as_ref() != expected_root);
        if reconnect
            || !self
                .agent
                .as_ref()
                .is_some_and(|panel| self.agent_matches_workspace(panel))
        {
            self.connect_agent();
        } else {
            self.poll_agent();
        }
        self.request_redraw();
    }

    pub(super) fn refresh_agent_worktrees(&mut self) {
        let Some(repo) = self.agent.as_ref().and_then(|panel| panel.git_repo.clone()) else {
            return;
        };
        if self.link.proxy.is_none() && self.git.tool.get().is_none() {
            // In unit tests without a running event loop, only spawn git if a tool was explicitly
            // configured or avoid slowing down unrelated snapshot tests.
            return;
        }
        let sender = self.agent_worktrees.sender.clone();
        self.spawn_git(UserEvent::AgentWorktrees, move |tool| {
            let worktrees = match tool {
                Ok(git) if git.reaches(&repo) => {
                    let cancel = winc_editor::format::Cancel::default();
                    Ok(git.worktrees(&repo, &cancel).unwrap_or_default())
                }
                _ => Err(()),
            };
            let _ = sender.send(WorktreeOutcome::Listed { repo, worktrees });
        });
    }

    pub(crate) fn choose_agent_worktree(&mut self, target: Option<PathBuf>) {
        let Some(panel) = self.agent.as_mut() else {
            return;
        };
        if panel.selected_worktree == target {
            return;
        }
        if panel.any_active() {
            self.flash(t!("agent-worktree-busy"));
            return;
        }
        panel.selected_worktree = target;
        panel.worktree_open = false;
        self.connect_agent();
        self.request_redraw();
    }

    fn create_agent_worktree(&mut self) {
        let Some(panel) = self.agent.as_mut() else {
            return;
        };
        if panel.worktree_creating {
            return;
        }
        if panel.any_active() {
            self.flash(t!("agent-worktree-busy"));
            return;
        }
        let Some(repo) = panel.git_repo.clone() else {
            return;
        };
        panel.worktree_creating = true;
        panel.worktree_open = false;
        let pane_right = self.agent_focus_right;
        let sender = self.agent_worktrees.sender.clone();
        let worktree_root = self.config.git_worktree_root.clone();
        let copy_globs = self.config.git_worktree_copy.clone();
        let trusted_fp = self.state.trusted_repo(&repo);
        self.spawn_git(UserEvent::AgentWorktrees, move |tool| {
            let result = (|| -> Result<(PathBuf, Vec<winc_editor::git::Worktree>), String> {
                let git = tool.map_err(|e| e.to_string())?;
                if !git.reaches(&repo) {
                    return Err(winc_editor::git::Unavailable::NotInstalled.to_string());
                }
                if let Some(fp) = trusted_fp {
                    git.trust(&repo, fp);
                }
                let cancel = winc_editor::format::Cancel::default();
                let existing = git.worktrees(&repo, &cancel).unwrap_or_default();
                let branches = git.branches(&repo, &cancel).unwrap_or_default();
                let main = existing
                    .first()
                    .map(|w| w.path.as_path())
                    .unwrap_or(repo.as_path());
                let branch = next_agent_worktree_branch(&branches, &existing);
                let home = winc_editor::wsl::home_dir(main);
                let dir = winc_editor::git::worktree_dir(
                    main,
                    worktree_root.as_deref(),
                    home.as_deref(),
                    &branch,
                );
                let target = winc_editor::host::arg(&repo, &dir);
                git.operate(
                    &repo,
                    &["worktree", "add", "-b", &branch, "--end-of-options", &target],
                )?;
                let _ = git.copy_ignored(&repo, &dir, &copy_globs, &cancel);
                let refreshed = git.worktrees(&repo, &cancel).unwrap_or_default();
                let dir = winc_config::files::canonicalize(&dir).unwrap_or(dir);
                let path = refreshed
                    .iter()
                    .find(|w| w.branch() == Some(branch.as_str()))
                    .map(|w| w.path.clone())
                    .unwrap_or(dir);
                Ok((path, refreshed))
            })();
            let _ = sender.send(WorktreeOutcome::Created {
                repo,
                pane_right,
                result,
            });
        });
        self.request_redraw();
    }

    pub(in crate::app) fn poll_agent_worktrees(&mut self) {
        let mut outcomes = Vec::new();
        while let Ok(outcome) = self.agent_worktrees.receiver.try_recv() {
            outcomes.push(outcome);
        }
        if outcomes.is_empty() {
            return;
        }
        let apply_list = |panel: &mut Panel,
                          repo: &Path,
                          worktrees: &Result<Vec<winc_editor::git::Worktree>, ()>| {
            if panel.git_repo.as_deref() == Some(repo) {
                match worktrees {
                    Ok(list) => panel.worktrees = list.clone(),
                    Err(()) => {
                        panel.git_repo = None;
                        panel.selected_worktree = None;
                        panel.worktrees.clear();
                        panel.worktree_open = false;
                        panel.worktree_creating = false;
                    }
                }
            }
            for parked in panel.parked_backends.values_mut() {
                if parked.git_repo.as_deref() == Some(repo) {
                    match worktrees {
                        Ok(list) => parked.worktrees = list.clone(),
                        Err(()) => {
                            parked.git_repo = None;
                            parked.selected_worktree = None;
                            parked.worktrees.clear();
                        }
                    }
                }
            }
        };
        for outcome in outcomes {
            match outcome {
                WorktreeOutcome::Listed { repo, worktrees } => {
                    if let Some(panel) = &mut self.agent {
                        apply_list(panel, &repo, &worktrees);
                    }
                    if let Some(peer) = &mut self.agent_peer {
                        apply_list(peer, &repo, &worktrees);
                    }
                }
                WorktreeOutcome::Created {
                    repo,
                    pane_right,
                    result,
                } => {
                    let is_focused = pane_right == self.agent_focus_right;
                    if !is_focused && self.agent_peer.is_some() {
                        self.swap_agent_panes();
                    }
                    if let Some(panel) = &mut self.agent {
                        panel.worktree_creating = false;
                    }
                    match result {
                        Ok((path, list)) => {
                            let ok_list = Ok(list);
                            if let Some(panel) = &mut self.agent {
                                apply_list(panel, &repo, &ok_list);
                            }
                            if let Some(peer) = &mut self.agent_peer {
                                apply_list(peer, &repo, &ok_list);
                            }
                            if self.agent.as_ref().and_then(|p| p.git_repo.as_deref())
                                == Some(repo.as_path())
                            {
                                self.choose_agent_worktree(Some(path));
                            }
                            if self.git.root.as_deref() == Some(repo.as_path()) {
                                self.git_refresh();
                            }
                        }
                        Err(error) => {
                            let message = t!("agent-worktree-create-failed", error = error.as_str());
                            self.flash(message);
                        }
                    }
                    if !is_focused && self.agent_peer.is_some() {
                        self.swap_agent_panes();
                    }
                }
            }
        }
        self.request_redraw();
    }

    fn swap_agent_panes(&mut self) {
        if let (Some(active), Some(peer)) = (&mut self.agent, &mut self.agent_peer) {
            peer.rects = active.rects;
            peer.shown = active.shown;
            peer.width = active.width;
            peer.maximized = active.maximized;
        }
        std::mem::swap(&mut self.agent, &mut self.agent_peer);
        self.agent_focus_right = !self.agent_focus_right;
    }

    fn toggle_agent_split(&mut self) {
        if self.agent_split {
            if let Some(panel) = &mut self.agent {
                panel.width = ((panel.rects.body.w - 1.0) * 0.5).max(0.0).round();
                panel.maximized = false;
            }
        }
        if self.agent_peer.is_none() {
            let mut peer = Box::new(Panel::new());
            if let Some(active) = &self.agent {
                peer.permissions = active.permissions;
                peer.backend = active.backend;
            }
            peer.apply_saved_preferences(&self.state);
            self.agent_peer = Some(peer);
            self.swap_agent_panes();
            self.connect_agent();
            self.swap_agent_panes();
        }
        self.agent_split = !self.agent_split;
        self.relayout();
        self.request_redraw();
    }

    fn focus_agent_pane_at(&mut self, x: f32, y: f32) {
        let body = self.agent_rects().body;
        if self.agent_split
            && body.contains(x, y)
            && (x >= body.x + body.w * 0.5) != self.agent_focus_right
        {
            self.swap_agent_panes();
            self.focus = Focus::Agent;
            self.request_redraw();
        }
    }

    pub(in crate::app) fn scroll_agent(&mut self, x: f32, y: f32, pixels: f32) -> bool {
        self.focus_agent_pane_at(x, y);
        if !self.agent_rects().body.contains(x, y) {
            return false;
        }
        if let Some(panel) = &mut self.agent {
            if panel.queue_viewport.contains(x, y) {
                panel.queue_scroll = (panel.queue_scroll + pixels).max(0.0);
            } else if panel.field.view.area.contains(x, y) {
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
    fn antigravity_renders_modes_models_tier_and_elicitation_dialog() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.backend = Backend::Antigravity;
        panel.event(Event::AccountTier(Some("Google AI Pro".into())));
        panel.event(Event::Tokens(turn.clone(), 0, 0, Some(1_048_576)));
        panel.event(Event::SessionModes(
            turn.session.clone(),
            vec![
                ModeOption {
                    id: "default".into(),
                    name: "Default".into(),
                    description: "Ask before edits".into(),
                },
                ModeOption {
                    id: "auto".into(),
                    name: "Auto".into(),
                    description: "Auto approve safe actions".into(),
                },
            ],
            Some("default".into()),
        ));
        panel.event(Event::SessionModels {
            session: turn.session.clone(),
            models: vec![winc_agent::Model {
                id: "gemini-4-neon".into(),
                name: "Gemini 4 Neon".into(),
                efforts: vec!["low".into(), "high".into()],
                default_effort: Some("high".into()),
                is_default: false,
            }],
            current_model: Some("gemini-4-neon".into()),
            current_effort: Some("high".into()),
        });
        assert!(panel.has_permission_control());
        assert!(panel.has_model_control());
        assert!(panel.token_summary().is_some());

        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::TogglePermissions))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::Show(Tab::Models)))
        );

        // Open mode popup and pick "auto".
        let perm = panel
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::TogglePermissions).then_some(*r))
            .unwrap();
        assert!(app.agent_click(perm.x + perm.w / 2.0, perm.y + perm.h / 2.0));
        paint(&mut app);
        let auto_rect = app
            .agent
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::SetMode(id) if id == "auto").then_some(*r))
            .unwrap();
        assert!(app.agent_click(auto_rect.x + auto_rect.w / 2.0, auto_rect.y + auto_rect.h / 2.0));
        assert_eq!(
            app.agent
                .as_ref()
                .unwrap()
                .current_mode()
                .map(|m| m.id.as_str()),
            Some("auto")
        );

        // Elicitation selection box renders options and custom input.
        app.agent
            .as_mut()
            .unwrap()
            .event(Event::Elicitation(Elicitation {
                id: 7,
                session: turn.session,
                message: "The agent has a question.".into(),
                fields: vec![
                    winc_agent::ElicitationField {
                        name: "q1".into(),
                        title: "Pick a color".into(),
                        description: String::new(),
                        required: true,
                        kind: ElicitationFieldKind::SingleSelect(vec![
                            winc_agent::ElicitationOption {
                                value: "1".into(),
                                label: "Red".into(),
                                description: String::new(),
                            },
                            winc_agent::ElicitationOption {
                                value: "2".into(),
                                label: "Blue".into(),
                                description: String::new(),
                            },
                        ]),
                        default_text: None,
                        default_values: Vec::new(),
                        default_bool: None,
                    },
                    winc_agent::ElicitationField {
                        name: "q1_other".into(),
                        title: "Other".into(),
                        description: "Custom answer".into(),
                        required: false,
                        kind: ElicitationFieldKind::Text,
                        default_text: None,
                        default_values: Vec::new(),
                        default_bool: None,
                    },
                ],
            }));
        paint(&mut app);
        let opt_rect = app
            .agent
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::ElicitSelect(0, v) if v == "2").then_some(*r))
            .unwrap();
        assert!(app.agent_click(opt_rect.x + opt_rect.w / 2.0, opt_rect.y + opt_rect.h / 2.0));
        let draft = app.agent.as_ref().unwrap().elicitation().unwrap();
        assert_eq!(draft.single[0].as_deref(), Some("2"));
        assert!(draft.can_submit());
    }

    #[test]
    fn acp_preferences_save_per_agent_and_restore_on_new_session() {
        use std::cell::RefCell;
        use std::rc::Rc;

        struct RecordingAcp {
            commands: Rc<RefCell<Vec<Command>>>,
            events: Vec<Event>,
        }
        impl AgentProvider for RecordingAcp {
            fn submit(&mut self, command: Command) -> Result<u64, SubmitError> {
                let mut list = self.commands.borrow_mut();
                let id = (list.len() + 2) as u64;
                list.push(command);
                Ok(id)
            }
            fn drain(&mut self) -> Vec<Event> {
                std::mem::take(&mut self.events)
            }
        }

        let click = |app: &mut App, matches_hit: &dyn Fn(&Hit) -> bool| {
            paint(app);
            let rect = app
                .agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .find_map(|(rect, hit)| matches_hit(hit).then_some(*rect))
                .unwrap();
            assert!(app.agent_click(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0));
        };

        let modes = || {
            vec![
                ModeOption {
                    id: "default".into(),
                    name: "Default".into(),
                    description: "Ask before edits".into(),
                },
                ModeOption {
                    id: "auto".into(),
                    name: "Auto".into(),
                    description: "Auto approve".into(),
                },
            ]
        };
        let flash_active_models = || {
            vec![
                winc_agent::Model {
                    id: "gemini-3-flash".into(),
                    name: "Gemini 3 Flash".into(),
                    efforts: vec!["low".into(), "medium".into()],
                    default_effort: Some("medium".into()),
                    is_default: false,
                },
                winc_agent::Model {
                    id: "gemini-4-neon".into(),
                    name: "Gemini 4 Neon".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
            ]
        };
        let neon_active_models = || {
            vec![
                winc_agent::Model {
                    id: "gemini-3-flash".into(),
                    name: "Gemini 3 Flash".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
                winc_agent::Model {
                    id: "gemini-4-neon".into(),
                    name: "Gemini 4 Neon".into(),
                    efforts: vec!["low".into(), "high".into()],
                    default_effort: Some("high".into()),
                    is_default: false,
                },
            ]
        };

        // 1. User chooses mode, model, and effort on Antigravity.
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.backend = Backend::Antigravity;
        panel.event(Event::SessionModes(
            turn.session.clone(),
            modes(),
            Some("default".into()),
        ));
        panel.event(Event::SessionModels {
            session: turn.session.clone(),
            models: flash_active_models(),
            current_model: Some("gemini-3-flash".into()),
            current_effort: Some("medium".into()),
        });

        click(&mut app, &|h| matches!(h, Hit::TogglePermissions));
        click(&mut app, &|h| matches!(h, Hit::SetMode(id) if id == "auto"));
        click(&mut app, &|h| matches!(h, Hit::Show(Tab::Models)));
        click(&mut app, &|h| {
            matches!(h, Hit::ChooseModel(id) if id == "gemini-4-neon")
        });
        app.agent.as_mut().unwrap().event(Event::SessionModels {
            session: turn.session.clone(),
            models: neon_active_models(),
            current_model: Some("gemini-4-neon".into()),
            current_effort: Some("high".into()),
        });
        click(&mut app, &|h| matches!(h, Hit::Show(Tab::Models)));
        click(&mut app, &|h| {
            matches!(h, Hit::ChooseEffort(effort) if effort == "low")
        });

        assert_eq!(
            app.state.agent_preferences.get("antigravity"),
            Some(&winc_config::AgentPreferences {
                mode: Some("auto".into()),
                model: Some("gemini-4-neon".into()),
                effort: Some("low".into()),
            })
        );

        // 2. Claude saves its own preferences independently.
        app.choose_agent_backend(Backend::Claude);
        let claude_panel = app.agent.as_mut().unwrap();
        claude_panel.connection = Connection::Ready;
        claude_panel.opening = Some(Opening {
            request: 10,
            send: false,
            is_new: true,
        });
        claude_panel.event(Event::SessionOpened(
            10,
            Session {
                id: "c1".into(),
                cwd: PathBuf::new(),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        claude_panel.event(Event::SessionModes(
            "c1".into(),
            vec![
                ModeOption {
                    id: "default".into(),
                    name: "Default".into(),
                    description: String::new(),
                },
                ModeOption {
                    id: "acceptEdits".into(),
                    name: "Accept Edits".into(),
                    description: String::new(),
                },
            ],
            Some("default".into()),
        ));
        claude_panel.event(Event::SessionModels {
            session: "c1".into(),
            models: vec![
                winc_agent::Model {
                    id: "claude-haiku".into(),
                    name: "Claude Haiku".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
                winc_agent::Model {
                    id: "claude-sonnet-4".into(),
                    name: "Claude Sonnet 4".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
            ],
            current_model: Some("claude-haiku".into()),
            current_effort: None,
        });
        click(&mut app, &|h| matches!(h, Hit::TogglePermissions));
        click(&mut app, &|h| matches!(h, Hit::SetMode(id) if id == "acceptEdits"));
        click(&mut app, &|h| matches!(h, Hit::Show(Tab::Models)));
        click(&mut app, &|h| {
            matches!(h, Hit::ChooseModel(id) if id == "claude-sonnet-4")
        });

        assert_eq!(
            app.state.agent_preferences.get("claude"),
            Some(&winc_config::AgentPreferences {
                mode: Some("acceptEdits".into()),
                model: Some("claude-sonnet-4".into()),
                effort: None,
            })
        );
        assert_eq!(
            app.state.agent_preferences.get("antigravity").and_then(|p| p.mode.as_deref()),
            Some("auto")
        );

        // 3. Reopen Winc from persisted state.json and create a new Antigravity session.
        let mut reopened = blank_app(false);
        reopened.state = winc_config::State::parse(&app.state.to_json());
        reopened.toggle_agent();
        reopened.choose_agent_backend(Backend::Antigravity);
        let ag_commands = Rc::new(RefCell::new(Vec::new()));
        let panel = reopened.agent.as_mut().unwrap();
        panel.provider = Some(Box::new(RecordingAcp {
            commands: Rc::clone(&ag_commands),
            events: vec![Event::Ready {
                login_required: false,
            }],
        }));
        reopened.poll_agent();
        assert!(matches!(ag_commands.borrow()[0], Command::NewSession));
        ag_commands.borrow_mut().clear();

        let panel = reopened.agent.as_mut().unwrap();
        panel.event(Event::SessionOpened(
            2,
            Session {
                id: "s2".into(),
                cwd: PathBuf::from("/project"),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.event(Event::SessionModes("s2".into(), modes(), Some("default".into())));
        panel.event(Event::SessionModels {
            session: "s2".into(),
            models: flash_active_models(),
            current_model: Some("gemini-3-flash".into()),
            current_effort: Some("medium".into()),
        });

        // Mode and model are restored right away; effort waits for SetModel to complete.
        assert!(ag_commands.borrow().iter().any(|cmd| matches!(
            cmd,
            Command::SetMode { session, mode } if session == "s2" && mode == "auto"
        )));
        assert!(ag_commands.borrow().iter().any(|cmd| matches!(
            cmd,
            Command::SetModel { session, model } if session == "s2" && model == "gemini-4-neon"
        )));
        assert!(
            !ag_commands
                .borrow()
                .iter()
                .any(|cmd| matches!(cmd, Command::SetEffort { .. }))
        );

        // An intermediate SetMode response with the old model must not trigger SetEffort early
        // or flicker the selected model away from gemini-4-neon.
        ag_commands.borrow_mut().clear();
        panel.event(Event::SessionModes("s2".into(), modes(), Some("auto".into())));
        panel.event(Event::SessionModels {
            session: "s2".into(),
            models: flash_active_models(),
            current_model: Some("gemini-3-flash".into()),
            current_effort: Some("medium".into()),
        });
        assert!(ag_commands.borrow().is_empty());
        assert_eq!(panel.model().map(|m| m.id.as_str()), Some("gemini-4-neon"));

        // Once SetModel finishes and reports gemini-4-neon's efforts, SetEffort("low") is sent.
        panel.event(Event::SessionModels {
            session: "s2".into(),
            models: neon_active_models(),
            current_model: Some("gemini-4-neon".into()),
            current_effort: Some("high".into()),
        });
        assert!(ag_commands.borrow().iter().any(|cmd| matches!(
            cmd,
            Command::SetEffort { session, effort } if session == "s2" && effort == "low"
        )));
        assert_eq!(panel.current_mode().map(|m| m.id.as_str()), Some("auto"));
        assert_eq!(panel.model().map(|m| m.id.as_str()), Some("gemini-4-neon"));
        assert_eq!(panel.effort(), Some("low"));

        // 4. Resuming a historical session preserves its own settings without applying saved defaults.
        ag_commands.borrow_mut().clear();
        panel.choose("past-1".into());
        let resume_req = panel.opening.unwrap().request;
        panel.event(Event::SessionOpened(
            resume_req,
            Session {
                id: "past-1".into(),
                cwd: PathBuf::from("/project"),
                title: "Past".into(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        ag_commands.borrow_mut().clear();
        panel.event(Event::SessionModes(
            "past-1".into(),
            modes(),
            Some("default".into()),
        ));
        panel.event(Event::SessionModels {
            session: "past-1".into(),
            models: flash_active_models(),
            current_model: Some("gemini-3-flash".into()),
            current_effort: Some("medium".into()),
        });
        assert!(ag_commands.borrow().is_empty());
        assert_eq!(panel.current_mode().map(|m| m.id.as_str()), Some("default"));
        assert_eq!(panel.model().map(|m| m.id.as_str()), Some("gemini-3-flash"));

        // 5. Switching to Claude in the reopened app restores Claude's own saved preferences.
        reopened.choose_agent_backend(Backend::Claude);
        let claude_commands = Rc::new(RefCell::new(Vec::new()));
        let panel = reopened.agent.as_mut().unwrap();
        panel.provider = Some(Box::new(RecordingAcp {
            commands: Rc::clone(&claude_commands),
            events: vec![Event::Ready {
                login_required: false,
            }],
        }));
        reopened.poll_agent();
        let new_req = reopened.agent.as_ref().unwrap().opening.unwrap().request;
        claude_commands.borrow_mut().clear();
        let panel = reopened.agent.as_mut().unwrap();
        panel.event(Event::SessionOpened(
            new_req,
            Session {
                id: "c2".into(),
                cwd: PathBuf::from("/project"),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.event(Event::SessionModes(
            "c2".into(),
            vec![
                ModeOption {
                    id: "default".into(),
                    name: "Default".into(),
                    description: String::new(),
                },
                ModeOption {
                    id: "acceptEdits".into(),
                    name: "Accept Edits".into(),
                    description: String::new(),
                },
            ],
            Some("default".into()),
        ));
        panel.event(Event::SessionModels {
            session: "c2".into(),
            models: vec![
                winc_agent::Model {
                    id: "claude-haiku".into(),
                    name: "Claude Haiku".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
                winc_agent::Model {
                    id: "claude-sonnet-4".into(),
                    name: "Claude Sonnet 4".into(),
                    efforts: Vec::new(),
                    default_effort: None,
                    is_default: false,
                },
            ],
            current_model: Some("claude-haiku".into()),
            current_effort: None,
        });
        assert!(claude_commands.borrow().iter().any(|cmd| matches!(
            cmd,
            Command::SetMode { session, mode } if session == "c2" && mode == "acceptEdits"
        )));
        assert!(claude_commands.borrow().iter().any(|cmd| matches!(
            cmd,
            Command::SetModel { session, model } if session == "c2" && model == "claude-sonnet-4"
        )));
    }

    #[test]
    fn antigravity_discovers_and_opens_historical_sessions() {
        struct MockAcp {
            commands: Vec<Command>,
            events: Vec<Event>,
        }
        impl AgentProvider for MockAcp {
            fn submit(&mut self, command: Command) -> Result<u64, SubmitError> {
                let id = (self.commands.len() + 2) as u64;
                self.commands.push(command);
                Ok(id)
            }
            fn drain(&mut self) -> Vec<Event> {
                std::mem::take(&mut self.events)
            }
        }

        let mut app = blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.backend = Backend::Antigravity;
        panel.provider = Some(Box::new(MockAcp {
            commands: Vec::new(),
            events: vec![Event::Ready {
                login_required: false,
            }],
        }));
        app.poll_agent();

        // Ready triggers NewSession (id=2) and History(None) (id=3) for Antigravity too.
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::SessionOpened(
            2,
            Session {
                id: "fresh".into(),
                cwd: PathBuf::from("/project"),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.event(Event::History(
            3,
            vec![Session {
                id: "past-1".into(),
                cwd: PathBuf::from("/project"),
                title: "Saved Antigravity Chat".into(),
                model: None,
                effort: None,
            }],
            None,
        ));
        assert_eq!(panel.history().len(), 2);
        assert_eq!(panel.history()[1].id, "past-1");

        // Clicking the historical session in the History tab loads and displays its transcript.
        panel.show_tab(Tab::History);
        paint(&mut app);
        let row = app
            .agent
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::ChooseSession(id) if id == "past-1").then_some(*r))
            .unwrap();
        assert!(app.agent_click(row.x + row.w / 2.0, row.y + row.h / 2.0));

        let turn = TurnKey {
            session: "past-1".into(),
            turn: "history-1".into(),
        };
        app.agent.as_mut().unwrap().event(Event::SessionOpened(
            5,
            Session {
                id: "past-1".into(),
                cwd: PathBuf::from("/project"),
                title: String::new(),
                model: None,
                effort: None,
            },
            vec![winc_agent::Turn {
                key: turn,
                status: TurnStatus::Completed,
                items: vec![
                    Item {
                        id: "u1".into(),
                        kind: ItemKind::User,
                        text: "Previous prompt".into(),
                        output: String::new(),
                        status: String::new(),
                        files: Vec::new(),
                        file_stats: Vec::new(),
                    },
                    Item {
                        id: "a1".into(),
                        kind: ItemKind::Message,
                        text: "Previous answer".into(),
                        output: String::new(),
                        status: String::new(),
                        files: Vec::new(),
                        file_stats: Vec::new(),
                    },
                ],
            }],
            None,
        ));
        let panel = app.agent.as_mut().unwrap();
        assert_eq!(panel.selected_id(), Some("past-1"));
        assert_eq!(panel.session_title(), Some("Saved Antigravity Chat"));
        assert_eq!(panel.transcript().unwrap().cards.len(), 2);
        paint(&mut app);
        assert!(
            !app.agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::CopyMessage(_)))
        );
    }

    #[test]
    fn user_message_bubble_has_symmetric_vertical_padding() {
        let (mut app, turn) = test_panel(0);
        app.agent.as_mut().unwrap().event(Event::Item(
            turn,
            Item {
                id: "u1".into(),
                kind: ItemKind::User,
                text: "Hello".into(),
                output: String::new(),
                status: String::new(),
                files: Vec::new(),
                file_stats: Vec::new(),
            },
            true,
        ));
        paint(&mut app);
        let card = app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .cards
            .front()
            .unwrap();
        // 1 line (20) + top/bottom pad (2*8=16) + footer (16 + 2*4=24) + card gap (8) = 68.
        assert_eq!(card.height, 68.0);
    }

    #[test]
    fn backend_picker_preserves_drafts_and_rejects_switching_during_a_turn() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.field.set_text("codex draft");
        panel.show_tab(Tab::Backends);
        paint(&mut app);
        let panel = app.agent.as_mut().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::ChooseBackend(Backend::Claude)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::ChooseBackend(Backend::Gemini)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::ChooseBackend(Backend::Antigravity)))
        );
        panel.event(Event::TurnStarted(None, turn.clone()));
        app.choose_agent_backend(Backend::Claude);
        let panel = app.agent.as_mut().unwrap();
        assert_eq!(panel.backend, Backend::Codex);
        panel.event(Event::TurnEnded(turn, TurnStatus::Completed));

        app.choose_agent_backend(Backend::Claude);
        let panel = app.agent.as_mut().unwrap();
        assert_eq!(panel.backend, Backend::Claude);
        assert_eq!(panel.field.text(), "");
        panel.field.set_text("claude draft");
        app.choose_agent_backend(Backend::Codex);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "codex draft");
        app.choose_agent_backend(Backend::Claude);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "claude draft");
        app.choose_agent_backend(Backend::Antigravity);
        let panel = app.agent.as_mut().unwrap();
        assert_eq!(panel.backend, Backend::Antigravity);
        assert!(!panel.supports_steer());
        panel.field.set_text("antigravity draft");
        app.choose_agent_backend(Backend::Codex);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "codex draft");
        app.choose_agent_backend(Backend::Antigravity);
        assert_eq!(
            app.agent.as_ref().unwrap().field.text(),
            "antigravity draft"
        );
    }

    #[test]
    fn split_panes_preserve_independent_drafts_and_focus() {
        let (mut app, _) = test_panel(0);
        app.agent.as_mut().unwrap().field.set_text("left draft");
        let mut peer = Box::new(Panel::new());
        peer.field.set_text("right draft");
        app.agent_peer = Some(peer);
        app.agent_split = true;
        paint(&mut app);
        let body = app.agent_rects().body;
        let left_field = app.agent.as_ref().unwrap().field.view.area;
        let right_field = app.agent_peer.as_ref().unwrap().field.view.area;
        assert!(left_field.right() < right_field.x);
        app.focus_agent_pane_at(body.right() - 10.0, body.y + 50.0);
        assert!(app.agent_focus_right);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "right draft");
        app.focus_agent_pane_at(body.x + 10.0, body.y + 50.0);
        assert!(!app.agent_focus_right);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "left draft");
        assert_eq!(app.agent_peer.as_ref().unwrap().field.text(), "right draft");
    }

    #[test]
    fn merging_split_preserves_focused_pane_width_and_exits_maximized() {
        for maximized in [false, true] {
            for focus_right in [false, true] {
                let (mut app, _) = test_panel(0);
                app.agent.as_mut().unwrap().field.set_text("left draft");
                app.agent.as_mut().unwrap().maximized = maximized;
                let mut peer = Box::new(Panel::new());
                peer.field.set_text("right draft");
                app.agent_peer = Some(peer);
                app.agent_split = true;
                app.relayout();
                paint(&mut app);
                let body = app.agent_rects().body;
                let pane_width = ((body.w - 1.0) * 0.5).round();
                if focus_right {
                    app.focus_agent_pane_at(body.right() - 10.0, body.y + 50.0);
                }
                app.toggle_agent_split();
                assert!(!app.agent_split);
                let panel = app.agent.as_ref().unwrap();
                assert!(!panel.maximized);
                assert_eq!(panel.rects.body.w, pane_width);
                assert_eq!(panel.width, pane_width);
                assert_eq!(
                    panel.field.text(),
                    if focus_right {
                        "right draft"
                    } else {
                        "left draft"
                    }
                );
                assert!(app.agent_peer.is_some());
            }
        }
    }

    #[test]
    fn queued_acknowledgement_preserves_the_current_draft() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.field.set_text("same text");
        panel.current_mut().unwrap().send_request = Some(Submission {
            queued: true,
            id: 999,
            steer: false,
            text: "same text".into(),
            attachment: None,
            image: None,
        });
        panel.acknowledge_submission(&turn.session, 999, false);
        assert_eq!(panel.field.text(), "same text");
        assert!(panel.current().unwrap().send_request.is_none());
    }

    #[test]
    fn composer_permission_menu_selects_a_mode() {
        let (mut app, _) = test_panel(0);
        let click = |app: &mut App, matches_hit: fn(&Hit) -> bool| {
            paint(app);
            let rect = app
                .agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .find_map(|(rect, hit)| matches_hit(hit).then_some(*rect))
                .unwrap();
            assert!(app.agent_click(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0));
        };
        click(&mut app, |hit| matches!(hit, Hit::TogglePermissions));
        click(&mut app, |hit| {
            matches!(hit, Hit::SetPermissions(PermissionMode::AutoReview))
        });
        let panel = app.agent.as_ref().unwrap();
        assert_eq!(panel.permissions, PermissionMode::AutoReview);
        assert!(!panel.permissions_open);
        let mut reopened = blank_app(false);
        reopened.state = winc_config::State::parse(&app.state.to_json());
        reopened.toggle_agent();
        assert_eq!(
            reopened.agent.as_ref().unwrap().permissions,
            PermissionMode::AutoReview
        );
    }

    #[test]
    fn pasted_image_appears_in_the_composer_and_can_be_removed() {
        let (mut app, _) = test_panel(0);
        app.focus = Focus::Agent;
        app.clipboard.copied_image = winc_editor::paste::encode_png(1, 1, &[255, 0, 0, 255]);
        app.paste();
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        assert!(panel.image.is_some());
        let remove = panel
            .hits
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::RemoveImage).then_some(*rect))
            .unwrap();
        assert!(app.agent_click(remove.x + remove.w / 2.0, remove.y + remove.h / 2.0));
        assert!(app.agent.as_ref().unwrap().image.is_none());
    }

    #[test]
    fn renders_streamed_code_and_exposes_approval_actions() {
        let mut app = blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.opening = Some(Opening {
            request: 4,
            send: false,
            is_new: true,
        });
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
            panel
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
        panel.opening = Some(Opening {
            request: 1,
            send: false,
            is_new: true,
        });
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
    fn completion_notice_requires_a_running_turn_outside_the_focused_chat() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::TurnStarted(None, turn.clone()));
        let completed = Event::TurnEnded(turn.clone(), TurnStatus::Completed);
        assert!(panel.completion_notice(&completed, true).is_none());
        assert!(
            panel
                .completion_notice(&completed, false)
                .unwrap()
                .contains(panel.session_title().unwrap())
        );
        panel.show_tab(Tab::History);
        assert!(panel.completion_notice(&completed, true).is_some());
        assert!(
            panel
                .completion_notice(&Event::TurnEnded(turn, TurnStatus::Failed), false)
                .is_none()
        );
        panel.event(completed);
        let completed = Event::TurnEnded(
            panel.current().unwrap().finished.clone().unwrap(),
            TurnStatus::Completed,
        );
        assert!(panel.completion_notice(&completed, false).is_none());
    }

    #[test]
    fn sidebar_title_uses_the_first_message_until_the_session_is_named() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.current_mut().unwrap().session.title.clear();
        let entry = |text: &str| Item {
            id: text.into(),
            kind: ItemKind::User,
            text: text.into(),
            output: String::new(),
            status: String::new(),
            files: Vec::new(),
            file_stats: Vec::new(),
        };
        panel.event(Event::Item(
            turn.clone(),
            entry("\n First question\nDetails"),
            true,
        ));
        panel.event(Event::Item(turn.clone(), entry("Follow-up"), true));
        assert_eq!(panel.session_title(), Some("First question"));
        panel.event(Event::SessionTitle(
            turn.session,
            Some("Named session".into()),
        ));
        assert_eq!(panel.session_title(), Some("Named session"));
    }

    #[test]
    fn completed_process_reduces_its_height_without_shrinking_the_summary() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::TurnStarted(None, turn.clone()));
        panel.event(Event::Delta(
            turn.clone(),
            "progress".into(),
            DeltaKind::Commentary,
            "Checking the files".into(),
        ));
        let chat = panel.transcript().unwrap();
        chat.layout_processes(true, true);
        assert_eq!(
            chat.cards[0].display_height(),
            PROCESS_ROW + ESTIMATED_HEIGHT
        );

        panel.event(Event::TurnEnded(turn, TurnStatus::Completed));
        let chat = panel.transcript().unwrap();
        chat.layout_processes(true, true);
        assert!(chat.process_animating);
        chat.cards[0].process_collapse = Some(super::super::animation::Clock::stopped(
            PROCESS_COLLAPSE,
            PROCESS_COLLAPSE / 2,
        ));
        chat.layout_processes(true, true);
        assert_eq!(
            chat.cards[0].display_height(),
            PROCESS_ROW + ESTIMATED_HEIGHT / 2.0
        );

        chat.cards[0].process_collapse = Some(super::super::animation::Clock::stopped(
            PROCESS_COLLAPSE,
            PROCESS_COLLAPSE,
        ));
        chat.layout_processes(true, true);
        assert_eq!(chat.cards[0].display_height(), PROCESS_ROW);
        assert!(!chat.process_animating);
    }

    #[test]
    fn execution_process_collapses_on_completion_and_can_be_reopened() {
        let (mut app, turn) = test_panel(0);
        app.config.ui_animations = Some(false);
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::TurnStarted(None, turn.clone()));
        panel.event(Event::Delta(
            turn.clone(),
            "progress".into(),
            DeltaKind::Commentary,
            "Checking the files".into(),
        ));
        panel.event(Event::Delta(
            turn.clone(),
            "tool".into(),
            DeltaKind::Output,
            "Tool output".into(),
        ));
        let state = |app: &mut App| {
            paint(app);
            let panel = app.agent.as_mut().unwrap();
            let toggle = panel.hits.iter().find_map(|(rect, hit)| {
                matches!(hit, Hit::ToggleProcess(_))
                    .then_some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
            });
            let chat = panel.transcript().unwrap();
            let summary = chat.cards[0].process_summary.unwrap();
            assert_eq!(summary.tools, 1);
            assert_eq!(chat.cards[1].display_height() > 0.0, summary.expanded);
            (summary.expanded, toggle.unwrap())
        };
        assert!(state(&mut app).0);
        app.config.agent_expand_process = Some(false);
        let (expanded, (x, y)) = state(&mut app);
        assert!(!expanded);
        assert!(app.agent_click(x, y));
        assert!(state(&mut app).0);

        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::Delta(
            turn.clone(),
            "final".into(),
            DeltaKind::Message,
            "Finished".into(),
        ));
        assert!(state(&mut app).0);
        app.agent
            .as_mut()
            .unwrap()
            .event(Event::TurnEnded(turn, TurnStatus::Completed));
        let (expanded, (x, y)) = state(&mut app);
        assert!(!expanded);
        let panel = app.agent.as_mut().unwrap();
        let chat = panel.transcript().unwrap();
        assert!(chat.cards[0].turn_elapsed.is_some());
        assert!(!chat.cards[2].process_hidden);
        assert!(app.agent_click(x, y));
        assert!(state(&mut app).0);
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
        let quota_refresh = app.agent.as_ref().unwrap().rate_limits_refresh;
        assert_eq!(app.agent_timer_tick(Instant::now()), quota_refresh);
        assert!(app.agent.as_ref().unwrap().timer_wake.is_none());
    }

    #[test]
    fn thinking_stays_visible_until_the_first_message_text() {
        let (mut app, turn) = test_panel(0);
        let panel = app.agent.as_mut().unwrap();
        assert!(!panel.thinking());
        panel.event(Event::TurnStarted(None, turn.clone()));
        assert!(panel.thinking());

        for text in [" ", "Hello"] {
            panel.event(Event::Delta(
                turn.clone(),
                "message".into(),
                DeltaKind::Message,
                text.into(),
            ));
            assert_eq!(panel.thinking(), text.trim().is_empty());
        }

        panel.event(Event::TurnEnded(turn, TurnStatus::Completed));
        assert!(!panel.thinking());
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

    #[test]
    fn large_stream_burst_buffers_and_drains_smoothly_preserving_copy_and_reconciliation() {
        let (mut app, turn) = test_panel(0);
        app.config.ui_animations = Some(true);
        let panel = app.agent.as_mut().unwrap();
        panel.event(Event::TurnStarted(None, turn.clone()));
        assert!(panel.thinking());

        let burst = "这是一段一次性大批量输出的中文与 English 文本。\n".repeat(40);
        panel.ingest_event(
            Event::Delta(turn.clone(), "m1".into(), DeltaKind::Message, burst.clone()),
            true,
        );
        // Thinking indicator hides immediately even before the first tick drains text.
        assert!(!panel.thinking());
        assert!(app.agent_animating());

        // First tick releases only a small slice, not the whole burst.
        let t0 = Instant::now();
        app.advance_agent_stream(t0);
        let revealed_1 = app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .cards
            .front()
            .unwrap()
            .item
            .text
            .len();
        assert!(revealed_1 > 0 && revealed_1 < burst.len() / 5);
        assert!(app.agent_animating());

        // Copying mid-stream still yields the complete received text.
        {
            let chat = app.agent.as_mut().unwrap().transcript().unwrap();
            let card = chat.cards.front().unwrap();
            assert_eq!(chat.full_card_text(card), burst);
        }

        // Codex-style final Item completion reconciles without dumping the remaining buffer.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Item(
                turn.clone(),
                Item {
                    id: "m1".into(),
                    kind: ItemKind::Message,
                    text: burst.clone(),
                    output: String::new(),
                    status: "completed".into(),
                    files: Vec::new(),
                    file_stats: Vec::new(),
                },
                true,
            ),
            true,
        );
        let after_item_len = app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .cards
            .front()
            .unwrap()
            .item
            .text
            .len();
        assert_eq!(after_item_len, revealed_1);

        // TurnEnded lets the final Message keep draining smoothly.
        app.agent
            .as_mut()
            .unwrap()
            .ingest_event(Event::TurnEnded(turn, TurnStatus::Completed), true);
        assert!(app.agent_animating());

        // Advance frame by frame until completely drained.
        let mut now = t0;
        let mut ticks = 0;
        while app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .has_stream_pending()
        {
            now += Duration::from_millis(16);
            app.advance_agent_stream(now);
            ticks += 1;
            assert!(ticks < 1200, "stream buffer should finish in bounded time");
        }
        assert!(ticks > 5, "burst should span multiple frames, got {ticks}");
        let final_text = &app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .cards
            .front()
            .unwrap()
            .item
            .text;
        assert_eq!(final_text, &burst);
        assert!(!app.agent_animating());
    }

    #[test]
    fn markdown_link_holds_across_deltas_and_releases_atomically_without_stalling_on_unclosed() {
        let (mut app, turn) = test_panel(0);
        app.config.ui_animations = Some(true);
        app.agent
            .as_mut()
            .unwrap()
            .event(Event::TurnStarted(None, turn.clone()));

        let card_text = |app: &mut App| {
            app.agent
                .as_mut()
                .unwrap()
                .transcript()
                .unwrap()
                .cards
                .front()
                .map(|c| c.item.text.clone())
                .unwrap_or_default()
        };

        // 1. Prefix releases cleanly while incomplete link `[agent` is held.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Delta(turn.clone(), "m1".into(), DeltaKind::Message, "请查看 [agent".into()),
            true,
        );
        let mut now = Instant::now();
        for _ in 0..5 {
            now += Duration::from_millis(16);
            app.advance_agent_stream(now);
        }
        assert_eq!(card_text(&mut app), "请查看 ");

        // 2. Partial link target continues holding.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Delta(
                turn.clone(),
                "m1".into(),
                DeltaKind::Message,
                ".rs](file:///C:/repo/agent".into(),
            ),
            true,
        );
        now += Duration::from_millis(16);
        app.advance_agent_stream(now);
        assert_eq!(card_text(&mut app), "请查看 ");

        // 3. Closing `)` arrives: entire `[agent.rs](file:///C:/repo/agent.rs)` releases in one tick.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Delta(
                turn.clone(),
                "m1".into(),
                DeltaKind::Message,
                ".rs) 的改动".into(),
            ),
            true,
        );
        let mut saw_atomic_link = false;
        while app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .has_stream_pending()
        {
            now += Duration::from_millis(16);
            app.advance_agent_stream(now);
            let revealed = card_text(&mut app);
            if revealed.len() > "请查看 ".len() {
                assert!(
                    revealed.starts_with("请查看 [agent.rs](file:///C:/repo/agent.rs)"),
                    "partial link leaked to UI: {revealed:?}"
                );
                saw_atomic_link = true;
                break;
            }
        }
        assert!(saw_atomic_link);
        while app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .has_stream_pending()
        {
            now += Duration::from_millis(16);
            app.advance_agent_stream(now);
        }
        assert_eq!(card_text(&mut app), "请查看 [agent.rs](file:///C:/repo/agent.rs) 的改动");

        // 4. Unclosed `[unclosed` falls back cleanly after STREAM_HOLD_IDLE_TIMEOUT.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Delta(turn, "m1".into(), DeltaKind::Message, " [unclosed".into()),
            true,
        );
        now += Duration::from_millis(16);
        app.advance_agent_stream(now);
        assert_eq!(card_text(&mut app), "请查看 [agent.rs](file:///C:/repo/agent.rs) 的改动 ");

        for _ in 0..40 {
            if !app
                .agent
                .as_mut()
                .unwrap()
                .transcript()
                .unwrap()
                .has_stream_pending()
            {
                break;
            }
            now += Duration::from_millis(16);
            app.advance_agent_stream(now);
        }
        assert!(!app.agent.as_mut().unwrap().transcript().unwrap().has_stream_pending());
        assert_eq!(
            card_text(&mut app),
            "请查看 [agent.rs](file:///C:/repo/agent.rs) 的改动 [unclosed"
        );
    }

    #[test]
    fn smooth_follow_glides_toward_bottom_and_detaches_without_jump_on_scroll_up() {
        let (mut app, turn) = test_panel(0);
        app.config.ui_animations = Some(true);
        app.agent.as_mut().unwrap().event(Event::Delta(
            turn.clone(),
            "base".into(),
            DeltaKind::Message,
            "Seed line\n".repeat(40),
        ));
        paint(&mut app);
        let initial_pos = app
            .agent
            .as_mut()
            .unwrap()
            .transcript()
            .unwrap()
            .follow_pos
            .unwrap();

        // Append a large chunk via smooth path (e.g. a completed item) so target moves down.
        app.agent.as_mut().unwrap().ingest_event(
            Event::Item(
                turn,
                Item {
                    id: "extra".into(),
                    kind: ItemKind::Message,
                    text: "More content line\n".repeat(30),
                    output: String::new(),
                    status: "completed".into(),
                    files: Vec::new(),
                    file_stats: Vec::new(),
                },
                true,
            ),
            true,
        );
        paint(&mut app);
        let mid_pos = {
            let chat = app.agent.as_mut().unwrap().transcript().unwrap();
            assert!(chat.anchor.is_none());
            assert!(chat.follow_animating);
            chat.follow_pos.unwrap()
        };
        assert!(app.agent_animating());
        // Should have advanced smoothly past initial_pos without snapping all the way to bottom.
        assert!(mid_pos.0 > initial_pos.0 || mid_pos.1 > initial_pos.1);
        let bottom = {
            let panel = app.agent.as_mut().unwrap();
            let h = panel.viewport.h;
            let chat = panel.transcript().unwrap();
            let (row, off) = chat.bottom(h);
            (chat.first + row, off)
        };
        assert!(mid_pos.0 < bottom.0 || (mid_pos.0 == bottom.0 && mid_pos.1 < bottom.1));

        // Scrolling up mid-glide anchors relative to the visual mid_pos instead of jumping to bottom.
        let area = app.agent.as_ref().unwrap().viewport;
        assert!(app.scroll_agent(area.x + 5.0, area.y + 5.0, -40.0));
        let chat = app.agent.as_mut().unwrap().transcript().unwrap();
        assert!(!chat.follow_animating);
        let anchor = chat.anchor.unwrap();
        assert!(anchor.0 < bottom.0 || (anchor.0 == bottom.0 && anchor.1 < bottom.1 - 40.0));
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("winc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app_in_dir(path: &Path) -> App {
        let mut app = blank_app(false);
        app.editor.open_folder(path).unwrap();
        app.relayout();
        app
    }

    #[test]
    fn worktree_selector_hidden_without_git_and_shown_left_of_elapsed_with_git() {
        // Standalone panel has no git selector.
        let (mut standalone_app, _) = test_panel(0);
        paint(&mut standalone_app);
        assert!(
            !standalone_app
                .agent
                .as_ref()
                .unwrap()
                .has_git_worktree_control()
        );
        assert!(
            !standalone_app
                .agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::ToggleWorktrees))
        );

        // Non-git folder has no git selector.
        let non_git = test_temp_dir("agent-no-git-worktree");
        let mut plain_app = app_in_dir(&non_git);
        plain_app.toggle_agent();
        paint(&mut plain_app);
        assert!(!plain_app.agent.as_ref().unwrap().has_git_worktree_control());
        assert!(
            !plain_app
                .agent
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .any(|(_, hit)| matches!(hit, Hit::ToggleWorktrees))
        );
        let _ = std::fs::remove_dir_all(&non_git);

        // Folder with .git shows the selector to the left of the elapsed time and stop button.
        let git_dir = test_temp_dir("agent-with-git-worktree");
        std::fs::create_dir_all(git_dir.join(".git")).unwrap();
        let mut app = app_in_dir(&git_dir);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.width = 520.0;
        panel.opening = Some(Opening {
            request: 1,
            send: false,
            is_new: true,
        });
        let key = TurnKey {
            session: "s1".into(),
            turn: "t1".into(),
        };
        panel.event(Event::SessionOpened(
            1,
            Session {
                id: key.session.clone(),
                cwd: git_dir.clone(),
                title: "Git test".into(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.connection = Connection::Ready;
        panel.event(Event::TurnStarted(None, key));
        paint(&mut app);

        let panel = app.agent.as_ref().unwrap();
        assert!(panel.has_git_worktree_control());
        let wt_rect = panel
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::ToggleWorktrees).then_some(*r))
            .expect("worktree toggle button should be visible for git project");
        let stop_rect = panel
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::Stop).then_some(*r))
            .expect("stop button should be visible while running");
        // Worktree selector sits on the same status row and strictly to the left of elapsed + stop.
        assert!((wt_rect.y - (stop_rect.y - 2.0)).abs() < 1.0);
        assert!(wt_rect.right() < stop_rect.x - 20.0);
        let _ = std::fs::remove_dir_all(&git_dir);
    }

    #[test]
    fn worktree_selector_switches_between_local_and_worktree_independently_per_pane() {
        let base = test_temp_dir("agent-wt-switch");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let wt_path = base.with_file_name(format!("agent-wt-switch-feat-{}", std::process::id()));
        std::fs::create_dir_all(&wt_path).unwrap();

        let mut app = app_in_dir(&base);
        let editor_root = app.editor.root().unwrap().to_path_buf();
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.width = 560.0;
        panel.connection = Connection::Ready;
        panel.worktrees = vec![
            winc_editor::git::Worktree {
                path: editor_root.clone(),
                head: Some(winc_editor::git::Head::Branch("main".into())),
                oid: Some("1111111".into()),
                locked: false,
                prunable: false,
            },
            winc_editor::git::Worktree {
                path: wt_path.clone(),
                head: Some(winc_editor::git::Head::Branch("feat-ui".into())),
                oid: Some("2222222".into()),
                locked: false,
                prunable: false,
            },
        ];

        // Open worktree popup via click.
        paint(&mut app);
        let toggle = app
            .agent
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find_map(|(r, h)| matches!(h, Hit::ToggleWorktrees).then_some(*r))
            .unwrap();
        assert!(app.agent_click(toggle.x + toggle.w / 2.0, toggle.y + toggle.h / 2.0));
        assert!(app.agent.as_ref().unwrap().worktree_open);
        paint(&mut app);

        let panel = app.agent.as_ref().unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::SelectWorktree(None)))
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::CreateWorktree))
        );
        let wt_row = panel
            .hits
            .iter()
            .find_map(|(r, h)| {
                matches!(h, Hit::SelectWorktree(Some(p)) if p == &wt_path).then_some(*r)
            })
            .expect("linked worktree row should appear in popup");

        // Switch active pane to the linked worktree.
        assert!(app.agent_click(wt_row.x + wt_row.w / 2.0, wt_row.y + wt_row.h / 2.0));
        let panel = app.agent.as_ref().unwrap();
        assert!(!panel.worktree_open);
        assert_eq!(panel.selected_worktree.as_ref(), Some(&wt_path));
        assert_eq!(panel.root.as_ref(), Some(&wt_path));
        assert!(app.agent_matches_workspace(panel));
        // Editor stays on the original local folder.
        assert_eq!(app.editor.root(), Some(editor_root.as_path()));

        // Split pane starts independently on Local.
        app.toggle_agent_split();
        assert!(app.agent_split);
        let peer = app.agent_peer.as_ref().unwrap();
        assert_eq!(peer.selected_worktree, None);
        assert_eq!(peer.root.as_ref(), Some(&editor_root));
        assert_eq!(
            app.agent.as_ref().unwrap().selected_worktree.as_ref(),
            Some(&wt_path)
        );

        // Switching is blocked while a turn is active.
        let panel = app.agent.as_mut().unwrap();
        panel.opening = Some(Opening {
            request: 1,
            send: false,
            is_new: true,
        });
        let turn = TurnKey {
            session: "active".into(),
            turn: "t1".into(),
        };
        panel.event(Event::SessionOpened(
            1,
            Session {
                id: turn.session.clone(),
                cwd: wt_path.clone(),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.event(Event::TurnStarted(None, turn.clone()));
        app.choose_agent_worktree(None);
        assert_eq!(
            app.agent.as_ref().unwrap().selected_worktree.as_ref(),
            Some(&wt_path)
        );

        // Once turn ends, switching back to Local works.
        app.agent
            .as_mut()
            .unwrap()
            .event(Event::TurnEnded(turn, TurnStatus::Completed));
        app.choose_agent_worktree(None);
        let panel = app.agent.as_ref().unwrap();
        assert_eq!(panel.selected_worktree, None);
        assert_eq!(panel.root.as_ref(), Some(&editor_root));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&wt_path);
    }

    #[test]
    fn worktree_create_creates_real_git_worktree_and_switches_agent_to_it() {
        let base = test_temp_dir("agent-wt-create");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run_git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
        };
        let Ok(init) = run_git(&["init", "-b", "main"]) else {
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        if !init.status.success() {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let _ = run_git(&["config", "user.email", "test@example.com"]);
        let _ = run_git(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        let _ = run_git(&["add", "."]);
        let commit = run_git(&["commit", "-m", "initial"]).unwrap();
        assert!(commit.status.success());

        let mut app = app_in_dir(&repo);
        app.format = crate::app::FormatState::new(winc_editor::format::Formatter::with_vars(
            std::env::vars_os().collect(),
        ));
        let editor_root = app.editor.root().unwrap().to_path_buf();
        app.toggle_agent();
        assert!(app.agent.as_ref().unwrap().has_git_worktree_control());
        assert_eq!(app.agent.as_ref().unwrap().selected_worktree, None);

        app.create_agent_worktree();
        let panel = app.agent.as_ref().unwrap();
        assert!(!panel.worktree_creating);
        let created = panel
            .selected_worktree
            .clone()
            .unwrap_or_else(|| {
                panic!(
                    "new worktree should be created and selected; flash = {:?}",
                    app.status.message
                )
            });
        assert!(created.is_dir());
        assert_ne!(created, editor_root);
        assert_eq!(panel.root.as_ref(), Some(&created));
        assert_eq!(app.editor.root(), Some(editor_root.as_path()));
        assert!(panel.worktrees.len() >= 2);

        let _ = std::fs::remove_dir_all(&base);
    }
}
