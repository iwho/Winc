//! Interactions with the native Codex panel and its existing IME pipeline.

use super::*;
use winit::event::Ime;

#[test]
fn agent_panel_is_lazy_and_uses_the_multiline_input_pipeline() {
    let mut app = blank_app(false);
    assert!(app.agent.is_none());
    let editor_width = app.layout.editor.w;
    let button = app.layout.agent_button;
    assert!(app.agent_click(button.x + 5.0, button.y + 5.0));
    assert_eq!(app.focus, Focus::Agent);
    assert!(app.layout.editor.w < editor_width);
    assert!(app.layout.editor.right() < app.layout.agent.x);

    app.on_ime(Ime::Preedit("ni".into(), Some((0, 2))));
    app.on_ime(Ime::Commit("你好".into()));
    app.edit_field(|field| field.insert("\n第二行"));
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");

    let before = app.layout.agent.w;
    let divider = app.layout.agent_divider;
    assert!(app.agent_click(divider.x + 2.0, divider.y + 30.0));
    app.on_mouse_move(divider.x - 48.0, divider.y + 30.0);
    app.on_mouse_up(MouseButton::Left);
    assert!(app.layout.agent.w > before);

    app.toggle_agent();
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.layout.editor.w, editor_width);
    assert_eq!(app.agent.as_ref().unwrap().field.text(), "你好\n第二行");
}

#[test]
fn agent_configuration_hides_the_entry_point() {
    let mut app = blank_app(false);
    app.config.agent_enabled = Some(false);
    app.relayout();
    assert_eq!(app.layout.agent_button.w, 0.0);
    app.toggle_agent();
    assert!(app.agent.is_none());
}
