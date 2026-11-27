//! Interactions with the native Codex panel and its existing IME pipeline.

use super::*;
use winit::event::Ime;

fn codex_button(app: &App) -> Option<(f32, f32)> {
    app.hits.status.iter().find_map(|&(rect, item)| {
        (item == StatusItem::Agent).then_some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
    })
}

#[test]
fn agent_panel_is_lazy_and_uses_the_multiline_input_pipeline() {
    let mut app = blank_app(false);
    assert!(app.agent.is_none());
    let editor_width = app.layout.editor.w;
    paint(&mut app);
    let button = codex_button(&app).unwrap();
    assert!(app.layout.status.contains(button.0, button.1));
    assert!(app.agent.is_none());
    click(&mut app, button);
    assert_eq!(app.focus, Focus::Agent);
    let panel = app.agent.as_ref().unwrap();
    assert!(panel.standalone);
    assert!(app.agent_matches_workspace(panel));
    assert!(panel.attachment.is_none());
    assert!(app.editor.workspace.is_none());
    assert!(app.editor.active().is_none());
    assert!(app.layout.editor.w < editor_width);
    assert!(app.layout.editor.right() < app.agent_rects().body.x);

    app.on_ime(Ime::Preedit("ni".into(), Some((0, 2))));
    app.on_ime(Ime::Commit("你好".into()));
    app.edit_field(|field| field.insert("\n第二行"));
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");

    let before = app.agent_rects().body.w;
    let divider = app.agent_rects().divider;
    let edge = (divider.x + divider.w / 2.0, divider.y + 30.0);
    app.on_mouse_move(edge.0, edge.1);
    paint(&mut app);
    assert!(near(pixel(&app, edge), app.palette.frame_bg));

    assert!(app.agent_click(divider.x + 2.0, divider.y + 30.0));
    app.on_mouse_move(divider.x - 500.0, divider.y + 30.0);
    app.on_mouse_up(MouseButton::Left);
    assert_eq!(app.agent_rects().body.w, before);

    assert!(app.agent_click(divider.x + 2.0, divider.y + 30.0));
    app.on_mouse_move(divider.x + 50.0, divider.y + 30.0);
    app.on_mouse_up(MouseButton::Left);
    let narrowed = app.agent_rects().body.w;
    assert!(narrowed < before);

    paint(&mut app);
    assert_eq!(codex_button(&app), Some(button));
    click(&mut app, button);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.layout.editor.w, editor_width);
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");

    click(&mut app, button);
    assert_eq!(app.agent_rects().body.w, narrowed);
}

#[test]
fn agent_configuration_hides_the_entry_point() {
    let mut app = blank_app(false);
    app.config.agent_enabled = Some(false);
    paint(&mut app);
    assert!(codex_button(&app).is_none());
    app.toggle_agent();
    assert!(app.agent.is_none());
}

#[test]
fn agent_attaches_the_editor_selection_and_opens_local_references() {
    let root = sample_folder("agent-context");
    let mut app = app_with(&root, false);
    let root = app.editor.workspace.as_ref().unwrap().root.clone();
    app.open_path(&root.join("short.rs"));
    app.go_to(1, 4, 7);
    app.agent_attach(true);
    let panel = app.agent.as_ref().unwrap();
    assert_eq!(panel.attachment.as_ref().unwrap().path(), "short.rs");

    let link = crate::app::agent::Link::parse("a.txt:2:5", true).unwrap();
    app.finish_agent_link(root.clone(), Some(root.join("a.txt")), link, false);
    let doc = app.editor.active().unwrap();
    assert_eq!(doc.name(), "a.txt");
    assert_eq!(doc.text().pos_to_line_col(doc.selection.head), (1, 4));
}

#[test]
fn standalone_agent_shows_a_composer_without_file_actions() {
    let mut app = blank_app(true);
    app.window.controls = true;
    paint(&mut app);
    let button = codex_button(&app).unwrap();
    click(&mut app, button);
    if std::env::var_os("WINC_SNAPSHOT_DIR").is_some() {
        shoot(&mut app, "codex-standalone-dark");
    } else {
        paint(&mut app);
    }
    let panel = app.agent.as_ref().unwrap();
    assert!(panel.field.view.area.h > 0.0);
    assert_eq!(panel.rects.body.y, app.layout.window.y);
    assert!(near(
        pixel(&app, (panel.rects.body.x + 2.0, panel.rects.body.y + 2.0)),
        app.palette.editor_bg
    ));
    assert!(
        !panel
            .hits
            .iter()
            .any(|(_, hit)| matches!(hit, crate::app::agent::Hit::Attach(_)))
    );
    let (close, _) = panel
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, crate::app::agent::Hit::Close))
        .unwrap();
    assert_eq!((close.w, close.h), (BUTTON, BUTTON));
    assert_eq!(close.y, app.layout.sidebar_button.y);
    assert!(close.right() <= app.layout.controls.x);
    assert!(app.caption_controls().contains(close));
    let point = (close.x + close.w / 2.0, close.y + close.h / 2.0);
    assert!(!app.over_title_bar(point.0, point.1));
    assert!(app.over_title_bar(panel.rects.body.x + SIDEBAR_GAP + BUTTON_SMALL, point.1));
    let controls = app.layout.controls;
    assert!(!app.agent_click(controls.x + 4.0, controls.y + 4.0));
    click(&mut app, point);
    assert!(!app.agent.as_ref().unwrap().shown);
}

#[test]
fn agent_header_groups_session_actions_before_panel_controls() {
    use crate::app::agent::{Connection, Hit, Tab};

    let mut app = blank_app(false);
    app.window.controls = true;
    app.toggle_agent();
    app.agent.as_mut().unwrap().connection = Connection::Ready;

    for width in [750.0, 240.0] {
        app.agent.as_mut().unwrap().width = width;
        paint(&mut app);
        let panel = app.agent.as_ref().unwrap();
        let caption_controls = app.caption_controls();
        let actions: Vec<_> = panel
            .hits
            .iter()
            .filter(|(_, hit)| {
                matches!(
                    hit,
                    Hit::New | Hit::Show(Tab::History) | Hit::Maximize | Hit::Split | Hit::Close
                )
            })
            .collect();
        assert!(matches!(
            actions.as_slice(),
            [
                (_, Hit::New),
                (_, Hit::Show(Tab::History)),
                (_, Hit::Maximize),
                (_, Hit::Split),
                (_, Hit::Close)
            ]
        ));
        for (rect, _) in &actions {
            assert_eq!(rect.y, app.layout.sidebar_button.y);
            assert!(rect.right() <= app.layout.controls.x);
            assert!(caption_controls.contains(rect));
        }
        let gaps: Vec<_> = actions
            .windows(2)
            .map(|pair| pair[1].0.x - pair[0].0.right())
            .collect();
        assert!(gaps.iter().all(|&gap| gap > 0.0));
        assert!(gaps[1] > gaps[0]);
    }
}
