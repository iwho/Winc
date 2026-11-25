//! Composer completion through the existing project-file index and panel commands.

use super::*;

#[derive(Clone, Copy)]
pub(crate) enum Slash {
    New,
    Resume,
    Diff,
    Model,
}

impl Slash {
    fn name(self) -> &'static str {
        match self {
            Self::New => "/new",
            Self::Resume => "/resume",
            Self::Diff => "/diff",
            Self::Model => "/model",
        }
    }
}

enum Choice {
    File(String),
    Slash(Slash),
}

#[derive(Default)]
pub(crate) struct Completion {
    choices: Vec<Choice>,
    selected: usize,
    file_range: Option<std::ops::Range<usize>>,
}

impl Completion {
    pub(crate) fn labels(&self) -> impl Iterator<Item = (&str, bool)> {
        self.choices.iter().enumerate().map(|(i, choice)| {
            let label = match choice {
                Choice::File(path) => path,
                Choice::Slash(command) => command.name(),
            };
            (label, i == self.selected)
        })
    }
}

fn slash_start(text: &str) -> Option<&str> {
    let query = text.strip_prefix('/')?;
    query
        .chars()
        .all(|c| c.is_ascii_alphabetic())
        .then_some(text)
}

fn file_start(text: &str, caret: usize) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    let start = before.rfind(char::is_whitespace).map_or(0, |start| {
        start
            + before[start..]
                .chars()
                .next()
                .expect("whitespace")
                .len_utf8()
    });
    let query = before[start..].strip_prefix('@')?;
    Some((start, query))
}

impl App {
    pub(in crate::app) fn composer_changed(&mut self) {
        let Some(panel) = &mut self.agent else { return };
        panel.selection = None;
        panel.completion = Completion::default();
        if let Some(prefix) = slash_start(panel.field.text()) {
            let ready = panel.connection == Connection::Ready;
            let choices = [
                (Slash::New, ready && panel.opening.is_none()),
                (Slash::Resume, ready),
                (Slash::Diff, panel.current().is_some()),
                (Slash::Model, !panel.models().is_empty()),
            ];
            panel
                .completion
                .choices
                .extend(choices.into_iter().filter_map(|(command, enabled)| {
                    (enabled && command.name().starts_with(prefix))
                        .then_some(Choice::Slash(command))
                }));
        }
    }

    pub(in crate::app) fn agent_completion_key(&mut self, key: &winit::keyboard::Key) -> bool {
        use winit::keyboard::{Key, NamedKey};

        let Key::Named(key) = key else { return false };
        let Some(panel) = &mut self.agent else {
            return false;
        };
        let completion = &mut panel.completion;
        if !completion.choices.is_empty() {
            match key {
                NamedKey::Tab | NamedKey::Enter if !self.pointer.modifiers.shift_key() => {
                    let selected = completion.selected;
                    self.agent_complete(selected);
                    return true;
                }
                NamedKey::Escape => {
                    *completion = Completion::default();
                    return true;
                }
                NamedKey::ArrowUp | NamedKey::ArrowDown if !self.pointer.modifiers.alt_key() => {
                    let n = completion.choices.len();
                    let step = if *key == NamedKey::ArrowUp { n - 1 } else { 1 };
                    completion.selected = (completion.selected + step) % n;
                    return true;
                }
                _ => {}
            }
        }
        if *key != NamedKey::Tab {
            return false;
        }
        let caret = panel.field.caret();
        let Some((start, query)) = file_start(panel.field.text(), caret) else {
            return false;
        };
        let Some(root) = panel.root.clone().filter(|_| !panel.standalone) else {
            return false;
        };
        let query = query.to_owned();
        self.prepare_index();
        let index = &mut self.overlays.picker.index;
        let choices = index
            .search(&query)
            .into_iter()
            .filter_map(|item| {
                let path = index.absolute(item)?;
                let relative = path.strip_prefix(&root).ok()?.to_str()?;
                Some(Choice::File(relative.to_owned()))
            })
            .take(6)
            .collect();
        self.agent.as_mut().expect("active agent").completion = Completion {
            choices,
            selected: 0,
            file_range: Some(start..caret),
        };
        true
    }

    pub(super) fn agent_complete(&mut self, selected: usize) {
        let Some(panel) = &mut self.agent else { return };
        let completion = std::mem::take(&mut panel.completion);
        let Some(choice) = completion.choices.into_iter().nth(selected) else {
            return;
        };
        match choice {
            Choice::File(path) => {
                let Some(range) = completion.file_range else {
                    return;
                };
                if range.end != panel.field.caret()
                    || file_start(panel.field.text(), range.end).map(|(start, _)| start)
                        != Some(range.start)
                {
                    return;
                }
                let insertion = if path.contains(char::is_whitespace) {
                    format!("`{path}` ")
                } else {
                    format!("{path} ")
                };
                panel.field.set_caret(range.start, false);
                panel.field.set_caret(range.end, true);
                panel.field.insert(&insertion);
            }
            Choice::Slash(command) => {
                if !slash_start(panel.field.text())
                    .is_some_and(|prefix| command.name().starts_with(prefix))
                {
                    return;
                }
                if matches!(command, Slash::New) && !self.agent_project_matches() {
                    return;
                }
                let panel = self.agent.as_mut().expect("active agent");
                panel.field.set_text("");
                match command {
                    Slash::New => panel.new_conversation(false),
                    Slash::Resume => panel.show_tab(Tab::History),
                    Slash::Diff => panel.show_tab(Tab::Changes),
                    Slash::Model => panel.show_tab(Tab::Models),
                }
            }
        }
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examines_only_the_active_prompt_token() {
        assert_eq!(file_start("检查 @src/main", 16), Some((7, "src/main")));
        assert_eq!(file_start("name@example.com", 16), None);
        assert_eq!(slash_start("/res"), Some("/res"));
        assert_eq!(slash_start("/src/main.rs"), None);
    }
}
