//! Interactions with the native Codex panel and its existing IME pipeline.

use super::*;
use winit::event::Ime;

#[test]
fn agent_panel_is_lazy_and_uses_the_multiline_input_pipeline() {
    let mut app = blank_app(false);
    assert!(app.agent.is_none());
    let editor_width = app.layout.editor.w;
    assert!(app.run_action(crate::keymap::Action::ToggleAgent));
    assert_eq!(app.focus, Focus::Agent);
    assert!(app.layout.editor.w < editor_width);
    assert!(app.layout.editor.right() < app.agent_rects().body.x);

    app.on_ime(Ime::Preedit("ni".into(), Some((0, 2))));
    app.on_ime(Ime::Commit("你好".into()));
    app.edit_field(|field| field.insert("\n第二行"));
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");

    let before = app.agent_rects().body.w;
    let divider = app.agent_rects().divider;
    assert!(app.agent_click(divider.x + 2.0, divider.y + 30.0));
    app.on_mouse_move(divider.x - 48.0, divider.y + 30.0);
    app.on_mouse_up(MouseButton::Left);
    assert!(app.agent_rects().body.w > before);

    let button = app.agent_rects().button;
    assert!(app.agent_click(button.x + 5.0, button.y + 5.0));
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.layout.editor.w, editor_width);
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");
}

#[test]
fn agent_configuration_hides_the_entry_point() {
    let mut app = blank_app(false);
    app.config.agent_enabled = Some(false);
    app.relayout();
    assert_eq!(app.agent_rects().button.w, 0.0);
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
