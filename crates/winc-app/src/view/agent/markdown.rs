//! Agent 侧边栏专用的 Markdown 排版与绘制。
//!
//! 复用了 `winc_preview::markdown::parse` 的解析结果与 `winc_syntax` 语法高亮，
//! 在不改动 `winc-typeset`（编辑器 Markdown 预览内核样式）的前提下，针对窄侧边栏对话流
//! 单独定制标题比例、块间距、正文行高、行内代码胶囊、引用卡片与带语言头栏的代码块；
//! 表格、公式与 Mermaid 图则委托给 `winc_typeset` 处理。

use std::collections::{HashMap, HashSet};
use std::f32::consts::{FRAC_PI_2, PI};
use std::sync::Arc;

use winc_preview::markdown::{Alert, Align, Block, IMAGE_MARK, Inline, Item as ListItem, Style};
use winc_render::{Color, Faux, FontKind, Frame, Rect, Stroke};
use winc_typeset::{Colors, Cx, Env, Item as Mark, LaidBlock, Memo, Out, Span};

use super::*;

/// 对话正文字号的行高系数（中英混排更舒展）。
const BODY_LINE: f32 = 1.6;
/// 标题行高系数。
const HEADING_LINE: f32 = 1.35;
/// 代码块外框圆角半径（逻辑像素）。
const CODE_RADIUS: f32 = 7.0;
/// 代码块顶部语言与复制栏高度（逻辑像素）。
const CODE_HEADER: f32 = 26.0;
/// 代码块正文水平内边距（逻辑像素）。
const CODE_PAD_X: f32 = 12.0;
/// 代码块正文垂直内边距（逻辑像素）。
const CODE_PAD_Y: f32 = 10.0;
/// 行内代码圆角半径（逻辑像素）。
const INLINE_CODE_RADIUS: f32 = 4.0;
/// 查找 emoji 序列起点时的最大回溯字符数。
const SEQUENCE_LOOKBACK: usize = 16;

/// 一个排好版的顶层 Markdown 块及其在消息卡片中的纵向偏移。
pub(super) struct RichBlock {
    hash: u64,
    pub(super) top: f32,
    pub(super) laid: LaidBlock,
    table_offsets: Vec<f32>,
}

impl RichBlock {
    pub(super) fn bottom(&self) -> f32 {
        self.top + self.laid.height
    }

    pub(super) fn text(&self) -> &str {
        &self.laid.text
    }

    pub(super) fn scroll_table(&mut self, table: usize, pixels: f32) {
        if let Some(offset) = self.table_offsets.get_mut(table)
            && let Some(scroller) = self.laid.scrolls.get(table)
        {
            *offset = (*offset + pixels).clamp(0.0, scroller.max());
        }
    }
}

/// 将一段 Markdown 文本排版为顶层块列表；内容未变的前缀块沿用旧的排版结果。
pub(super) fn layout(
    frame: &mut Frame<'_>,
    app: &App,
    text: &str,
    width: f32,
    old: Vec<RichBlock>,
) -> Vec<RichBlock> {
    let doc = winc_preview::markdown::parse(text);
    let base = app.fonts.ui;
    let mut memo = Memo::default();
    let mut cx = ChatTypeset {
        frame,
        app,
        base,
        style: ChatStyle::from_palette(app.palette),
        memo: &mut memo,
        text: String::new(),
        chars: 0,
    };
    let env = BlockEnv {
        color: app.palette.text,
        depth: 0,
    };
    let mut top = 0.0;
    let mut prev_bottom: Option<f32> = None;
    let mut old = old.into_iter();
    doc.blocks
        .iter()
        .zip(doc.hashes)
        .map(|(block, hash)| {
            let (margin_top, margin_bottom) = block_margins(block, base, false);
            if let Some(below) = prev_bottom {
                top += below.max(margin_top);
            }
            let (laid, table_offsets) = match old.next() {
                Some(old) if old.hash == hash => (old.laid, old.table_offsets),
                _ => {
                    let laid = cx.lay_top_block(block, width, env);
                    let offsets = vec![0.0; laid.scrolls.len()];
                    (laid, offsets)
                }
            };
            let rich = RichBlock {
                hash,
                top,
                laid,
                table_offsets,
            };
            top += rich.laid.height;
            prev_bottom = Some(margin_bottom);
            rich
        })
        .collect()
}

/// 绘制一个排好版的 Markdown 顶层块，并记录其中的可选文字范围与可点击区域。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    actions: &mut Actions<'_, '_, '_>,
    rich: &RichBlock,
    left: f32,
    top: f32,
    clip: Rect,
    visible: &mut Vec<u32>,
    selection: &mut SelectionPaint,
    section: usize,
) {
    let block = &rich.laid;
    block.visible(clip.y - top, clip.bottom() - top, visible);
    let mut controls = Vec::new();
    for &index in visible.iter() {
        let item = &block.items[index as usize];
        let table = block
            .scrolls
            .iter()
            .enumerate()
            .find(|(_, table)| table.items.contains(&index));
        let clip = table.map_or(clip, |(_, table)| {
            let r = table.rect;
            clip.intersect(Rect::new(
                left + r.x,
                top + r.y,
                r.w,
                r.h - winc_typeset::SCROLL_BAR_ROOM,
            ))
        });
        let left = table.map_or(left, |(i, _)| left - rich.table_offsets[i]);
        let at = |r: &Rect| Rect::new(left + r.x, top + r.y, r.w, r.h);
        match item {
            Mark::Text {
                x,
                baseline,
                text,
                font,
                faux,
                size,
                color,
                span,
            } => {
                if let Some(span) = span {
                    let metrics = actions.frame.metrics(*font, *size);
                    let rect = Rect::new(
                        left + x,
                        top + baseline - metrics.ascent,
                        0.0,
                        metrics.ascent + metrics.descent,
                    );
                    selection.run(
                        actions.frame,
                        actions.app,
                        section,
                        span.start,
                        left + x,
                        rect,
                        clip,
                        span.advances.clone(),
                    );
                }
                actions.frame.text_faux(
                    *font,
                    *faux,
                    *size,
                    left + x,
                    top + baseline,
                    text,
                    *color,
                    clip,
                );
                if span.is_some() {
                    for word in text.split_whitespace() {
                        if let Some(link) = Link::parse(word, false) {
                            let index = word.as_ptr() as usize - text.as_ptr() as usize;
                            let dx =
                                actions
                                    .frame
                                    .measure_faux(*font, *faux, *size, &text[..index]);
                            let width = actions.frame.measure_faux(*font, *faux, *size, word);
                            let r =
                                Rect::new(left + x + dx, top + baseline - size, width, *size + 3.0)
                                    .intersect(clip);
                            controls.push((r, Hit::Open(link, false)));
                        }
                    }
                }
            }
            Mark::Fill {
                rect,
                color,
                radius,
            } => actions.frame.rect_clipped(at(rect), *color, *radius, clip),
            Mark::Corners { rect, color, radii } => {
                actions.frame.rect_corners(at(rect), *color, *radii, clip)
            }
            Mark::Check { rect, checked, .. } => crate::view::preview::markdown::draw_check(
                actions.frame,
                actions.app.palette,
                at(rect),
                *checked,
                clip,
            ),
            Mark::Path {
                strokes,
                width,
                color,
                ..
            } => {
                let moved: Vec<_> = strokes
                    .iter()
                    .map(|&s| winc_typeset::shift(s, left, top))
                    .collect();
                actions.frame.stroke(&moved, *width, *color, clip);
            }
            Mark::Polygon { points, color, .. } => {
                let moved: Vec<_> = points.iter().map(|&(x, y)| (left + x, top + y)).collect();
                actions.frame.polygon(&moved, *color, clip);
            }
            Mark::Link { rect, url } => {
                if let Some(link) = Link::parse(url, true) {
                    controls.push((at(rect).intersect(clip), Hit::Open(link, false)));
                }
            }
            Mark::Code {
                rect,
                text,
                diagram,
            } => {
                let r = at(rect);
                let label = t!("agent-copy-code");
                let text_w = actions
                    .frame
                    .measure(FontKind::Ui, actions.app.fonts.ui_small, label)
                    .ceil();
                let btn_w = (text_w + 26.0).min((r.w - 12.0).max(0.0));
                let btn_h = 18.0;
                let btn_y = if *diagram {
                    r.y + 4.0
                } else {
                    r.y + (CODE_HEADER - btn_h) / 2.0
                };
                let button = Rect::new(r.right() - btn_w - 6.0, btn_y, btn_w, btn_h);
                controls.push((button, Hit::CopyCode(text.clone())));
            }
            _ => {}
        }
    }
    for (i, table) in block.scrolls.iter().enumerate() {
        let r = table.rect;
        let y = top + r.bottom() - winc_typeset::SCROLL_BAR_ROOM;
        let left = left + r.x;
        let scroll = rich.table_offsets[i];
        let max = table.max();
        let button_w = 18.0;
        if max <= 0.0 || r.w < button_w * 2.0 || y + 10.0 <= clip.y || y >= clip.bottom() {
            continue;
        }
        let width = r.w - button_w * 2.0;
        let thumb = (width * r.w / table.content).max(6.0).min(width);
        let x = left + button_w + (width - thumb) * scroll / max;
        actions.frame.rect_clipped(
            Rect::new(x, y + 4.0, thumb, 3.0),
            actions.app.palette.border,
            2.0,
            clip,
        );
        for (x, label, delta) in [
            (left, "‹", -r.w * 0.7),
            (left + r.w - button_w, "›", r.w * 0.7),
        ] {
            let rect = Rect::new(x, y, button_w, 12.0).intersect(clip);
            actions.button(
                rect,
                label,
                Hit::ScrollTable(selection.card, section, i, delta),
                false,
                if delta < 0.0 {
                    scroll > 0.0
                } else {
                    scroll < max
                },
            );
        }
    }
    for (rect, hit) in controls.into_iter().rev() {
        let clipped = rect.intersect(clip);
        if clipped.w <= 0.0 || clipped.h <= 0.0 {
            continue;
        }
        if matches!(hit, Hit::CopyCode(_)) {
            draw_code_copy_button(actions, rect, clip, hit);
        } else {
            actions.hits.push((clipped, hit));
        }
    }
}

/// 在代码块头栏右侧绘制紧凑的「复制」按钮（带复制图标，不破坏头栏高度）。
fn draw_code_copy_button(actions: &mut Actions<'_, '_, '_>, rect: Rect, clip: Rect, hit: Hit) {
    let p = actions.app.palette;
    let hit_rect = rect.intersect(clip);
    if hit_rect.w <= 0.0 || hit_rect.h <= 0.0 {
        return;
    }
    let (mx, my) = actions.app.mouse();
    let hovered = actions.app.cursor_inside() && hit_rect.contains(mx, my);
    if hovered {
        actions.frame.rect_clipped(rect, p.hover, RADIUS_ROW, clip);
    }
    let color = if hovered { p.text } else { p.text_muted };
    let cy = center(rect).1;
    let icon_x = rect.x + 8.5;
    draw_copy_icon(actions.frame, (icon_x, cy), color, hit_rect);
    let label_area = Rect::new(rect.x + 18.0, rect.y, (rect.w - 22.0).max(0.0), rect.h);
    if label_area.w > 4.0 {
        let label = ellipsize(
            actions.frame,
            t!("agent-copy-code"),
            actions.app.fonts.ui_small,
            label_area.w,
        );
        ui_text_small(
            actions.frame,
            actions.app,
            label_area.x,
            label_area,
            &label,
            color,
            label_area.intersect(clip),
        );
    }
    actions.hits.push((hit_rect, hit));
}

/// Agent 侧边栏专用的紧凑复制图标（约 9×8.5 逻辑像素），视觉高度与时间等次要文字接近。
pub(super) fn draw_copy_icon(
    frame: &mut Frame<'_>,
    (cx, cy): (f32, f32),
    color: Color,
    clip: Rect,
) {
    let (width, height, r, shift, stroke) = (7.0, 6.5, 0.9, 2.0, 1.1);
    let (x, y) = (
        cx - width / 2.0 + shift / 2.0,
        cy - height / 2.0 + shift / 2.0,
    );
    let front = [
        Stroke::Line(x + r, y, x + width - r, y),
        Stroke::Arc(x + width - r, y + r, r, -FRAC_PI_2, 0.0),
        Stroke::Line(x + width, y + r, x + width, y + height - r),
        Stroke::Arc(x + width - r, y + height - r, r, 0.0, FRAC_PI_2),
        Stroke::Line(x + width - r, y + height, x + r, y + height),
        Stroke::Arc(x + r, y + height - r, r, FRAC_PI_2, PI),
        Stroke::Line(x, y + height - r, x, y + r),
        Stroke::Arc(x + r, y + r, r, -PI, -FRAC_PI_2),
    ];
    frame.stroke(&front, stroke, color, clip);
    let (bx, by) = (x - shift, y - shift);
    let back = [
        Stroke::Line(x - 0.8, by + height, bx + r, by + height),
        Stroke::Arc(bx + r, by + height - r, r, FRAC_PI_2, PI),
        Stroke::Line(bx, by + height - r, bx, by + r),
        Stroke::Arc(bx + r, by + r, r, -PI, -FRAC_PI_2),
        Stroke::Line(bx + r, by, bx + width - r, by),
        Stroke::Arc(bx + width - r, by + r, r, -FRAC_PI_2, 0.0),
        Stroke::Line(bx + width, by + r, bx + width, y - 0.8),
    ];
    frame.stroke(&back, stroke, color, clip);
}

/// Agent 对话 Markdown 的配色集合，由当前 Winc 主题的 [`Palette`] 派生。
#[derive(Clone, Copy)]
struct ChatStyle {
    dark: bool,
    text: Color,
    muted: Color,
    link: Color,
    link_underline: Color,
    divider: Color,
    inline_code_bg: Color,
    inline_code_border: Color,
    code_bg: Color,
    code_header_bg: Color,
    code_border: Color,
    quote_bg: Color,
    quote_bar: Color,
    mark_bg: Color,
}

impl ChatStyle {
    fn from_palette(p: &crate::theme::Palette) -> Self {
        let dark = p.dark;
        Self {
            dark,
            text: p.text,
            muted: p.text_muted,
            link: p.accent,
            link_underline: p.accent.alpha(if dark { 0.42 } else { 0.36 }),
            divider: p.text.alpha(if dark { 0.12 } else { 0.10 }),
            inline_code_bg: p.text.alpha(if dark { 0.11 } else { 0.07 }),
            inline_code_border: p.text.alpha(if dark { 0.10 } else { 0.09 }),
            code_bg: p.group_bg,
            code_header_bg: p.text.alpha(if dark { 0.045 } else { 0.035 }),
            code_border: p.border,
            quote_bg: p.text.alpha(if dark { 0.028 } else { 0.024 }),
            quote_bar: p.text.alpha(if dark { 0.26 } else { 0.22 }),
            mark_bg: if dark {
                Color::hex(0xbb8009).alpha(0.30)
            } else {
                Color::hex(0xfff8c5)
            },
        }
    }

    fn alert(dark: bool, alert: Alert) -> Color {
        let (light, night) = match alert {
            Alert::Note => (0x0969da, 0x4493f8),
            Alert::Tip => (0x1a7f37, 0x3fb950),
            Alert::Important => (0x8250df, 0xab7df8),
            Alert::Warning => (0x9a6700, 0xd29922),
            Alert::Caution => (0xd1242f, 0xf85149),
        };
        Color::hex(if dark { night } else { light })
    }
}

#[derive(Clone, Copy)]
struct BlockEnv {
    color: Color,
    depth: usize,
}

#[derive(Clone, Copy)]
struct Para {
    size: f32,
    bold: bool,
    color: Color,
    line: f32,
    align: Align,
}

#[derive(Clone, Copy)]
struct Flow {
    height: f32,
    first_baseline: Option<f32>,
}

struct ChatTypeset<'a, 'f> {
    frame: &'a mut Frame<'f>,
    app: &'a App,
    base: f32,
    style: ChatStyle,
    memo: &'a mut Memo,
    text: String,
    chars: usize,
}

impl ChatTypeset<'_, '_> {
    fn lay_top_block(&mut self, block: &Block, width: f32, env: BlockEnv) -> LaidBlock {
        let mut out = Out::default();
        self.text.clear();
        self.chars = 0;
        let flow = self.block(block, 0.0, 0.0, width, env, &mut out);
        let mut laid = LaidBlock::new(
            flow.height,
            out.items,
            out.anchors,
            std::mem::take(&mut self.text).into_boxed_str(),
            self.chars,
            Box::default(),
        );
        laid.scrolls = out.scrolls;
        laid
    }

    fn blocks(
        &mut self,
        blocks: &[Block],
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let mut top = y;
        let mut first_baseline = None;
        let mut prev_bottom: Option<f32> = None;
        for block in blocks {
            let (margin_top, margin_bottom) = block_margins(block, self.base, env.depth > 0);
            if let Some(below) = prev_bottom {
                top += below.max(margin_top);
            }
            let flow = self.block(block, x, top, width, env, out);
            if first_baseline.is_none() {
                first_baseline = flow.first_baseline;
            }
            top += flow.height;
            prev_bottom = Some(margin_bottom);
        }
        Flow {
            height: top - y,
            first_baseline,
        }
    }

    fn block(
        &mut self,
        block: &Block,
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        match block {
            Block::Paragraph(inline) if !has_math(inline) => {
                self.flow(inline, x, y, width, self.body_para(env), &mut out.items)
            }
            Block::Heading(level, inline) if !has_math(inline) => {
                self.heading(*level, inline, x, y, width, env, out)
            }
            Block::Code { lang, text, .. } if lang != "mermaid" => {
                self.code(lang, text, x, y, width, &mut out.items)
            }
            Block::Quote(alert, blocks) => self.quote(*alert, blocks, x, y, width, env, out),
            Block::List(start, items) => self.list(*start, items, x, y, width, env, out),
            Block::Rule(_) => self.rule(x, y, width, &mut out.items),
            Block::Footnote(label, blocks) => self.footnote(label, blocks, x, y, width, env, out),
            Block::Details {
                open,
                summary,
                blocks,
            } => self.details(*open, summary, blocks, x, y, width, env, out),
            // 表格、公式、Mermaid 图及包含行内公式的段落继续委托给 winc-typeset。
            _ => self.delegate(block, x, y, width, env, out),
        }
    }

    fn body_para(&self, env: BlockEnv) -> Para {
        Para {
            size: self.base,
            bold: false,
            color: env.color,
            line: BODY_LINE,
            align: Align::None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn heading(
        &mut self,
        level: u8,
        inline: &Inline,
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let scale = [1.38, 1.23, 1.12, 1.0, 0.92, 0.88][(level as usize).clamp(1, 6) - 1];
        let size = (self.base * scale).round().max(10.0);
        let color = if level >= 5 {
            self.style.muted
        } else {
            env.color
        };
        let para = Para {
            size,
            bold: true,
            color,
            line: HEADING_LINE,
            align: Align::None,
        };
        let mut flow = self.flow(inline, x, y, width, para, &mut out.items);
        if level <= 2 {
            let gap = 5.0;
            out.items.push(Mark::Fill {
                rect: Rect::new(x, y + flow.height + gap, width, 1.0),
                color: self.style.divider,
                radius: 0.0,
            });
            flow.height += gap + 1.0;
        }
        flow
    }

    fn code(
        &mut self,
        lang: &str,
        text: &str,
        x: f32,
        y: f32,
        width: f32,
        out: &mut Vec<Mark>,
    ) -> Flow {
        let size = (self.base * 0.88).round().max(10.0);
        let line_h = (size * 1.5).round();
        let metrics = self.frame.metrics(FontKind::Mono, size);
        let room = (width - CODE_PAD_X * 2.0).max(size);
        let language = winc_syntax::find(lang);
        let mut state = language.map_or(winc_syntax::LineState::Normal, |l| l.start_state());
        let bg_at = out.len();
        let mut top = y + CODE_HEADER + CODE_PAD_Y;
        let mut first_baseline = None;

        for line in text.split('\n') {
            let flow = self.code_line(
                line,
                language,
                &mut state,
                x + CODE_PAD_X,
                top,
                room,
                size,
                line_h,
                metrics.ascent,
                metrics.descent,
                out,
            );
            first_baseline.get_or_insert(top + flow.first_baseline.unwrap_or(0.0));
            top += flow.height;
        }

        let height = (top + CODE_PAD_Y - y).max(CODE_HEADER + CODE_PAD_Y * 2.0);
        let rect = Rect::new(x, y, width, height);

        // 先画代码块整体背景、顶部语言栏与底部分隔线（位于代码文字之下）。
        let header_rect = Rect::new(x + 1.0, y + 1.0, (width - 2.0).max(0.0), CODE_HEADER - 1.0);
        let mut chrome = vec![
            Mark::Fill {
                rect,
                color: self.style.code_bg,
                radius: CODE_RADIUS,
            },
            Mark::Corners {
                rect: header_rect,
                color: self.style.code_header_bg,
                radii: [CODE_RADIUS - 1.0, CODE_RADIUS - 1.0, 0.0, 0.0],
            },
            Mark::Fill {
                rect: Rect::new(x + 1.0, y + CODE_HEADER, (width - 2.0).max(0.0), 1.0),
                color: self.style.code_border.alpha(0.75),
                radius: 0.0,
            },
        ];
        let lang_label = if lang.is_empty() { "code" } else { lang };
        let ui_small = self.app.fonts.ui_small;
        let ui_metrics = self.frame.metrics(FontKind::Ui, ui_small);
        let label_baseline =
            y + (CODE_HEADER - ui_metrics.ascent - ui_metrics.descent) / 2.0 + ui_metrics.ascent;
        chrome.push(Mark::Text {
            x: x + CODE_PAD_X,
            baseline: label_baseline,
            text: lang_label.into(),
            font: FontKind::Ui,
            faux: Faux::default(),
            size: ui_small,
            color: self.style.muted,
            span: None,
        });
        out.splice(bg_at..bg_at, chrome);

        // 外框细描边盖在最上层，保持圆角锐利；最后压入复制按钮命中项。
        out.push(Mark::path(
            rounded_frame(rect, CODE_RADIUS),
            1.0,
            self.style.code_border,
        ));
        out.push(Mark::Code {
            rect,
            text: text.into(),
            diagram: false,
        });

        Flow {
            height,
            first_baseline,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn code_line(
        &mut self,
        line: &str,
        language: Option<&'static winc_syntax::Language>,
        state: &mut winc_syntax::LineState,
        x: f32,
        y: f32,
        room: f32,
        size: f32,
        line_h: f32,
        ascent: f32,
        descent: f32,
        out: &mut Vec<Mark>,
    ) -> Flow {
        let chars: Vec<char> = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .collect();
        let text_color = self.style.text;
        let mut colors = vec![text_color; chars.len()];
        if let Some(language) = language.filter(|_| chars.len() <= winc_syntax::MAX_LINE_CHARS) {
            let mut tokens = Vec::new();
            *state = winc_syntax::highlight_line(language, &chars, *state, &mut tokens);
            for token in &tokens {
                let color = self.app.palette.token(token.kind);
                for c in &mut colors[token.start.min(chars.len())..token.end.min(chars.len())] {
                    *c = color;
                }
            }
        }
        let baseline_at = |top: f32| y + top + (line_h - ascent - descent) / 2.0 + ascent;
        let flush = |out: &mut Vec<Mark>,
                     run: &mut (String, Vec<f32>, usize, f32, Option<Color>),
                     top: f32| {
            if !run.0.is_empty() {
                out.push(Mark::Text {
                    x: x + run.3,
                    baseline: baseline_at(top),
                    text: std::mem::take(&mut run.0).into(),
                    font: FontKind::Mono,
                    faux: Faux::default(),
                    size,
                    color: run.4.unwrap_or(text_color),
                    span: Some(Span {
                        start: run.2,
                        advances: std::mem::take(&mut run.1).into(),
                        source: None,
                        jumps: None,
                    }),
                });
            }
            run.4 = None;
        };
        let (mut top, mut pen) = (0.0, 0.0);
        let mut run = (String::new(), Vec::new(), 0, 0.0, None);
        let widths = self
            .frame
            .advances_faux(FontKind::Mono, Faux::default(), size, &chars);
        for (i, &c) in chars.iter().enumerate() {
            let w = widths[i];
            if pen + w > room && pen > 0.0 {
                flush(out, &mut run, top);
                top += line_h;
                pen = 0.0;
            }
            if run.4.is_some_and(|rc| rc != colors[i]) {
                flush(out, &mut run, top);
            }
            if run.0.is_empty() {
                run.2 = self.chars;
                run.3 = pen;
            }
            run.4 = Some(colors[i]);
            run.0.push(c);
            run.1.push(w);
            self.text.push(c);
            self.chars += 1;
            pen += w;
        }
        flush(out, &mut run, top);
        if chars.is_empty() {
            out.push(Mark::Text {
                x,
                baseline: baseline_at(0.0),
                text: "".into(),
                font: FontKind::Mono,
                faux: Faux::default(),
                size,
                color: text_color,
                span: Some(Span {
                    start: self.chars,
                    advances: Arc::new([]),
                    source: None,
                    jumps: None,
                }),
            });
        }
        self.text.push('\n');
        self.chars += 1;
        Flow {
            height: top + line_h,
            first_baseline: Some((line_h - ascent - descent) / 2.0 + ascent),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        alert: Option<Alert>,
        blocks: &[Block],
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let base = self.base;
        let bar_w = 3.0;
        let pad_left = 12.0;
        let pad_right = 8.0;
        let pad_y = 6.0;
        let inner_x = x + bar_w + pad_left;
        let room = (width - bar_w - pad_left - pad_right).max(1.0);
        let (bar_color, bg_color, inner_env) = match alert {
            Some(alert) => {
                let c = ChatStyle::alert(self.style.dark, alert);
                let bg = c.alpha(if self.style.dark { 0.09 } else { 0.06 });
                (
                    c,
                    bg,
                    BlockEnv {
                        depth: env.depth + 1,
                        ..env
                    },
                )
            }
            None => (
                self.style.quote_bar,
                self.style.quote_bg,
                BlockEnv {
                    color: self.style.muted,
                    depth: env.depth + 1,
                },
            ),
        };
        let bg_at = out.items.len();
        let mut top = y + pad_y;
        let mut first_baseline = None;
        if let Some(alert) = alert {
            let title = match alert {
                Alert::Note => "Note",
                Alert::Tip => "Tip",
                Alert::Important => "Important",
                Alert::Warning => "Warning",
                Alert::Caution => "Caution",
            };
            let inline = Inline {
                text: title.to_owned(),
                runs: vec![winc_preview::markdown::Run {
                    end: title.len(),
                    style: Style::default(),
                }],
                ..Inline::default()
            };
            let para = Para {
                size: base,
                bold: true,
                color: bar_color,
                line: BODY_LINE,
                align: Align::None,
            };
            let flow = self.flow(&inline, inner_x, top, room, para, &mut out.items);
            first_baseline = flow.first_baseline;
            top += flow.height + 3.0;
        }
        let flow = self.blocks(blocks, inner_x, top, room, inner_env, out);
        first_baseline = first_baseline.or(flow.first_baseline);
        let height = (top + flow.height + pad_y - y).max(base * BODY_LINE);
        out.items.insert(
            bg_at,
            Mark::Corners {
                rect: Rect::new(x, y, width, height),
                color: bg_color,
                radii: [2.0, 5.0, 5.0, 2.0],
            },
        );
        out.items.push(Mark::Fill {
            rect: Rect::new(x, y, bar_w, height),
            color: bar_color,
            radius: 1.5,
        });
        Flow {
            height,
            first_baseline,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn list(
        &mut self,
        start: Option<u64>,
        items: &[ListItem],
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let base = self.base;
        let mut indent = (base * 1.45).round();
        if let Some(start) = start {
            let last_num = start.saturating_add(items.len().saturating_sub(1) as u64);
            let marker = format!("{last_num}.");
            let needed = self.frame.measure(FontKind::Prose, base, &marker) + base * 0.5;
            indent = indent.max(needed.ceil());
        }
        let content_x = x + indent;
        let room = (width - indent).max(1.0);
        let inner = BlockEnv {
            depth: env.depth + 1,
            ..env
        };
        let gap = (base * 0.3).round();
        let mut top = y;
        let mut first_baseline = None;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                top += gap;
            }
            let (item_x, item_room) = if item.bare {
                (x, width)
            } else {
                (content_x, room)
            };
            let flow = self.blocks(&item.blocks, item_x, top, item_room, inner, out);
            first_baseline = first_baseline.or(flow.first_baseline);
            let baseline = flow.first_baseline;
            match (item.task, start) {
                _ if item.bare => {}
                (Some(checked), _) => {
                    let size = (base * 0.9).round();
                    let mid = baseline.map_or(top + base * 0.8, |b| b - base * 0.35);
                    out.items.push(Mark::Check {
                        rect: Rect::new(
                            content_x - size - base * 0.4,
                            mid - size / 2.0,
                            size,
                            size,
                        ),
                        checked,
                        source: u32::MAX,
                    });
                }
                (None, Some(start)) => {
                    let marker = format!("{}.", start + i as u64);
                    self.marker(
                        &marker,
                        content_x,
                        top,
                        baseline,
                        self.style.muted,
                        &mut out.items,
                    );
                }
                (None, None) => self.bullet(content_x, top, baseline, env.depth, &mut out.items),
            }
            top += flow.height.max(if item.blocks.is_empty() {
                base * BODY_LINE
            } else {
                0.0
            });
        }
        Flow {
            height: top - y,
            first_baseline,
        }
    }

    fn rule(&self, x: f32, y: f32, width: f32, out: &mut Vec<Mark>) -> Flow {
        out.push(Mark::Fill {
            rect: Rect::new(x, y, width, 1.0),
            color: self.style.divider,
            radius: 0.0,
        });
        Flow {
            height: 1.0,
            first_baseline: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn footnote(
        &mut self,
        label: &str,
        blocks: &[Block],
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let inner = BlockEnv {
            color: self.style.muted,
            depth: env.depth + 1,
        };
        let marker = format!("{label}.");
        let indent = (self.base * 1.45)
            .max(self.frame.measure(FontKind::Prose, self.base, &marker) + self.base * 0.5)
            .ceil();
        let room = (width - indent).max(1.0);
        let flow = self.blocks(blocks, x + indent, y, room, inner, out);
        self.marker(
            &marker,
            x + indent,
            y,
            flow.first_baseline,
            self.style.muted,
            &mut out.items,
        );
        flow
    }

    #[allow(clippy::too_many_arguments)]
    fn details(
        &mut self,
        open: bool,
        summary: &Inline,
        blocks: &[Block],
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let indent = (self.base * 1.25).round();
        let room = (width - indent).max(1.0);
        let para = Para {
            bold: true,
            ..self.body_para(env)
        };
        let flow = self.flow(summary, x + indent, y, room, para, &mut out.items);
        let arrow = if open { "▾" } else { "▸" };
        self.marker(
            arrow,
            x + indent,
            y,
            flow.first_baseline,
            self.style.muted,
            &mut out.items,
        );
        if !open {
            return flow;
        }
        let top = y + flow.height + (self.base * 0.4).round();
        let inner = self.blocks(
            blocks,
            x,
            top,
            width,
            BlockEnv {
                depth: env.depth + 1,
                ..env
            },
            out,
        );
        Flow {
            height: top + inner.height - y,
            first_baseline: flow.first_baseline,
        }
    }

    fn marker(
        &mut self,
        marker: &str,
        content_x: f32,
        y: f32,
        baseline: Option<f32>,
        color: Color,
        out: &mut Vec<Mark>,
    ) {
        let size = self.base;
        let baseline = baseline.unwrap_or_else(|| {
            let m = self.frame.metrics(FontKind::Prose, size);
            y + (size * BODY_LINE - m.ascent - m.descent) / 2.0 + m.ascent
        });
        let w = self.frame.measure(FontKind::Prose, size, marker);
        out.push(Mark::Text {
            x: content_x - size * 0.4 - w,
            baseline,
            text: marker.into(),
            font: FontKind::Prose,
            faux: Faux::default(),
            size,
            color,
            span: None,
        });
    }

    fn bullet(
        &self,
        content_x: f32,
        top: f32,
        baseline: Option<f32>,
        depth: usize,
        out: &mut Vec<Mark>,
    ) {
        let base = self.base;
        let mid = baseline.map_or(top + base * 0.8, |b| b - base * 0.34);
        let cx = content_x - base * 0.65;
        let color = self.style.muted;
        match depth {
            0 => {
                let r = (base * 0.16).max(2.0);
                out.push(Mark::Fill {
                    rect: Rect::new(cx - r, mid - r, r * 2.0, r * 2.0),
                    color,
                    radius: r,
                });
            }
            1 => {
                let r = (base * 0.15).max(2.0);
                out.push(Mark::path(
                    vec![Stroke::Circle(cx, mid, r)],
                    (base * 0.08).max(1.0),
                    color,
                ));
            }
            _ => {
                let side = (base * 0.28).max(3.5);
                out.push(Mark::Fill {
                    rect: Rect::new(cx - side / 2.0, mid - side / 2.0, side, side),
                    color,
                    radius: 1.0,
                });
            }
        }
    }

    fn delegate(
        &mut self,
        block: &Block,
        x: f32,
        y: f32,
        width: f32,
        env: BlockEnv,
        out: &mut Out,
    ) -> Flow {
        let p = self.app.palette;
        let images = HashMap::new();
        let toggled = HashSet::new();
        let mut sub_out = Out::default();
        let mut cx = Cx {
            frame: self.frame,
            base: self.base,
            token: &|kind| p.token(kind),
            surface: p.editor_bg,
            accent: p.accent,
            colors: Colors::new(p.text, p.text_muted, p.accent, p.dark),
            dark: p.dark,
            images: &images,
            image_path: &|_| None,
            wanted: Vec::new(),
            memo: self.memo,
            toggled: &toggled,
            text: String::new(),
            chars: 0,
            raw: None,
            cell: None,
            preedit: false,
            line_numbers: false,
            code_header: 0.0,
            bleed: (0.0, 0.0),
            tall: 360.0,
            repeat: 0,
            toggles: HashMap::new(),
        };
        let sub_env = Env {
            color: env.color,
            depth: env.depth,
        };
        let flow = cx.block(block, x, y, width, sub_env, &mut sub_out);
        let item_base = out.items.len() as u32;
        let char_base = self.chars;
        for item in &mut sub_out.items {
            if let Mark::Text {
                span: Some(span), ..
            } = item
            {
                span.start += char_base;
            }
        }
        for scroller in &mut sub_out.scrolls {
            scroller.items = scroller.items.start + item_base..scroller.items.end + item_base;
        }
        out.items.extend(sub_out.items);
        out.anchors.extend(sub_out.anchors);
        out.scrolls.extend(sub_out.scrolls);
        self.text.push_str(&cx.text);
        self.chars += cx.chars;
        self.end_paragraph();
        Flow {
            height: flow.height,
            first_baseline: None,
        }
    }

    fn end_paragraph(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
            self.chars += 1;
        }
    }

    fn flow(
        &mut self,
        inline: &Inline,
        x: f32,
        y: f32,
        width: f32,
        para: Para,
        out: &mut Vec<Mark>,
    ) -> Flow {
        let (atoms, runs) = self.atoms(inline, para);
        let lines = break_lines(&atoms, width);
        let mut top = y;
        let mut first_baseline = None;
        let main = self.frame.metrics(FontKind::Prose, para.size);
        let mut prev_end = 0;
        for line in lines {
            if atoms[prev_end..line.start]
                .iter()
                .any(|a| matches!(a.kind, AtomKind::Newline))
            {
                self.text.push('\n');
                self.chars += 1;
            }
            prev_end = line.end;
            let slice = &atoms[line];
            let (mut ascent, mut descent) = (main.ascent, main.descent);
            let mut line_h = para.size * para.line;
            for atom in slice {
                if matches!(atom.kind, AtomKind::Char(_)) {
                    let st = runs[atom.run];
                    if st.size > para.size {
                        let m = self.frame.metrics(st.font, st.size);
                        ascent = ascent.max(m.ascent);
                        descent = descent.max(m.descent);
                        line_h = line_h.max(st.size * para.line);
                    }
                }
            }
            let half = ((line_h - ascent - descent) / 2.0).max(0.0);
            let baseline = top + ascent + half;
            first_baseline.get_or_insert(baseline);
            let height = ascent + descent + half * 2.0;

            let used: f32 = slice
                .iter()
                .rev()
                .skip_while(|a| a.space)
                .map(|a| a.width)
                .sum();
            let align = match para.align {
                Align::None => inline.align,
                align => align,
            };
            let mut pen = x + match align {
                Align::Center => ((width - used) / 2.0).max(0.0),
                Align::Right => (width - used).max(0.0),
                Align::None | Align::Left => 0.0,
            };
            self.emit_line(slice, &runs, &mut pen, baseline, (top, height), inline, out);
            top += height;
        }
        self.end_paragraph();
        Flow {
            height: top - y,
            first_baseline,
        }
    }

    fn style_of(&self, style: Style, para: Para) -> RunStyle {
        let code = style.has(Style::CODE);
        let note = style.has(Style::NOTE);
        let (sup, sub) = (style.has(Style::SUP), style.has(Style::SUB));
        let size = if code {
            (para.size * 0.88).round().max(10.0)
        } else if note || sup || sub {
            (para.size * 0.75).round().max(9.0)
        } else if style.has(Style::SMALL) {
            (para.size * 0.88).round().max(10.0)
        } else {
            para.size
        };
        RunStyle {
            font: if code {
                FontKind::Mono
            } else {
                FontKind::Prose
            },
            faux: Faux {
                bold: !code && (para.bold || style.has(Style::STRONG)),
                italic: style.has(Style::EMPHASIS),
            },
            size,
            color: if style.has(Style::SYNTAX) {
                self.style.muted
            } else if style.link != 0 || note {
                self.style.link
            } else {
                para.color
            },
            flags: style.flags,
            link: style.link,
            rise: if note || sup {
                para.size * 0.32
            } else if sub {
                -para.size * 0.14
            } else {
                0.0
            },
        }
    }

    fn atoms(&mut self, inline: &Inline, para: Para) -> (Vec<Atom>, Vec<RunStyle>) {
        let mut atoms = Vec::with_capacity(inline.text.len());
        let mut runs = Vec::new();
        let mut images = inline.images.iter();
        for (range, style) in inline.runs() {
            let run = runs.len();
            let st = self.style_of(style, para);
            runs.push(st);
            let code = style.has(Style::CODE);
            let pad = (st.size * 0.38).round().max(3.0);
            let lead = atoms.len();
            if code {
                atoms.push(Atom {
                    kind: AtomKind::Pad,
                    width: pad,
                    run,
                    brk: true,
                    space: false,
                });
            }
            for c in inline.text[range].chars() {
                if c == IMAGE_MARK {
                    let alt = images
                        .next()
                        .filter(|img| !img.alt.is_empty())
                        .map_or(winc_i18n::t!("preview-markdown-image-alt"), |img| {
                            img.alt.as_str()
                        });
                    for alt_c in alt.chars() {
                        self.push_char(&mut atoms, alt_c, run, st);
                    }
                    continue;
                }
                if c == '\n' {
                    atoms.push(Atom {
                        kind: AtomKind::Newline,
                        width: 0.0,
                        run,
                        brk: true,
                        space: false,
                    });
                    continue;
                }
                self.push_char(&mut atoms, c, run, st);
            }
            if code {
                if let Some(first) = atoms.get_mut(lead + 1) {
                    first.brk = false;
                }
                atoms.push(Atom {
                    kind: AtomKind::Pad,
                    width: pad,
                    run,
                    brk: false,
                    space: false,
                });
            }
        }
        (atoms, runs)
    }

    fn push_char(&mut self, atoms: &mut Vec<Atom>, c: char, run: usize, st: RunStyle) {
        let c = if c == '\t' { ' ' } else { c };
        let prev = atoms.last().and_then(|a| match a.kind {
            AtomKind::Char(p) => Some(p),
            _ => None,
        });
        if prev.is_some_and(|p| winc_emoji::may_join(p, c))
            && let Some(start) = sequence_start(atoms, c, run)
        {
            let chars: Vec<char> = atoms[start..]
                .iter()
                .filter_map(|a| match a.kind {
                    AtomKind::Char(p) => Some(p),
                    _ => None,
                })
                .chain([c])
                .collect();
            let widths = self.frame.advances_faux(st.font, st.faux, st.size, &chars);
            for (atom, &width) in atoms[start..].iter_mut().zip(&widths) {
                atom.width = width;
            }
            for atom in &mut atoms[start + 1..] {
                atom.brk = false;
            }
            atoms.push(Atom {
                kind: AtomKind::Char(c),
                width: 0.0,
                run,
                brk: false,
                space: false,
            });
            return;
        }
        let width = self.frame.advance_faux(st.font, st.faux, st.size, c);
        let brk = match prev {
            Some(p) => can_break(p, c),
            None => !atoms.is_empty(),
        };
        atoms.push(Atom {
            kind: AtomKind::Char(c),
            width,
            run,
            brk,
            space: is_space(c),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_line(
        &mut self,
        atoms: &[Atom],
        runs: &[RunStyle],
        pen: &mut f32,
        baseline: f32,
        (top, height): (f32, f32),
        inline: &Inline,
        out: &mut Vec<Mark>,
    ) {
        let mut i = 0;
        while i < atoms.len() {
            let run = atoms[i].run;
            let st = runs[run];
            let group = out.len();
            let start = *pen;
            let at = baseline - st.rise;
            let mut text = String::new();
            let mut advances = Vec::new();
            let mut text_start = 0;
            let mut text_x = 0.0;
            let mut j = i;
            let flush = |out: &mut Vec<Mark>,
                         text: &mut String,
                         advances: &mut Vec<f32>,
                         text_x: f32,
                         text_start: usize| {
                if !text.is_empty() {
                    out.push(Mark::Text {
                        x: text_x,
                        baseline: at,
                        text: std::mem::take(text).into(),
                        font: st.font,
                        faux: st.faux,
                        size: st.size,
                        color: st.color,
                        span: Some(Span {
                            start: text_start,
                            advances: std::mem::take(advances).into(),
                            source: None,
                            jumps: None,
                        }),
                    });
                }
            };
            while j < atoms.len() && atoms[j].run == run {
                let atom = &atoms[j];
                match atom.kind {
                    AtomKind::Char(c) => {
                        if text.is_empty() {
                            text_x = *pen;
                            text_start = self.chars;
                        }
                        text.push(c);
                        advances.push(atom.width);
                        self.text.push(c);
                        self.chars += 1;
                    }
                    AtomKind::Pad | AtomKind::Newline => {
                        flush(out, &mut text, &mut advances, text_x, text_start);
                    }
                }
                *pen += atom.width;
                j += 1;
            }
            flush(out, &mut text, &mut advances, text_x, text_start);
            let trailing: f32 = atoms[i..j]
                .iter()
                .rev()
                .take_while(|a| a.space)
                .map(|a| a.width)
                .sum();
            let end = *pen - trailing;
            if st.flags & Style::CODE != 0 && end > start {
                let m = self.frame.metrics(st.font, st.size);
                let pad_y = (st.size * 0.16).round().max(1.5);
                let rect = Rect::new(
                    start,
                    baseline - m.ascent - pad_y,
                    end - start,
                    m.ascent + m.descent + pad_y * 2.0,
                );
                out.insert(
                    group,
                    Mark::Fill {
                        rect,
                        color: self.style.inline_code_bg,
                        radius: INLINE_CODE_RADIUS,
                    },
                );
                out.push(Mark::path(
                    rounded_frame(rect, INLINE_CODE_RADIUS),
                    1.0,
                    self.style.inline_code_border,
                ));
            }
            if st.flags & Style::MARK != 0 && end > start {
                let m = self.frame.metrics(st.font, st.size);
                let pad = st.size * 0.1;
                let rect = Rect::new(
                    start,
                    baseline - m.ascent - pad,
                    end - start,
                    m.ascent + m.descent + pad * 2.0,
                );
                out.insert(
                    group,
                    Mark::Fill {
                        rect,
                        color: self.style.mark_bg,
                        radius: 2.0,
                    },
                );
            }
            if st.flags & Style::UNDERLINE != 0 && end > start {
                let thick = (st.size / 16.0).max(1.0);
                out.push(Mark::Fill {
                    rect: Rect::new(start, baseline + st.size * 0.14, end - start, thick),
                    color: st.color,
                    radius: 0.0,
                });
            }
            if st.flags & Style::STRIKE != 0 && end > start {
                let thick = (st.size / 16.0).max(1.0);
                out.push(Mark::Fill {
                    rect: Rect::new(start, baseline - st.size * 0.3, end - start, thick),
                    color: st.color,
                    radius: 0.0,
                });
            }
            if st.link != 0
                && let Some(url) = inline.links.get(st.link as usize - 1)
            {
                if st.flags & Style::CODE == 0 && end > start {
                    out.push(Mark::Fill {
                        rect: Rect::new(start, baseline + st.size * 0.16, end - start, 1.0),
                        color: self.style.link_underline,
                        radius: 0.0,
                    });
                }
                out.push(Mark::Link {
                    rect: Rect::new(start, top, (end - start).max(1.0), height),
                    url: url.as_str().into(),
                });
            }
            i = j;
        }
    }
}

#[derive(Clone, Copy)]
struct RunStyle {
    font: FontKind,
    faux: Faux,
    size: f32,
    color: Color,
    flags: u16,
    link: u16,
    rise: f32,
}

struct Atom {
    kind: AtomKind,
    width: f32,
    run: usize,
    brk: bool,
    space: bool,
}

enum AtomKind {
    Char(char),
    Pad,
    Newline,
}

fn has_math(inline: &Inline) -> bool {
    inline.runs.iter().any(|run| run.style.has(Style::MATH))
}

/// 对话流专用的块外边距（逻辑像素）；嵌套在列表或引用中时收紧。
fn block_margins(block: &Block, base: f32, tight: bool) -> (f32, f32) {
    if tight {
        return match block {
            Block::List(..) => ((base * 0.25).round(), 0.0),
            Block::Paragraph(_) => ((base * 0.3).round(), (base * 0.3).round()),
            _ => ((base * 0.45).round(), (base * 0.45).round()),
        };
    }
    match block {
        Block::Heading(1 | 2, _) => ((base * 1.25).round(), (base * 0.55).round()),
        Block::Heading(..) => ((base * 1.0).round(), (base * 0.4).round()),
        Block::Code { .. } | Block::Table(..) | Block::Math(..) => {
            ((base * 0.65).round(), (base * 0.75).round())
        }
        Block::Quote(..) => ((base * 0.6).round(), (base * 0.7).round()),
        Block::List(..) => ((base * 0.5).round(), (base * 0.65).round()),
        Block::Rule(_) => ((base * 1.0).round(), (base * 1.0).round()),
        Block::Footnote(..) => ((base * 0.4).round(), (base * 0.4).round()),
        Block::Paragraph(_) | Block::Details { .. } => {
            ((base * 0.55).round(), (base * 0.65).round())
        }
    }
}

/// 沿 `rect` 内侧生成圆角描边线段，线宽 1 时精确落于外圈像素。
fn rounded_frame(rect: Rect, radius: f32) -> Vec<Stroke> {
    let (l, t) = (rect.x + 0.5, rect.y + 0.5);
    let (r, b) = (rect.right() - 0.5, rect.bottom() - 0.5);
    let k = radius.min((r - l) / 2.0).min((b - t) / 2.0).max(0.0);
    vec![
        Stroke::Line(l + k, t, r - k, t),
        Stroke::Line(r, t + k, r, b - k),
        Stroke::Line(r - k, b, l + k, b),
        Stroke::Line(l, b - k, l, t + k),
        Stroke::Arc(l + k, t + k, k, -PI, -FRAC_PI_2),
        Stroke::Arc(r - k, t + k, k, -FRAC_PI_2, 0.0),
        Stroke::Arc(r - k, b - k, k, 0.0, FRAC_PI_2),
        Stroke::Arc(l + k, b - k, k, FRAC_PI_2, PI),
    ]
}

fn sequence_start(atoms: &[Atom], c: char, run: usize) -> Option<usize> {
    let tail: Vec<char> = atoms
        .iter()
        .rev()
        .take(SEQUENCE_LOOKBACK)
        .map_while(|a| match a.kind {
            AtomKind::Char(p) if a.run == run => Some(p),
            _ => None,
        })
        .collect();
    (1..=tail.len()).rev().find_map(|count| {
        let unit = tail[..count].iter().rev().copied().chain([c]);
        (winc_emoji::sequence_len(unit) == count + 1).then(|| atoms.len() - count)
    })
}

fn break_lines(atoms: &[Atom], width: f32) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut x = 0.0;
    let mut last_break: Option<usize> = None;
    while i < atoms.len() {
        let atom = &atoms[i];
        if matches!(atom.kind, AtomKind::Newline) {
            lines.push(start..i);
            start = i + 1;
            i = start;
            x = 0.0;
            last_break = None;
            continue;
        }
        if atom.brk && i > start {
            last_break = Some(i);
        }
        if x + atom.width > width + 0.01 && i > start && !atom.space {
            let forced = |mut at: usize| {
                while at > start + 1
                    && match atoms[at].kind {
                        AtomKind::Char(c) => no_break_before(c),
                        AtomKind::Pad => !atoms[at].brk,
                        _ => false,
                    }
                {
                    at -= 1;
                }
                if at > start + 1
                    && matches!(atoms[at - 1].kind, AtomKind::Pad)
                    && atoms[at - 1].brk
                {
                    at -= 1;
                }
                at
            };
            let at = last_break
                .filter(|&b| b > start)
                .unwrap_or_else(|| forced(i));
            lines.push(start..at);
            start = at;
            while start < atoms.len() && atoms[start].space {
                start += 1;
            }
            i = start;
            x = 0.0;
            last_break = None;
            continue;
        }
        x += atom.width;
        i += 1;
    }
    if start < atoms.len() || lines.is_empty() {
        lines.push(start..atoms.len());
    }
    lines
}

fn is_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF
        | 0x20000..=0x2FA1F)
}

fn can_break(p: char, c: char) -> bool {
    (is_space(p) || is_cjk(p) || is_cjk(c))
        && !no_break_before(c)
        && (is_space(p) || !no_break_after(p))
}

fn no_break_before(c: char) -> bool {
    "，。．、；：？！）〕］｝」』】》〉〗〙〟…‥—”’»›,.;:?!)]}%·・".contains(c)
}

fn no_break_after(c: char) -> bool {
    "（〔［｛「『【《〈〖〘〝“‘«‹([{".contains(c)
}
