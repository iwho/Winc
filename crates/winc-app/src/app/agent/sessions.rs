//! Project history, model choices, and draft isolation across loaded Codex threads.

use winc_agent::Model;

use super::*;

const MAX_HISTORY: usize = 250;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    #[default]
    Chat,
    Changes,
    History,
    Models,
    Efforts,
}

#[derive(Default)]
pub(super) struct Catalog {
    pub(super) history: Vec<Session>,
    pub(super) models: Vec<Model>,
    history_cursor: Option<String>,
    model_cursor: Option<String>,
    history_request: Option<u64>,
    model_request: Option<u64>,
}

impl Panel {
    pub(crate) fn history(&self) -> &[Session] {
        &self.catalog.history
    }

    pub(crate) fn models(&self) -> &[Model] {
        &self.catalog.models
    }

    pub(crate) fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub(crate) fn model(&self) -> Option<&Model> {
        let chosen = self
            .current()
            .and_then(|s| s.model.as_deref())
            .or(self.default_model.as_deref());
        self.models()
            .iter()
            .find(|model| Some(model.id.as_str()) == chosen)
            .or_else(|| self.models().iter().find(|model| model.is_default))
            .or_else(|| self.models().first())
    }

    pub(crate) fn effort(&self) -> Option<&str> {
        let model = self.model()?;
        self.current()
            .and_then(|s| s.effort.as_deref())
            .or(self.default_effort.as_deref())
            .filter(|effort| model.efforts.iter().any(|item| item == effort))
            .or(model.default_effort.as_deref())
    }

    pub(crate) fn more(&self) -> bool {
        match self.tab {
            Tab::History => self.catalog.history_cursor.is_some(),
            Tab::Models => self.catalog.model_cursor.is_some(),
            _ => false,
        }
    }

    pub(crate) fn pending(&self, id: &str) -> bool {
        self.approvals
            .iter()
            .any(|approval| approval.turn.session == id)
            || self
                .conversations
                .get(id)
                .is_some_and(|session| session.running.is_some())
    }

    pub(crate) fn token_label(&self) -> Option<String> {
        let (total, last, window) = self.current()?.tokens;
        (total > 0).then(|| match window {
            Some(window) => t!(
                "agent-tokens-window",
                total = total as f64,
                last = last as f64,
                window = window as f64
            ),
            None => t!("agent-tokens", total = total as f64, last = last as f64),
        })
    }

    pub(crate) fn writer_count(&self) -> usize {
        self.conversations
            .values()
            .filter(|s| s.running.is_some() || s.send_request.is_some())
            .count()
    }

    pub(super) fn catalog_ready(&self) -> bool {
        self.catalog.model_request.is_none()
    }

    pub(super) fn discover(&mut self) {
        if self.catalog.history_request.is_none() {
            self.catalog.history_request = self.submit(Command::History(None));
        }
        if self.catalog.model_request.is_none() {
            self.catalog.model_request = self.submit(Command::Models(None));
        }
    }

    pub(super) fn next_page(&mut self) {
        match self.tab {
            Tab::History if self.catalog.history_request.is_none() => {
                if let Some(cursor) = self.catalog.history_cursor.clone() {
                    self.catalog.history_request = self.submit(Command::History(Some(cursor)));
                }
            }
            Tab::Models if self.catalog.model_request.is_none() => {
                if let Some(cursor) = self.catalog.model_cursor.clone() {
                    self.catalog.model_request = self.submit(Command::Models(Some(cursor)));
                }
            }
            _ => {}
        }
    }

    pub(super) fn history_received(
        &mut self,
        request: u64,
        sessions: Vec<Session>,
        cursor: Option<String>,
    ) {
        if self.catalog.history_request != Some(request) {
            return;
        }
        self.catalog.history_request = None;
        for session in sessions {
            if self.catalog.history.len() == MAX_HISTORY {
                break;
            }
            if !self
                .catalog
                .history
                .iter()
                .any(|entry| entry.id == session.id)
            {
                self.catalog.history.push(session);
            }
        }
        self.catalog.history_cursor = cursor.filter(|_| self.catalog.history.len() < MAX_HISTORY);
    }

    pub(super) fn models_received(
        &mut self,
        request: u64,
        models: Vec<Model>,
        cursor: Option<String>,
    ) {
        if self.catalog.model_request != Some(request) {
            return;
        }
        self.catalog.model_request = None;
        for model in models {
            if !self.catalog.models.iter().any(|entry| entry.id == model.id) {
                self.catalog.models.push(model);
            }
        }
        self.catalog.model_cursor = cursor;
    }

    pub(super) fn catalog_failed(&mut self, id: u64) {
        if self.catalog.history_request == Some(id) {
            self.catalog.history_request = None;
        }
        if self.catalog.model_request == Some(id) {
            self.catalog.model_request = None;
        }
    }

    pub(super) fn choose(&mut self, id: String) {
        if self.opening.is_some() {
            return;
        }
        if self.conversations.get(&id).is_some_and(|s| s.loaded) {
            self.select(id);
        } else if self.connection == Connection::Ready
            && let Some(request) = self.submit(Command::Resume(id))
        {
            self.opening = Some((request, false));
        }
    }

    pub(super) fn choose_model(&mut self, id: String) {
        let effort = self
            .models()
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.default_effort.clone());
        if let Some(session) = self.current_mut() {
            session.model = Some(id);
            session.effort = effort;
        } else {
            self.default_model = Some(id);
            self.default_effort = effort;
        }
        self.show_tab(Tab::Chat);
    }

    pub(super) fn choose_effort(&mut self, effort: String) {
        if let Some(session) = self.current_mut() {
            session.effort = Some(effort);
        } else {
            self.default_effort = Some(effort);
        }
        self.show_tab(Tab::Chat);
    }

    pub(super) fn title_received(&mut self, id: &str, title: Option<String>) {
        let title = title.unwrap_or_default();
        if let Some(session) = self.conversations.get_mut(id) {
            session.session.title.clone_from(&title);
        }
        if let Some(entry) = self.catalog.history.iter_mut().find(|entry| entry.id == id) {
            entry.title = title;
        }
    }

    pub(super) fn remember_session(&mut self, session: &Session) {
        if let Some(i) = self
            .catalog
            .history
            .iter()
            .position(|entry| entry.id == session.id)
        {
            if session.title.is_empty() {
                return;
            }
            self.catalog.history.remove(i);
        }
        self.catalog.history.insert(0, session.clone());
        self.catalog.history.truncate(MAX_HISTORY);
    }

    pub(super) fn select(&mut self, id: String) {
        if self.selected.as_ref() != Some(&id) {
            let had_selection = self.selected.is_some();
            if let Some(old) = self.selected.take()
                && let Some(session) = self.conversations.get_mut(&old)
            {
                session.draft = std::mem::replace(&mut self.field, TextField::multiline());
                session.attachment = self.attachment.take();
            }
            if let Some(session) = self.conversations.get_mut(&id) {
                // The first submitted prompt starts in the unattached composer.
                if had_selection || self.field.text().is_empty() {
                    std::mem::swap(&mut self.field, &mut session.draft);
                }
                if session.attachment.is_some() {
                    self.attachment = session.attachment.take();
                }
            }
            self.selected = Some(id);
        }
        self.show_tab(Tab::Chat);
    }

    pub(super) fn show_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selection = None;
        self.completion = composer::Completion::default();
        self.scroll = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            cwd: PathBuf::from("/project"),
            title: String::new(),
            model: None,
            effort: None,
        }
    }

    #[test]
    fn sessions_keep_independent_drafts_and_late_acknowledgements() {
        let mut panel = Panel::new();
        for id in ["first", "second"] {
            panel
                .conversations
                .insert(id.into(), Conversation::new(session(id)));
        }
        panel.select("first".into());
        panel.field.set_text("first prompt");
        panel.current_mut().unwrap().send_request = Some(Submission {
            id: 9,
            steer: false,
            text: "first prompt".into(),
            attachment: None,
        });
        panel.select("second".into());
        panel.field.set_text("second draft");
        let key = TurnKey {
            session: "first".into(),
            turn: "turn".into(),
        };
        panel.event(Event::TurnStarted(None, key.clone()));
        panel.event(Event::TurnEnded(key.clone(), TurnStatus::Completed));
        panel.event(Event::TurnStarted(Some(9), key));
        assert_eq!(panel.field.text(), "second draft");
        panel.select("first".into());
        assert!(panel.field.text().is_empty());
        assert_eq!(panel.current().unwrap().status, Some(TurnStatus::Completed));
        assert!(!panel.busy());
    }

    #[test]
    fn catalog_default_comes_from_the_server() {
        let mut panel = Panel::new();
        panel.catalog.models = vec![Model {
            id: "available".into(),
            name: String::new(),
            efforts: vec!["medium".into()],
            default_effort: Some("medium".into()),
            is_default: true,
        }];
        panel.default_model = Some("unavailable".into());
        assert_eq!(panel.model().unwrap().id, "available");
        assert_eq!(panel.effort(), Some("medium"));
    }
}
