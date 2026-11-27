//! Paints the Codex panel with the existing renderer and multiline input field.

use winc_agent::{ApprovalDecision, ApprovalKind, ItemKind, PermissionMode};
use winc_typeset::{Colors, Cx, Env, Item as Mark, LaidBlock, Memo, Out};

use crate::app::Focus;
use crate::app::agent::{
    Card, Connection, Hit, Link, Panel, Tab, TextHit, TextPoint, TextSelection,
};
use crate::layout::wrap_text;

use super::*;

const ROW: f32 = 20.0;
const CARD_PAD: f32 = TAB_INSET;
const CARD_GAP: f32 = SIDEBAR_GAP;
const CONTENT_TOP: f32 = SIDEBAR_LIST_GAP;
const USER_MESSAGE_PAD_Y: f32 = SIDEBAR_GAP;
const MESSAGE_HEADER: f32 = 16.0;
const STATUS_TOP_GAP: f32 = SIDEBAR_GAP + SIDEBAR_LIST_GAP;
const RUNNING_SPIN_SCALE: f32 = 0.5;

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
    panel.viewport = Rect::default();
    if area.w <= 0.0 || area.h <= 0.0 {
        return;
    }
    let p = app.palette;
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
    let close = Rect::new(end - BUTTON, button_y, BUTTON, BUTTON).intersect(header);
    let project_matches = app.agent_matches_workspace(panel);
    let (label, action, enabled) = match panel.connection {
        _ if !project_matches && panel.root.is_some() => (
            t!("agent-switch-project"),
            Hit::SwitchProject,
            !panel.any_active(),
        ),
        Connection::Ready => (t!("agent-new"), Hit::New, true),
        Connection::Connecting => (t!("agent-reconnect"), Hit::Retry, false),
        _ => (t!("agent-reconnect"), Hit::Retry, !panel.any_active()),
    };
    let icon = Rect::new(header.x, button_y, BUTTON_SMALL, BUTTON).intersect(header);
    let title_x = icon.right() + TAB_GAP;
    let title_min = frame.measure(FontKind::Ui, app.fonts.ui, t!("agent-title"));
    let history_label = t!("agent-history");
    let history_w = button_width(app, frame, history_label);
    let action_w =
        button_width(app, frame, label).min((close.x - title_x - title_min - TAB_GAP).max(0.0));
    let actions_end = close.x - TAB_GAP;
    let inline_history =
        actions_end - history_w - TAB_GAP - action_w >= title_x + title_min + SIDEBAR_GAP;
    let history = Rect::new(actions_end - history_w, button_y, history_w, BUTTON);
    let action_end = if inline_history {
        history.x - TAB_GAP
    } else {
        actions_end
    };
    let action_rect =
        Rect::new(action_end - action_w, button_y, action_w, BUTTON).intersect(header);
    let title_end = action_rect.x - TAB_GAP;
    let title = Rect::new(title_x, header.y, (title_end - title_x).max(0.0), header.h);
    let title_text = panel.session_title().unwrap_or(t!("agent-title"));
    let title_text = truncate_end(frame, title_text, app.fonts.ui, title.w);
    frame.rect_clipped(icon.inset(0.0, 2.0), p.selected, RADIUS_ROW, header);
    icons::codex(frame, center(icon), p.accent, icon);
    ui_text(frame, app, title.x, title, &title_text, p.text, title);
    let history_enabled = panel.connection == Connection::Ready;
    {
        let history_selected = panel.tab == Tab::History;
        let mut buttons = Actions {
            app,
            frame,
            hits: &mut panel.hits,
        };
        buttons.button(action_rect, label, action, false, enabled);
        if inline_history {
            buttons.button(
                history,
                history_label,
                Hit::Show(Tab::History),
                history_selected,
                history_enabled,
            );
        }
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

    let toolbar = Rect::new(
        body.x,
        header.bottom() + BUTTON_GAP,
        body.w,
        BUTTON + 2.0 * BUTTON_GAP,
    );
    let history = (!inline_history).then_some((history_label, history_w, history_enabled));
    draw_session_controls(app, frame, panel, toolbar, history);
    let mut top = toolbar.bottom() + SIDEBAR_LIST_GAP;
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
    let focused = app.focus == Focus::Agent;
    let input_h = 60.0_f32.min((body.h * 0.22).max(20.0));
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
    let has_context = show_selection || has_attachment || panel.standalone;
    let context_h = if has_context {
        BUTTON_SMALL + SIDEBAR_LIST_GAP
    } else {
        0.0
    };
    let composer_h = input_h + BUTTON_SMALL + 3.0 * SIDEBAR_LIST_GAP + context_h;
    let composer = Rect::new(body.x, body.bottom() - composer_h, body.w, composer_h);
    let footer = Rect::new(
        composer.x + SIDEBAR_LIST_GAP,
        composer.bottom() - BUTTON_SMALL - SIDEBAR_LIST_GAP,
        (composer.w - 2.0 * SIDEBAR_LIST_GAP).max(0.0),
        BUTTON_SMALL,
    );
    let can_send = project_matches && panel.can_send();
    let can_stop = panel.can_stop();
    let send_label = if can_stop {
        t!("agent-steer")
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
    let permission_label = permission_label(panel.permissions);
    let permission_end = if can_review { changes.x } else { send.x };
    let full_access = panel.permissions == PermissionMode::FullAccess;
    let shield_gutter = 15.0;
    let permission_width =
        button_width(app, frame, permission_label) + if full_access { shield_gutter } else { 0.0 };
    let attach_label = t!("agent-attach-file");
    let permission_space = (permission_end - footer.x - BUTTON_GAP).max(0.0);
    let (permission_w, attach_w) = if show_attach {
        let available = (permission_space - BUTTON_GAP).max(0.0);
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
    let context = Rect::new(footer.x, footer.y - context_h, footer.w, footer.h);
    let context_end = context.right();
    let status_bottom = composer.y - SIDEBAR_GAP;
    // On short windows, prioritize turn controls and the composer over token details.
    if status_bottom - top >= 4.0 * ROW
        && let Some((tokens, fraction)) = panel.token_summary()
    {
        top = draw_tokens(
            app,
            frame,
            Rect::new(body.x, top, body.w, ROW),
            &tokens,
            fraction,
        );
    }
    let status_top = draw_status(
        app,
        frame,
        panel,
        Rect::new(body.x, top, body.w, (status_bottom - top).max(0.0)),
        project_matches,
    );
    let attachment_path = panel.attachment.as_ref().map(|attached| attached.path());
    let preview_size = input_h.min(44.0).min(composer.w * 0.3);
    let preview_gutter = panel
        .image
        .as_ref()
        .map_or(0.0, |_| preview_size + SIDEBAR_GAP);
    let field = Rect::new(
        composer.x + SIDEBAR_GAP,
        composer.y + SIDEBAR_LIST_GAP,
        (composer.w - 2.0 * SIDEBAR_GAP - preview_gutter).max(0.0),
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
                permission_label,
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
        } else {
            actions.button(
                permission,
                permission_label,
                Hit::TogglePermissions,
                panel.permissions_open,
                project_matches,
            );
        }
        if show_attach {
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
        } else if show_selection {
            let selection = Rect::new(
                context.x,
                context.y,
                (context.w - 4.0).clamp(0.0, 72.0),
                context.h,
            );
            if selection.w >= 40.0 {
                actions.button(
                    selection,
                    t!("agent-attach-selection"),
                    Hit::Attach(true),
                    false,
                    true,
                );
            }
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
    let chat_end = status_top - STATUS_TOP_GAP;
    let area = Rect::new(body.x, top, body.w, (chat_end - top).max(0.0));
    frame.rect_clipped(
        Rect::new(body.x, top - SIDEBAR_LIST_GAP, body.w, 1.0),
        p.border.alpha(0.55),
        0.0,
        body,
    );
    if area.h > 0.0 {
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
    }
    if panel.permissions_open {
        draw_permissions(app, frame, panel, body, permission);
    } else if focused && app.ime.field_preedit.is_empty() {
        let choices: Vec<_> = panel.completion.labels().collect();
        let count = choices.len().min((area.h / TREE_ROW) as usize);
        let mut overlay = Vec::new();
        for (index, (label, selected)) in choices.into_iter().take(count).enumerate() {
            let row = Rect::new(
                area.x,
                area.bottom() - (count - index) as f32 * TREE_ROW,
                area.w,
                TREE_ROW,
            );
            frame.rect_clipped(row, p.popup_bg, 0.0, area);
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

fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => t!("agent-permission-ask"),
        PermissionMode::AutoReview => t!("agent-permission-auto"),
        PermissionMode::FullAccess => t!("agent-permission-full"),
    }
}

fn draw_permissions(app: &App, frame: &mut Frame<'_>, panel: &mut Panel, body: Rect, button: Rect) {
    let p = app.palette;
    let options = [
        (PermissionMode::Ask, t!("agent-permission-ask-detail")),
        (
            PermissionMode::AutoReview,
            t!("agent-permission-auto-detail"),
        ),
        (
            PermissionMode::FullAccess,
            t!("agent-permission-full-detail"),
        ),
    ];
    let width = body.w.min(420.0);
    let bottom = button.y - BUTTON_GAP;
    let row_h = ((bottom - body.y - 2.0 * BUTTON_GAP) / 3.0).clamp(0.0, 43.0);
    let menu_h = 3.0 * row_h + 2.0 * BUTTON_GAP;
    let menu = Rect::new(body.x, bottom - menu_h, width, menu_h);
    surface(frame, menu, p.popup_bg, p.border, RADIUS_ROW + 4.0, body);
    let (x, y) = app.mouse();
    for (index, (mode, description)) in options.into_iter().enumerate() {
        let row = Rect::new(
            menu.x + BUTTON_GAP,
            menu.y + BUTTON_GAP + index as f32 * row_h,
            (menu.w - 2.0 * BUTTON_GAP).max(0.0),
            row_h,
        );
        let selected = mode == panel.permissions;
        draw_button(frame, p, row, menu, row.contains(x, y), selected);
        let text = row.inset(SIDEBAR_GAP, 0.0);
        let title = Rect::new(
            text.x,
            text.y,
            text.w,
            if row_h >= 36.0 { 21.0 } else { row_h },
        );
        let label = permission_label(mode);
        let label = if selected {
            format!("✓  {label}")
        } else {
            label.to_owned()
        };
        let label = ellipsize(frame, &label, app.fonts.ui, title.w);
        let color = if mode == PermissionMode::FullAccess {
            p.warning
        } else {
            p.text
        };
        ui_text(frame, app, title.x, title, &label, color, row);
        if row_h >= 36.0 {
            let detail = Rect::new(
                text.x,
                title.bottom(),
                text.w,
                row.bottom() - title.bottom(),
            );
            let description = ellipsize(frame, description, app.fonts.ui_small, detail.w);
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
        panel.hits.push((row, Hit::SetPermissions(mode)));
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
    bar: Rect,
    history: Option<(&str, f32, bool)>,
) {
    let p = app.palette;
    surface(frame, bar, p.group_bg, p.border, RADIUS_ROW + 2.0, bar);
    let inner = bar.inset(BUTTON_GAP, BUTTON_GAP);
    let history_w = history.map_or(0.0, |(_, width, _)| width.min(inner.w / 3.0));
    let available = (inner.w - history_w).max(0.0);
    let effort = panel.effort().unwrap_or(t!("agent-effort"));
    let effort_w = (button_width(app, frame, effort) + 12.0).min(available * 0.45);
    let model = panel
        .model()
        .map_or(t!("agent-model"), |model| model.name.as_str());
    let model_w = (button_width(app, frame, model) + 12.0).min(available - effort_w);
    let model_rect = Rect::new(inner.x, inner.y, model_w, inner.h);
    let effort_rect = Rect::new(inner.right() - effort_w, inner.y, effort_w, inner.h);
    let model_enabled = !panel.models().is_empty();
    let effort_enabled = panel.model().is_some();
    let (model_hit, effort_hit) = (panel.tab == Tab::Models, panel.tab == Tab::Efforts);
    let controls = [
        (model_rect, model, Tab::Models, model_enabled, model_hit),
        (
            effort_rect,
            effort,
            Tab::Efforts,
            effort_enabled,
            effort_hit,
        ),
    ];
    let (x, y) = app.mouse();
    for (rect, label, tab, enabled, selected) in controls {
        draw_button(
            frame,
            p,
            rect,
            inner,
            enabled && rect.contains(x, y),
            selected || tab == Tab::Models,
        );
        let arrow_w = if enabled { 13.0 } else { 0.0 };
        let room = Rect::new(
            rect.x + SIDEBAR_LIST_GAP,
            rect.y,
            (rect.w - 2.0 * SIDEBAR_LIST_GAP - arrow_w).max(0.0),
            rect.h,
        );
        let label = truncate_end(frame, label, app.fonts.ui_small, room.w);
        ui_text_small(
            frame,
            app,
            room.x,
            room,
            &label,
            if enabled { p.text } else { p.text_dim },
            room,
        );
        if enabled && rect.w > 0.0 {
            icons::chevron(
                frame,
                (rect.right() - SIDEBAR_GAP, center(rect).1),
                7.0,
                if selected {
                    icons::Direction::Up
                } else {
                    icons::Direction::Down
                },
                p.text_muted,
                rect,
            );
        }
    }
    let hits = controls.map(|(rect, _, tab, enabled, _)| {
        (enabled && rect.w > 0.0).then_some((rect, Hit::Show(tab)))
    });
    panel.hits.extend(hits.into_iter().flatten());
    if let Some((label, _, enabled)) = history {
        let rect = Rect::new(effort_rect.x - history_w, inner.y, history_w, inner.h);
        Actions {
            app,
            frame,
            hits: &mut panel.hits,
        }
        .button(
            rect,
            label,
            Hit::Show(Tab::History),
            panel.tab == Tab::History,
            enabled,
        );
    }
}

fn draw_tokens(
    app: &App,
    frame: &mut Frame<'_>,
    row: Rect,
    label: &str,
    fraction: Option<f32>,
) -> f32 {
    let p = app.palette;
    let percent = fraction.map(|fraction| format!("{:.0}%", fraction * 100.0));
    let percent_w = percent.as_deref().map_or(0.0, |percent| {
        frame
            .measure(FontKind::Ui, app.fonts.ui_small, percent)
            .ceil()
            + SIDEBAR_GAP
    });
    let inline = frame.measure(FontKind::Ui, app.fonts.ui_small, label) + percent_w <= row.w;
    let label_w = if inline { row.w - percent_w } else { row.w };
    let lines = wrap_text(label, label_w.max(4.0), |text| {
        frame.measure(FontKind::Ui, app.fonts.ui_small, text)
    });
    let mut top = row.y;
    for text in lines.into_iter().take(2) {
        let line = Rect::new(row.x, top, label_w, row.h);
        ui_text_small(frame, app, line.x, line, text, p.text_muted, line);
        top += ROW - BUTTON_GAP;
    }
    if let (Some(fraction), Some(percent)) = (fraction, percent) {
        let percent_y = if inline { row.y } else { top };
        let value = Rect::new(row.right() - percent_w, percent_y, percent_w, row.h);
        ui_text_small(
            frame,
            app,
            value.x + SIDEBAR_GAP,
            value,
            &percent,
            p.text_muted,
            value,
        );
        let track = if inline {
            Rect::new(row.x, top + BUTTON_GAP, row.w, 4.0)
        } else {
            Rect::new(
                row.x,
                top + (ROW - 4.0) / 2.0,
                (row.w - percent_w).max(0.0),
                4.0,
            )
        };
        frame.rect_clipped(track, p.switch_off, 2.0, track);
        let fill = Rect::new(track.x, track.y, track.w * fraction, track.h);
        frame.rect_clipped(fill, p.accent, 2.0, track);
        top = if inline {
            track.bottom()
        } else {
            value.bottom()
        };
    }
    top + SIDEBAR_GAP
}

fn draw_status(
    app: &App,
    frame: &mut Frame<'_>,
    panel: &mut Panel,
    area: Rect,
    project_matches: bool,
) -> f32 {
    let max_rows = ((area.h / BUTTON_SMALL) as usize).min(3);
    if max_rows == 0 {
        return area.bottom();
    }
    let p = app.palette;
    let warning = panel.error.is_some()
        || panel.connection != Connection::Ready
        || !project_matches
        || panel.approval().is_some();
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
    let status_w = (elapsed_end - elapsed_w - SIDEBAR_GAP - status_x).max(0.0);
    let rows = wrap_text(status, status_w.max(4.0), |text| {
        frame.measure(FontKind::Ui, app.fonts.ui_small, text)
    });
    let top = area.bottom() - BUTTON_SMALL * rows.len().clamp(1, max_rows) as f32;
    frame.rect_clipped(
        Rect::new(area.x, top - 1.0, area.w, 1.0),
        p.border,
        0.0,
        area,
    );
    let glyph = (area.x + 8.0, top + BUTTON_SMALL / 2.0);
    if warning {
        icons::warning(frame, glyph, p.warning, area);
    } else if panel.busy() {
        icons::spinner(
            frame,
            glyph,
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
    top
}

fn draw_empty_chat(app: &App, frame: &mut Frame<'_>, area: Rect) {
    if area.h < 110.0 {
        return;
    }
    let p = app.palette;
    let top = area.y + (area.h * 0.25).min(130.0);
    let center = area.x + area.w / 2.0;
    icons::codex(frame, (center, top + BUTTON / 2.0), p.text_muted, area);
    let label = t!("agent-empty-chat");
    let width = frame.measure(FontKind::Ui, app.fonts.ui, label);
    let row = Rect::new(area.x, top + BUTTON + SIDEBAR_LIST_GAP, area.w, TREE_ROW);
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
    let stride = TREE_ROW + BUTTON_GAP;
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
        let row = Rect::new(area.x, y, area.w, TREE_ROW).intersect(area);
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
            let compact = compact_tool_card(card);
            let block = Rect::new(
                area.x,
                y,
                width,
                card.height - if compact { BUTTON_GAP } else { CARD_GAP },
            );
            let p = app.palette;
            let header_height = card_header_height(card.item.kind);
            match card.item.kind {
                ItemKind::User => {
                    let bubble =
                        Rect::new(block.x, y + header_height, block.w, block.h - header_height);
                    surface(
                        frame,
                        bubble,
                        p.selected.alpha(0.25),
                        p.tab_edge.alpha(0.5),
                        RADIUS_TAB,
                        area,
                    );
                }
                ItemKind::Message => {}
                _ => surface(frame, block, p.group_bg, p.border, RADIUS_TAB, area),
            }
            let heading = Rect::new(
                block.x + CARD_PAD,
                y,
                (block.w - 62.0).max(0.0),
                header_height,
            )
            .intersect(area);
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
                let (x, mouse_y) = app.mouse();
                if app.cursor_inside() && block.intersect(area).contains(x, mouse_y) {
                    ui_text_small(
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
                    let copy = Rect::new(
                        block.right() - CARD_PAD - BUTTON_SMALL,
                        y,
                        BUTTON_SMALL,
                        header_height,
                    );
                    let hit = copy.intersect(area);
                    draw_button(
                        actions.frame,
                        p,
                        copy,
                        area,
                        hit.contains(x, mouse_y),
                        false,
                    );
                    icons::copy(actions.frame, center(copy), p.text_muted, hit);
                    if hit.h > 0.0 {
                        actions.hits.push((hit, Hit::CopyMessage(ordinal)));
                    }
                    if let Some((hour, minute)) = card.message_time {
                        let label = format!("{hour:02}:{minute:02}");
                        let width = actions
                            .frame
                            .measure(FontKind::Ui, app.fonts.ui_small, &label);
                        let time = Rect::new(copy.x - BUTTON_GAP - width, y, width, header_height);
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
                }
            } else {
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
                    let x = block.x + CARD_PAD;
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
        let at_bottom = next == chat.cards.len() && y <= area.bottom() + 1.0;
        if at_bottom {
            chat.follow();
        }
        let before = chat
            .cards
            .iter()
            .take(first)
            .map(|card| card.height)
            .sum::<f32>();
        let position = before + offset;
        let total = position + y - area.y
            + chat
                .cards
                .iter()
                .skip(next)
                .map(|card| card.height)
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
        if !at_bottom {
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
            icons::spinner(
                frame,
                glyph,
                icons::spin_turn() * RUNNING_SPIN_SCALE,
                p.text_muted,
                hit,
            );
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
    if matches!(kind, ItemKind::User | ItemKind::Message) {
        MESSAGE_HEADER
    } else {
        TREE_ROW
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
    let content_width = (width - 2.0 * CARD_PAD).max(4.0);
    let compact = compact_tool_card(card);
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
    card.height = card_header_height(card.item.kind)
        + if compact {
            BUTTON_GAP
        } else if card.item.kind == ItemKind::User {
            2.0 * USER_MESSAGE_PAD_Y + CARD_GAP
        } else {
            CONTENT_TOP + SIDEBAR_LIST_GAP + CARD_GAP
        }
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
        surface: p.editor_bg,
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
    fn button(&mut self, area: Rect, label: &str, hit: Hit, emphasized: bool, enabled: bool) {
        if area.w <= 0.0 || area.h <= 0.0 {
            return;
        }
        let left = matches!(
            hit,
            Hit::Expand(_)
                | Hit::Open(_, false)
                | Hit::ChooseSession(_)
                | Hit::ChooseModel(_)
                | Hit::ChooseEffort(_)
                | Hit::Complete(_)
        );
        let p = self.app.palette;
        let (x, y) = self.app.mouse();
        let hovered = enabled && area.contains(x, y);
        if emphasized && !left || matches!(hit, Hit::Decide(..)) {
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
