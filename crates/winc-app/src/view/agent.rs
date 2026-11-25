//! Paints the Codex panel with the existing renderer and multiline input field.

use winc_agent::{ApprovalDecision, ApprovalKind, ItemKind};
use winc_typeset::{Colors, Cx, Env, Item as Mark, LaidBlock, Memo, Out};

use crate::app::Focus;
use crate::app::agent::{
    Card, Connection, Hit, Link, Panel, Tab, TextHit, TextPoint, TextSelection,
};
use crate::layout::wrap_text;

use super::*;

const ROW: f32 = 20.0;
const PAD: f32 = 14.0;

pub(super) fn draw(app: &mut App, frame: &mut Frame<'_>) {
    if app.agent_rects().body.w <= 0.0 {
        return;
    }
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
    panel.text_hits.clear();
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
    let body = area.inset(PAD, 12.0);
    let header = Rect::new(body.x, body.y, body.w, 30.0);
    let close = Rect::new(header.right() - 25.0, header.y + 2.0, 25.0, 25.0);
    let new = Rect::new(close.x - 96.0, header.y + 2.0, 90.0, 25.0);
    let project_matches = app.agent_matches_workspace(panel);
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
    let hovered = close.contains(x, y);
    draw_button(frame, p, close, area, hovered, false);
    icons::close(
        frame,
        (close.x + close.w / 2.0, close.y + close.h / 2.0),
        9.0,
        p.text_muted,
        area,
    );
    panel.hits.push((close, Hit::Close));

    let icon = Rect::new(header.x, header.y + 2.0, 26.0, 25.0);
    frame.rect_clipped(icon, p.accent.alpha(0.12), 7.0, area);
    icons::codex(frame, (icon.x + 13.0, icon.y + 12.0), p.accent, area);
    let title = Rect::new(
        icon.right() + 8.0,
        header.y,
        (new.x - icon.right() - 12.0).max(0.0),
        header.h,
    );
    let title_text = panel.session_title().unwrap_or(t!("agent-title"));
    let title_text = ellipsize(frame, title_text, app.fonts.ui, title.w);
    ui_text(frame, app, title.x, title, &title_text, p.text, title);

    let nav = Rect::new(body.x, header.bottom() + 8.0, body.w, 26.0);
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
        let actions = Actions {
            app,
            frame,
            hits: &mut nav_hits,
        };
        let w = ((nav.w - 8.0) / 3.0).max(0.0);
        for (i, (label, tab, enabled)) in names.iter().enumerate() {
            let area = Rect::new(nav.x + i as f32 * (w + 4.0), nav.y, w, nav.h);
            let selected = panel.tab == *tab;
            let hovered = *enabled && area.contains(x, y);
            draw_toggle(actions.frame, p, area, nav, hovered, selected);
            let color = if !enabled {
                p.text_dim
            } else if selected {
                p.accent
            } else {
                p.text_muted
            };
            let text_area = area.inset(6.0, 0.0);
            let label = ellipsize(actions.frame, label, app.fonts.ui_small, text_area.w);
            ui_text_small(
                actions.frame,
                app,
                text_area.x,
                area,
                &label,
                color,
                text_area,
            );
            if *enabled {
                actions.hits.push((area, Hit::Show(*tab)));
            }
        }
    }
    panel.hits.extend(nav_hits);
    let status_top = nav.bottom() + 9.0;
    let warning = panel.error.is_some() || !project_matches || panel.approval().is_some();
    let status_color = if warning { p.warning } else { p.text_muted };
    let dot = if warning {
        p.warning
    } else if panel.busy() {
        p.accent
    } else {
        p.text_dim
    };
    let status = if !project_matches && panel.root.is_some() {
        t!("agent-project-changed")
    } else {
        panel.status()
    };
    let dot_rect = Rect::new(body.x + 2.0, status_top + 7.0, 5.0, 5.0);
    frame.rect_clipped(dot_rect, dot, 2.5, body);
    let rows = wrap_text(status, (body.w - 14.0).max(4.0), |text| {
        frame.measure(FontKind::Ui, app.fonts.ui, text)
    });
    for (i, text) in rows.iter().take(3).enumerate() {
        let line = Rect::new(
            body.x + 14.0,
            status_top + ROW * i as f32,
            body.w - 14.0,
            ROW,
        );
        ui_text_small(frame, app, line.x, line, text, status_color, body);
    }
    let mut top = status_top + ROW * rows.len().min(3) as f32 + 8.0;
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
    let input_h = 60.0_f32.min((body.h * 0.22).max(20.0));
    let composer = Rect::new(
        body.x,
        body.bottom() - input_h - 36.0,
        body.w,
        input_h + 36.0,
    );
    let footer = Rect::new(
        composer.x + 7.0,
        composer.bottom() - 30.0,
        (composer.w - 14.0).max(0.0),
        24.0,
    );
    let send = Rect::new(footer.right() - 58.0, footer.y, 58.0, footer.h);
    let stop = Rect::new(send.x - 54.0, footer.y, 50.0, footer.h);
    let action_y = composer.y - 28.0;
    let can_attach = project_matches
        && !panel.standalone
        && app
            .editor
            .active()
            .is_some_and(|doc| doc.path.is_some() && doc.holds_file_text());
    let can_review =
        !panel.standalone || panel.changes().is_some_and(|changes| !changes.is_empty());
    let can_send = project_matches && panel.can_send();
    let can_stop = panel.can_stop();
    let attachment_path = panel.attachment.as_ref().map(|attached| attached.path());
    let field = Rect::new(
        composer.x + 9.0,
        composer.y + 4.0,
        (composer.w - 18.0).max(0.0),
        input_h,
    );
    let outline = if focused {
        p.accent.alpha(0.46)
    } else {
        p.border
    };
    frame.rect_clipped(composer, p.group_bg, 10.0, body);
    frame.stroke(
        &icons::rounded_outline(
            composer.x + 0.5,
            composer.y + 0.5,
            (composer.w - 1.0).max(0.0),
            composer.h - 1.0,
            9.5,
        ),
        1.0,
        outline,
        body,
    );
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        let send_label = if can_stop {
            t!("agent-steer")
        } else {
            t!("agent-send")
        };
        actions.button(send, send_label, Hit::Send, true, can_send);
        if can_stop {
            actions.button(stop, t!("agent-stop"), Hit::Stop, false, true);
        }
        if can_review {
            let changes = Rect::new(body.right() - 67.0, action_y, 67.0, 22.0);
            let change_label = if panel.tab == Tab::Changes {
                t!("agent-chat")
            } else {
                t!("agent-changes")
            };
            actions.button(
                changes,
                change_label,
                Hit::Show(Tab::Changes),
                false,
                project_matches,
            );
        }
        if let Some(path) = attachment_path {
            let end = if can_stop { stop.x } else { send.x };
            let remove = Rect::new(
                footer.x,
                footer.y,
                (end - footer.x - 4.0).clamp(0.0, 58.0),
                footer.h,
            );
            actions.button(remove, t!("agent-detach"), Hit::Detach, false, true);
            let bounds = Rect::new(
                remove.right() + 3.0,
                footer.y,
                (end - remove.right() - 7.0).max(0.0),
                footer.h,
            );
            let path = ellipsize(actions.frame, path, app.fonts.ui_small, bounds.w);
            ui_text_small(
                actions.frame,
                app,
                bounds.x,
                bounds,
                &path,
                p.text_muted,
                bounds,
            );
        } else if can_attach {
            let end = if can_stop { stop.x } else { send.x };
            let attach = Rect::new(
                footer.x,
                footer.y,
                (end - footer.x - 5.0).clamp(0.0, 64.0),
                footer.h,
            );
            actions.button(
                attach,
                t!("agent-attach-file"),
                Hit::Attach(false),
                false,
                true,
            );
            let selection = Rect::new(
                attach.right() + 3.0,
                footer.y,
                (end - attach.right() - 7.0).clamp(0.0, 72.0),
                footer.h,
            );
            if selection.w >= 40.0
                && app
                    .editor
                    .active()
                    .is_some_and(|doc| !doc.selection.is_empty())
            {
                actions.button(
                    selection,
                    t!("agent-attach-selection"),
                    Hit::Attach(true),
                    false,
                    true,
                );
            }
        } else if panel.standalone && footer.w >= 205.0 {
            let end = if can_stop { stop.x } else { send.x };
            let hint = Rect::new(
                footer.x + 3.0,
                footer.y,
                (end - footer.x - 9.0).max(0.0),
                footer.h,
            );
            ui_text_small(
                actions.frame,
                app,
                hint.x,
                hint,
                t!("agent-standalone"),
                p.text_dim,
                hint,
            );
        }
    }
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
    let chat_end = if can_review { action_y } else { composer.y } - 8.0;
    let area = Rect::new(body.x, top, body.w, (chat_end - top).max(0.0));
    frame.rect_clipped(
        Rect::new(body.x, top - 4.0, body.w, 1.0),
        p.border.alpha(0.55),
        0.0,
        body,
    );
    if area.h <= 0.0 {
        return;
    }
    match panel.tab {
        Tab::Chat if panel.approval().is_some() => draw_approval(app, frame, panel, area),
        Tab::Chat => {
            if panel.transcript().is_none_or(|chat| chat.cards.is_empty()) {
                panel.viewport = area;
                draw_empty_chat(app, frame, area);
            } else {
                draw_chat(app, frame, panel, area);
            }
        }
        Tab::Changes => draw_changes(app, frame, panel, area),
        Tab::History | Tab::Models | Tab::Efforts => draw_catalog(app, frame, panel, area),
    }
    if focused && app.ime.field_preedit.is_empty() {
        let choices: Vec<_> = panel.completion.labels().collect();
        let count = choices.len().min((area.h / 24.0) as usize);
        let mut overlay = Vec::new();
        for (index, (label, selected)) in choices.into_iter().take(count).enumerate() {
            let row = Rect::new(
                area.x,
                area.bottom() - (count - index) as f32 * 24.0,
                area.w,
                24.0,
            );
            frame.rect_clipped(row, p.sidebar_bg, 0.0, area);
            Actions {
                app,
                frame,
                hits: &mut overlay,
            }
            .button(row, label, Hit::Complete(index), selected, true);
        }
        panel.hits.splice(0..0, overlay);
    }
}

fn draw_empty_chat(app: &App, frame: &mut Frame<'_>, area: Rect) {
    if area.h < 110.0 {
        return;
    }
    let p = app.palette;
    let top = area.y + (area.h * 0.25).min(130.0);
    let center = area.x + area.w / 2.0;
    let icon = Rect::new(center - 18.0, top, 36.0, 36.0);
    frame.rect_clipped(icon, p.accent.alpha(0.1), 10.0, area);
    icons::codex(frame, (center, top + 18.0), p.accent, area);
    let label = t!("agent-empty-chat");
    let width = frame.measure(FontKind::Ui, app.fonts.ui, label);
    let row = Rect::new(area.x, icon.bottom() + 9.0, area.w, 24.0);
    ui_text(
        frame,
        app,
        (center - width / 2.0).max(area.x),
        row,
        label,
        p.text_muted,
        area,
    );
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
    for (index, (path, change)) in changes.iter().enumerate().skip(start) {
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
        let tag = if change.reported {
            t!("agent-change-reported")
        } else {
            t!("agent-change-observed")
        };
        let tag = change.stat.map_or_else(
            || tag.to_owned(),
            |stat| format!("{tag}   +{} −{}", stat.added, stat.removed),
        );
        ui_text(
            actions.frame,
            app,
            row.x,
            row,
            &tag,
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
    let decisions = approval.decisions.clone();
    let decision_row = Rect::new(area.x, head.bottom(), area.w, 24.0);
    let copy = Rect::new(area.x, decision_row.bottom() + 3.0, 65.0, 22.0);
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
        let width = ((decision_row.w + 3.0) / decisions.len().max(1) as f32 - 3.0).min(122.0);
        for (index, decision) in decisions.into_iter().enumerate() {
            let (label, grant) = match decision {
                ApprovalDecision::Accept => (t!("agent-approve"), true),
                ApprovalDecision::AcceptForSession => (t!("agent-approve-session"), true),
                ApprovalDecision::Decline => (t!("agent-deny"), false),
                ApprovalDecision::Cancel => (t!("agent-cancel-turn"), false),
            };
            let button = Rect::new(
                decision_row.x + index as f32 * (width + 3.0),
                decision_row.y,
                width,
                decision_row.h,
            );
            actions.button(
                button,
                label,
                Hit::Decide(id, decision),
                decision == ApprovalDecision::Accept,
                !grant || reviewable,
            );
        }
        actions.button(copy, t!("agent-copy"), Hit::CopyApproval, false, true);
    }
    ui_text(frame, app, head.x, head, title, app.palette.warning, head);
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
    let width = (area.w - 9.0).max(4.0);
    panel.viewport = area;
    let older = panel.older();
    let mut hits = std::mem::take(&mut panel.hits);
    let mut text = SelectionPaint {
        hits: std::mem::take(&mut panel.text_hits),
        selection: panel.selection,
        card: 0,
    };
    if let Some(chat) = panel.transcript().filter(|chat| !chat.cards.is_empty()) {
        let (first, offset) = if let Some((ordinal, offset)) = chat.anchor {
            let first = ordinal.saturating_sub(chat.first).min(chat.cards.len() - 1);
            layout_card(frame, app, &mut chat.cards[first], width);
            let offset = if ordinal < chat.first {
                0.0
            } else {
                offset.min(chat.cards[first].height - 1.0)
            };
            chat.anchor = Some((chat.first + first, offset));
            (first, offset)
        } else {
            let mut remaining = area.h;
            let mut first = 0;
            for (index, card) in chat.cards.iter_mut().enumerate().rev() {
                layout_card(frame, app, card, width);
                remaining -= card.height;
                first = index;
                if remaining <= 0.0 {
                    break;
                }
            }
            (first, (-remaining).max(0.0))
        };
        let mut y = area.y - offset;
        let mut next = first;
        let mut visible = Vec::new();
        for (index, card) in chat.cards.iter_mut().enumerate().skip(first) {
            if y >= area.bottom() {
                break;
            }
            next = index + 1;
            layout_card(frame, app, card, width);
            let layout = card.layout.as_ref().expect("card laid out");
            let ordinal = chat.first + index;
            text.card = ordinal;
            let compact =
                matches!(card.item.kind, ItemKind::Read | ItemKind::Search) && !card.expanded;
            let block = Rect::new(
                area.x,
                y,
                width,
                card.height - if compact { 1.0 } else { 4.0 },
            );
            let p = app.palette;
            match card.item.kind {
                ItemKind::User => frame.rect_clipped(block, p.accent.alpha(0.095), 8.0, area),
                ItemKind::Message => {}
                _ => frame.rect_clipped(block, p.group_bg, 6.0, area),
            }
            let heading =
                Rect::new(block.x + 5.0, y, (block.w - 62.0).max(0.0), 24.0).intersect(area);
            let name = match card.item.kind {
                ItemKind::User => t!("agent-user"),
                ItemKind::Message => t!("agent-title"),
                ItemKind::Command => t!("agent-command"),
                ItemKind::Read => t!("agent-read"),
                ItemKind::Search => t!("agent-search"),
                ItemKind::Test => t!("agent-test"),
                ItemKind::Subagent => t!("agent-subagent"),
                ItemKind::Plan => t!("agent-plan"),
                ItemKind::Error => t!("agent-turn-error"),
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
                    if card.item.kind == ItemKind::User {
                        p.accent
                    } else {
                        p.text_muted
                    },
                    heading,
                );
                let copy = Rect::new(block.right() - 57.0, y, 53.0, 24.0).intersect(area);
                actions.button(
                    copy,
                    t!("agent-copy"),
                    Hit::CopyMessage(ordinal),
                    false,
                    true,
                );
            } else {
                let sign = if card.expanded { "▾" } else { "▸" };
                let status = if card.item.status.is_empty() {
                    if card.done {
                        t!("agent-done-tool")
                    } else {
                        t!("agent-running-tool")
                    }
                } else {
                    match card.item.status.as_str() {
                        "inProgress" => t!("agent-running-tool"),
                        "completed" => t!("agent-done-tool"),
                        "failed" => t!("agent-failed"),
                        "declined" => t!("agent-deny"),
                        "interrupted" | "cancelled" => t!("agent-interrupted"),
                        status => status,
                    }
                };
                let label = if compact {
                    format!(
                        "{sign} {name} · {status}  {}",
                        winc_agent::prefix(&card.item.text, 180)
                    )
                } else {
                    format!("{sign} {name} · {status}")
                };
                let header = Rect::new(block.x + 4.0, y, block.w - 8.0, 24.0).intersect(area);
                actions.button(header, &label, Hit::Expand(ordinal), false, true);
            }
            let row_top = y + 24.0;
            let clip = Rect::new(block.x, row_top, block.w, card.height - 28.0).intersect(area);
            match &layout.body {
                Body::Lines(lines) => {
                    let x = block.x + 7.0;
                    let first = ((clip.y - row_top) / ROW).floor().max(0.0) as usize;
                    for (section, line) in lines.iter().enumerate().skip(first) {
                        let y = row_top + section as f32 * ROW;
                        if y >= clip.bottom() {
                            break;
                        }
                        let rect = Rect::new(x, y, (clip.right() - x).max(0.0), ROW);
                        let baseline = ui_baseline(app, rect, app.fonts.ui);
                        text.run(
                            actions.frame,
                            app,
                            section,
                            0,
                            x,
                            rect,
                            clip,
                            line.advances.clone(),
                        );
                        actions.frame.text(
                            line.font,
                            app.fonts.ui,
                            x,
                            baseline,
                            &line.text,
                            app.palette.text,
                            clip,
                        );
                    }
                }
                Body::Markdown(blocks) => {
                    let left = block.x + 7.0;
                    let mut shown = blocks
                        .partition_point(|block| row_top + block.top + block.laid.height < clip.y);
                    while let Some(block) = blocks.get(shown) {
                        let top = row_top + block.top;
                        if top >= clip.bottom() {
                            break;
                        }
                        draw_markdown(
                            &mut actions,
                            block,
                            left,
                            top,
                            clip,
                            &mut visible,
                            &mut text,
                            shown,
                        );
                        shown += 1;
                    }
                }
            }
            y += card.height;
        }
        let range = chat.first + first..chat.first + next;
        for ordinal in
            chat.laid.start.max(chat.first)..chat.laid.end.min(chat.first + chat.cards.len())
        {
            if !range.contains(&ordinal) {
                chat.cards[ordinal - chat.first].layout = None;
            }
        }
        chat.laid = range;
        if first > 0 || offset > 0.0 || next < chat.cards.len() {
            let thumb = (area.h * (next - first) as f32 / chat.cards.len() as f32)
                .max(18.0)
                .min(area.h);
            let fraction = if chat.anchor.is_none() {
                1.0
            } else {
                let position = first as f32 + offset / chat.cards[first].height;
                (position / chat.bottom(area.h).0.max(1) as f32).min(1.0)
            };
            let y = area.y + (area.h - thumb) * fraction;
            frame.rect_clipped(
                Rect::new(area.right() - 4.0, y, 3.0, thumb),
                app.palette.border,
                2.0,
                area,
            );
        }
        if let Some(enabled) = older.filter(|_| first == 0 && offset == 0.0) {
            let label = if enabled {
                t!("agent-load-older")
            } else {
                t!("agent-loading-older")
            };
            let button = Rect::new(area.right() - 128.0, area.y + 1.0, 117.0, 24.0);
            let mut overlay = Vec::new();
            Actions {
                app,
                frame,
                hits: &mut overlay,
            }
            .button(button, label, Hit::Older, true, enabled);
            hits.splice(0..0, overlay);
        }
        if chat.anchor.is_some() {
            let label = if chat.unread {
                t!("agent-unread")
            } else {
                t!("agent-bottom")
            };
            let button = Rect::new(area.right() - 128.0, area.bottom() - 27.0, 117.0, 24.0);
            // The overlay takes precedence over the content underneath it.
            let mut overlay = Vec::new();
            Actions {
                app,
                frame,
                hits: &mut overlay,
            }
            .button(button, label, Hit::Follow, true, true);
            hits.splice(0..0, overlay);
        }
    }
    panel.hits = hits;
    panel.text_hits = text.hits;
}

pub(crate) struct CardLayout {
    width: f32,
    size: f32,
    palette: &'static crate::theme::Palette,
    expanded: bool,
    body: Body,
}

impl CardLayout {
    pub(crate) fn scroll_table(&mut self, section: usize, table: usize, pixels: f32) {
        if let Body::Markdown(blocks) = &mut self.body
            && let Some(block) = blocks.get_mut(section)
            && let Some(offset) = block.table_offsets.get_mut(table)
        {
            *offset = (*offset + pixels).clamp(0.0, block.laid.scrolls[table].max());
        }
    }

    pub(crate) fn selected_text(&self, selection: TextSelection) -> String {
        let (first, last) = selection.ordered();
        let mut runs: Box<dyn Iterator<Item = &str> + '_> = match &self.body {
            Body::Lines(lines) => Box::new(lines.iter().map(|line| line.text.as_str())),
            Body::Markdown(blocks) => Box::new(blocks.iter().map(|block| &*block.laid.text)),
        };
        runs.by_ref()
            .enumerate()
            .skip(first.section)
            .take(last.section - first.section + 1)
            .map(|(section, text)| {
                let from = if section == first.section {
                    first.offset
                } else {
                    0
                };
                let to = if section == last.section {
                    last.offset
                } else {
                    usize::MAX
                };
                text.chars()
                    .skip(from)
                    .take(to.saturating_sub(from))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

enum Body {
    Lines(Vec<Line>),
    Markdown(Vec<RichBlock>),
}

struct RichBlock {
    hash: u64,
    top: f32,
    laid: LaidBlock,
    table_offsets: Vec<f32>,
}

struct Line {
    text: String,
    font: FontKind,
    advances: std::sync::Arc<[f32]>,
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
                advances: std::sync::Arc::default(),
            })
        })
        .collect()
}

fn layout_card(frame: &mut Frame<'_>, app: &App, card: &mut Card, width: f32) {
    let layout = card.layout.as_ref();
    let same_geometry = layout.is_some_and(|layout| {
        layout.width == width
            && layout.size == app.fonts.ui
            && std::ptr::eq(layout.palette, app.palette)
    });
    if same_geometry && !card.dirty && layout.is_some_and(|layout| layout.expanded == card.expanded)
    {
        return;
    }
    let content_width = (width - 14.0).max(4.0);
    let compact = matches!(card.item.kind, ItemKind::Read | ItemKind::Search) && !card.expanded;
    let body = if compact {
        Body::Lines(Vec::new())
    } else if card.item.kind == ItemKind::Message {
        let previous = card
            .layout
            .take()
            .filter(|_| same_geometry)
            .and_then(|layout| match layout.body {
                Body::Markdown(blocks) => Some(blocks),
                Body::Lines(_) => None,
            });
        Body::Markdown(layout_markdown(
            frame,
            app,
            &card.item.text,
            content_width,
            previous.unwrap_or_default(),
        ))
    } else {
        let prose = matches!(
            card.item.kind,
            ItemKind::User | ItemKind::Plan | ItemKind::Error
        );
        let text = if card.item.kind == ItemKind::User || card.expanded {
            card.item.text.as_str()
        } else {
            winc_agent::prefix(&card.item.text, 512)
        };
        let font = if prose { FontKind::Ui } else { FontKind::Mono };
        let mut lines = wrap_lines(frame, app, text, content_width, font);
        if card.item.kind != ItemKind::User && !card.expanded {
            lines.truncate(2);
        }
        if card.expanded && !card.item.output.is_empty() {
            lines.extend(wrap_lines(
                frame,
                app,
                &card.item.output,
                content_width,
                FontKind::Mono,
            ));
        }
        for line in &mut lines {
            let chars: Vec<_> = line.text.chars().collect();
            line.advances = frame
                .advances_faux(font, winc_render::Faux::default(), app.fonts.ui, &chars)
                .into();
        }
        Body::Lines(lines)
    };
    card.height = (if compact { 25.0 } else { 30.0 })
        + match &body {
            Body::Lines(lines) => lines.len() as f32 * ROW,
            Body::Markdown(blocks) => blocks.last().map_or(0.0, |b| b.top + b.laid.height),
        };
    card.dirty = false;
    card.layout = Some(CardLayout {
        width,
        size: app.fonts.ui,
        palette: app.palette,
        expanded: card.expanded,
        body,
    });
}

fn layout_markdown(
    frame: &mut Frame<'_>,
    app: &App,
    text: &str,
    width: f32,
    old: Vec<RichBlock>,
) -> Vec<RichBlock> {
    use std::collections::{HashMap, HashSet};
    let doc = winc_preview::markdown::parse(text);
    let p = app.palette;
    let mut memo = Memo::default();
    let mut cx = Cx {
        frame,
        base: app.fonts.ui,
        token: &|kind| p.token(kind),
        surface: p.sidebar_bg,
        accent: p.accent,
        colors: Colors::new(p.text, p.text_muted, p.accent, p.dark),
        dark: p.dark,
        images: &HashMap::new(),
        image_path: &|_| None,
        wanted: Vec::new(),
        memo: &mut memo,
        toggled: &HashSet::new(),
        text: String::new(),
        chars: 0,
        raw: None,
        cell: None,
        preedit: false,
        line_numbers: false,
        code_header: 12.0,
        tall: 360.0,
        repeat: 0,
        toggles: HashMap::new(),
    };
    let env = Env {
        color: p.text,
        depth: 0,
    };
    let mut top = 0.0;
    let mut old = old.into_iter();
    doc.blocks
        .iter()
        .zip(doc.hashes)
        .map(|(block, hash)| {
            let (laid, table_offsets) = match old.next() {
                Some(old) if old.hash == hash => (old.laid, old.table_offsets),
                _ => {
                    let mut out = Out::default();
                    cx.text.clear();
                    cx.chars = 0;
                    let flow = cx.block(block, 0.0, 0.0, width, env, &mut out);
                    let mut laid = LaidBlock::new(
                        flow.height,
                        out.items,
                        out.anchors,
                        std::mem::take(&mut cx.text).into_boxed_str(),
                        cx.chars,
                        Box::default(),
                    );
                    laid.scrolls = out.scrolls;
                    let offsets = vec![0.0; laid.scrolls.len()];
                    (laid, offsets)
                }
            };
            let block = RichBlock {
                hash,
                top,
                laid,
                table_offsets,
            };
            top += block.laid.height + 5.0;
            block
        })
        .collect()
}

struct SelectionPaint {
    hits: Vec<TextHit>,
    selection: Option<TextSelection>,
    card: usize,
}

impl SelectionPaint {
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        frame: &mut Frame<'_>,
        app: &App,
        section: usize,
        offset: usize,
        x: f32,
        rect: Rect,
        clip: Rect,
        advances: std::sync::Arc<[f32]>,
    ) {
        let width = advances.iter().sum();
        let rect = Rect::new(x, rect.y, width, rect.h);
        let hit = rect.intersect(clip);
        if hit.w <= 0.0 || hit.h <= 0.0 {
            return;
        }
        if let Some(selection) = self.selection.filter(|s| s.card == self.card) {
            let (first, last) = selection.ordered();
            if (first.section..=last.section).contains(&section) {
                let from = if section == first.section {
                    first.offset.saturating_sub(offset)
                } else {
                    0
                };
                let to = if section == last.section {
                    last.offset.saturating_sub(offset)
                } else {
                    advances.len()
                };
                let from = from.min(advances.len());
                let to = to.min(advances.len());
                if from < to {
                    let left = x + advances[..from].iter().sum::<f32>();
                    let width = advances[from..to].iter().sum();
                    frame.rect_clipped(
                        Rect::new(left, rect.y, width, rect.h),
                        app.palette.selection,
                        0.0,
                        clip,
                    );
                }
            }
        }
        self.hits.push(TextHit {
            card: self.card,
            point: TextPoint { section, offset },
            rect: hit,
            x,
            advances,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_markdown(
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
                for word in text.split_whitespace() {
                    if let Some(link) = Link::parse(word, false) {
                        let index = word.as_ptr() as usize - text.as_ptr() as usize;
                        let dx = actions
                            .frame
                            .measure_faux(*font, *faux, *size, &text[..index]);
                        let width = actions.frame.measure_faux(*font, *faux, *size, word);
                        let r = Rect::new(left + x + dx, top + baseline - size, width, *size + 3.0)
                            .intersect(clip);
                        controls.push((r, Hit::Open(link, false)));
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
            Mark::Code { rect, text, .. } => {
                let r = at(rect);
                let button = Rect::new(r.right() - 78.0, r.y + 2.0, 75.0, ROW).intersect(clip);
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
    for (rect, hit) in controls
        .into_iter()
        .rev()
        .filter(|(r, _)| r.w > 0.0 && r.h > 0.0)
    {
        if matches!(hit, Hit::CopyCode(_)) {
            actions.button(rect, t!("agent-copy-code"), hit, false, true);
        } else {
            actions.hits.push((rect, hit));
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
        let color = app.palette.text;
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
        let left = matches!(
            hit,
            Hit::Expand(_) | Hit::Open(_, false) | Hit::ChooseSession(_) | Hit::Complete(_)
        );
        paint_button(self.app, self.frame, area, label, primary, enabled, left);
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
    left: bool,
) {
    let p = app.palette;
    let (x, y) = app.mouse();
    let fill = if primary && enabled {
        p.accent_fill
    } else if area.contains(x, y) && enabled {
        p.hover
    } else {
        p.text.alpha(0.0)
    };
    let color = if !enabled {
        p.text_dim
    } else if primary {
        p.text_on_accent
    } else {
        p.text
    };
    if fill.a > 0.0 {
        frame.rect_clipped(area, fill, 6.0, area);
    }
    let label = ellipsize(frame, label, app.fonts.ui_small, (area.w - 10.0).max(0.0));
    let width = frame.measure(FontKind::Ui, app.fonts.ui_small, &label);
    let inset = if left {
        6.0
    } else {
        ((area.w - width) / 2.0).max(4.0)
    };
    let x = area.x + inset;
    ui_text_small(frame, app, x, area, &label, color, area.inset(3.0, 0.0));
}
