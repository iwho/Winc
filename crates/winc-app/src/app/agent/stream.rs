//! Markdown-aware atomic chunking for smooth Agent transcript streaming.
//!
//! Buffers incomplete Markdown syntax (such as `[label](url)`, inline code,
//! emphasis/bold, ATX headings, list/quote markers, fenced code headers, and
//! table rows) briefly until their closing token arrives so partial syntax does
//! not flicker and re-layout across frames.

use std::time::{Duration, Instant};

pub(super) const STREAM_BASE_COLS_PER_SEC: f32 = 140.0;
pub(super) const STREAM_MAX_COLS_PER_SEC: f32 = 360.0;
pub(super) const STREAM_RAMP_THRESHOLD: f32 = 500.0;
pub(super) const STREAM_RAMP_GAIN: f32 = 0.20;
pub(super) const STREAM_NEWLINE_COLS: f32 = 14.0;

pub(super) const STREAM_HOLD_IDLE_TIMEOUT: Duration = Duration::from_millis(350);
pub(super) const STREAM_HOLD_MAX_TIMEOUT: Duration = Duration::from_millis(1500);

const MAX_LINK_BYTES: usize = 512;
const MAX_AUTOLINK_BYTES: usize = 256;
const MAX_HTML_TAG_BYTES: usize = 48;
const MAX_INLINE_CODE_BYTES: usize = 128;
const MAX_INLINE_STYLE_BYTES: usize = 96;
const MAX_INLINE_MATH_BYTES: usize = 80;
const MAX_DISPLAY_MATH_BYTES: usize = 160;
const MAX_FENCE_LINE_BYTES: usize = 64;
const MAX_TABLE_ROW_BYTES: usize = 256;
const MAX_RULE_LINE_BYTES: usize = 32;
const MAX_ALERT_LINE_BYTES: usize = 32;
const MAX_LEADING_SPACE_HOLD: usize = 8;
const MAX_ATOMIC_COST: f32 = 24.0;

/// Tracks how long the stream buffer has been waiting on an unclosed construct.
#[derive(Clone, Copy, Debug)]
pub(super) struct HoldState {
    pub(super) first_held: Instant,
    pub(super) last_growth: Instant,
    pub(super) observed_len: usize,
    pub(super) timed_out: bool,
}

impl HoldState {
    /// Returns whether the stream may hold an incomplete Markdown construct.
    ///
    /// Resets wait timers whenever new bytes arrive, and forces release once
    /// either the idle or overall hold timeout expires.
    pub(super) fn can_hold(
        hold: &mut Option<Self>,
        now: Instant,
        rest_len: usize,
        allow_wait: bool,
    ) -> bool {
        if !allow_wait {
            return false;
        }
        let Some(state) = hold.as_mut() else {
            return true;
        };
        if rest_len > state.observed_len {
            state.first_held = if state.timed_out { now } else { state.first_held };
            state.last_growth = now;
            state.observed_len = rest_len;
            state.timed_out = false;
        }
        if state.timed_out {
            return false;
        }
        let within_idle =
            now.saturating_duration_since(state.last_growth) < STREAM_HOLD_IDLE_TIMEOUT;
        let within_max = now.saturating_duration_since(state.first_held) < STREAM_HOLD_MAX_TIMEOUT;
        if !(within_idle && within_max) {
            state.timed_out = true;
            return false;
        }
        true
    }

    /// Records that a chunk was held so timeouts can be enforced across ticks.
    pub(super) fn mark_held(hold: &mut Option<Self>, now: Instant, rest_len: usize) {
        hold.get_or_insert(Self {
            first_held: now,
            last_growth: now,
            observed_len: rest_len,
            timed_out: false,
        });
    }

    /// Updates or clears hold state after emitting `bytes`.
    pub(super) fn on_release(hold: &mut Option<Self>, bytes: usize, released: &str) {
        let Some(state) = hold.as_mut() else {
            return;
        };
        if !state.timed_out || released.contains('\n') {
            *hold = None;
        } else {
            state.observed_len = state.observed_len.saturating_sub(bytes);
        }
    }
}

/// Returns the display column weight of a single character for pacing.
pub(super) fn char_columns(ch: char) -> f32 {
    if ch == '\n' {
        STREAM_NEWLINE_COLS
    } else if (ch as u32) >= 0x1100 {
        2.0
    } else {
        1.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineFenceProbe {
    Indent(usize),
    Run { ch: u8, len: usize },
    OpenInfo { ch: u8, len: usize },
    CloseTrail { ch: u8, len: usize },
    NotFence,
}

/// Line and fenced-code context derived from already-revealed card text.
#[derive(Clone, Copy)]
pub(super) struct LineContext {
    pub(super) in_fence: Option<(u8, usize)>,
    pub(super) line_start: bool,
    pub(super) line_indent_only: bool,
    pub(super) line_indent_spaces: usize,
    pub(super) prev_char: Option<char>,
    probe: LineFenceProbe,
}

impl Default for LineContext {
    fn default() -> Self {
        Self {
            in_fence: None,
            line_start: true,
            line_indent_only: true,
            line_indent_spaces: 0,
            prev_char: None,
            probe: LineFenceProbe::Indent(0),
        }
    }
}

impl LineContext {
    /// Context that treats text as plain (non-Markdown) content.
    pub(super) fn plain_text() -> Self {
        Self {
            in_fence: Some((b'`', 3)),
            ..Self::default()
        }
    }

    /// Builds context by scanning the text already revealed on a card.
    pub(super) fn from_revealed(revealed: &str) -> Self {
        let mut ctx = Self::default();
        ctx.advance(revealed);
        ctx
    }

    fn advance(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            self.prev_char = Some(ch);
            if ch == '\n' {
                match self.probe {
                    LineFenceProbe::Run { ch: fch, len }
                    | LineFenceProbe::OpenInfo { ch: fch, len }
                        if self.in_fence.is_none() && len >= 3 =>
                    {
                        self.in_fence = Some((fch, len));
                    }
                    LineFenceProbe::Run { ch: fch, len }
                    | LineFenceProbe::CloseTrail { ch: fch, len }
                        if self
                            .in_fence
                            .is_some_and(|(open_ch, open_len)| open_ch == fch && len >= open_len) =>
                    {
                        self.in_fence = None;
                    }
                    _ => {}
                }
                self.line_start = true;
                self.line_indent_only = true;
                self.line_indent_spaces = 0;
                self.probe = LineFenceProbe::Indent(0);
                continue;
            }
            self.line_start = false;
            if self.line_indent_only {
                if ch == ' ' && self.line_indent_spaces < 3 {
                    self.line_indent_spaces += 1;
                } else {
                    self.line_indent_only = false;
                }
            }
            self.probe = match self.probe {
                LineFenceProbe::Indent(n) => {
                    if ch == ' ' && n < 3 {
                        LineFenceProbe::Indent(n + 1)
                    } else if ch == '`' || ch == '~' {
                        LineFenceProbe::Run {
                            ch: ch as u8,
                            len: 1,
                        }
                    } else {
                        LineFenceProbe::NotFence
                    }
                }
                LineFenceProbe::Run { ch: fch, len } => {
                    if ch as u32 == u32::from(fch) {
                        LineFenceProbe::Run {
                            ch: fch,
                            len: len + 1,
                        }
                    } else if self.in_fence.is_none() && len >= 3 {
                        LineFenceProbe::OpenInfo { ch: fch, len }
                    } else if self
                        .in_fence
                        .is_some_and(|(open_ch, open_len)| open_ch == fch && len >= open_len)
                        && matches!(ch, ' ' | '\t' | '\r')
                    {
                        LineFenceProbe::CloseTrail { ch: fch, len }
                    } else {
                        LineFenceProbe::NotFence
                    }
                }
                LineFenceProbe::OpenInfo { ch: fch, len } => {
                    if fch == b'`' && ch == '`' {
                        LineFenceProbe::NotFence
                    } else {
                        LineFenceProbe::OpenInfo { ch: fch, len }
                    }
                }
                LineFenceProbe::CloseTrail { ch: fch, len } => {
                    if matches!(ch, ' ' | '\t' | '\r') {
                        LineFenceProbe::CloseTrail { ch: fch, len }
                    } else {
                        LineFenceProbe::NotFence
                    }
                }
                LineFenceProbe::NotFence => LineFenceProbe::NotFence,
            };
        }
    }
}

/// Decision returned by [`plan_chunk`] for the current tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ChunkPlan {
    Release { bytes: usize, cost: f32 },
    Hold,
}

enum Unit {
    Emit { bytes: usize, cost: f32 },
    Hold,
}

/// Plans how many bytes to release from `rest` within `budget` columns.
pub(super) fn plan_chunk(
    rest: &str,
    ctx: &LineContext,
    budget: f32,
    can_hold: bool,
) -> ChunkPlan {
    let mut ctx = *ctx;
    let mut take_bytes = 0usize;
    let mut spent = 0.0f32;
    while take_bytes < rest.len() {
        let slice = &rest[take_bytes..];
        match next_unit(slice, &ctx, can_hold) {
            Unit::Hold => {
                return if take_bytes > 0 {
                    ChunkPlan::Release {
                        bytes: take_bytes,
                        cost: spent,
                    }
                } else {
                    ChunkPlan::Hold
                };
            }
            Unit::Emit { bytes, cost } => {
                debug_assert!(bytes > 0 && slice.is_char_boundary(bytes));
                if take_bytes > 0 && spent + cost > budget {
                    break;
                }
                ctx.advance(&slice[..bytes]);
                take_bytes += bytes;
                spent += cost;
                if spent >= budget {
                    break;
                }
            }
        }
    }
    ChunkPlan::Release {
        bytes: take_bytes,
        cost: spent,
    }
}

fn next_unit(s: &str, ctx: &LineContext, can_hold: bool) -> Unit {
    if let Some((fence_ch, fence_len)) = ctx.in_fence {
        if ctx.line_indent_only
            && let Some(unit) = try_closing_fence(s, ctx.line_indent_spaces, fence_ch, fence_len, can_hold)
        {
            return unit;
        }
        return next_grapheme(s);
    }

    if ctx.line_start || ctx.line_indent_only {
        if let Some(unit) = try_line_start(s, ctx, can_hold, true) {
            return unit;
        }
    }

    next_inline_unit(s, ctx.prev_char, can_hold)
}

fn try_closing_fence(
    s: &str,
    indent_used: usize,
    fence_ch: u8,
    fence_len: usize,
    can_hold: bool,
) -> Option<Unit> {
    let bytes = s.as_bytes();
    let max_spaces = 3usize.saturating_sub(indent_used);
    let mut i = 0;
    while i < bytes.len() && i < max_spaces && bytes[i] == b' ' {
        i += 1;
    }
    if i == bytes.len() {
        return None;
    }
    let start_fence = i;
    while i < bytes.len() && bytes[i] == fence_ch {
        i += 1;
    }
    let count = i - start_fence;
    if count == 0 {
        return None;
    }
    if i == bytes.len() {
        return can_hold.then_some(Unit::Hold);
    }
    if count < fence_len {
        return None;
    }
    while i < bytes.len() && i < MAX_FENCE_LINE_BYTES {
        match bytes[i] {
            b'\n' => {
                return Some(Unit::Emit {
                    bytes: i + 1,
                    cost: 6.0,
                });
            }
            b' ' | b'\t' | b'\r' => i += 1,
            _ => return None,
        }
    }
    (i == bytes.len() && s.len() <= MAX_FENCE_LINE_BYTES && can_hold).then_some(Unit::Hold)
}

fn try_line_start(
    s: &str,
    ctx: &LineContext,
    can_hold: bool,
    allow_blockquote: bool,
) -> Option<Unit> {
    let bytes = s.as_bytes();
    if ctx.line_start && bytes.len() <= MAX_LEADING_SPACE_HOLD && can_hold {
        if bytes.iter().all(|&b| matches!(b, b' ' | b'\t')) {
            return Some(Unit::Hold);
        }
    }

    let max_spaces = 3usize.saturating_sub(ctx.line_indent_spaces);
    let mut indent = 0;
    while indent < bytes.len() && indent < max_spaces && bytes[indent] == b' ' {
        indent += 1;
    }

    if let Some(unit) = try_opening_fence(s, indent, can_hold) {
        return Some(unit);
    }
    if allow_blockquote && let Some(unit) = try_blockquote_prefix(s, indent, can_hold) {
        return Some(unit);
    }
    if let Some(unit) = try_horizontal_rule(s, indent, can_hold) {
        return Some(unit);
    }
    if let Some(unit) = try_heading_prefix(s, indent, can_hold) {
        return Some(unit);
    }
    if let Some(unit) = try_list_prefix(s, can_hold) {
        return Some(unit);
    }
    if let Some(unit) = try_table_row(s, indent, can_hold) {
        return Some(unit);
    }
    None
}

fn try_opening_fence(s: &str, indent: usize, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let ch = *bytes.get(indent)?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let mut i = indent;
    while i < bytes.len() && bytes[i] == ch {
        i += 1;
    }
    let count = i - indent;
    if count < 3 {
        return (i == bytes.len() && can_hold).then_some(Unit::Hold);
    }
    while i < bytes.len() && i < MAX_FENCE_LINE_BYTES {
        if ch == b'`' && bytes[i] == b'`' {
            return None;
        }
        if bytes[i] == b'\n' {
            return Some(Unit::Emit {
                bytes: i + 1,
                cost: 6.0,
            });
        }
        i += 1;
    }
    (i == bytes.len() && s.len() <= MAX_FENCE_LINE_BYTES && can_hold).then_some(Unit::Hold)
}

fn try_heading_prefix(s: &str, indent: usize, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let mut i = indent;
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    let hashes = i - indent;
    if hashes == 0 || hashes > 6 {
        return None;
    }
    if i == bytes.len() {
        return can_hold.then_some(Unit::Hold);
    }
    if !matches!(bytes[i], b' ' | b'\t') {
        return None;
    }
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    Some(attach_after_prefix(s, i, Some(' '), can_hold))
}

fn try_blockquote_prefix(s: &str, indent: usize, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    if bytes.get(indent) != Some(&b'>') {
        return None;
    }
    let mut i = indent;
    let mut depth = 0usize;
    while i < bytes.len() && depth < 8 {
        let mut spaces = 0usize;
        while i + spaces < bytes.len() && spaces < 3 && bytes[i + spaces] == b' ' {
            spaces += 1;
        }
        if bytes.get(i + spaces) != Some(&b'>') {
            break;
        }
        depth += 1;
        i += spaces + 1;
        if i == bytes.len() {
            return can_hold.then_some(Unit::Hold);
        }
        if matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
    }
    if depth == 0 {
        return None;
    }
    if i == bytes.len() {
        return can_hold.then_some(Unit::Hold);
    }
    if bytes[i] == b'\n' {
        return Some(Unit::Emit {
            bytes: i + 1,
            cost: STREAM_NEWLINE_COLS,
        });
    }

    // Handle GFM alerts like `> [!NOTE]\n`.
    if bytes[i] == b'[' {
        if i + 1 == bytes.len() {
            return can_hold.then_some(Unit::Hold);
        }
        if bytes[i + 1] == b'!' {
            let mut j = i + 2;
            let limit = (i + MAX_ALERT_LINE_BYTES).min(bytes.len());
            let mut closed = false;
            while j < limit {
                if bytes[j] == b'\n' {
                    if closed {
                        return Some(Unit::Emit {
                            bytes: j + 1,
                            cost: 6.0,
                        });
                    }
                    break;
                }
                if bytes[j] == b']' {
                    closed = true;
                }
                j += 1;
            }
            if j == bytes.len() && bytes.len() - i <= MAX_ALERT_LINE_BYTES && can_hold {
                return Some(Unit::Hold);
            }
        }
    }

    let body = &s[i..];
    let inner_ctx = LineContext {
        in_fence: None,
        line_start: true,
        line_indent_only: true,
        line_indent_spaces: 0,
        prev_char: Some(' '),
        probe: LineFenceProbe::Indent(0),
    };
    if let Some(unit) = try_line_start(body, &inner_ctx, can_hold, false) {
        return Some(match unit {
            Unit::Hold => Unit::Hold,
            Unit::Emit { bytes, cost } => Unit::Emit {
                bytes: i + bytes,
                cost: (1.0 + cost).min(MAX_ATOMIC_COST),
            },
        });
    }
    Some(attach_after_prefix(s, i, Some(' '), can_hold))
}

fn try_horizontal_rule(s: &str, indent: usize, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let ch = *bytes.get(indent)?;
    if !matches!(ch, b'-' | b'*' | b'_') {
        return None;
    }
    let mut i = indent;
    let mut count = 0usize;
    let limit = (indent + MAX_RULE_LINE_BYTES).min(bytes.len());
    while i < limit {
        match bytes[i] {
            b'\n' => {
                return (count >= 3).then_some(Unit::Emit {
                    bytes: i + 1,
                    cost: STREAM_NEWLINE_COLS,
                });
            }
            b if b == ch => {
                count += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            _ => return None,
        }
    }
    (i == bytes.len() && bytes.len() - indent <= MAX_RULE_LINE_BYTES && count >= 2 && can_hold)
        .then_some(Unit::Hold)
}

fn try_list_prefix(s: &str, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let first = *bytes.get(i)?;
    let prefix_end = if matches!(first, b'-' | b'*' | b'+') {
        i += 1;
        if i == bytes.len() {
            return can_hold.then_some(Unit::Hold);
        }
        if !matches!(bytes[i], b' ' | b'\t') {
            return None;
        }
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        // Optional task checkbox: `[ ] `, `[x] `, `[X] `.
        if i < bytes.len() && bytes[i] == b'[' {
            let rem = &bytes[i..];
            if matches!(rem, b"[" | b"[ " | b"[x" | b"[X" | b"[ ]" | b"[x]" | b"[X]")
                && can_hold
            {
                return Some(Unit::Hold);
            }
            if rem.len() >= 4
                && matches!(rem[1], b' ' | b'x' | b'X')
                && rem[2] == b']'
                && matches!(rem[3], b' ' | b'\t')
            {
                i += 3;
                while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                    i += 1;
                }
            }
        }
        i
    } else if first.is_ascii_digit() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() && i - start < 9 {
            i += 1;
        }
        if i == bytes.len() || !matches!(bytes[i], b'.' | b')') {
            return None;
        }
        i += 1;
        if i == bytes.len() {
            return can_hold.then_some(Unit::Hold);
        }
        if !matches!(bytes[i], b' ' | b'\t') {
            return None;
        }
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        i
    } else {
        return None;
    };
    Some(attach_after_prefix(s, prefix_end, Some(' '), can_hold))
}

fn try_table_row(s: &str, indent: usize, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    if bytes.get(indent) != Some(&b'|') {
        return None;
    }
    let limit = MAX_TABLE_ROW_BYTES.min(bytes.len());
    let mut nl = None;
    for (idx, &b) in bytes[indent..limit].iter().enumerate() {
        if b == b'\n' {
            nl = Some(indent + idx);
            break;
        }
    }
    let Some(nl) = nl else {
        return (bytes.len() <= MAX_TABLE_ROW_BYTES && can_hold).then_some(Unit::Hold);
    };
    let after = &s[nl + 1..];
    if let Some(sep_end) = complete_table_separator_line(after) {
        let total = nl + 1 + sep_end;
        let cost = visible_inline_columns(&s[..nl]).clamp(4.0, MAX_ATOMIC_COST);
        return Some(Unit::Emit { bytes: total, cost });
    }
    if is_incomplete_table_separator_prefix(after) && can_hold {
        return Some(Unit::Hold);
    }
    let cost = visible_inline_columns(&s[..nl]).clamp(2.0, MAX_ATOMIC_COST);
    Some(Unit::Emit {
        bytes: nl + 1,
        cost,
    })
}

fn complete_table_separator_line(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let limit = MAX_TABLE_ROW_BYTES.min(bytes.len());
    let mut has_dash = false;
    let mut has_pipe = false;
    for (idx, &b) in bytes[..limit].iter().enumerate() {
        match b {
            b'\n' => return (has_dash && has_pipe).then_some(idx + 1),
            b'-' => has_dash = true,
            b'|' => has_pipe = true,
            b':' | b' ' | b'\t' | b'\r' => {}
            _ => return None,
        }
    }
    None
}

fn is_incomplete_table_separator_prefix(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_TABLE_ROW_BYTES {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    if bytes.get(i) != Some(&b'|') {
        return false;
    }
    bytes[i..]
        .iter()
        .all(|&b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t' | b'\r'))
}

fn attach_after_prefix(
    s: &str,
    prefix_len: usize,
    prev_char: Option<char>,
    can_hold: bool,
) -> Unit {
    let body = &s[prefix_len..];
    if body.is_empty() {
        return if can_hold {
            Unit::Hold
        } else {
            Unit::Emit {
                bytes: prefix_len,
                cost: 1.0,
            }
        };
    }
    if body.starts_with('\n') {
        return Unit::Emit {
            bytes: prefix_len + 1,
            cost: STREAM_NEWLINE_COLS,
        };
    }
    match next_inline_unit(body, prev_char, can_hold) {
        Unit::Hold => Unit::Hold,
        Unit::Emit { bytes, cost } => Unit::Emit {
            bytes: prefix_len + bytes,
            cost: (1.0 + cost).min(MAX_ATOMIC_COST),
        },
    }
}

fn next_inline_unit(s: &str, prev_char: Option<char>, can_hold: bool) -> Unit {
    let bytes = s.as_bytes();
    match bytes[0] {
        b'\\' => {
            if let Some(unit) = try_escape(s, can_hold) {
                return unit;
            }
        }
        b'[' => {
            if let Some(unit) = try_link_or_image(s, false, can_hold) {
                return unit;
            }
        }
        b'!' if bytes.get(1) == Some(&b'[') => {
            if let Some(unit) = try_link_or_image(s, true, can_hold) {
                return unit;
            }
        }
        b'<' => {
            if let Some(unit) = try_autolink_or_tag(s, can_hold) {
                return unit;
            }
        }
        b'`' => {
            if let Some(unit) = try_inline_code(s, can_hold) {
                return unit;
            }
        }
        b'*' | b'~' | b'=' | b'_' => {
            if let Some(unit) = try_inline_style(s, prev_char, can_hold) {
                return unit;
            }
        }
        b'$' => {
            if let Some(unit) = try_inline_math(s, can_hold) {
                return unit;
            }
        }
        _ => {}
    }
    next_grapheme(s)
}

fn try_escape(s: &str, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    if bytes.len() == 1 {
        return can_hold.then_some(Unit::Hold);
    }
    bytes[1].is_ascii_punctuation().then_some(Unit::Emit {
        bytes: 2,
        cost: 1.0,
    })
}

fn try_link_or_image(s: &str, is_image: bool, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let start = if is_image { 2 } else { 1 };
    let mut i = start;
    let mut bracket_depth = 0usize;
    let mut label_end = None;
    let limit = MAX_LINK_BYTES.min(bytes.len());
    while i < limit {
        match bytes[i] {
            b'\n' | b'\r' => return None,
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return (bytes.len() <= MAX_LINK_BYTES && can_hold).then_some(Unit::Hold);
                }
                i += 1 + s[i + 1..].chars().next()?.len_utf8();
            }
            b'`' => {
                let Some(next) = skip_inline_code(s, i, MAX_LINK_BYTES) else {
                    return (bytes.len() <= MAX_LINK_BYTES && can_hold).then_some(Unit::Hold);
                };
                if next == usize::MAX {
                    return None;
                }
                i = next;
            }
            b'[' => {
                bracket_depth += 1;
                i += 1;
            }
            b']' => {
                if bracket_depth == 0 {
                    label_end = Some(i);
                    break;
                }
                bracket_depth -= 1;
                i += 1;
            }
            _ => i += s[i..].chars().next()?.len_utf8(),
        }
    }
    let Some(label_end) = label_end else {
        return (i == bytes.len() && bytes.len() < MAX_LINK_BYTES && can_hold).then_some(Unit::Hold);
    };
    let after_bracket = label_end + 1;
    if after_bracket == bytes.len() {
        return can_hold.then_some(Unit::Hold);
    }
    if bytes[after_bracket] != b'(' {
        return None;
    }
    let mut j = after_bracket + 1;
    let mut paren_depth = 0usize;
    while j < limit {
        match bytes[j] {
            b'\n' | b'\r' => return None,
            b'\\' => {
                if j + 1 >= bytes.len() {
                    return (bytes.len() <= MAX_LINK_BYTES && can_hold).then_some(Unit::Hold);
                }
                j += 1 + s[j + 1..].chars().next()?.len_utf8();
            }
            b'(' => {
                paren_depth += 1;
                j += 1;
            }
            b')' => {
                if paren_depth == 0 {
                    let cost =
                        visible_inline_columns(&s[start..label_end]).clamp(1.0, MAX_ATOMIC_COST);
                    return Some(Unit::Emit { bytes: j + 1, cost });
                }
                paren_depth -= 1;
                j += 1;
            }
            _ => j += s[j..].chars().next()?.len_utf8(),
        }
    }
    (j == bytes.len() && bytes.len() <= MAX_LINK_BYTES && can_hold).then_some(Unit::Hold)
}

/// Skips an inline code span starting at `start`. Returns `None` if incomplete
/// before end of `s`, or `Some(usize::MAX)` if invalid/exceeding `cap`.
fn skip_inline_code(s: &str, start: usize, cap: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut open = 0usize;
    while start + open < bytes.len() && bytes[start + open] == b'`' {
        open += 1;
    }
    let mut i = start + open;
    let limit = cap.min(bytes.len());
    while i < limit {
        match bytes[i] {
            b'\n' | b'\r' => return Some(usize::MAX),
            b'`' => {
                let mut close = 0usize;
                while i + close < bytes.len() && bytes[i + close] == b'`' {
                    close += 1;
                }
                if close == open {
                    return Some(i + close);
                }
                i += close;
            }
            _ => i += s[i..].chars().next()?.len_utf8(),
        }
    }
    if i == bytes.len() && bytes.len() <= cap {
        None
    } else {
        Some(usize::MAX)
    }
}

fn is_autolink_prefix(rest: &str) -> bool {
    const SCHEMES: [&str; 4] = ["http://", "https://", "file://", "mailto:"];
    let lower = rest.get(..rest.len().min(8)).unwrap_or(rest);
    if !lower.is_ascii() {
        return false;
    }
    SCHEMES.iter().any(|scheme| {
        if rest.len() >= scheme.len() {
            rest[..scheme.len()].eq_ignore_ascii_case(scheme)
        } else {
            scheme[..rest.len()].eq_ignore_ascii_case(rest)
        }
    })
}

fn try_autolink_or_tag(s: &str, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let &second = bytes.get(1)?;
    let rest = &s[1..];
    let autolink = is_autolink_prefix(rest);
    if !autolink && !second.is_ascii_alphabetic() && second != b'/' {
        return None;
    }
    let cap = if autolink {
        MAX_AUTOLINK_BYTES
    } else {
        MAX_HTML_TAG_BYTES
    };
    let limit = cap.min(bytes.len());
    let mut i = 1;
    while i < limit {
        match bytes[i] {
            b'\n' | b'\r' | b'<' => return None,
            b if autolink && b.is_ascii_whitespace() => return None,
            b'>' => {
                let cost = if autolink {
                    visible_inline_columns(&s[1..i]).clamp(1.0, 16.0)
                } else {
                    1.0
                };
                return Some(Unit::Emit { bytes: i + 1, cost });
            }
            _ => i += s[i..].chars().next()?.len_utf8(),
        }
    }
    (i == bytes.len() && bytes.len() <= cap && can_hold).then_some(Unit::Hold)
}

fn try_inline_code(s: &str, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let mut open = 0usize;
    while open < bytes.len() && bytes[open] == b'`' {
        open += 1;
    }
    if open == 0 || open > 2 {
        return None;
    }
    if open == bytes.len() {
        return can_hold.then_some(Unit::Hold);
    }
    let limit = MAX_INLINE_CODE_BYTES.min(bytes.len());
    let mut i = open;
    while i < limit {
        match bytes[i] {
            b'\n' | b'\r' => return None,
            b'`' => {
                let mut close = 0usize;
                while i + close < bytes.len() && bytes[i + close] == b'`' {
                    close += 1;
                }
                if close == open {
                    let cost = visible_inline_columns(&s[open..i]).clamp(1.0, 20.0);
                    return Some(Unit::Emit {
                        bytes: i + close,
                        cost,
                    });
                }
                i += close;
            }
            _ => i += s[i..].chars().next()?.len_utf8(),
        }
    }
    (i == bytes.len() && bytes.len() <= MAX_INLINE_CODE_BYTES && can_hold).then_some(Unit::Hold)
}

fn try_inline_style(s: &str, prev_char: Option<char>, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    let first = bytes[0];
    let mut run = 0usize;
    while run < bytes.len() && bytes[run] == first {
        run += 1;
    }

    if first == b'_' && prev_char.is_some_and(char::is_alphanumeric) {
        return None;
    }

    let delim_len = match first {
        b'*' => {
            if run == bytes.len() && run <= 3 {
                return can_hold.then_some(Unit::Hold);
            }
            if run > 3 {
                return None;
            }
            run
        }
        b'~' | b'=' | b'_' => {
            if run == 1 && bytes.len() == 1 {
                return can_hold.then_some(Unit::Hold);
            }
            if run != 2 {
                return None;
            }
            if bytes.len() == 2 {
                return can_hold.then_some(Unit::Hold);
            }
            2
        }
        _ => return None,
    };

    let next_ch = s[delim_len..].chars().next()?;
    if next_ch.is_whitespace() {
        return None;
    }
    if first == b'*'
        && delim_len == 1
        && prev_char.is_some_and(|c| c.is_ascii_digit())
        && next_ch.is_ascii_digit()
    {
        return None;
    }

    let delim = &s[..delim_len];
    let mut cap = MAX_INLINE_STYLE_BYTES;
    let mut i = delim_len;
    let mut prev_ws = false;
    while i < bytes.len() && i < cap {
        let b = bytes[i];
        match b {
            b'\n' | b'\r' => return None,
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return (bytes.len() <= cap && can_hold).then_some(Unit::Hold);
                }
                let ch = s[i + 1..].chars().next()?;
                prev_ws = ch.is_whitespace();
                i += 1 + ch.len_utf8();
                continue;
            }
            b'`' => {
                let Some(next) = skip_inline_code(s, i, cap) else {
                    return (bytes.len() <= cap && can_hold).then_some(Unit::Hold);
                };
                if next == usize::MAX {
                    return None;
                }
                prev_ws = false;
                i = next;
                continue;
            }
            b']' if bytes.get(i + 1) == Some(&b'(') => {
                cap = MAX_LINK_BYTES;
            }
            _ => {}
        }

        if !prev_ws && s[i..].starts_with(delim) {
            let after_idx = i + delim_len;
            let intraword_close = first == b'_'
                && s[after_idx..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            if !intraword_close {
                let cost = visible_inline_columns(&s[delim_len..i]).clamp(1.0, 20.0);
                return Some(Unit::Emit {
                    bytes: after_idx,
                    cost,
                });
            }
        }

        let ch = s[i..].chars().next()?;
        prev_ws = ch.is_whitespace();
        i += ch.len_utf8();
    }
    (i == bytes.len() && bytes.len() <= cap && can_hold).then_some(Unit::Hold)
}

fn try_inline_math(s: &str, can_hold: bool) -> Option<Unit> {
    let bytes = s.as_bytes();
    if bytes.starts_with(b"$$") {
        if bytes.len() == 2 {
            return can_hold.then_some(Unit::Hold);
        }
        let limit = MAX_DISPLAY_MATH_BYTES.min(bytes.len());
        let mut i = 2;
        while i < limit {
            match bytes[i] {
                b'\n' | b'\r' => return None,
                b'\\' => {
                    if i + 1 >= bytes.len() {
                        return (bytes.len() <= MAX_DISPLAY_MATH_BYTES && can_hold)
                            .then_some(Unit::Hold);
                    }
                    i += 1 + s[i + 1..].chars().next()?.len_utf8();
                }
                b'$' if bytes.get(i + 1) == Some(&b'$') => {
                    let cost = visible_inline_columns(&s[2..i]).clamp(1.0, MAX_ATOMIC_COST);
                    return Some(Unit::Emit { bytes: i + 2, cost });
                }
                _ => i += s[i..].chars().next()?.len_utf8(),
            }
        }
        return (i == bytes.len() && bytes.len() <= MAX_DISPLAY_MATH_BYTES && can_hold)
            .then_some(Unit::Hold);
    }

    if bytes.len() == 1 {
        return can_hold.then_some(Unit::Hold);
    }
    let next_ch = s[1..].chars().next()?;
    if next_ch.is_whitespace() || next_ch == '$' {
        return None;
    }
    let starts_with_digit = next_ch.is_ascii_digit();
    let limit = MAX_INLINE_MATH_BYTES.min(bytes.len());
    let mut i = 1;
    let mut prev_ws = false;
    while i < limit {
        match bytes[i] {
            b'\n' | b'\r' => return None,
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return (bytes.len() <= MAX_INLINE_MATH_BYTES && can_hold)
                        .then_some(Unit::Hold);
                }
                let ch = s[i + 1..].chars().next()?;
                prev_ws = ch.is_whitespace();
                i += 1 + ch.len_utf8();
            }
            b'$' if !prev_ws => {
                if s[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_digit())
                {
                    return None;
                }
                let cost = visible_inline_columns(&s[1..i]).clamp(1.0, MAX_ATOMIC_COST);
                return Some(Unit::Emit { bytes: i + 1, cost });
            }
            _ => {
                let ch = s[i..].chars().next()?;
                if starts_with_digit && ch.is_whitespace() {
                    return None;
                }
                prev_ws = ch.is_whitespace();
                i += ch.len_utf8();
            }
        }
    }
    (i == bytes.len() && bytes.len() <= MAX_INLINE_MATH_BYTES && can_hold).then_some(Unit::Hold)
}

fn is_combining_or_modifier(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE0E}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn next_grapheme(s: &str) -> Unit {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return Unit::Emit {
            bytes: 0,
            cost: 0.0,
        };
    };
    let mut end = first.len_utf8();
    if first == '\n' {
        return Unit::Emit {
            bytes: end,
            cost: STREAM_NEWLINE_COLS,
        };
    }
    while let Some(next) = s[end..].chars().next() {
        if is_combining_or_modifier(next) {
            end += next.len_utf8();
            continue;
        }
        if next == '\u{200D}' {
            let after_zwj = end + next.len_utf8();
            if let Some(chained) = s[after_zwj..].chars().next()
                && chained != '\n'
            {
                end = after_zwj + chained.len_utf8();
                continue;
            }
        }
        break;
    }
    Unit::Emit {
        bytes: end,
        cost: char_columns(first),
    }
}

fn visible_inline_columns(s: &str) -> f32 {
    let bytes = s.as_bytes();
    let mut cols = 0.0f32;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b']' && bytes.get(i + 1) == Some(&b'(') {
            let mut j = i + 2;
            let mut depth = 0usize;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => {
                        let next_len = s[j + 1..].chars().next().map_or(0, char::len_utf8);
                        j += 1 + next_len;
                    }
                    b'(' => {
                        depth += 1;
                        j += 1;
                    }
                    b')' => {
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                        depth -= 1;
                        j += 1;
                    }
                    _ => j += s[j..].chars().next().map_or(1, char::len_utf8),
                }
            }
            i = j;
            continue;
        }
        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        if !matches!(ch, '[' | '`' | '*' | '~' | '=') {
            cols += char_columns(ch);
        }
        i += ch.len_utf8();
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_steps(chunks: &[&str], budget_per_step: f32) -> Vec<String> {
        let mut revealed = String::new();
        let mut out = Vec::new();
        for (idx, chunk) in chunks.iter().enumerate() {
            let mut rest = *chunk;
            let is_last_chunk = idx + 1 == chunks.len();
            while !rest.is_empty() {
                let ctx = LineContext::from_revealed(&revealed);
                match plan_chunk(rest, &ctx, budget_per_step, !is_last_chunk) {
                    ChunkPlan::Hold => break,
                    ChunkPlan::Release { bytes, .. } => {
                        let part = &rest[..bytes];
                        revealed.push_str(part);
                        out.push(part.to_owned());
                        rest = &rest[bytes..];
                    }
                }
            }
        }
        out
    }

    #[test]
    fn complete_markdown_link_releases_atomically_with_label_cost() {
        let link = "[agent.rs](file:///C:/Users/hundo/code/Winc-agent-split/crates/winc-app/src/app/agent.rs#L159-L163)";
        let rest = format!("{link} 说明");
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk(&rest, &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: link.len(),
                cost: 8.0,
            }
        );
    }

    #[test]
    fn incomplete_markdown_link_holds_when_allowed_and_falls_back_on_timeout() {
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk("[agent.rs](file:///C:/repo", &ctx, 20.0, true),
            ChunkPlan::Hold
        );
        assert_eq!(
            plan_chunk("[agent.rs]", &ctx, 20.0, true),
            ChunkPlan::Hold
        );
        assert_eq!(
            plan_chunk("[agent.rs](file:///C:/repo", &ctx, 20.0, false),
            ChunkPlan::Release {
                bytes: "[agent.rs](file:///C".len(),
                cost: 20.0,
            }
        );
    }

    #[test]
    fn plain_bracket_followed_by_space_does_not_hold() {
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk("[auto-sync] skipped", &ctx, 5.0, true),
            ChunkPlan::Release {
                bytes: 5,
                cost: 5.0,
            }
        );
    }

    #[test]
    fn image_and_nested_paren_link_release_atomically() {
        let ctx = LineContext::default();
        let img = "![diagram](https://example.com/a.png)";
        assert_eq!(
            plan_chunk(&format!("{img} end"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: img.len(),
                cost: 7.0,
            }
        );
        let wiki = "[wiki](https://en.wikipedia.org/wiki/Foo_(bar))";
        assert_eq!(
            plan_chunk(&format!("{wiki} ok"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: wiki.len(),
                cost: 4.0,
            }
        );
    }

    #[test]
    fn inline_code_spans_release_atomically() {
        let ctx = LineContext::default();
        let code = "`STREAM_BASE_COLS_PER_SEC`";
        assert_eq!(
            plan_chunk(&format!("{code} ok"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: code.len(),
                cost: 20.0,
            }
        );
        let dbl = "``a ` b``";
        assert_eq!(
            plan_chunk(&format!("{dbl} ok"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: dbl.len(),
                cost: 4.0,
            }
        );
        assert_eq!(plan_chunk("`unclosed", &ctx, 10.0, true), ChunkPlan::Hold);
    }

    #[test]
    fn inline_styles_release_atomically_and_intraword_underscore_streams_normally() {
        let ctx = LineContext::default();
        for styled in [
            "**同步详情**",
            "~~旧值~~",
            "==重点==",
            "*斜体*",
            "**`agent.rs`**",
            "**[agent.rs](file:///a/b/c.rs)**",
        ] {
            let input = format!("{styled} 后续");
            let plan = plan_chunk(&input, &ctx, 24.0, true);
            assert!(
                matches!(plan, ChunkPlan::Release { bytes, .. } if bytes >= styled.len()),
                "expected atomic release for {styled}, got {plan:?}"
            );
        }

        let ctx_after_word = LineContext::from_revealed("foo");
        assert_eq!(
            plan_chunk("_bar_baz", &ctx_after_word, 3.0, true),
            ChunkPlan::Release {
                bytes: 3,
                cost: 3.0,
            }
        );
    }

    #[test]
    fn block_prefixes_bundle_with_first_content_unit() {
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk("### 标题", &ctx, 2.0, true),
            ChunkPlan::Release {
                bytes: "### 标".len(),
                cost: 3.0,
            }
        );
        assert_eq!(
            plan_chunk("- 列表", &ctx, 2.0, true),
            ChunkPlan::Release {
                bytes: "- 列".len(),
                cost: 3.0,
            }
        );
        assert_eq!(
            plan_chunk("1. 有序", &ctx, 2.0, true),
            ChunkPlan::Release {
                bytes: "1. 有".len(),
                cost: 3.0,
            }
        );
        assert_eq!(
            plan_chunk("- [x] 任务", &ctx, 2.0, true),
            ChunkPlan::Release {
                bytes: "- [x] 任".len(),
                cost: 3.0,
            }
        );
        assert_eq!(
            plan_chunk("> [!NOTE]\n正文", &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: "> [!NOTE]\n".len(),
                cost: 6.0,
            }
        );
        assert_eq!(
            plan_chunk("---\n下一段", &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: "---\n".len(),
                cost: STREAM_NEWLINE_COLS,
            }
        );
        assert_eq!(plan_chunk("### ", &ctx, 10.0, true), ChunkPlan::Hold);
    }

    #[test]
    fn fenced_code_blocks_release_headers_and_disable_markdown_rules_inside() {
        let open = "```rust\n";
        let body = "#[test]\nlet x = *ptr;\nlet y = [a](b);\n";
        let close = "```\n";
        let mut revealed = String::new();
        let ctx = LineContext::from_revealed(&revealed);
        assert_eq!(
            plan_chunk(&format!("{open}{body}{close}"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: open.len(),
                cost: 6.0,
            }
        );
        revealed.push_str(open);
        let in_code = LineContext::from_revealed(&revealed);
        assert_eq!(in_code.in_fence, Some((b'`', 3)));
        // Inside fence, `#[test]`, `*ptr`, and `[a](b)` do not hold or bundle as Markdown.
        assert_eq!(
            plan_chunk("#[test", &in_code, 1.0, true),
            ChunkPlan::Release {
                bytes: 1,
                cost: 1.0,
            }
        );
        assert_eq!(
            plan_chunk("[a](b", &in_code, 2.0, true),
            ChunkPlan::Release {
                bytes: 2,
                cost: 2.0,
            }
        );

        revealed.push_str(body);
        let before_close = LineContext::from_revealed(&revealed);
        assert_eq!(
            plan_chunk("``", &before_close, 10.0, true),
            ChunkPlan::Hold
        );
        assert_eq!(
            plan_chunk("```\nafter", &before_close, 1.0, true),
            ChunkPlan::Release {
                bytes: close.len(),
                cost: 6.0,
            }
        );
        revealed.push_str(close);
        assert_eq!(LineContext::from_revealed(&revealed).in_fence, None);
    }

    #[test]
    fn markdown_table_bundles_header_and_separator_and_releases_rows() {
        let ctx = LineContext::default();
        let header = "| 列1 | 列2 |\n| --- | --- |\n";
        let row = "| 值1 | 值2 |\n";
        assert_eq!(
            plan_chunk(&format!("{header}{row}"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: header.len(),
                cost: 13.0,
            }
        );
        let row_ctx = LineContext::from_revealed(header);
        assert_eq!(
            plan_chunk(row, &row_ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: row.len(),
                cost: 13.0,
            }
        );
    }

    #[test]
    fn currency_and_comparisons_do_not_false_trigger_while_math_releases_atomically() {
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk("$100 到 $200", &ctx, 4.0, true),
            ChunkPlan::Release {
                bytes: 4,
                cost: 4.0,
            }
        );
        assert_eq!(
            plan_chunk("< b", &LineContext::from_revealed("a "), 3.0, true),
            ChunkPlan::Release {
                bytes: 3,
                cost: 3.0,
            }
        );
        let math = "$a + b$";
        assert_eq!(
            plan_chunk(&format!("{math} ok"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: math.len(),
                cost: 5.0,
            }
        );
        let display = "$$E = mc^2$$";
        assert_eq!(
            plan_chunk(&format!("{display} ok"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: display.len(),
                cost: 7.0,
            }
        );
    }

    #[test]
    fn escapes_and_emoji_zwj_sequences_stay_intact() {
        let ctx = LineContext::default();
        assert_eq!(
            plan_chunk("\\*text", &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: 2,
                cost: 1.0,
            }
        );
        let sun = "☀️";
        assert_eq!(
            plan_chunk(&format!("{sun}晴"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: sun.len(),
                cost: 2.0,
            }
        );
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(
            plan_chunk(&format!("{family}家"), &ctx, 1.0, true),
            ChunkPlan::Release {
                bytes: family.len(),
                cost: 2.0,
            }
        );
    }

    #[test]
    fn hold_state_refreshes_on_growth_and_respects_idle_and_max_timeouts() {
        let t0 = Instant::now();
        let mut hold = None;
        assert!(HoldState::can_hold(&mut hold, t0, 5, true));
        HoldState::mark_held(&mut hold, t0, 5);

        // Growth at 250ms resets the 350ms idle window.
        let t1 = t0 + Duration::from_millis(250);
        assert!(HoldState::can_hold(&mut hold, t1, 10, true));

        // Still held at t1 + 250ms = t0 + 500ms (< 350ms idle, < 1500ms max).
        let t2 = t1 + Duration::from_millis(250);
        assert!(HoldState::can_hold(&mut hold, t2, 10, true));

        // Stalled past 350ms idle -> expires.
        let t3 = t1 + STREAM_HOLD_IDLE_TIMEOUT;
        assert!(!HoldState::can_hold(&mut hold, t3, 10, true));

        // Max timeout expires even under continuous growth.
        let mut long_hold = None;
        HoldState::mark_held(&mut long_hold, t0, 1);
        for step in 1..=6 {
            let now = t0 + Duration::from_millis(step * 240);
            assert!(HoldState::can_hold(&mut long_hold, now, 1 + step as usize, true));
        }
        let past_max = t0 + STREAM_HOLD_MAX_TIMEOUT;
        assert!(!HoldState::can_hold(&mut long_hold, past_max, 99, true));

        // Once timed out on `**unclosed`, subsequent `*` does not wait another 350ms.
        let steps = drain_steps(&["[a](b)", " ok"], 12.0);
        assert_eq!(steps[0], "[a](b)");
    }
}
