//! Paints the Codex panel with the existing renderer and multiline input field.

use winc_agent::{ApprovalKind, ItemKind};
use winc_preview::markdown::{Block, Inline};

use crate::app::Focus;
use crate::app::agent::{Connection, Hit, Link, Panel, Tab};
use crate::layout::wrap_text;

use super::*;

const ROW: f32 = 20.0;
const PAD: f32 = 10.0;

pub(super) fn draw(app: &mut App, frame: &mut Frame<'_>) {
    let toggle = app.agent_rects().button;
    if toggle.w <= 0.0 {
        return;
    }
    paint_button(app, frame, toggle, t!("agent-title"), true, true);
    // The field and chat caches are mutated while drawing. The existing
    // input renderer also reads the window's theme, metrics and IME state.
    if let Some(mut panel) = app.agent.take() {
        draw_panel(app, frame, &mut panel);
        app.agent = Some(panel);
    }
}

fn draw_panel(app: &mut App, frame: &mut Frame<'_>, panel: &mut Panel) {
    let area = panel.rects.body;
    panel.hits.clear();
    if area.w <= 0.0 || area.h < 110.0 {
        return;
    }
    let p = app.palette;
    frame.rect_clipped(area, p.sidebar_bg, RADIUS_ISLAND, area);
    let divider = panel.rects.divider;
    let (x, y) = app.mouse();
    if divider.contains(x, y) {
        frame.rect(
            Rect::new(divider.x + 3.0, divider.y, 2.0, divider.h),
            p.accent,
        );
    }
    let body = area.inset(PAD, 8.0);
    let header = Rect::new(body.x, body.y, body.w, 26.0);
    let new = Rect::new(header.right() - 84.0, header.y, 84.0, header.h);
    let project_matches = panel.root.as_ref() == app.editor.workspace.as_ref().map(|ws| &ws.root);
    let (label, action, enabled) = match panel.connection {
        _ if !project_matches && panel.root.is_some() => (
            t!("agent-switch-project"),
            Hit::SwitchProject,
            !panel.any_active(),
        ),
        Connection::Ready => (t!("agent-new"), Hit::New, true),
        Connection::Connecting => (t!("agent-new"), Hit::New, false),
        _ => (t!("agent-reconnect"), Hit::Retry, true),
    };
    Actions {
        app,
        frame,
        hits: &mut panel.hits,
    }
    .button(new, label, action, false, enabled);
    let title = Rect::new(
        header.x,
        header.y,
        (new.x - header.x - 4.0).max(0.0),
        header.h,
    );
    let root_name = panel
        .root
        .as_ref()
        .and_then(|path| path.file_name())
        .map_or_else(|| t!("agent-title").into(), |name| name.to_string_lossy());
    ui_text(frame, app, title.x, title, &root_name, p.text, title);

    let nav = Rect::new(body.x, header.bottom() + 4.0, body.w, 25.0);
    let names = [
        (
            t!("agent-history"),
            Tab::History,
            panel.connection == Connection::Ready,
        ),
        (
            panel
                .model()
                .map_or(t!("agent-model"), |model| model.name.as_str()),
            Tab::Models,
            !panel.models().is_empty(),
        ),
        (
            panel.effort().unwrap_or(t!("agent-effort")),
            Tab::Efforts,
            panel.model().is_some(),
        ),
    ];
    let mut nav_hits = Vec::new();
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut nav_hits,
        };
        let w = ((nav.w - 8.0) / 3.0).max(0.0);
        for (i, (label, tab, enabled)) in names.iter().enumerate() {
            let area = Rect::new(nav.x + i as f32 * (w + 4.0), nav.y, w, nav.h);
            actions.button(area, label, Hit::Show(*tab), panel.tab == *tab, *enabled);
        }
    }
    panel.hits.extend(nav_hits);
    let status_top = nav.bottom() + 4.0;
    let status_color = if panel.error.is_some() {
        p.warning
    } else {
        p.text_muted
    };
    let status = if !project_matches && panel.root.is_some() {
        t!("agent-project-changed")
    } else {
        panel.status()
    };
    let rows = wrap_text(status, body.w, |text| {
        frame.measure(FontKind::Ui, app.fonts.ui, text)
    });
    for (i, text) in rows.iter().take(3).enumerate() {
        let line = Rect::new(body.x, status_top + ROW * i as f32, body.w, ROW);
        ui_text(frame, app, line.x, line, text, status_color, body);
    }
    let mut top = status_top + ROW * rows.len().min(3) as f32 + 4.0;
    if panel.workspace_parallel {
        let warning = t!("agent-parallel");
        let lines = wrap_text(warning, body.w, |text| {
            frame.measure(FontKind::Ui, app.fonts.ui, text)
        });
        for text in lines {
            let row = Rect::new(body.x, top, body.w, ROW);
            ui_text(frame, app, row.x, row, text, p.warning, body);
            top += ROW;
        }
    }
    if let Some(tokens) = panel.token_label() {
        let row = Rect::new(body.x, top, body.w, ROW);
        ui_text(frame, app, row.x, row, &tokens, p.text_dim, body);
        top += ROW;
    }

    let focused = app.focus == Focus::Agent;
    let input_h = 62.0_f32.min((body.h * 0.22).max(20.0));
    let footer = Rect::new(body.x, body.bottom() - 26.0, body.w, 26.0);
    let input = Rect::new(body.x, footer.y - input_h - 4.0, body.w, input_h);
    let send = Rect::new(footer.right() - 66.0, footer.y, 66.0, footer.h);
    let stop = Rect::new(send.x - 70.0, footer.y, 64.0, footer.h);
    let action_y = input.y - 27.0;
    let attach_file = Rect::new(body.x, action_y, 64.0, 22.0);
    let attach_selection = Rect::new(attach_file.right() + 4.0, action_y, 75.0, 22.0);
    let changes = Rect::new(body.right() - 67.0, action_y, 67.0, 22.0);
    let (can_send, can_stop) = (project_matches && panel.can_send(), panel.can_stop());
    let attachment_path = panel
        .attachment
        .as_ref()
        .map(|attached| attached.path().to_owned());
    let change_label = if panel.tab == Tab::Changes {
        t!("agent-chat")
    } else {
        t!("agent-changes")
    };
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        actions.button(send, t!("agent-send"), Hit::Send, true, can_send);
        actions.button(stop, t!("agent-stop"), Hit::Stop, false, can_stop);
        if changes.x >= attach_selection.right() {
            actions.button(
                attach_file,
                t!("agent-attach-file"),
                Hit::Attach(false),
                false,
                true,
            );
            actions.button(
                attach_selection,
                t!("agent-attach-selection"),
                Hit::Attach(true),
                false,
                true,
            );
            actions.button(
                changes,
                change_label,
                Hit::Show(Tab::Changes),
                panel.tab == Tab::Changes,
                true,
            );
        }
        if let Some(path) = &attachment_path
            && send.x - footer.x >= 138.0
        {
            let remove = Rect::new(footer.x, footer.y, 63.0, footer.h);
            actions.button(remove, t!("agent-detach"), Hit::Detach, false, true);
            let bounds = Rect::new(
                remove.right() + 3.0,
                footer.y,
                (stop.x - remove.right() - 6.0).max(0.0),
                footer.h,
            );
            ui_text(
                actions.frame,
                app,
                bounds.x,
                bounds,
                path,
                p.text_muted,
                bounds,
            );
        }
    }
    frame.rect_clipped(input, p.group_bg, 6.0, body);
    let field = input.inset(6.0, 2.0);
    let wrap = wrap_area_field(app, frame, field.w, &mut panel.field, focused);
    let cursor = draw_area_field(
        app,
        frame,
        field,
        &mut panel.field,
        wrap,
        t!("agent-placeholder"),
        focused,
    );
    if focused {
        app.ime.cursor = cursor;
    }
    top = top.min(action_y);
    let area = Rect::new(body.x, top, body.w, (action_y - top - 6.0).max(0.0));
    if area.h <= 0.0 {
        return;
    }
    match panel.tab {
        Tab::Chat if panel.approval().is_some() => draw_approval(app, frame, panel, area),
        Tab::Chat => draw_chat(app, frame, panel, area),
        Tab::Changes => draw_changes(app, frame, panel, area),
        Tab::History | Tab::Models | Tab::Efforts => draw_catalog(app, frame, panel, area),
    }
}

fn draw_catalog(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let mut rows: Vec<(String, Hit, bool)> = match panel.tab {
        Tab::History => panel
            .history()
            .iter()
            .map(|session| {
                let title = if session.title.is_empty() {
                    t!("agent-untitled").into()
                } else {
                    session.title.replace(['\n', '\r'], " ")
                };
                let title = if panel.pending(&session.id) {
                    t!("agent-session-active", name = title.as_str())
                } else {
                    title
                };
                (
                    title,
                    Hit::ChooseSession(session.id.clone()),
                    panel.selected_id() == Some(&session.id),
                )
            })
            .collect(),
        Tab::Models => panel
            .models()
            .iter()
            .map(|model| {
                let label = if model.is_default {
                    t!("agent-model-default", name = model.name.as_str())
                } else {
                    model.name.clone()
                };
                (
                    label,
                    Hit::ChooseModel(model.id.clone()),
                    panel.model().is_some_and(|m| m.id == model.id),
                )
            })
            .collect(),
        Tab::Efforts => panel
            .model()
            .into_iter()
            .flat_map(|model| &model.efforts)
            .map(|effort| {
                (
                    effort.clone(),
                    Hit::ChooseEffort(effort.clone()),
                    panel.effort() == Some(effort),
                )
            })
            .collect(),
        _ => return,
    };
    panel.viewport = area;
    if panel.more() {
        rows.push((t!("agent-more").into(), Hit::More, false));
    }
    if rows.is_empty() {
        let label = match panel.tab {
            Tab::History => t!("agent-history-empty"),
            _ => t!("agent-models-empty"),
        };
        let row = Rect::new(area.x, area.y, area.w, ROW);
        ui_text(frame, app, row.x, row, label, app.palette.text_muted, area);
        panel.max_scroll = 0.0;
        return;
    }
    let stride = 30.0;
    panel.max_scroll = (rows.len() as f32 * stride - area.h).max(0.0);
    panel.scroll = panel.scroll.min(panel.max_scroll);
    let first = (panel.scroll / stride) as usize;
    let mut actions = Actions {
        app,
        frame,
        hits: &mut panel.hits,
    };
    for (index, (label, hit, selected)) in rows.into_iter().enumerate().skip(first) {
        let y = area.y + index as f32 * stride - panel.scroll;
        if y >= area.bottom() {
            break;
        }
        let row = Rect::new(area.x, y, area.w, stride - 3.0).intersect(area);
        actions.button(row, &label, hit, selected, true);
    }
}

fn draw_changes(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let Some(changes) = panel.changes().filter(|changes| !changes.is_empty()) else {
        let row = Rect::new(area.x, area.y, area.w, ROW);
        ui_text(
            frame,
            app,
            row.x,
            row,
            t!("agent-change-empty"),
            app.palette.text_muted,
            area,
        );
        panel.viewport = area;
        panel.max_scroll = 0.0;
        return;
    };
    let stride = ROW * 2.0 + 5.0;
    let max = (changes.len() as f32 * stride - area.h).max(0.0);
    let scroll = panel.scroll.min(max);
    let start = (scroll / stride) as usize;
    let mut hits = Vec::new();
    let mut actions = Actions {
        app,
        frame,
        hits: &mut hits,
    };
    for (index, (path, reported)) in changes.iter().enumerate().skip(start) {
        let y = area.y + index as f32 * stride - scroll;
        if y >= area.bottom() {
            break;
        }
        let link = Link::from_path(path.clone());
        let label = panel
            .root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .display()
            .to_string();
        let open = Rect::new(area.x, y, (area.w - 67.0).max(0.0), ROW).intersect(area);
        let diff = Rect::new(area.right() - 63.0, y, 63.0, ROW).intersect(area);
        actions.button(open, &label, Hit::Open(link.clone(), false), false, true);
        actions.button(diff, t!("agent-diff"), Hit::Open(link, true), false, true);
        let row = Rect::new(area.x + 5.0, y + ROW, area.w - 5.0, ROW);
        let tag = if *reported {
            t!("agent-change-reported")
        } else {
            t!("agent-change-observed")
        };
        ui_text(
            actions.frame,
            app,
            row.x,
            row,
            tag,
            app.palette.text_muted,
            area,
        );
    }
    panel.hits.extend(hits);
    panel.viewport = area;
    panel.max_scroll = max;
    panel.scroll = scroll;
}

fn draw_approval(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let Some(approval) = panel.approval() else {
        return;
    };
    let (id, kind, reviewable) = (approval.id, approval.kind, approval.reviewable);
    let head = Rect::new(area.x, area.y, area.w, 25.0);
    let approve = Rect::new(area.right() - 82.0, head.y, 82.0, head.h);
    let reject = Rect::new(approve.x - 55.0, head.y, 51.0, head.h);
    let copy = Rect::new(area.x, head.bottom(), 65.0, 22.0);
    let top = copy.bottom() + 4.0;
    let viewport = Rect::new(area.x, top, area.w, (area.bottom() - top).max(0.0));
    let lines = wrap_lines(
        frame,
        app,
        &approval.details,
        viewport.w - 12.0,
        FontKind::Ui,
    );
    let title = match kind {
        ApprovalKind::Command => t!("agent-approve-command"),
        ApprovalKind::Files => t!("agent-approve-files"),
        ApprovalKind::Permissions => t!("agent-approve-permissions"),
    };
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        actions.button(
            approve,
            t!("agent-approve"),
            Hit::Decide(id, true),
            true,
            reviewable,
        );
        actions.button(
            reject,
            t!("agent-deny"),
            Hit::Decide(id, false),
            false,
            true,
        );
        actions.button(copy, t!("agent-copy"), Hit::CopyApproval, false, true);
    }
    ui_text(
        frame,
        app,
        head.x,
        head,
        title,
        app.palette.warning,
        Rect::new(head.x, head.y, (reject.x - head.x - 5.0).max(0.0), head.h),
    );
    panel.viewport = viewport;
    panel.max_scroll = (lines.len() as f32 * ROW - viewport.h).max(0.0);
    if panel.max_scroll > 0.0 {
        let hint = Rect::new(
            copy.right() + 6.0,
            copy.y,
            (area.right() - copy.right() - 6.0).max(0.0),
            copy.h,
        );
        ui_text(
            frame,
            app,
            hint.x,
            hint,
            t!("agent-approval-details"),
            app.palette.text_muted,
            hint,
        );
    }
    panel.scroll = panel.scroll.min(panel.max_scroll);
    paint_lines(
        frame,
        app,
        &lines,
        viewport.x + 3.0,
        viewport.y - panel.scroll,
        viewport,
    );
}

fn draw_chat(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let width = (area.w - 15.0).max(4.0);
    let mut bottom = 0.0;
    if let Some(cards) = panel.cards() {
        for card in cards {
            let layout = card.layout.get_or_insert_with(CardLayout::default);
            if layout.width != width
                || layout.size != app.fonts.ui
                || layout.expanded != card.expanded
            {
                *layout = card_layout(
                    frame,
                    app,
                    &mut card.markdown,
                    &card.item,
                    card.expanded,
                    width,
                );
            }
            layout.top = bottom;
            bottom += layout.height();
        }
    }
    panel.viewport = area;
    panel.max_scroll = (bottom - area.h).max(0.0);
    panel.scroll = if panel.follow {
        panel.max_scroll
    } else {
        panel.scroll.min(panel.max_scroll)
    };

    let scroll = panel.scroll;
    let mut hits = std::mem::take(&mut panel.hits);
    if let Some(cards) = panel.cards() {
        let first = cards.partition_point(|card| {
            card.layout
                .as_ref()
                .is_some_and(|layout| layout.top + layout.height() <= scroll)
        });
        for (index, card) in cards.iter().enumerate().skip(first) {
            let Some(layout) = &card.layout else { continue };
            let y = area.y + layout.top - scroll;
            if y >= area.bottom() {
                break;
            }
            let block = Rect::new(area.x, y, width, layout.height() - 4.0);
            frame.rect_clipped(block, app.palette.group_bg, 5.0, area);
            let heading =
                Rect::new(block.x + 5.0, y, (block.w - 62.0).max(0.0), 24.0).intersect(area);
            let name = match card.item.kind {
                ItemKind::User => t!("agent-user"),
                ItemKind::Message => t!("agent-title"),
                ItemKind::Command => t!("agent-command"),
                ItemKind::FileChange => t!("agent-files"),
                ItemKind::Tool => t!("agent-tool"),
            };
            let mut actions = Actions {
                app,
                frame,
                hits: &mut hits,
            };
            if matches!(card.item.kind, ItemKind::Message | ItemKind::User) {
                ui_text(
                    actions.frame,
                    app,
                    heading.x,
                    heading,
                    name,
                    app.palette.text_muted,
                    heading,
                );
                let copy = Rect::new(block.right() - 57.0, y, 53.0, 24.0).intersect(area);
                actions.button(copy, t!("agent-copy"), Hit::CopyMessage(index), false, true);
            } else {
                let sign = if card.expanded { "▾" } else { "▸" };
                let status = if card.item.status.is_empty() {
                    if card.done {
                        t!("agent-done-tool")
                    } else {
                        t!("agent-running-tool")
                    }
                } else {
                    card.item.status.as_str()
                };
                let label = format!("{sign} {name} · {status}");
                let header = Rect::new(block.x + 4.0, y, block.w - 8.0, 24.0).intersect(area);
                actions.button(header, &label, Hit::Expand(index), false, true);
            }
            let row_top = y + 24.0;
            paint_lines(
                actions.frame,
                app,
                &layout.lines,
                block.x + 6.0,
                row_top,
                area,
            );
            let first = ((area.y - row_top) / ROW).floor().max(0.0) as usize;
            for (i, line) in layout.lines.iter().enumerate().skip(first) {
                let y = row_top + i as f32 * ROW;
                if y >= area.bottom() {
                    break;
                }
                if let Some(link) = &line.link {
                    let hit = Rect::new(block.x + 4.0, y, block.w - 8.0, ROW).intersect(area);
                    if hit.h > 0.0 {
                        actions.hits.push((hit, Hit::Open(link.clone(), false)));
                    }
                }
                if let Some(block_index) = line.copy {
                    let button = Rect::new(block.right() - 82.0, y, 78.0, ROW).intersect(area);
                    actions.button(
                        button,
                        t!("agent-copy-code"),
                        Hit::CopyCode(index, block_index),
                        false,
                        true,
                    );
                }
            }
        }
    }
    panel.hits = hits;
    if panel.max_scroll > 0.0 && area.h > 0.0 {
        let thumb = (area.h * area.h / bottom).max(18.0).min(area.h);
        let y = area.y + (area.h - thumb) * scroll / panel.max_scroll;
        frame.rect_clipped(
            Rect::new(area.right() - 5.0, y, 4.0, thumb),
            app.palette.border,
            2.0,
            area,
        );
    }
}

#[derive(Default)]
pub(crate) struct CardLayout {
    width: f32,
    size: f32,
    top: f32,
    expanded: bool,
    lines: Vec<Line>,
}

impl CardLayout {
    fn height(&self) -> f32 {
        32.0 + self.lines.len() as f32 * ROW
    }
}

struct Line {
    text: String,
    font: FontKind,
    copy: Option<usize>,
    link: Option<Link>,
    heading: bool,
}

fn wrap_lines(
    frame: &mut Frame<'_>,
    app: &App,
    text: &str,
    width: f32,
    font: FontKind,
) -> Vec<Line> {
    text.split('\n')
        .flat_map(|text| {
            wrap_text(text, width.max(4.0), |text| {
                frame.measure(font, app.fonts.ui, text)
            })
            .into_iter()
            .map(|text| Line {
                text: text.to_owned(),
                font,
                copy: None,
                link: None,
                heading: false,
            })
        })
        .collect()
}

fn card_layout(
    frame: &mut Frame<'_>,
    app: &App,
    markdown: &mut Option<winc_preview::markdown::Markdown>,
    item: &winc_agent::Item,
    expanded: bool,
    width: f32,
) -> CardLayout {
    let width_text = width - 16.0;
    let mut lines = Vec::new();
    match item.kind {
        ItemKind::Message => {
            let doc = markdown.get_or_insert_with(|| winc_preview::markdown::parse(&item.text));
            blocks(frame, app, &doc.blocks, width_text, true, &mut lines);
        }
        ItemKind::User => lines = wrap_lines(frame, app, &item.text, width_text, FontKind::Ui),
        _ => {
            if !item.text.is_empty() {
                lines = wrap_lines(frame, app, &item.text, width_text, FontKind::Mono);
                if !expanded {
                    lines.truncate(2);
                }
            }
            if expanded && !item.output.is_empty() {
                lines.extend(wrap_lines(
                    frame,
                    app,
                    &item.output,
                    width_text,
                    FontKind::Mono,
                ));
            }
        }
    }
    CardLayout {
        width,
        size: app.fonts.ui,
        top: 0.0,
        expanded,
        lines,
    }
}

fn blocks(
    frame: &mut Frame<'_>,
    app: &App,
    content: &[Block],
    width: f32,
    top_level: bool,
    lines: &mut Vec<Line>,
) {
    fn inline(frame: &mut Frame<'_>, app: &App, text: &Inline, width: f32) -> Vec<Line> {
        let mut lines = wrap_lines(frame, app, &text.text, width, FontKind::Ui);
        let mut seen = Vec::new();
        for link in text
            .links
            .iter()
            .filter_map(|url| Link::parse(url, true))
            .chain(
                text.text
                    .split_whitespace()
                    .filter_map(|word| Link::parse(word, false)),
            )
            .take(8)
        {
            if seen.contains(&link) {
                continue;
            }
            seen.push(link.clone());
            let label = link.label();
            lines.push(Line {
                text: label,
                font: FontKind::Ui,
                copy: None,
                link: Some(link),
                heading: false,
            });
        }
        lines
    }
    for (index, block) in content.iter().enumerate() {
        match block {
            Block::Paragraph(text) => lines.extend(inline(frame, app, text, width)),
            Block::Heading(_, text) => {
                lines.extend(inline(frame, app, text, width).into_iter().map(|mut line| {
                    line.heading = true;
                    line
                }));
            }
            Block::Code { lang, text, .. } => {
                lines.push(Line {
                    text: lang.clone(),
                    font: FontKind::Mono,
                    copy: top_level.then_some(index),
                    heading: true,
                    link: None,
                });
                lines.extend(wrap_lines(frame, app, text, width, FontKind::Mono));
            }
            Block::Quote(_, children) | Block::Footnote(_, children) => {
                blocks(frame, app, children, width, false, lines);
            }
            Block::Details {
                summary,
                blocks: children,
                ..
            } => {
                lines.extend(inline(frame, app, summary, width));
                blocks(frame, app, children, width, false, lines);
            }
            Block::List(start, items) => {
                for (i, item) in items.iter().enumerate() {
                    let prefix = start.map_or_else(
                        || "• ".to_owned(),
                        |start| format!("{}. ", start + i as u64),
                    );
                    let at = lines.len();
                    blocks(frame, app, &item.blocks, width - 18.0, false, lines);
                    if let Some(line) = lines.get_mut(at) {
                        line.text.insert_str(0, &prefix);
                    }
                }
            }
            Block::Table(_, rows, _) => {
                for row in rows {
                    let text = row
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<Vec<_>>()
                        .join("  │  ");
                    lines.extend(wrap_lines(frame, app, &text, width, FontKind::Ui));
                }
            }
            Block::Math(text, _) => {
                lines.extend(wrap_lines(frame, app, text, width, FontKind::Mono))
            }
            Block::Rule(_) => lines.push(Line {
                text: "────────".into(),
                font: FontKind::Ui,
                copy: None,
                heading: true,
                link: None,
            }),
        }
    }
}

fn paint_lines(frame: &mut Frame<'_>, app: &App, lines: &[Line], x: f32, top: f32, clip: Rect) {
    let first = ((clip.y - top) / ROW).floor().max(0.0) as usize;
    for (i, line) in lines.iter().enumerate().skip(first) {
        let y = top + i as f32 * ROW;
        if y >= clip.bottom() {
            break;
        }
        let rect = Rect::new(x, y, (clip.right() - x - 5.0).max(0.0), ROW);
        let color = if line.heading || line.link.is_some() {
            app.palette.accent
        } else {
            app.palette.text
        };
        if matches!(line.font, FontKind::Mono) {
            frame.rect_clipped(rect, app.palette.editor_bg, 0.0, clip);
        }
        frame.text(
            line.font,
            app.fonts.ui,
            x,
            ui_baseline(app, rect, app.fonts.ui),
            &line.text,
            color,
            clip,
        );
    }
}

struct Actions<'app, 'frame, 'target> {
    app: &'app App,
    frame: &'frame mut Frame<'target>,
    hits: &'frame mut Vec<(Rect, Hit)>,
}

impl Actions<'_, '_, '_> {
    fn button(&mut self, area: Rect, label: &str, hit: Hit, primary: bool, enabled: bool) {
        if area.w <= 0.0 || area.h <= 0.0 {
            return;
        }
        paint_button(self.app, self.frame, area, label, primary, enabled);
        if enabled {
            self.hits.push((area, hit));
        }
    }
}

fn paint_button(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    primary: bool,
    enabled: bool,
) {
    let p = app.palette;
    let (x, y) = app.mouse();
    let fill = if primary && enabled {
        p.accent_fill
    } else if area.contains(x, y) && enabled {
        p.hover
    } else {
        p.tab_fill
    };
    let color = if !enabled {
        p.text_dim
    } else if primary {
        p.text_on_accent
    } else {
        p.text
    };
    frame.rect_clipped(area, fill, 5.0, area);
    ui_text(
        frame,
        app,
        area.x + 6.0,
        area,
        label,
        color,
        area.inset(3.0, 0.0),
    );
}
