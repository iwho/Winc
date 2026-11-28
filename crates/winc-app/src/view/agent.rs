//! Paints the Codex panel with the existing renderer and multiline input field.

mod markdown;

use winc_agent::{
    ApprovalDecision, ApprovalKind, Backend, ElicitationFieldKind, ItemKind, PermissionMode,
};

use crate::app::Focus;
use crate::app::agent::{
    Card, Connection, Hit, Link, Panel, Tab, TextHit, TextPoint, TextSelection,
};
use crate::layout::wrap_text;

use super::*;

const ROW: f32 = 20.0;
const CARD_PAD: f32 = TAB_INSET;
const USER_MESSAGE_PAD_X: f32 = CARD_PAD + 5.0;
const CARD_GAP: f32 = SIDEBAR_GAP;
const CONTENT_TOP: f32 = SIDEBAR_LIST_GAP;
const USER_MESSAGE_PAD_Y: f32 = SIDEBAR_GAP;
const MESSAGE_FOOTER: f32 = 16.0;
const STATUS_TOP_GAP: f32 = SIDEBAR_GAP + SIDEBAR_LIST_GAP;
const STATUS_PAD_Y: f32 = 6.0;
const RUNNING_SPIN_SCALE: f32 = 0.5;
const FOLLOW_MIN_SPEED: f32 = 180.0;
const FOLLOW_MAX_SPEED: f32 = 720.0;
const FOLLOW_CATCHUP_GAIN: f32 = 3.0;
const FOLLOW_MAX_CARDS_GAP: usize = 16;

fn follow_distance(
    cards: &std::collections::VecDeque<Card>,
    from: (usize, f32),
    to: (usize, f32),
) -> f32 {
    if from.0 > to.0 {
        return -follow_distance(cards, to, from);
    }
    if from.0 == to.0 {
        return to.1 - from.1;
    }
    let mut dist = (cards[from.0].display_height() - from.1).max(0.0);
    for index in (from.0 + 1)..to.0 {
        dist += cards[index].display_height();
    }
    dist + to.1
}

fn follow_advance(
    cards: &std::collections::VecDeque<Card>,
    from: (usize, f32),
    step: f32,
) -> (usize, f32) {
    let mut row = from.0;
    let mut offset = from.1 + step;
    let last = cards.len().saturating_sub(1);
    while row < last && offset >= cards[row].display_height() {
        offset -= cards[row].display_height();
        row += 1;
    }
    (row, offset.max(0.0))
}

pub(super) fn draw(app: &mut App, frame: &mut Frame<'_>) {
    if app.agent_rects().body.w <= 0.0 {
        return;
    }
    // The field and chat caches are mutated while drawing. The existing
    // input renderer also reads the window's theme, metrics and IME state.
    if let Some(mut panel) = app.agent.take() {
        let whole = panel.rects.body;
        if app.agent_split {
            frame.rect(whole, app.palette.editor_bg);
            let left_w = ((whole.w - 1.0) * 0.5).floor().max(0.0);
            let right_x = whole.x + left_w + 1.0;
            let left = Rect::new(whole.x, whole.y, left_w, whole.h);
            let right = Rect::new(
                right_x,
                whole.y,
                (whole.right() - right_x).max(0.0),
                whole.h,
            );
            panel.rects.body = if app.agent_focus_right { right } else { left };
            draw_panel(app, frame, &mut panel);
            if let Some(mut peer) = app.agent_peer.take() {
                peer.rects.body = if app.agent_focus_right { left } else { right };
                let focus = app.focus;
                app.focus = Focus::Editor;
                draw_panel(app, frame, &mut peer);
                app.focus = focus;
                peer.rects.body = whole;
                app.agent_peer = Some(peer);
            }
            frame.rect(
                Rect::new(whole.x + left_w, whole.y, 1.0, whole.h),
                app.palette.border,
            );
        } else {
            draw_panel(app, frame, &mut panel);
        }
        panel.rects.body = whole;
        app.agent = Some(panel);
    }
}

fn draw_panel(app: &mut App, frame: &mut Frame<'_>, panel: &mut Panel) {
    let area = panel.rects.body;
    panel.hits.clear();
    panel.text_hits.clear();
    panel.viewport = Rect::default();
    panel.queue_viewport = Rect::default();
    if area.w <= 0.0 || area.h <= 0.0 {
        return;
    }
    let p = app.palette;
    let native_controls = panel.backend == Backend::Codex;
    frame.rect(area, p.editor_bg);
    if area.h < 110.0 {
        panel.field.view.area = Rect::default();
        return;
    }
    let (x, y) = app.mouse();
    let body = area.inset(SIDEBAR_GAP, SIDEBAR_GAP);
    let caption = app.layout.top_bar;
    let overlaps_caption = |rect: Rect| rect.y < caption.bottom() && rect.bottom() > caption.y;
    // Like the terminal, keep the sidebar toggle and the window buttons clear.
    // The sidebar toggle is already positioned beyond the native traffic lights.
    let toggle = app.layout.sidebar_button;
    let start = if overlaps_caption(toggle) {
        body.x.max(toggle.right() + TAB_GAP)
    } else {
        body.x
    };
    let controls = app.layout.controls;
    let end = if controls.w > 0.0 && overlaps_caption(controls) {
        (controls.x - TAB_GAP).min(body.right())
    } else {
        body.right()
    };
    let header = Rect::new(start, caption.y, (end - start).max(0.0), caption.h);
    let button_y = header.y + (header.h - BUTTON) / 2.0;
    let separator_width = 1.0;
    let title_space = 32.0;
    let gap = if header.w >= title_space + 5.0 * BUTTON + 6.0 * TAB_GAP + separator_width {
        TAB_GAP
    } else {
        BUTTON_GAP
    };
    let header_button_width = ((header.w - title_space - 6.0 * gap - separator_width) / 5.0)
        .floor()
        .clamp(18.0, BUTTON);
    let step = header_button_width + gap;
    let button = |right| {
        Rect::new(
            right - header_button_width,
            button_y,
            header_button_width,
            BUTTON,
        )
    };
    let close = button(header.right());
    let split = button(header.right() - step);
    let maximize = button(header.right() - 2.0 * step);
    let separator = Rect::new(
        maximize.x - gap - separator_width,
        button_y + 7.0,
        separator_width,
        BUTTON - 14.0,
    );
    let history = button(separator.x - gap);
    let new = button(history.x - gap);
    let title = Rect::new(
        header.x,
        button_y,
        (new.x - gap - header.x).max(0.0),
        BUTTON,
    );
    let project_matches = app.agent_matches_workspace(panel);
    let title = if title.w >= BUTTON_SMALL + TAB_GAP + title_space {
        let icon = Rect::new(title.x, button_y, BUTTON_SMALL, BUTTON);
        frame.rect_clipped(icon.inset(0.0, 2.0), p.selected, RADIUS_ROW, header);
        icons::codex(frame, center(icon), p.accent, icon);
        let x = icon.right() + TAB_GAP;
        Rect::new(x, title.y, title.right() - x, title.h)
    } else {
        title
    };
    if title.w > 0.0 {
        let backend = panel.backend.name();
        let width = (frame.measure(FontKind::Ui, app.fonts.ui, backend) + 30.0).min(title.w);
        let button = Rect::new(title.x, title.y, width, title.h);
        draw_button(
            frame,
            p,
            button,
            header,
            button.contains(x, y),
            panel.tab == Tab::Backends,
        );
        let label = ellipsize(frame, backend, app.fonts.ui, (button.w - 28.0).max(0.0));
        ui_text(frame, app, button.x + 8.0, button, &label, p.text, button);
        icons::chevron(
            frame,
            (button.right() - 10.0, center(button).1),
            6.0,
            icons::Direction::Down,
            p.text_muted,
            button,
        );
        panel.hits.push((button, Hit::Show(Tab::Backends)));
        if let Some(session) = panel.session_title() {
            let session_area = Rect::new(
                button.right() + TAB_GAP,
                title.y,
                (title.right() - button.right() - TAB_GAP).max(0.0),
                title.h,
            );
            let label = ellipsize(frame, session, app.fonts.ui, session_area.w);
            ui_text(
                frame,
                app,
                session_area.x,
                session_area,
                &label,
                p.text_muted,
                session_area,
            );
        }
    }
    draw_session_actions(
        app,
        frame,
        panel,
        new.intersect(header),
        history.intersect(header),
    );
    frame.rect(separator.intersect(header), p.border);
    let maximize = maximize.intersect(header);
    let split = split.intersect(header);
    let close = close.intersect(header);
    draw_button(frame, p, maximize, area, maximize.contains(x, y), false);
    if panel.maximized {
        icons::shrink(frame, center(maximize), p.text_muted, maximize);
    } else {
        icons::expand(frame, center(maximize), p.text_muted, maximize);
    }
    if maximize.w > 0.0 {
        panel.hits.push((maximize, Hit::Maximize));
    }
    draw_button(frame, p, split, area, split.contains(x, y), app.agent_split);
    icons::split(frame, center(split), p.text_muted, split);
    if split.w > 0.0 {
        panel.hits.push((split, Hit::Split));
    }
    draw_button(frame, p, close, area, close.contains(x, y), false);
    icons::collapse(
        frame,
        center(close),
        icons::Direction::Right,
        p.text_muted,
        close,
    );
    if close.w > 0.0 {
        panel.hits.push((close, Hit::Close));
    }

    let top = header.bottom() + SIDEBAR_LIST_GAP;
    let mut top = draw_session_usage(app, frame, panel, body, top);
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
    let focused = app.focus == Focus::Agent && !panel.elicitation_field_focused();
    let has_permissions = panel.has_permission_control();
    let has_models = panel.has_model_control();
    let composer_pad = 10.0;
    let composer_pad_y = 8.0;
    let preview_size = 44.0_f32.min(body.w * 0.3);
    let preview_gutter = panel
        .image
        .as_ref()
        .map_or(0.0, |_| preview_size + SIDEBAR_GAP);
    let input_width = (body.w - 2.0 * composer_pad - preview_gutter).max(0.0);
    let wrap = wrap_area_field(app, frame, input_width, &mut panel.field, focused);
    let input_h = wrap.lines().clamp(2, 3) as f32 * fields::AREA_LINE;
    let can_attach = project_matches
        && !panel.standalone
        && app
            .editor
            .active()
            .is_some_and(|doc| doc.path.is_some() && doc.holds_file_text());
    let has_attachment = panel.attachment.is_some();
    let show_attach = can_attach && !has_attachment;
    let show_selection = show_attach
        && app
            .editor
            .active()
            .is_some_and(|doc| !doc.selection.is_empty());
    let has_context = has_attachment || panel.standalone;
    let context_h = if has_context {
        BUTTON_SMALL + SIDEBAR_LIST_GAP
    } else {
        0.0
    };
    let composer_h = input_h + BUTTON_SMALL + 2.0 * composer_pad_y + SIDEBAR_LIST_GAP + context_h;
    let composer = Rect::new(body.x, body.bottom() - composer_h, body.w, composer_h);
    let footer = Rect::new(
        composer.x + composer_pad,
        composer.bottom() - BUTTON_SMALL - composer_pad_y,
        (composer.w - 2.0 * composer_pad).max(0.0),
        BUTTON_SMALL,
    );
    let can_send = project_matches && panel.can_send();
    let can_stop = panel.can_stop();
    let send_label = if panel.editing_queue() {
        t!("agent-queue-save")
    } else if can_stop {
        t!("agent-queue-add")
    } else {
        t!("agent-send")
    };
    let send_w = button_width(app, frame, send_label).min(footer.w);
    let send = Rect::new(footer.right() - send_w, footer.y, send_w, footer.h);
    let can_review =
        !panel.standalone || panel.changes().is_some_and(|changes| !changes.is_empty());
    let change_label = if panel.tab == Tab::Changes {
        t!("agent-chat")
    } else {
        t!("agent-changes")
    };
    let change_w =
        button_width(app, frame, change_label).min((send.x - footer.x - BUTTON_GAP).max(0.0));
    let changes = Rect::new(send.x - BUTTON_GAP - change_w, footer.y, change_w, footer.h);
    let permission_label: std::borrow::Cow<'static, str> = if native_controls {
        std::borrow::Cow::Borrowed(permission_label(panel.permissions))
    } else {
        std::borrow::Cow::Owned(
            panel
                .current_mode()
                .map_or_else(String::new, |mode| mode.name.clone()),
        )
    };
    let permission_end = if can_review { changes.x } else { send.x };
    let full_access = native_controls && panel.permissions == PermissionMode::FullAccess;
    let shield_gutter = 15.0;
    let permission_width = if has_permissions {
        button_width(app, frame, &permission_label) + if full_access { shield_gutter } else { 0.0 }
    } else {
        0.0
    };
    let attach_label = t!("agent-attach-file");
    let selection_label = t!("agent-attach-selection");
    let selection_space = if show_selection {
        button_width(app, frame, selection_label) + BUTTON_GAP
    } else {
        0.0
    };
    let model = panel
        .model()
        .map_or(t!("agent-model"), |model| model.name.as_str());
    let model_text = if native_controls {
        let effort = panel.effort().unwrap_or(t!("agent-effort"));
        format!("{model} {effort}")
    } else if let Some(effort) = panel.effort() {
        format!("{model} {effort}")
    } else {
        model.to_owned()
    };
    let model_w = if has_models {
        (frame.measure(FontKind::Ui, app.fonts.ui_small, &model_text)
            + frame.measure(FontKind::Ui, app.fonts.ui_small, " ")
            + 18.0)
            .min((permission_end - footer.x).max(0.0) * 0.55)
    } else {
        0.0
    };
    let model_gap = if has_permissions && has_models {
        BUTTON_GAP
    } else {
        0.0
    };
    let permission_space = (permission_end - footer.x - BUTTON_GAP - model_w - model_gap).max(0.0);
    let (permission_w, attach_w) = if show_attach {
        let available = (permission_space - BUTTON_GAP - selection_space).max(0.0);
        let attach_w = button_width(app, frame, attach_label)
            .min((available - permission_width).max(available / 2.0));
        (permission_width.min(available - attach_w), attach_w)
    } else {
        (permission_width.min(permission_space), 0.0)
    };
    let permission = Rect::new(footer.x, footer.y, permission_w, footer.h);
    let attach = Rect::new(
        permission_end - BUTTON_GAP - attach_w,
        footer.y,
        attach_w,
        footer.h,
    );
    let selection_w = (selection_space - BUTTON_GAP)
        .max(0.0)
        .min((attach.x - BUTTON_GAP - footer.x).max(0.0));
    let selection = Rect::new(
        attach.x - BUTTON_GAP - selection_w,
        footer.y,
        selection_w,
        footer.h,
    );
    let model_x = if permission.w > 0.0 {
        permission.right() + BUTTON_GAP
    } else {
        footer.x
    };
    let model_control = Rect::new(model_x, footer.y, model_w, footer.h);

    let context = Rect::new(footer.x, footer.y - context_h, footer.w, footer.h);
    let context_end = context.right();
    let queue_count = panel.queued().count();
    let queue_content_h = queue_count as f32 * BUTTON;
    let queue_h = (queue_content_h + SIDEBAR_GAP).min((composer.y - top).max(0.0) * 0.35);
    let queue_h = if queue_count > 0 { queue_h } else { 0.0 };
    panel.queue_scroll = panel
        .queue_scroll
        .min((queue_content_h + SIDEBAR_GAP - queue_h).max(0.0));
    let status_bottom = composer.y - STATUS_PAD_Y;
    let (status_top, worktree_button) = draw_status(
        app,
        frame,
        panel,
        Rect::new(
            body.x,
            top + queue_h + STATUS_PAD_Y,
            body.w,
            (status_bottom - top - queue_h - STATUS_PAD_Y).max(0.0),
        ),
        project_matches,
    );
    let queue = Rect::new(body.x, status_top - STATUS_PAD_Y - queue_h, body.w, queue_h);
    panel.queue_viewport = queue;

    let field = Rect::new(
        composer.x + composer_pad,
        composer.y + composer_pad_y,
        (composer.w - 2.0 * composer_pad - preview_gutter).max(0.0),
        input_h,
    );
    surface(
        frame,
        composer,
        p.group_bg,
        if focused {
            p.accent.alpha(0.5)
        } else {
            p.border
        },
        RADIUS_ROW + 3.0,
        body,
    );
    if has_models {
        draw_session_controls(app, frame, panel, model_control, &model_text);
    }
    let attachment_path = panel.attachment.as_ref().map(|attached| attached.path());
    if queue_count > 0 {
        frame.rect_clipped(queue, p.group_bg, RADIUS_TAB, body);
        let bottom = Rect::new(queue.x, queue.bottom() - RADIUS_TAB, queue.w, RADIUS_TAB);
        frame.rect_clipped(bottom, p.group_bg, 0.0, body);
        let entries: Vec<_> = panel.queued().map(|entry| entry.text.clone()).collect();
        let queue_enabled = !panel.editing_queue();
        let queue_send = queue_enabled && (!panel.busy() || panel.supports_steer());
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        for (index, text) in entries.iter().enumerate() {
            let row = Rect::new(
                queue.x + composer_pad,
                queue.y + SIDEBAR_LIST_GAP + index as f32 * BUTTON - panel.queue_scroll,
                (queue.w - 2.0 * composer_pad).max(0.0),
                BUTTON,
            );
            if index > 0 {
                let mut dash_x = row.x;
                while dash_x < row.right() {
                    actions.frame.rect_clipped(
                        Rect::new(dash_x, row.y, (row.right() - dash_x).min(3.0), 1.0),
                        p.border.alpha(0.25),
                        0.0,
                        queue,
                    );
                    dash_x += 6.0;
                }
            }
            if row.y < queue.y || row.bottom() > queue.bottom() {
                continue;
            }
            let mut end = row.right();
            for (label, hit, enabled) in [
                (t!("agent-queue-now"), Hit::QueueSend(index), queue_send),
                (
                    t!("agent-queue-delete"),
                    Hit::QueueDelete(index),
                    queue_enabled,
                ),
                (t!("agent-queue-edit"), Hit::QueueEdit(index), queue_enabled),
            ] {
                if !app.cursor_inside() || !row.contains(x, y) {
                    continue;
                }
                let width = button_width(app, actions.frame, label).min((end - row.x).max(0.0));
                let button = Rect::new(end - width, row.y, width, row.h);
                actions.button(button, label, hit, false, enabled);
                end = button.x - BUTTON_GAP;
            }
            let bounds = Rect::new(row.x, row.y, (end - row.x).max(0.0), row.h);
            let summary = text
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .unwrap_or(t!("agent-queue-attachment"));
            let label = ellipsize(actions.frame, summary, app.fonts.ui_small, bounds.w);
            ui_text_small(
                actions.frame,
                app,
                bounds.x,
                bounds,
                &label,
                p.text_muted,
                bounds,
            );
        }
    }
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        if let Some(image) = &panel.image {
            let preview = Rect::new(
                field.right() + SIDEBAR_GAP,
                field.y,
                preview_size,
                preview_size,
            );
            let pixels = &image.preview;
            let scale = preview_size / pixels.width.max(pixels.height) as f32;
            let width = pixels.width as f32 * scale;
            let height = pixels.height as f32 * scale;
            let fit = Rect::new(
                preview.x + (preview.w - width) / 2.0,
                preview.y + (preview.h - height) / 2.0,
                width,
                height,
            );
            surface(
                actions.frame,
                preview,
                p.editor_bg,
                p.border,
                RADIUS_ROW,
                composer,
            );
            actions
                .frame
                .pixels_scaled(fit, pixels.width, pixels.height, &pixels.rgba, preview);
            let size = preview_size.min(18.0);
            let remove = Rect::new(preview.right() - size, preview.y, size, size);
            surface(
                actions.frame,
                remove,
                p.group_bg,
                p.border,
                RADIUS_ROW,
                composer,
            );
            let (x, y) = app.mouse();
            draw_button(
                actions.frame,
                p,
                remove,
                composer,
                remove.contains(x, y),
                false,
            );
            icons::close(actions.frame, center(remove), 8.0, p.text, remove);
            actions.hits.push((remove, Hit::RemoveImage));
        }
        actions.button(send, send_label, Hit::Send, true, can_send);
        if can_review {
            actions.button(
                changes,
                change_label,
                Hit::Show(Tab::Changes),
                panel.tab == Tab::Changes,
                project_matches,
            );
        }
        if full_access {
            let (x, y) = app.mouse();
            draw_button(
                actions.frame,
                p,
                permission,
                permission,
                project_matches && permission.contains(x, y),
                panel.permissions_open,
            );
            let content = permission.inset(SIDEBAR_LIST_GAP, 0.0);
            let label = ellipsize(
                actions.frame,
                &permission_label,
                app.fonts.ui_small,
                (content.w - shield_gutter).max(0.0),
            );
            let label_width = actions
                .frame
                .measure(FontKind::Ui, app.fonts.ui_small, &label);
            let (cx, cy) = center(permission);
            let left = (cx - (shield_gutter + label_width) / 2.0).max(content.x);
            icons::shield_plus(actions.frame, (left + 5.0, cy), p.warning, permission);
            ui_text_small(
                actions.frame,
                app,
                left + shield_gutter,
                permission,
                &label,
                p.warning,
                content,
            );
            if project_matches && permission.w > 0.0 {
                actions.hits.push((permission, Hit::TogglePermissions));
            }
        } else if has_permissions {
            actions.button(
                permission,
                &permission_label,
                Hit::TogglePermissions,
                panel.permissions_open,
                project_matches,
            );
        }
        if show_attach {
            if show_selection && selection.w > 0.0 {
                actions.button(selection, selection_label, Hit::Attach(true), false, true);
            }
            actions.button(attach, attach_label, Hit::Attach(false), false, true);
        }
        if let Some(path) = attachment_path {
            let end = context_end;
            let remove = Rect::new(
                context.x,
                context.y,
                (end - context.x - 4.0).clamp(0.0, 58.0),
                context.h,
            );
            actions.button(remove, t!("agent-detach"), Hit::Detach, false, true);
            let bounds = Rect::new(
                remove.right() + 3.0,
                context.y,
                (end - remove.right() - 7.0).max(0.0),
                context.h,
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
        } else if panel.standalone && context.w >= 205.0 {
            let end = context_end;
            let hint = Rect::new(
                context.x + 3.0,
                context.y,
                (end - context.x - 9.0).max(0.0),
                context.h,
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
    let cursor = draw_area_field(
        app,
        frame,
        field,
        &mut panel.field,
        wrap,
        if native_controls {
            t!("agent-placeholder")
        } else {
            t!("agent-backend-placeholder")
        },
        focused,
    );
    if focused {
        app.ime.cursor = cursor;
    }
    frame.rect_clipped(
        Rect::new(body.x, status_top - STATUS_PAD_Y - 1.0, body.w, 1.0),
        p.border.alpha(0.55),
        0.0,
        body,
    );
    let chat_end = queue.y - STATUS_TOP_GAP;
    let area = Rect::new(body.x, top, body.w, (chat_end - top).max(0.0));
    if panel.token_summary().is_some() || panel.account_tier.is_some() {
        frame.rect_clipped(
            Rect::new(body.x, top - SIDEBAR_LIST_GAP, body.w, 1.0),
            p.border.alpha(0.55),
            0.0,
            body,
        );
    }
    if area.h > 0.0 {
        match panel.tab {
            Tab::Chat if panel.approval().is_some() => draw_approval(app, frame, panel, area),
            Tab::Chat if panel.elicitation().is_some() => draw_elicitation(app, frame, panel, area),
            Tab::Chat => {
                let mut chat = area;
                let thinking = panel.thinking();
                if thinking {
                    let height = ROW.min(area.h);
                    chat.h -= height;
                    let row = Rect::new(area.x, chat.bottom(), area.w, height);
                    draw_thinking(app, frame, row);
                }
                if panel
                    .transcript()
                    .is_none_or(|transcript| transcript.cards.is_empty())
                {
                    panel.viewport = chat;
                    if !thinking {
                        draw_empty_chat(app, frame, chat, panel.backend);
                    }
                } else {
                    draw_chat(app, frame, panel, chat);
                }
            }
            Tab::Changes => draw_changes(app, frame, panel, area),
            Tab::History | Tab::Models | Tab::Backends => draw_catalog(app, frame, panel, area),
        }
    }
    if panel.permissions_open {
        draw_permissions(app, frame, panel, body, permission);
    } else if panel.worktree_open
        && let Some(button) = worktree_button
    {
        draw_worktrees(app, frame, panel, body, button);
    } else if focused && app.ime.field_preedit.is_empty() {
        let choices: Vec<_> = panel.completion.items().collect();
        let total = choices.len();
        let visible = total.min(((area.h / TREE_ROW) as usize).min(10));
        if visible > 0 {
            let selected = panel.completion.selected().min(total - 1);
            let start = selected
                .saturating_sub(visible - 1)
                .min(total.saturating_sub(visible));
            let popup = Rect::new(
                area.x,
                area.bottom() - visible as f32 * TREE_ROW,
                area.w,
                visible as f32 * TREE_ROW,
            );
            surface(frame, popup, p.popup_bg, p.border, RADIUS_ROW, area);
            let mut overlay = Vec::new();
            for (slot, (label, description, is_selected)) in
                choices.into_iter().skip(start).take(visible).enumerate()
            {
                let index = start + slot;
                let row = Rect::new(
                    popup.x + 1.0,
                    popup.y + slot as f32 * TREE_ROW,
                    (popup.w - 2.0).max(0.0),
                    TREE_ROW,
                );
                let hovered = app.cursor_inside() && row.contains(x, y);
                draw_button(frame, p, row, popup, hovered, is_selected);
                let inner = row.inset(SIDEBAR_GAP, 0.0);
                if let Some(desc) = description {
                    let label_w = frame
                        .measure(FontKind::Ui, app.fonts.ui, label)
                        .min(inner.w * 0.55);
                    let label_rect = Rect::new(inner.x, inner.y, label_w, inner.h);
                    let label_text = ellipsize(frame, label, app.fonts.ui, label_rect.w);
                    ui_text(
                        frame,
                        app,
                        label_rect.x,
                        label_rect,
                        &label_text,
                        p.text,
                        label_rect,
                    );
                    let desc_x = label_rect.right() + SIDEBAR_GAP;
                    let desc_w = (inner.right() - desc_x).max(0.0);
                    if desc_w > 16.0 {
                        let desc_rect = Rect::new(desc_x, inner.y, desc_w, inner.h);
                        let desc_text = ellipsize(frame, desc, app.fonts.ui_small, desc_rect.w);
                        ui_text_small(
                            frame,
                            app,
                            desc_rect.x,
                            desc_rect,
                            &desc_text,
                            p.text_muted,
                            desc_rect,
                        );
                    }
                } else {
                    let label_text = ellipsize(frame, label, app.fonts.ui, inner.w);
                    ui_text(frame, app, inner.x, inner, &label_text, p.text, inner);
                }
                overlay.push((row, Hit::Complete(index)));
            }
            panel.hits.splice(0..0, overlay);
        }
    }
}

fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => t!("agent-permission-ask"),
        PermissionMode::AutoReview => t!("agent-permission-auto"),
        PermissionMode::FullAccess => t!("agent-permission-full"),
    }
}

fn draw_permissions(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, body: Rect, button: Rect) {
    let p = app.palette;
    let items: Vec<(String, String, bool, bool, Hit)> = if panel.backend == Backend::Codex {
        [
            (PermissionMode::Ask, t!("agent-permission-ask-detail")),
            (
                PermissionMode::AutoReview,
                t!("agent-permission-auto-detail"),
            ),
            (
                PermissionMode::FullAccess,
                t!("agent-permission-full-detail"),
            ),
        ]
        .into_iter()
        .map(|(mode, description)| {
            (
                permission_label(mode).to_owned(),
                description.to_owned(),
                mode == panel.permissions,
                mode == PermissionMode::FullAccess,
                Hit::SetPermissions(mode),
            )
        })
        .collect()
    } else {
        let current = panel.current_mode().map(|mode| mode.id.as_str());
        panel
            .modes()
            .iter()
            .map(|mode| {
                (
                    mode.name.clone(),
                    mode.description.clone(),
                    current == Some(mode.id.as_str()),
                    false,
                    Hit::SetMode(mode.id.clone()),
                )
            })
            .collect()
    };
    if items.is_empty() {
        return;
    }
    let count = items.len() as f32;
    let width = body.w.min(420.0);
    let bottom = button.y - BUTTON_GAP;
    let row_h = ((bottom - body.y - 2.0 * BUTTON_GAP) / count).clamp(0.0, 43.0);
    let menu_h = count * row_h + 2.0 * BUTTON_GAP;
    let menu = Rect::new(body.x, bottom - menu_h, width, menu_h);
    surface(frame, menu, p.popup_bg, p.border, RADIUS_ROW + 4.0, body);
    let (x, y) = app.mouse();
    for (index, (name, description, selected, warning, hit)) in items.into_iter().enumerate() {
        let row = Rect::new(
            menu.x + BUTTON_GAP,
            menu.y + BUTTON_GAP + index as f32 * row_h,
            (menu.w - 2.0 * BUTTON_GAP).max(0.0),
            row_h,
        );
        draw_button(frame, p, row, menu, row.contains(x, y), selected);
        let text = row.inset(SIDEBAR_GAP, 0.0);
        let has_detail = row_h >= 36.0 && !description.is_empty();
        let title = Rect::new(text.x, text.y, text.w, if has_detail { 21.0 } else { row_h });
        let label = if selected {
            format!("✓  {name}")
        } else {
            name
        };
        let label = ellipsize(frame, &label, app.fonts.ui, title.w);
        let color = if warning { p.warning } else { p.text };
        ui_text(frame, app, title.x, title, &label, color, row);
        if has_detail {
            let detail = Rect::new(
                text.x,
                title.bottom(),
                text.w,
                row.bottom() - title.bottom(),
            );
            let description = ellipsize(frame, &description, app.fonts.ui_small, detail.w);
            ui_text_small(
                frame,
                app,
                detail.x,
                detail,
                &description,
                p.text_muted,
                row,
            );
        }
        panel.hits.push((row, hit));
    }
}

fn draw_worktrees(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, body: Rect, button: Rect) {
    let p = app.palette;
    let entries = panel.worktree_entries();
    let create_label = if panel.worktree_creating {
        t!("agent-worktree-creating").to_owned()
    } else {
        format!("+  {}", t!("agent-worktree-new"))
    };
    let create_detail = t!("agent-worktree-new-detail").to_owned();
    let busy = panel.any_active() || panel.worktree_creating;
    let count = (entries.len() + 1) as f32;
    let separator_h = 9.0;
    let width = body.w.min(300.0);
    let bottom = button.y - BUTTON_GAP;
    let row_h = ((bottom - body.y - 2.0 * BUTTON_GAP - separator_h) / count).clamp(0.0, 43.0);
    if row_h < 16.0 {
        return;
    }
    let menu_h = count * row_h + separator_h + 2.0 * BUTTON_GAP;
    let x = (button.right() - width).clamp(body.x, (body.right() - width).max(body.x));
    let menu = Rect::new(x, bottom - menu_h, width, menu_h);
    surface(frame, menu, p.popup_bg, p.border, RADIUS_ROW + 4.0, body);
    let (mx, my) = app.mouse();
    let mut y = menu.y + BUTTON_GAP;
    for (name, detail, selected, target) in entries {
        let row = Rect::new(
            menu.x + BUTTON_GAP,
            y,
            (menu.w - 2.0 * BUTTON_GAP).max(0.0),
            row_h,
        );
        let enabled = !busy || selected;
        draw_button(
            frame,
            p,
            row,
            menu,
            enabled && app.cursor_inside() && row.contains(mx, my),
            selected,
        );
        let text = row.inset(SIDEBAR_GAP, 0.0);
        let has_detail = row_h >= 36.0 && !detail.is_empty();
        let title = Rect::new(text.x, text.y, text.w, if has_detail { 21.0 } else { row_h });
        let label = if selected {
            format!("✓  {name}")
        } else {
            name
        };
        let label = ellipsize(frame, &label, app.fonts.ui, title.w);
        let color = if enabled { p.text } else { p.text_dim };
        ui_text(frame, app, title.x, title, &label, color, row);
        if has_detail {
            let sub = Rect::new(
                text.x,
                title.bottom(),
                text.w,
                row.bottom() - title.bottom(),
            );
            let detail = ellipsize(frame, &detail, app.fonts.ui_small, sub.w);
            ui_text_small(frame, app, sub.x, sub, &detail, p.text_muted, row);
        }
        if !busy {
            panel.hits.push((row, Hit::SelectWorktree(target)));
        }
        y += row_h;
    }
    frame.rect_clipped(
        Rect::new(
            menu.x + SIDEBAR_GAP,
            y + (separator_h - 1.0) * 0.5,
            (menu.w - 2.0 * SIDEBAR_GAP).max(0.0),
            1.0,
        ),
        p.border.alpha(0.6),
        0.0,
        menu,
    );
    y += separator_h;
    let row = Rect::new(
        menu.x + BUTTON_GAP,
        y,
        (menu.w - 2.0 * BUTTON_GAP).max(0.0),
        row_h,
    );
    let can_create = !busy;
    draw_button(
        frame,
        p,
        row,
        menu,
        can_create && app.cursor_inside() && row.contains(mx, my),
        false,
    );
    let text = row.inset(SIDEBAR_GAP, 0.0);
    let has_detail = row_h >= 36.0 && !create_detail.is_empty();
    let title = Rect::new(text.x, text.y, text.w, if has_detail { 21.0 } else { row_h });
    let label = ellipsize(frame, &create_label, app.fonts.ui, title.w);
    ui_text(
        frame,
        app,
        title.x,
        title,
        &label,
        if can_create { p.accent } else { p.text_dim },
        row,
    );
    if has_detail {
        let sub = Rect::new(
            text.x,
            title.bottom(),
            text.w,
            row.bottom() - title.bottom(),
        );
        let detail = ellipsize(frame, &create_detail, app.fonts.ui_small, sub.w);
        ui_text_small(frame, app, sub.x, sub, &detail, p.text_muted, row);
    }
    if can_create {
        panel.hits.push((row, Hit::CreateWorktree));
    }
}

fn button_width(app: &App, frame: &mut Frame<'_>, label: &str) -> f32 {
    frame.measure(FontKind::Ui, app.fonts.ui, label).ceil() + 2.0 * SIDEBAR_GAP
}

fn surface(frame: &mut Frame<'_>, rect: Rect, fill: Color, edge: Color, radius: f32, clip: Rect) {
    frame.rect_clipped(rect, edge, radius, clip);
    frame.rect_clipped(rect.inset(1.0, 1.0), fill, radius - 1.0, clip);
}

fn draw_session_controls(
    app: &App,
    frame: &mut Frame<'_>,
    panel: &mut Panel,
    rect: Rect,
    label: &str,
) {
    let p = app.palette;
    let enabled = !panel.models().is_empty();
    let (x, y) = app.mouse();
    let space = frame.measure(FontKind::Ui, app.fonts.ui_small, " ");
    let text = Rect::new(
        rect.x + 4.0,
        rect.y,
        (rect.w - 18.0 - space).max(0.0),
        rect.h,
    );
    let label = ellipsize(frame, label, app.fonts.ui_small, text.w);
    let arrow_x = text.x + frame.measure(FontKind::Ui, app.fonts.ui_small, &label) + space + 6.0;
    let rect = Rect::new(rect.x, rect.y, (arrow_x + 8.0 - rect.x).min(rect.w), rect.h);
    draw_button(frame, p, rect, rect, enabled && rect.contains(x, y), false);
    ui_text_small(
        frame,
        app,
        text.x,
        text,
        &label,
        if enabled { p.text } else { p.text_dim },
        text,
    );
    icons::chevron(
        frame,
        (arrow_x, center(rect).1),
        6.0,
        icons::Direction::Down,
        p.text_muted,
        rect,
    );
    if enabled && rect.w > 0.0 {
        panel.hits.push((rect, Hit::Show(Tab::Models)));
    }
}
fn draw_session_actions(
    app: &App,
    frame: &mut Frame<'_>,
    panel: &mut Panel,
    new: Rect,
    history: Rect,
) {
    let p = app.palette;
    let project_matches = app.agent_matches_workspace(panel);
    let action = if !project_matches && panel.root.is_some() {
        Hit::SwitchProject
    } else if panel.connection == Connection::Ready {
        Hit::New
    } else {
        Hit::Retry
    };
    let enabled = panel.connection == Connection::Ready
        || (panel.connection != Connection::Connecting && !panel.any_active());
    let (x, y) = app.mouse();
    for (button, hit, active) in [
        (new, action, enabled),
        (
            history,
            Hit::Show(Tab::History),
            panel.connection == Connection::Ready,
        ),
    ] {
        if button.w <= 0.0 || button.h <= 0.0 {
            continue;
        }
        draw_button(
            frame,
            p,
            button,
            button,
            active && button.contains(x, y),
            false,
        );
        let color = if active { p.text_muted } else { p.text_dim };
        match hit {
            Hit::New => icons::new_conversation(frame, center(button), color, button),
            Hit::Show(_) => icons::history(frame, center(button), color, button),
            _ => icons::refresh(frame, center(button), color, button),
        }
        if active {
            panel.hits.push((button, hit));
        }
    }
}

fn draw_session_usage(
    app: &App,
    frame: &mut Frame<'_>,
    panel: &Panel,
    body: Rect,
    top: f32,
) -> f32 {
    let summary = panel.token_summary();
    let tier = panel.account_tier.as_deref();
    if summary.is_none() && tier.is_none() {
        return top + SIDEBAR_GAP;
    }
    let p = app.palette;
    let detail_line_height = (app.fonts.ui_small * 1.6).ceil().max(24.0);
    let detail_top = if let Some((_, fraction)) = &summary {
        let track = Rect::new(body.x, top + 4.0, body.w, 2.0);
        frame.rect_clipped(track, p.switch_off, 1.0, track);
        if let Some(fraction) = fraction {
            frame.rect_clipped(
                Rect::new(track.x, track.y, track.w * fraction, track.h),
                p.accent,
                1.0,
                track,
            );
        }
        track.bottom() + SIDEBAR_LIST_GAP
    } else {
        top
    };
    let detail = Rect::new(body.x, detail_top, body.w, detail_line_height);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let mut quotas = Vec::new();
    for (minutes, period) in [
        (300, t!("agent-quota-five")),
        (10080, t!("agent-quota-week")),
    ] {
        let Some(limit) = panel
            .rate_limits
            .iter()
            .find(|limit| limit.minutes == minutes)
        else {
            continue;
        };
        let remaining = format!("{:.0}%", limit.remaining);
        let mut text = t!(
            "agent-quota-remaining",
            period = period,
            remaining = remaining.as_str()
        );
        if let Some(reset) = limit.resets_at
            && let Some((weekday, hour, minute)) = crate::system::local_reset_time(reset)
        {
            let reset_label = if minutes == 300 {
                format!("{hour:02}:{minute:02}")
            } else {
                let weekdays = [
                    t!("agent-mon"),
                    t!("agent-tue"),
                    t!("agent-wed"),
                    t!("agent-thu"),
                    t!("agent-fri"),
                    t!("agent-sat"),
                    t!("agent-sun"),
                ];
                let (today, now_hour, now_minute) =
                    crate::system::local_reset_time(now).unwrap_or((0, 0, 0));
                let week_end = u64::from(7 - today) * 86400
                    - u64::from(now_hour) * 3600
                    - u64::from(now_minute) * 60;
                let day = weekdays[weekday as usize];
                if reset.saturating_sub(now) >= week_end {
                    t!("agent-next-weekday", day = day)
                } else {
                    day.to_owned()
                }
            };
            text.push_str(&t!("agent-quota-reset", reset = reset_label.as_str()));
        }
        let color = if minutes == 300 && limit.remaining < 20.0 {
            p.error
        } else if minutes == 300 && limit.remaining < 50.0 {
            p.warning
        } else {
            p.text_muted
        };
        quotas.push((text, color));
    }
    if let Some(cost) = panel.cost_summary() {
        quotas.push((cost, p.text_muted));
    }
    if let Some(tier) = tier {
        quotas.push((tier.to_owned(), p.text_muted));
    }
    let label = summary.as_ref().map_or("", |(label, _)| label.as_str());
    let token_w = if label.is_empty() {
        0.0
    } else {
        frame.measure(FontKind::Ui, app.fonts.ui_small, label)
    };
    let quota_w: f32 = quotas
        .iter()
        .map(|(text, _)| frame.measure(FontKind::Ui, app.fonts.ui_small, text) + SIDEBAR_GAP)
        .sum();
    let inline = label.is_empty() || token_w + quota_w + SIDEBAR_GAP <= detail.w;
    if !label.is_empty() {
        let token = ellipsize(frame, label, app.fonts.ui_small, detail.w);
        ui_text_small(frame, app, detail.x, detail, &token, p.text_muted, detail);
    }
    let mut bottom = detail.bottom();
    let mut x = if label.is_empty() {
        detail.x
    } else {
        detail.right() - quota_w + SIDEBAR_GAP
    };
    for (text, color) in quotas {
        if inline {
            let width = frame.measure(FontKind::Ui, app.fonts.ui_small, &text);
            let bounds = Rect::new(x, detail.y, width.min(detail.right() - x).max(0.0), detail_line_height);
            ui_text_small(frame, app, x, bounds, &text, color, bounds);
            x += width + SIDEBAR_GAP;
        } else {
            for line in wrap_text(&text, detail.w.max(1.0), |text| {
                frame.measure(FontKind::Ui, app.fonts.ui_small, text)
            }) {
                let bounds = Rect::new(detail.x, bottom, detail.w, detail_line_height);
                ui_text_small(frame, app, bounds.x, bounds, &line, color, bounds);
                bottom += detail_line_height;
            }
        }
    }
    bottom + SIDEBAR_GAP
}
fn draw_status(
    app: &App,
    frame: &mut Frame<'_>,
    panel: &mut Panel,
    area: Rect,
    project_matches: bool,
) -> (f32, Option<Rect>) {
    let max_rows = ((area.h / BUTTON_SMALL) as usize).min(3);
    if max_rows == 0 {
        return (area.bottom(), None);
    }
    let p = app.palette;
    let warning = panel.error.is_some()
        || panel.connection != Connection::Ready
        || !project_matches
        || panel.approval().is_some()
        || panel.elicitation().is_some();
    let status_color = if warning { p.warning } else { p.text_muted };
    let status = if !project_matches && panel.root.is_some() {
        t!("agent-project-changed")
    } else {
        panel.status()
    };
    let elapsed = panel.turn_elapsed().map(|elapsed| {
        let seconds = elapsed.as_secs();
        let duration = format!("{}:{:02}", seconds / 60, seconds % 60);
        let label = t!("agent-elapsed", duration = duration.as_str());
        (duration, label)
    });
    let stop_label = t!("agent-stop");
    let stop_w = if panel.can_stop() {
        button_width(app, frame, stop_label).min(area.w)
    } else {
        0.0
    };
    let elapsed_w = elapsed.as_ref().map_or(0.0, |(_, label)| {
        (frame
            .measure(FontKind::Ui, app.fonts.ui_small, label)
            .ceil()
            + 17.0)
            .min((area.w - stop_w - 2.0 * SIDEBAR_GAP).max(0.0) * 0.6)
    });
    let elapsed_end = area.right()
        - if stop_w > 0.0 {
            stop_w + SIDEBAR_GAP
        } else {
            0.0
        };
    let status_x = area.x + BUTTON_SMALL - BUTTON_GAP;
    let worktree_end = elapsed_end
        - if elapsed_w > 0.0 {
            elapsed_w + SIDEBAR_GAP
        } else {
            0.0
        };
    let worktree_label = panel
        .has_git_worktree_control()
        .then(|| panel.worktree_label());
    let worktree_chrome = 34.0;
    let worktree_w = worktree_label.as_deref().map_or(0.0, |label| {
        let desired = frame
            .measure(FontKind::Ui, app.fonts.ui_small, label)
            .ceil()
            + worktree_chrome;
        let budget = (worktree_end - status_x - SIDEBAR_GAP - 40.0).clamp(0.0, 160.0);
        if budget >= 38.0 {
            desired.min(budget)
        } else {
            0.0
        }
    });
    let status_end = worktree_end
        - if worktree_w > 0.0 {
            worktree_w + SIDEBAR_GAP
        } else {
            0.0
        };
    let status_w = (status_end - SIDEBAR_GAP - status_x).max(0.0);
    let rows = wrap_text(status, status_w.max(4.0), |text| {
        frame.measure(FontKind::Ui, app.fonts.ui_small, text)
    });
    let top = area.bottom() - BUTTON_SMALL * rows.len().clamp(1, max_rows) as f32;
    let glyph = (area.x + 8.0, top + BUTTON_SMALL / 2.0);
    if warning {
        icons::warning(frame, glyph, p.warning, area);
    } else if panel.busy() {
        icons::spinner(
            frame,
            glyph,
            5.0,
            icons::spin_turn() * RUNNING_SPIN_SCALE,
            p.accent,
            area,
        );
    } else {
        frame.rect_clipped(
            Rect::new(glyph.0 - 2.5, glyph.1 - 2.5, 5.0, 5.0),
            p.text_dim,
            2.5,
            area,
        );
    }
    for (index, text) in rows.iter().take(max_rows).enumerate() {
        let line = Rect::new(
            status_x,
            top + BUTTON_SMALL * index as f32,
            status_w,
            BUTTON_SMALL,
        );
        let text = if index + 1 == max_rows && rows.len() > max_rows {
            truncate_end(frame, &format!("{text}…"), app.fonts.ui_small, line.w)
                .into_owned()
                .into()
        } else {
            truncate_end(frame, text, app.fonts.ui_small, line.w)
        };
        ui_text_small(
            frame,
            app,
            line.x,
            line,
            &text,
            status_color,
            line.intersect(area),
        );
    }
    let worktree_button = if let Some(label) = worktree_label
        && worktree_w > 0.0
    {
        let rect = Rect::new(worktree_end - worktree_w, top, worktree_w, BUTTON_SMALL);
        let (mx, my) = app.mouse();
        let hovered = project_matches && app.cursor_inside() && rect.contains(mx, my);
        let active = panel.selected_worktree.is_some();
        draw_button(frame, p, rect, area, hovered, panel.worktree_open);
        let cy = center(rect).1;
        let icon_pos = (rect.x + 10.0, cy);
        if panel.worktree_creating {
            icons::spinner(
                frame,
                icon_pos,
                4.5,
                icons::spin_turn() * RUNNING_SPIN_SCALE,
                p.accent,
                rect,
            );
        } else if active {
            icons::branch_small(frame, icon_pos, p.accent, rect);
        } else {
            let color = if hovered || panel.worktree_open {
                p.text
            } else {
                p.text_muted
            };
            icons::folder_small(frame, icon_pos, color, rect);
        }
        let text_area = Rect::new(
            rect.x + 19.0,
            rect.y,
            (rect.w - worktree_chrome).max(0.0),
            rect.h,
        );
        let text = ellipsize(frame, &label, app.fonts.ui_small, text_area.w);
        let text_color = if active || hovered || panel.worktree_open {
            p.text
        } else {
            p.text_muted
        };
        ui_text_small(frame, app, text_area.x, text_area, &text, text_color, text_area);
        icons::chevron(
            frame,
            (rect.right() - 8.0, cy),
            5.5,
            icons::Direction::Down,
            p.text_dim,
            rect,
        );
        if project_matches {
            panel.hits.push((rect, Hit::ToggleWorktrees));
        }
        Some(rect)
    } else {
        None
    };
    if let Some((duration, label)) = elapsed {
        let line = Rect::new(elapsed_end - elapsed_w, top, elapsed_w, BUTTON_SMALL);
        let label_w = frame.measure(FontKind::Ui, app.fonts.ui_small, &label);
        let (label, width) = if label_w + 17.0 <= line.w {
            (label, label_w)
        } else {
            let width = frame.measure(FontKind::Ui, app.fonts.ui_small, &duration);
            (duration, width)
        };
        let text_x = line.right() - width;
        if text_x - line.x >= 17.0 {
            icons::clock(frame, (text_x - 10.0, center(line).1), p.text_muted, line);
        }
        ui_text_small(
            frame,
            app,
            text_x.max(line.x),
            line,
            &label,
            p.text_muted,
            line,
        );
    }
    if stop_w > 0.0 {
        let stop = Rect::new(area.right() - stop_w, top + 2.0, stop_w, BUTTON_SMALL - 4.0);
        surface(frame, stop, p.group_bg, p.border, RADIUS_ROW, area);
        Actions {
            app,
            frame,
            hits: &mut panel.hits,
        }
        .button(stop, stop_label, Hit::Stop, false, true);
    }
    (top, worktree_button)
}

fn draw_thinking(app: &App, frame: &mut Frame<'_>, row: Rect) {
    const PERIOD: f32 = 1.8;
    const STRIPS: usize = 16;

    let x = row.x + CARD_PAD;
    let label = t!("agent-thinking");
    let end = ui_text_small(frame, app, x, row, label, app.palette.text_muted, row);
    let text_clip = row.intersect(Rect::new(x, row.y, end - x, row.h));

    // Clip the highlight to the glyphs by painting the same text in narrow strips.
    let width = ((end - x) * 0.65).max(1.0);
    let seconds = icons::spin_turn() / crate::theme::SPIN_SPEED;
    let phase = (seconds / PERIOD).fract();
    let left = x - width + (end - x + width) * phase;
    let strip_width = width / STRIPS as f32;
    for index in 0..STRIPS {
        let offset = index as f32 + 0.5;
        let strength = (std::f32::consts::PI * offset / STRIPS as f32).sin();
        let strip = Rect::new(left + index as f32 * strip_width, row.y, strip_width, row.h);
        let clip = text_clip.intersect(strip);
        if clip.w > 0.0 {
            ui_text_small(
                frame,
                app,
                x,
                row,
                label,
                app.palette.text.alpha(strength),
                clip,
            );
        }
    }
}

fn draw_empty_chat(app: &App, frame: &mut Frame<'_>, area: Rect, backend: Backend) {
    if area.h < 110.0 {
        return;
    }
    let p = app.palette;
    let top = area.y + (area.h * 0.25).min(130.0);
    let center = area.x + area.w / 2.0;
    if backend == Backend::Codex {
        icons::codex(frame, (center, top + BUTTON / 2.0), p.text_muted, area);
    }
    let label = if backend == Backend::Codex {
        std::borrow::Cow::Borrowed(t!("agent-empty-chat"))
    } else {
        std::borrow::Cow::Owned(t!("agent-empty-backend", name = backend.name()))
    };
    let width = frame.measure(FontKind::Ui, app.fonts.ui, &label);
    let row = Rect::new(area.x, top + BUTTON + SIDEBAR_LIST_GAP, area.w, TREE_ROW);
    ui_text(
        frame,
        app,
        (center - width / 2.0).max(area.x),
        row,
        &label,
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
        Tab::Backends => [
            Backend::Codex,
            Backend::Claude,
            Backend::Gemini,
            Backend::Antigravity,
        ]
        .into_iter()
        .map(|backend| {
            (
                backend.name().into(),
                Hit::ChooseBackend(backend),
                panel.backend == backend,
            )
        })
        .collect(),
        _ => return,
    };
    if panel.tab == Tab::Models {
        rows.extend(
            panel
                .model()
                .into_iter()
                .flat_map(|model| &model.efforts)
                .map(|effort| {
                    (
                        format!("{} · {}", t!("agent-effort"), effort),
                        Hit::ChooseEffort(effort.clone()),
                        panel.effort() == Some(effort),
                    )
                }),
        );
    }
    panel.viewport = area;
    if panel.more() {
        rows.push((t!("agent-more").into(), Hit::More, false));
    }
    if rows.is_empty() {
        let label = match panel.tab {
            Tab::History if panel.backend != Backend::Codex => std::borrow::Cow::Owned(t!(
                "agent-backend-history-empty",
                name = panel.backend.name()
            )),
            Tab::History => std::borrow::Cow::Borrowed(t!("agent-history-empty")),
            _ => std::borrow::Cow::Borrowed(t!("agent-models-empty")),
        };
        let row = Rect::new(area.x, area.y, area.w, ROW);
        ui_text(frame, app, row.x, row, &label, app.palette.text_muted, area);
        panel.max_scroll = 0.0;
        return;
    }
    let stride = TREE_ROW + BUTTON_GAP;
    panel.max_scroll = (rows.len() as f32 * stride - area.h).max(0.0);
    panel.scroll = panel.scroll.min(panel.max_scroll);
    let first = (panel.scroll / stride) as usize;
    let switching = panel.tab == Tab::Backends && panel.any_active();
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
        let row = Rect::new(area.x, y, area.w, TREE_ROW).intersect(area);
        actions.button(row, &label, hit, selected, !switching || selected);
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
    let head = Rect::new(area.x, area.y, area.w, TAB_HEIGHT);
    let decisions = approval.decisions.clone();
    let decision_row = Rect::new(area.x, head.bottom(), area.w, BUTTON_SMALL);
    let copy = Rect::new(
        area.x,
        decision_row.bottom() + BUTTON_GAP,
        65.0,
        BUTTON_SMALL,
    );
    let top = copy.bottom() + SIDEBAR_LIST_GAP;
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
        let width =
            ((decision_row.w + BUTTON_GAP) / decisions.len().max(1) as f32 - BUTTON_GAP).min(122.0);
        for (index, decision) in decisions.into_iter().enumerate() {
            let (label, grant) = match decision {
                ApprovalDecision::Accept => (t!("agent-approve"), true),
                ApprovalDecision::AcceptForSession => (t!("agent-approve-session"), true),
                ApprovalDecision::Decline => (t!("agent-deny"), false),
                ApprovalDecision::Cancel => (t!("agent-cancel-turn"), false),
            };
            let button = Rect::new(
                decision_row.x + index as f32 * (width + BUTTON_GAP),
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

fn draw_elicitation(app: &mut App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let Some(can_submit) = panel.elicitation().map(|draft| draft.can_submit()) else {
        return;
    };
    let p = app.palette;
    let head = Rect::new(area.x, area.y, area.w, TAB_HEIGHT);
    let action_row = Rect::new(area.x, head.bottom(), area.w, BUTTON_SMALL);
    let top = action_row.bottom() + SIDEBAR_GAP;
    let viewport = Rect::new(area.x, top, area.w, (area.bottom() - top).max(0.0));
    ui_text(
        frame,
        app,
        head.x,
        head,
        t!("agent-question-title"),
        p.accent,
        head,
    );
    {
        let mut actions = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        let specs = [
            (t!("agent-question-submit"), Hit::ElicitSubmit, true, can_submit),
            (t!("agent-question-decline"), Hit::ElicitDecline, false, true),
            (t!("agent-question-cancel"), Hit::ElicitCancel, false, true),
        ];
        let width = ((action_row.w + BUTTON_GAP) / specs.len() as f32 - BUTTON_GAP).min(110.0);
        for (index, (label, hit, emphasized, enabled)) in specs.into_iter().enumerate() {
            let button = Rect::new(
                action_row.x + index as f32 * (width + BUTTON_GAP),
                action_row.y,
                width,
                action_row.h,
            );
            actions.button(button, label, hit, emphasized, enabled);
        }
    }
    panel.viewport = viewport;
    if viewport.h <= 0.0 || viewport.w <= 0.0 {
        panel.max_scroll = 0.0;
        return;
    }

    let width = (viewport.w - 8.0).max(4.0);
    let mut hits = std::mem::take(&mut panel.hits);
    let scroll = panel.scroll;
    let (mx, my) = app.mouse();
    let app_focused = app.focus == Focus::Agent;
    let mut ime_cursor = None;

    let mut y = viewport.y - scroll;
    let start_y = y;

    if let Some(draft) = panel.elicitation_mut() {
        let message = draft.request.message.trim();
        let show_message = !message.is_empty()
            && !(draft.request.fields.len() == 1 && draft.request.fields[0].title.trim() == message);
        if show_message {
            for line in wrap_text(message, width, |text| {
                frame.measure(FontKind::Ui, app.fonts.ui_small, text)
            }) {
                let row = Rect::new(viewport.x, y, width, ROW);
                if row.bottom() > viewport.y && row.y < viewport.bottom() {
                    ui_text_small(
                        frame,
                        app,
                        row.x,
                        row,
                        line,
                        p.text_muted,
                        row.intersect(viewport),
                    );
                }
                y += ROW;
            }
            y += SIDEBAR_LIST_GAP;
        }

        let field_count = draft.request.fields.len();
        for field_index in 0..field_count {
            let title = if draft.request.fields[field_index].required {
                format!("{} *", draft.request.fields[field_index].title)
            } else {
                draft.request.fields[field_index].title.clone()
            };
            let description = draft.request.fields[field_index].description.clone();
            let is_text = matches!(
                draft.request.fields[field_index].kind,
                ElicitationFieldKind::Text | ElicitationFieldKind::Number { .. }
            );
            for line in wrap_text(&title, width, |text| {
                frame.measure(FontKind::Ui, app.fonts.ui, text)
            }) {
                let row = Rect::new(viewport.x, y, width, ROW);
                if row.bottom() > viewport.y && row.y < viewport.bottom() {
                    ui_text(frame, app, row.x, row, line, p.text, row.intersect(viewport));
                }
                y += ROW;
            }
            if !description.is_empty() && !is_text {
                for line in wrap_text(&description, width, |text| {
                    frame.measure(FontKind::Ui, app.fonts.ui_small, text)
                }) {
                    let row = Rect::new(viewport.x, y, width, ROW - 2.0);
                    if row.bottom() > viewport.y && row.y < viewport.bottom() {
                        ui_text_small(
                            frame,
                            app,
                            row.x,
                            row,
                            line,
                            p.text_muted,
                            row.intersect(viewport),
                        );
                    }
                    y += ROW - 2.0;
                }
            }
            y += SIDEBAR_LIST_GAP;

            match draft.request.fields[field_index].kind.clone() {
                ElicitationFieldKind::SingleSelect(options) => {
                    for option in &options {
                        let selected = draft.single[field_index].as_deref()
                            == Some(option.value.as_str());
                        let text_w = (width - 34.0).max(4.0);
                        let label_lines: Vec<String> = wrap_text(&option.label, text_w, |t| {
                            frame.measure(FontKind::Ui, app.fonts.ui, t)
                        })
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                        let desc_lines: Vec<String> = if option.description.is_empty() {
                            Vec::new()
                        } else {
                            wrap_text(&option.description, text_w, |t| {
                                frame.measure(FontKind::Ui, app.fonts.ui_small, t)
                            })
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                        };
                        let h = (label_lines.len().max(1) as f32 * ROW
                            + desc_lines.len() as f32 * (ROW - 2.0)
                            + 10.0)
                            .max(TREE_ROW + 4.0);
                        let card = Rect::new(viewport.x, y, width, h);
                        let hit = card.intersect(viewport);
                        if hit.h > 0.0 && hit.w > 0.0 {
                            let hovered = app.cursor_inside() && hit.contains(mx, my);
                            let fill = if selected {
                                p.selected
                            } else if hovered {
                                p.hover
                            } else {
                                p.group_bg
                            };
                            let edge = if selected { p.accent } else { p.border };
                            surface(frame, card, fill, edge, RADIUS_ROW + 2.0, viewport);
                            let dot_y = card.y + 5.0 + ROW * 0.5;
                            let outer = Rect::new(card.x + 10.0, dot_y - 5.0, 10.0, 10.0);
                            frame.rect_clipped(
                                outer,
                                if selected { p.accent } else { p.border },
                                5.0,
                                viewport,
                            );
                            frame.rect_clipped(
                                outer.inset(1.5, 1.5),
                                if selected { p.accent } else { fill },
                                3.5,
                                viewport,
                            );
                            if selected {
                                frame.rect_clipped(outer.inset(3.0, 3.0), p.editor_bg, 2.0, viewport);
                            }
                            let mut ty = card.y + 5.0;
                            for line in &label_lines {
                                let r = Rect::new(card.x + 26.0, ty, text_w, ROW);
                                ui_text(frame, app, r.x, r, line, p.text, hit);
                                ty += ROW;
                            }
                            for line in &desc_lines {
                                let r = Rect::new(card.x + 26.0, ty, text_w, ROW - 2.0);
                                ui_text_small(frame, app, r.x, r, line, p.text_muted, hit);
                                ty += ROW - 2.0;
                            }
                            hits.push((hit, Hit::ElicitSelect(field_index, option.value.clone())));
                        }
                        y += h + SIDEBAR_LIST_GAP;
                    }
                }
                ElicitationFieldKind::MultiSelect(options) => {
                    for option in &options {
                        let selected = draft.multi[field_index]
                            .iter()
                            .any(|v| v == &option.value);
                        let text_w = (width - 34.0).max(4.0);
                        let label_lines: Vec<String> = wrap_text(&option.label, text_w, |t| {
                            frame.measure(FontKind::Ui, app.fonts.ui, t)
                        })
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                        let desc_lines: Vec<String> = if option.description.is_empty() {
                            Vec::new()
                        } else {
                            wrap_text(&option.description, text_w, |t| {
                                frame.measure(FontKind::Ui, app.fonts.ui_small, t)
                            })
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                        };
                        let h = (label_lines.len().max(1) as f32 * ROW
                            + desc_lines.len() as f32 * (ROW - 2.0)
                            + 10.0)
                            .max(TREE_ROW + 4.0);
                        let card = Rect::new(viewport.x, y, width, h);
                        let hit = card.intersect(viewport);
                        if hit.h > 0.0 && hit.w > 0.0 {
                            let hovered = app.cursor_inside() && hit.contains(mx, my);
                            let fill = if selected {
                                p.selected
                            } else if hovered {
                                p.hover
                            } else {
                                p.group_bg
                            };
                            let edge = if selected { p.accent } else { p.border };
                            surface(frame, card, fill, edge, RADIUS_ROW + 2.0, viewport);
                            let box_y = card.y + 5.0 + ROW * 0.5 - 5.5;
                            let check = Rect::new(card.x + 10.0, box_y, 11.0, 11.0);
                            surface(
                                frame,
                                check,
                                if selected { p.accent } else { fill },
                                if selected { p.accent } else { p.border },
                                2.5,
                                viewport,
                            );
                            if selected {
                                ui_text_small(frame, app, check.x + 1.5, check, "✓", p.editor_bg, hit);
                            }
                            let mut ty = card.y + 5.0;
                            for line in &label_lines {
                                let r = Rect::new(card.x + 26.0, ty, text_w, ROW);
                                ui_text(frame, app, r.x, r, line, p.text, hit);
                                ty += ROW;
                            }
                            for line in &desc_lines {
                                let r = Rect::new(card.x + 26.0, ty, text_w, ROW - 2.0);
                                ui_text_small(frame, app, r.x, r, line, p.text_muted, hit);
                                ty += ROW - 2.0;
                            }
                            hits.push((hit, Hit::ElicitToggle(field_index, option.value.clone())));
                        }
                        y += h + SIDEBAR_LIST_GAP;
                    }
                }
                ElicitationFieldKind::Boolean => {
                    let selected = draft.bools[field_index];
                    let card = Rect::new(viewport.x, y, width, TREE_ROW + 6.0);
                    let hit = card.intersect(viewport);
                    if hit.h > 0.0 && hit.w > 0.0 {
                        let hovered = app.cursor_inside() && hit.contains(mx, my);
                        let fill = if selected {
                            p.selected
                        } else if hovered {
                            p.hover
                        } else {
                            p.group_bg
                        };
                        let edge = if selected { p.accent } else { p.border };
                        surface(frame, card, fill, edge, RADIUS_ROW + 2.0, viewport);
                        let label = if selected {
                            format!("✓  {}", t!("agent-question-yes"))
                        } else {
                            t!("agent-question-no").to_owned()
                        };
                        ui_text(frame, app, card.x + 10.0, card, &label, p.text, hit);
                        hits.push((hit, Hit::ElicitBool(field_index)));
                    }
                    y += card.h + SIDEBAR_LIST_GAP;
                }
                ElicitationFieldKind::Text | ElicitationFieldKind::Number { .. } => {
                    let box_rect = Rect::new(viewport.x, y, width, TREE_ROW + 8.0);
                    let hit = box_rect.intersect(viewport);
                    let field_focused = app_focused && draft.focused_field == Some(field_index);
                    if hit.h > 0.0 && hit.w > 0.0 {
                        surface(
                            frame,
                            box_rect,
                            p.group_bg,
                            if field_focused {
                                p.accent.alpha(0.75)
                            } else {
                                p.border
                            },
                            RADIUS_ROW + 2.0,
                            viewport,
                        );
                        let inner = box_rect.inset(8.0, 2.0).intersect(viewport);
                        let placeholder = if description.is_empty() {
                            t!("agent-question-placeholder")
                        } else {
                            description.as_str()
                        };
                        if let Some(cursor) = draw_bar_field(
                            app,
                            frame,
                            inner,
                            &mut draft.texts[field_index],
                            placeholder,
                            field_focused,
                        ) {
                            ime_cursor = Some(cursor);
                        }
                        hits.push((hit, Hit::ElicitFocusField(field_index)));
                    } else {
                        draft.texts[field_index].view.area = Rect::default();
                    }
                    y += box_rect.h + SIDEBAR_LIST_GAP;
                }
            }
            y += SIDEBAR_GAP;
        }
    }

    let content_h = (y - start_y).max(0.0);
    panel.max_scroll = (content_h - viewport.h).max(0.0);
    panel.scroll = panel.scroll.min(panel.max_scroll);
    if content_h > viewport.h {
        let thumb = (viewport.h * viewport.h / content_h)
            .max(18.0)
            .min(viewport.h);
        let fraction = if panel.max_scroll > 0.0 {
            (panel.scroll / panel.max_scroll).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_y = viewport.y + (viewport.h - thumb) * fraction;
        frame.rect_clipped(
            Rect::new(viewport.right() - 4.0, thumb_y, 3.0, thumb),
            p.border,
            2.0,
            viewport,
        );
    }
    panel.hits = hits;
    if let Some(cursor) = ime_cursor {
        app.ime.cursor = Some(cursor);
    }
}

fn draw_chat(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, area: Rect) {
    let width = (area.w - 9.0).max(4.0);
    panel.viewport = area;
    let older = panel.older();
    let backend = panel.backend;
    let mut hits = std::mem::take(&mut panel.hits);
    let mut text = SelectionPaint {
        hits: std::mem::take(&mut panel.text_hits),
        selection: panel.selection,
        card: 0,
    };
    if let Some(chat) = panel.transcript().filter(|chat| !chat.cards.is_empty()) {
        chat.layout_processes(
            app.config.agent_expand_process.unwrap_or(true),
            app.animations_on(),
        );
        let (first, offset) = if let Some((ordinal, offset)) = chat.anchor {
            chat.reset_follow_state();
            chat.follow_layout = Some((width, area.h));
            let mut first = ordinal.saturating_sub(chat.first).min(chat.cards.len() - 1);
            while first > 0 && chat.cards[first].display_height() <= 0.0 {
                first -= 1;
            }
            let card = &mut chat.cards[first];
            if !card.process_hidden {
                layout_card(frame, app, card, width);
            }
            let offset = if ordinal != chat.first + first {
                0.0
            } else {
                offset.min((card.display_height() - 1.0).max(0.0))
            };
            chat.anchor = Some((chat.first + first, offset));
            (first, offset)
        } else {
            let mut remaining = area.h;
            let mut target_first = 0;
            for (index, card) in chat.cards.iter_mut().enumerate().rev() {
                if !card.process_hidden {
                    layout_card(frame, app, card, width);
                }
                remaining -= card.display_height();
                target_first = index;
                if remaining <= 0.0 {
                    break;
                }
            }
            let target_offset = (-remaining).max(0.0);
            let same_layout = chat.follow_layout == Some((width, area.h));
            chat.follow_layout = Some((width, area.h));
            let (first, offset) = if app.animations_on()
                && same_layout
                && (chat.follow_smooth || chat.follow_animating)
                && let Some((prev_ordinal, prev_offset)) = chat.follow_pos
                && prev_ordinal >= chat.first
                && prev_ordinal - chat.first < chat.cards.len()
            {
                let prev_first = prev_ordinal - chat.first;
                if prev_first > target_first
                    || target_first - prev_first > FOLLOW_MAX_CARDS_GAP
                {
                    chat.follow_animating = false;
                    chat.follow_smooth = false;
                    chat.follow_last_tick = None;
                    (target_first, target_offset)
                } else {
                    for index in prev_first..target_first {
                        let card = &mut chat.cards[index];
                        if !card.process_hidden {
                            layout_card(frame, app, card, width);
                        }
                    }
                    let prev_offset = prev_offset.clamp(
                        0.0,
                        (chat.cards[prev_first].display_height() - 1.0).max(0.0),
                    );
                    let lag = follow_distance(
                        &chat.cards,
                        (prev_first, prev_offset),
                        (target_first, target_offset),
                    );
                    if lag <= 0.5 {
                        chat.follow_animating = false;
                        chat.follow_smooth = false;
                        chat.follow_last_tick = None;
                        (target_first, target_offset)
                    } else {
                        let now = std::time::Instant::now();
                        let dt = chat.follow_last_tick.map_or(1.0 / 60.0, |prev| {
                            now.saturating_duration_since(prev)
                                .as_secs_f32()
                                .clamp(0.0, 0.05)
                        });
                        let speed = (FOLLOW_MIN_SPEED + lag * FOLLOW_CATCHUP_GAIN)
                            .clamp(FOLLOW_MIN_SPEED, FOLLOW_MAX_SPEED);
                        let step = speed * dt;
                        if lag <= step {
                            chat.follow_animating = false;
                            chat.follow_smooth = false;
                            chat.follow_last_tick = None;
                            (target_first, target_offset)
                        } else {
                            let next_pos =
                                follow_advance(&chat.cards, (prev_first, prev_offset), step);
                            chat.follow_animating = true;
                            chat.follow_last_tick = Some(now);
                            next_pos
                        }
                    }
                }
            } else {
                chat.follow_animating = false;
                chat.follow_smooth = false;
                chat.follow_last_tick = None;
                (target_first, target_offset)
            };
            chat.follow_pos = Some((chat.first + first, offset));
            (first, offset)
        };
        let mut y = area.y - offset;
        let mut next = first;
        let mut visible = Vec::new();
        for (index, card) in chat.cards.iter_mut().enumerate().skip(first) {
            if y >= area.bottom() && card.display_height() > 0.0 {
                break;
            }
            next = index + 1;
            let ordinal = chat.first + index;
            if let Some(summary) = card.process_summary {
                let row = Rect::new(area.x, y, width, crate::app::agent::PROCESS_ROW);
                let hit = row.intersect(area);
                if hit.w > 0.0 && hit.h > 0.0 {
                    let p = app.palette;
                    let (x, mouse_y) = app.mouse();
                    draw_button(frame, p, row, area, hit.contains(x, mouse_y), false);
                    icons::chevron_turned(
                        frame,
                        (row.x + 10.0, center(row).1),
                        7.0,
                        card.process_share * std::f32::consts::FRAC_PI_2,
                        p.text_muted,
                        hit,
                    );
                    let mut label = if summary.tools == 0 {
                        t!("agent-process").to_owned()
                    } else {
                        t!("agent-process-tools", count = summary.tools)
                    };
                    if let Some(elapsed) = card.turn_elapsed {
                        let seconds = elapsed.as_secs();
                        let duration = format!("{}:{:02}", seconds / 60, seconds % 60);
                        label = t!(
                            "agent-process-duration",
                            summary = label.as_str(),
                            duration = duration.as_str()
                        );
                    }
                    let label = ellipsize(frame, &label, app.fonts.ui_small, row.w - 30.0);
                    ui_text_small(frame, app, row.x + 22.0, row, &label, p.text_muted, hit);
                    hits.push((hit, Hit::ToggleProcess(ordinal)));
                }
                y += row.h;
            }
            if card.process_hidden {
                card.layout = None;
                continue;
            }
            layout_card(frame, app, card, width);
            let height = card.height * card.process_share;
            let area = if card.process_share < 1.0 {
                area.intersect(Rect::new(area.x, y, width, height))
            } else {
                area
            };
            frame.with_opacity(card.process_share, |frame| {
                let layout = card.layout.as_ref().expect("card laid out");
                text.card = ordinal;
                let compact = compact_tool_card(card);
                let block = Rect::new(
                    area.x + width - layout.width,
                    y,
                    layout.width,
                    card.height - if compact { BUTTON_GAP } else { CARD_GAP },
                );
                let p = app.palette;
                let header_height = card_header_height(card.item.kind);
                match card.item.kind {
                    ItemKind::User => {
                        let footer = if card.message_time.is_some() {
                            MESSAGE_FOOTER + 2.0 * SIDEBAR_LIST_GAP
                        } else {
                            0.0
                        };
                        let bubble = Rect::new(
                            block.x,
                            y + header_height,
                            block.w,
                            (block.h - header_height - footer).max(0.0),
                        );
                        surface(
                            frame,
                            bubble,
                            p.group_bg,
                            p.border.alpha(0.4),
                            RADIUS_TAB,
                            area,
                        );
                    }
                    ItemKind::Message | ItemKind::Commentary => {}
                    _ => surface(frame, block, p.group_bg, p.border, RADIUS_TAB, area),
                }
                let name = match card.item.kind {
                    ItemKind::User => t!("agent-user"),
                    ItemKind::Message | ItemKind::Commentary => backend.name(),
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
                    if let Some((hour, minute)) = card.message_time {
                        let (x, mouse_y) = app.mouse();
                        let copy_w = 18.0;
                        let copy = Rect::new(
                            block.right() - CARD_PAD - copy_w + 2.0,
                            block.bottom() - MESSAGE_FOOTER - SIDEBAR_LIST_GAP,
                            copy_w,
                            MESSAGE_FOOTER,
                        );
                        let hit = copy.intersect(area);
                        draw_button(
                            actions.frame,
                            p,
                            copy,
                            area,
                            app.cursor_inside() && hit.contains(x, mouse_y),
                            false,
                        );
                        markdown::draw_copy_icon(actions.frame, center(copy), p.text_muted, hit);
                        if hit.w > 0.0 && hit.h > 0.0 {
                            actions.hits.push((hit, Hit::CopyMessage(ordinal)));
                        }
                        let label = format!("{hour:02}:{minute:02}");
                        let width = actions
                            .frame
                            .measure(FontKind::Ui, app.fonts.ui_small, &label);
                        let time = Rect::new(copy.x - BUTTON_GAP - width, copy.y, width, copy.h);
                        ui_text_small(
                            actions.frame,
                            app,
                            time.x,
                            time,
                            &label,
                            p.text_muted,
                            time.intersect(area),
                        );
                    }
                } else if card.item.kind != ItemKind::Commentary {
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
                        let preview = winc_agent::prefix(&card.item.text, 180)
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!("{name} · {status}  {preview}")
                    } else {
                        format!("{name} · {status}")
                    };
                    let header = Rect::new(block.x + 1.0, y, (block.w - 2.0).max(0.0), TREE_ROW);
                    draw_tool_heading(&mut actions, card, ordinal, header, area, &label);
                }
                let row_top = y + header_height + content_top(card.item.kind);
                let clip = Rect::new(
                    block.x,
                    row_top,
                    block.w,
                    (block.bottom() - row_top).max(0.0),
                )
                .intersect(area);
                match &layout.body {
                    Body::Lines(lines) => {
                        let x = block.x + card_pad_x(card.item.kind);
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
                        let left = block.x + CARD_PAD;
                        let mut shown =
                            blocks.partition_point(|block| row_top + block.bottom() < clip.y);
                        while let Some(block) = blocks.get(shown) {
                            let top = row_top + block.top;
                            if top >= clip.bottom() {
                                break;
                            }
                            markdown::draw(
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
            });
            y += height;
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
        let at_bottom = next == chat.cards.len() && y <= area.bottom() + 1.0;
        if at_bottom && chat.anchor.is_some() {
            chat.follow();
            chat.follow_pos = Some((chat.first + first, offset));
        }
        let before = chat
            .cards
            .iter()
            .take(first)
            .map(Card::display_height)
            .sum::<f32>();
        let position = before + offset;
        let total = position + y - area.y
            + chat
                .cards
                .iter()
                .skip(next)
                .map(Card::display_height)
                .sum::<f32>();
        if total > area.h {
            let thumb = (area.h * area.h / total).max(18.0).min(area.h);
            let fraction = if at_bottom {
                1.0
            } else {
                (position / (total - area.h)).clamp(0.0, 1.0)
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
        if !at_bottom && chat.anchor.is_some() {
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

fn draw_tool_heading(
    actions: &mut Actions<'_, '_, '_>,
    card: &Card,
    ordinal: usize,
    row: Rect,
    clip: Rect,
    label: &str,
) {
    let app = actions.app;
    let frame = &mut actions.frame;
    let p = app.palette;
    let hit = row.intersect(clip);
    if hit.w <= 0.0 || hit.h <= 0.0 {
        return;
    }
    let (x, y) = app.mouse();
    draw_button(frame, p, row, clip, hit.contains(x, y), false);
    let right = row.right() - 10.0;
    icons::chevron(
        frame,
        (right, center(row).1),
        7.0,
        if card.expanded {
            icons::Direction::Down
        } else {
            icons::Direction::Right
        },
        p.text_muted,
        hit,
    );
    let mut end = right - 10.0;
    if card.item.kind == ItemKind::FileChange
        && !card.item.files.is_empty()
        && card.item.files.len() == card.item.file_stats.len()
    {
        let mut stats = card.item.file_stats.iter();
        let totals = stats.try_fold((0usize, 0usize), |(added, removed), stat| {
            let stat = (*stat)?;
            Some((
                added.saturating_add(stat.added),
                removed.saturating_add(stat.removed),
            ))
        });
        if let Some((added, removed)) = totals {
            for (count, sign, color) in [(added, '+', p.git_added), (removed, '−', p.git_deleted)]
            {
                if count == 0 {
                    continue;
                }
                let text = format!("{sign}{count}");
                let w = frame
                    .measure(FontKind::Ui, app.fonts.ui_small, &text)
                    .ceil()
                    + SIDEBAR_GAP;
                if end - w < row.x + row.w * 0.45 {
                    break;
                }
                let badge = Rect::new(end - w, row.y + 3.0, w, row.h - 6.0);
                frame.rect_clipped(badge, color.alpha(0.13), RADIUS_ROW, hit);
                ui_text_small(frame, app, badge.x + 4.0, badge, &text, color, hit);
                end = badge.x - SIDEBAR_LIST_GAP;
            }
        }
    }
    let glyph = (row.x + 11.0, center(row).1);
    match card.item.status.as_str() {
        "completed" => icons::done(frame, glyph, p.text_muted, hit),
        "failed" => icons::error(frame, glyph, p.error, hit),
        _ if !card.done => {
            let seconds = icons::spin_turn() / crate::theme::SPIN_SPEED;
            let phase = seconds * std::f32::consts::TAU / 2.4;
            let opacity = 0.6 + 0.35 * phase.cos();
            icons::play_solid(frame, glyph, p.text_muted.alpha(opacity), hit);
        }
        _ => frame.rect_clipped(
            Rect::new(glyph.0 - 2.0, glyph.1 - 2.0, 4.0, 4.0),
            p.text_dim,
            2.0,
            hit,
        ),
    }
    let text = Rect::new(row.x + 23.0, row.y, (end - row.x - 23.0).max(0.0), row.h);
    let label = truncate_end(frame, label, app.fonts.ui_small, text.w);
    ui_text_small(
        frame,
        app,
        text.x,
        text,
        &label,
        p.text_muted,
        text.intersect(hit),
    );
    actions.hits.push((hit, Hit::Expand(ordinal)));
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
        {
            block.scroll_table(table, pixels);
        }
    }

    pub(crate) fn selected_text(&self, selection: TextSelection) -> String {
        let (first, last) = selection.ordered();
        let mut runs: Box<dyn Iterator<Item = &str> + '_> = match &self.body {
            Body::Lines(lines) => Box::new(lines.iter().map(|line| line.text.as_str())),
            Body::Markdown(blocks) => Box::new(blocks.iter().map(markdown::RichBlock::text)),
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
    Markdown(Vec<markdown::RichBlock>),
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

fn compact_tool_card(card: &Card) -> bool {
    !card.expanded
        && matches!(
            card.item.kind,
            ItemKind::Command
                | ItemKind::Read
                | ItemKind::Search
                | ItemKind::Test
                | ItemKind::Subagent
                | ItemKind::FileChange
                | ItemKind::Tool
        )
}

fn card_header_height(kind: ItemKind) -> f32 {
    if matches!(
        kind,
        ItemKind::User | ItemKind::Message | ItemKind::Commentary
    ) {
        0.0
    } else {
        TREE_ROW
    }
}

fn card_pad_x(kind: ItemKind) -> f32 {
    if kind == ItemKind::User {
        USER_MESSAGE_PAD_X
    } else {
        CARD_PAD
    }
}

fn content_top(kind: ItemKind) -> f32 {
    if kind == ItemKind::User {
        USER_MESSAGE_PAD_Y
    } else {
        CONTENT_TOP
    }
}

fn layout_card(frame: &mut Frame<'_>, app: &App, card: &mut Card, width: f32) {
    let pad_x = card_pad_x(card.item.kind);
    let width = if card.item.kind == ItemKind::User {
        let limit = if width < 360.0 {
            width - 12.0
        } else {
            width * 0.85
        };
        let text_width = card
            .item
            .text
            .lines()
            .map(|line| frame.measure(FontKind::Ui, app.fonts.ui, line))
            .fold(0.0, f32::max);
        (text_width + 2.0 * pad_x)
            .ceil()
            .max(80.0)
            .min(limit.max(4.0))
    } else {
        width
    };
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
    let content_width = (width - 2.0 * pad_x).max(4.0);
    let compact = compact_tool_card(card);
    let body = if compact {
        Body::Lines(Vec::new())
    } else if matches!(card.item.kind, ItemKind::Message | ItemKind::Commentary) {
        let previous = card
            .layout
            .take()
            .filter(|_| same_geometry)
            .and_then(|layout| match layout.body {
                Body::Markdown(blocks) => Some(blocks),
                Body::Lines(_) => None,
            });
        Body::Markdown(markdown::layout(
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
    let footer = if card.message_time.is_some() {
        match card.item.kind {
            ItemKind::User => MESSAGE_FOOTER + 2.0 * SIDEBAR_LIST_GAP,
            ItemKind::Message => MESSAGE_FOOTER + SIDEBAR_LIST_GAP,
            _ => 0.0,
        }
    } else {
        0.0
    };
    card.height = card_header_height(card.item.kind)
        + footer
        + if compact {
            BUTTON_GAP
        } else if card.item.kind == ItemKind::User {
            2.0 * USER_MESSAGE_PAD_Y + CARD_GAP
        } else {
            CONTENT_TOP + SIDEBAR_LIST_GAP + CARD_GAP
        }
        + match &body {
            Body::Lines(lines) => lines.len() as f32 * ROW,
            Body::Markdown(blocks) => blocks.last().map_or(0.0, markdown::RichBlock::bottom),
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
    fn button(&mut self, area: Rect, label: &str, hit: Hit, emphasized: bool, enabled: bool) {
        if area.w <= 0.0 || area.h <= 0.0 {
            return;
        }
        let left = matches!(
            hit,
            Hit::Expand(_)
                | Hit::ToggleProcess(_)
                | Hit::Open(_, false)
                | Hit::ChooseSession(_)
                | Hit::ChooseModel(_)
                | Hit::ChooseEffort(_)
                | Hit::Complete(_)
        );
        let p = self.app.palette;
        let (x, y) = self.app.mouse();
        let hovered = enabled && area.contains(x, y);
        if emphasized && !left
            || matches!(
                hit,
                Hit::Decide(..) | Hit::ElicitSubmit | Hit::ElicitDecline | Hit::ElicitCancel
            )
        {
            let role = if emphasized {
                ButtonRole::Primary
            } else {
                ButtonRole::Normal
            };
            let label = ellipsize(
                self.frame,
                label,
                self.app.fonts.ui,
                (area.w - 2.0 * SIDEBAR_LIST_GAP).max(0.0),
            );
            draw_action_button(self.app, self.frame, area, &label, role, enabled, hovered);
        } else {
            draw_button(self.frame, p, area, area, hovered, emphasized);
            let text = area.inset(SIDEBAR_LIST_GAP, 0.0);
            let label = ellipsize(self.frame, label, self.app.fonts.ui_small, text.w);
            let at = if left {
                text.x
            } else {
                let width = self
                    .frame
                    .measure(FontKind::Ui, self.app.fonts.ui_small, &label);
                (center(area).0 - width / 2.0).round()
            };
            let color = if enabled { p.text } else { p.text_dim };
            ui_text_small(self.frame, self.app, at, area, &label, color, text);
        }
        if enabled {
            self.hits.push((area, hit));
        }
    }
}

pub(super) fn tooltip(app: &App) -> Option<(Rect, String)> {
    if !app.cursor_inside() {
        return None;
    }
    let panel = app.agent.as_ref()?;
    if !panel.shown {
        return None;
    }
    let (x, y) = app.mouse();
    panel
        .hits
        .iter()
        .chain(
            app.agent_peer
                .iter()
                .filter(|_| app.agent_split)
                .flat_map(|peer| &peer.hits),
        )
        .find_map(|(rect, hit)| {
            if !rect.contains(x, y) {
                return None;
            }
            let label = match hit {
                Hit::Split => t!("agent-split"),
                Hit::New => t!("agent-new"),
                Hit::Show(Tab::History) => t!("agent-history"),
                Hit::Retry => t!("agent-reconnect"),
                Hit::SwitchProject => t!("agent-switch-project"),
                Hit::ToggleWorktrees => t!("agent-worktree-tooltip"),
                _ => return None,
            };
            Some((*rect, label.to_owned()))
        })
}
