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
    AgentCommand {
        label: String,
        description: String,
    },
    Skill {
        label: String,
        description: String,
    },
}

#[derive(Default)]
pub(crate) struct Completion {
    choices: Vec<Choice>,
    selected: usize,
    file_range: Option<std::ops::Range<usize>>,
}

impl Completion {
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn items(&self) -> impl Iterator<Item = (&str, Option<&str>, bool)> {
        self.choices.iter().enumerate().map(|(i, choice)| {
            let (label, description) = match choice {
                Choice::File(path) => (path.as_str(), None),
                Choice::Slash(command) => (command.name(), None),
                Choice::AgentCommand { label, description }
                | Choice::Skill { label, description } => (
                    label.as_str(),
                    (!description.is_empty()).then_some(description.as_str()),
                ),
            };
            (label, description, i == self.selected)
        })
    }
}

fn slash_start(text: &str) -> Option<&str> {
    let query = text.strip_prefix('/')?;
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .then_some(text)
}

fn skill_start(text: &str, caret: usize) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    let start = before.rfind(char::is_whitespace).map_or(0, |start| {
        start
            + before[start..]
                .chars()
                .next()
                .expect("whitespace")
                .len_utf8()
    });
    let query = before[start..].strip_prefix('$')?;
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .then_some((start, query))
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
        if let Some(elicit) = panel.elicitation_mut()
            && elicit.focused_field.is_some()
        {
            elicit.sync_other_text();
            return;
        }
        panel.selection = None;
        panel.completion = Completion::default();
        if let Some(prefix) = slash_start(panel.field.text()) {
            let ready = panel.connection == Connection::Ready;
            let query = prefix.strip_prefix('/').unwrap_or_default().to_ascii_lowercase();
            let local_choices = [
                (Slash::New, ready && panel.opening.is_none()),
                (Slash::Resume, ready),
                (Slash::Diff, panel.current().is_some()),
                (Slash::Model, !panel.models().is_empty()),
            ];
            let mut local_names = Vec::new();
            for (command, enabled) in local_choices {
                if !enabled {
                    continue;
                }
                let name = command.name().strip_prefix('/').unwrap_or_default();
                local_names.push(name);
                if name.starts_with(&query) {
                    panel.completion.choices.push(Choice::Slash(command));
                }
            }
            let commands: Vec<_> = panel
                .commands()
                .iter()
                .filter(|cmd| {
                    !local_names
                        .iter()
                        .any(|local| local.eq_ignore_ascii_case(&cmd.name))
                })
                .cloned()
                .collect();
            let to_choice = |cmd: &winc_agent::SlashCommand| {
                let description = if !cmd.description.is_empty() {
                    cmd.description.clone()
                } else {
                    cmd.input_hint.clone().unwrap_or_default()
                };
                Choice::AgentCommand {
                    label: format!("/{}", cmd.name),
                    description,
                }
            };
            for cmd in &commands {
                if cmd.name.to_ascii_lowercase().starts_with(&query) {
                    panel.completion.choices.push(to_choice(cmd));
                }
            }
            if !query.is_empty() {
                for cmd in &commands {
                    let lower = cmd.name.to_ascii_lowercase();
                    if !lower.starts_with(&query) && lower.contains(&query) {
                        panel.completion.choices.push(to_choice(cmd));
                    }
                }
            }
            let skills: Vec<_> = panel.skills().to_vec();
            let to_skill = |skill: &winc_agent::SlashCommand| Choice::Skill {
                label: format!("${}", skill.name),
                description: skill.description.clone(),
            };
            for skill in &skills {
                if skill.name.to_ascii_lowercase().starts_with(&query) {
                    panel.completion.choices.push(to_skill(skill));
                }
            }
            if !query.is_empty() {
                for skill in &skills {
                    let lower = skill.name.to_ascii_lowercase();
                    if !lower.starts_with(&query) && lower.contains(&query) {
                        panel.completion.choices.push(to_skill(skill));
                    }
                }
            }
            panel.completion.file_range = Some(0..panel.field.text().len());
        } else if let Some((start, query)) = skill_start(panel.field.text(), panel.field.caret()) {
            let query = query.to_ascii_lowercase();
            let skills: Vec<_> = panel.skills().to_vec();
            let to_skill = |skill: &winc_agent::SlashCommand| Choice::Skill {
                label: format!("${}", skill.name),
                description: skill.description.clone(),
            };
            for skill in &skills {
                if skill.name.to_ascii_lowercase().starts_with(&query) {
                    panel.completion.choices.push(to_skill(skill));
                }
            }
            if !query.is_empty() {
                for skill in &skills {
                    let lower = skill.name.to_ascii_lowercase();
                    if !lower.starts_with(&query) && lower.contains(&query) {
                        panel.completion.choices.push(to_skill(skill));
                    }
                }
            }
            if !panel.completion.choices.is_empty() {
                panel.completion.file_range = Some(start..panel.field.caret());
            }
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
        let Some(root) = panel
            .workspace_root
            .clone()
            .or_else(|| panel.root.clone())
            .filter(|_| !panel.standalone)
        else {
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
                if !slash_start(panel.field.text()).is_some_and(|prefix| {
                    command
                        .name()
                        .to_ascii_lowercase()
                        .starts_with(&prefix.to_ascii_lowercase())
                }) {
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
            Choice::AgentCommand { label, .. } => {
                if slash_start(panel.field.text()).is_none() {
                    return;
                }
                let text = format!("{label} ");
                panel.field.set_text(&text);
                panel.field.set_caret(text.len(), false);
            }
            Choice::Skill { label, .. } => {
                let Some(range) = completion.file_range else {
                    return;
                };
                if range.end > panel.field.text().len() {
                    return;
                }
                let insertion = format!("{label} ");
                panel.field.set_caret(range.start, false);
                panel.field.set_caret(range.end, true);
                panel.field.insert(&insertion);
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
        assert_eq!(slash_start("/grill-me"), Some("/grill-me"));
        assert_eq!(slash_start("/mcp:tool_1"), Some("/mcp:tool_1"));
        assert_eq!(slash_start("/src/main.rs"), None);
        assert_eq!(slash_start("/plan fix"), None);
    }

    #[test]
    fn completes_acp_commands_and_skills_into_composer() {
        let mut app = crate::app::snapshot::blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.connection = Connection::Ready;
        panel.opening = Some(Opening {
            request: 1,
            send: false,
            is_new: true,
        });
        panel.event(Event::SessionOpened(
            1,
            Session {
                id: "s1".into(),
                cwd: PathBuf::new(),
                title: String::new(),
                model: None,
                effort: None,
            },
            Vec::new(),
            None,
        ));
        panel.event(Event::SessionCommands(
            "s1".into(),
            vec![
                SlashCommand {
                    name: "plan".into(),
                    description: "Create a plan".into(),
                    input_hint: Some("task".into()),
                },
                SlashCommand {
                    name: "grill-me".into(),
                    description: "Design interview".into(),
                    input_hint: None,
                },
                SlashCommand {
                    name: "new".into(),
                    description: "Duplicate of local /new".into(),
                    input_hint: None,
                },
            ],
        ));

        app.agent.as_mut().unwrap().field.set_text("/gr");
        app.composer_changed();
        let items: Vec<_> = app
            .agent
            .as_ref()
            .unwrap()
            .completion
            .items()
            .map(|(l, d, s)| (l.to_owned(), d.map(str::to_owned), s))
            .collect();
        assert_eq!(
            items,
            vec![(
                "/grill-me".to_owned(),
                Some("Design interview".to_owned()),
                true
            )]
        );

        app.agent_complete(0);
        let panel = app.agent.as_ref().unwrap();
        assert_eq!(panel.field.text(), "/grill-me ");
        assert_eq!(panel.field.caret(), "/grill-me ".len());
        assert_eq!(panel.completion.items().count(), 0);

        // Local /new is not duplicated by the provider's "new".
        app.agent.as_mut().unwrap().field.set_text("/new");
        app.composer_changed();
        assert_eq!(
            app.agent.as_ref().unwrap().completion.items().count(),
            1
        );
    }

    #[test]
    fn completes_codex_skills_with_dollar_and_slash_prefixes() {
        let mut app = crate::app::snapshot::blank_app(false);
        app.toggle_agent();
        let panel = app.agent.as_mut().unwrap();
        panel.connection = Connection::Ready;
        panel.catalog.skills_request = Some(9);
        panel.event(Event::Skills(
            9,
            vec![SlashCommand {
                name: "code-review".into(),
                description: "Review changes".into(),
                input_hint: None,
            }],
        ));

        // Mid-sentence `$` completion inserts `$code-review ` in place.
        let prompt = "请用 $code";
        let panel = app.agent.as_mut().unwrap();
        panel.field.set_text(prompt);
        panel.field.set_caret(prompt.len(), false);
        app.composer_changed();
        let items: Vec<_> = app
            .agent
            .as_ref()
            .unwrap()
            .completion
            .items()
            .map(|(l, d, _)| (l.to_owned(), d.map(str::to_owned)))
            .collect();
        assert_eq!(
            items,
            vec![("$code-review".to_owned(), Some("Review changes".to_owned()))]
        );
        app.agent_complete(0);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "请用 $code-review ");

        // `/` prefix also surfaces Codex skills and inserts `$code-review `.
        let panel = app.agent.as_mut().unwrap();
        panel.field.set_text("/code");
        panel.field.set_caret(5, false);
        app.composer_changed();
        app.agent_complete(0);
        assert_eq!(app.agent.as_ref().unwrap().field.text(), "$code-review ");
    }
}
