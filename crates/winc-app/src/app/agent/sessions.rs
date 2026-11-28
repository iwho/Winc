//! Project history, model choices, and draft isolation across loaded Codex threads.

use winc_agent::Model;

use super::*;

pub(super) fn prompt_title(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(winc_agent::prefix(line, 512).to_owned())
}

const MAX_HISTORY: usize = 250;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    #[default]
    Chat,
    Changes,
    History,
    Models,
    Backends,
}

#[derive(Default)]
pub(super) struct Catalog {
    pub(super) history: Vec<Session>,
    pub(super) models: Vec<Model>,
    pub(super) skills: Vec<SlashCommand>,
    history_cursor: Option<String>,
    model_cursor: Option<String>,
    history_request: Option<u64>,
    history_reset: bool,
    model_request: Option<u64>,
    pub(super) skills_request: Option<u64>,
}

impl Panel {
    pub(crate) fn history(&self) -> &[Session] {
        &self.catalog.history
    }

    pub(crate) fn models(&self) -> &[Model] {
        &self.catalog.models
    }

    pub(crate) fn skills(&self) -> &[SlashCommand] {
        &self.catalog.skills
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
                .elicitations
                .iter()
                .any(|draft| draft.request.session == id)
            || self
                .conversations
                .get(id)
                .is_some_and(|session| session.running.is_some())
    }

    pub(crate) fn token_summary(&self) -> Option<(String, Option<f32>)> {
        let (total, last, window) = self.current()?.tokens;
        let window = window.filter(|&window| window > 0);
        if total == 0 && window.is_none() {
            return None;
        }
        let compact = |value: u64| {
            if value >= 1_000_000 {
                format!("{:.1}M", value as f64 / 1_000_000.0)
            } else if value > 1000 {
                format!("{:.1}k", value as f64 / 1000.0)
            } else {
                value.to_string()
            }
        };
        let last_label = compact(last);
        let window_label = window.map(compact);
        let label = match window_label.as_deref() {
            Some(window) => t!(
                "agent-tokens-window",
                current = last_label.as_str(),
                window = window
            ),
            None => t!("agent-tokens", current = last_label.as_str()),
        };
        // Total is cumulative across requests. The latest request reflects context usage.
        let fraction = window.map(|window| (last as f64 / window as f64).min(1.0) as f32);
        Some((label, fraction))
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
        self.refresh_history();
        if self.backend == Backend::Codex {
            if self.catalog.model_request.is_none() {
                self.catalog.model_request = self.submit(Command::Models(None));
            }
            if self.catalog.skills_request.is_none() {
                self.catalog.skills_request = self.submit(Command::Skills);
            }
        }
    }

    pub(super) fn refresh_history(&mut self) {
        if self.catalog.history_request.is_none()
            && let Some(id) = self.submit(Command::History(None))
        {
            self.catalog.history_request = Some(id);
            self.catalog.history_reset = true;
        }
    }

    pub(super) fn next_page(&mut self) {
        match self.tab {
            Tab::History if self.catalog.history_request.is_none() => {
                if let Some(cursor) = self.catalog.history_cursor.clone()
                    && let Some(id) = self.submit(Command::History(Some(cursor)))
                {
                    self.catalog.history_request = Some(id);
                    self.catalog.history_reset = false;
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
        let reset = std::mem::take(&mut self.catalog.history_reset);
        if reset {
            let mut merged = Vec::new();
            for existing in &self.catalog.history {
                if self.conversations.contains_key(&existing.id)
                    && !sessions.iter().any(|entry| entry.id == existing.id)
                {
                    merged.push(existing.clone());
                }
            }
            self.catalog.history = merged;
        }
        for mut session in sessions {
            if session.title.trim().is_empty()
                && let Some(known) = self.conversations.get(&session.id)
                && !known.session.title.trim().is_empty()
            {
                session.title.clone_from(&known.session.title);
            } else if !session.title.trim().is_empty()
                && let Some(known) = self.conversations.get_mut(&session.id)
                && known.session.title.trim().is_empty()
            {
                known.session.title.clone_from(&session.title);
            }
            if let Some(existing) = self
                .catalog
                .history
                .iter_mut()
                .find(|entry| entry.id == session.id)
            {
                if existing.title.trim().is_empty() && !session.title.trim().is_empty() {
                    existing.title = session.title;
                }
                continue;
            }
            if self.catalog.history.len() == MAX_HISTORY {
                break;
            }
            self.catalog.history.push(session);
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

    pub(super) fn skills_received(&mut self, request: u64, skills: Vec<SlashCommand>) {
        if self.catalog.skills_request != Some(request) {
            return;
        }
        self.catalog.skills_request = None;
        self.catalog.skills = skills;
    }

    pub(super) fn catalog_failed(&mut self, id: u64) {
        if self.catalog.history_request == Some(id) {
            self.catalog.history_request = None;
            self.catalog.history_reset = false;
        }
        if self.catalog.model_request == Some(id) {
            self.catalog.model_request = None;
        }
        if self.catalog.skills_request == Some(id) {
            self.catalog.skills_request = None;
        }
    }

    pub(super) fn catalog_reset_pending(&mut self) {
        self.catalog.history_request = None;
        self.catalog.history_reset = false;
        self.catalog.model_request = None;
        self.catalog.skills_request = None;
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
            self.opening = Some(Opening {
                request,
                send: false,
                is_new: false,
            });
        }
    }

    pub(super) fn choose_model(&mut self, id: String) -> (bool, Option<String>) {
        let already_selected = self.model().is_some_and(|model| model.id == id);
        let effort = if already_selected {
            self.effort().map(str::to_owned)
        } else {
            self.models()
                .iter()
                .find(|m| m.id == id)
                .and_then(|m| m.default_effort.clone())
        };
        if let Some(session) = self.current_mut() {
            session.model = Some(id.clone());
            session.effort = effort.clone();
            session.pending_restore.model = None;
            session.pending_restore.effort = None;
            session.pending_restore.awaiting_model = None;
            session.pending_restore.model_request = None;
        }
        self.default_model = Some(id.clone());
        self.default_effort = effort.clone();
        self.saved_preferences.model = Some(id.clone());
        if !already_selected {
            self.saved_preferences.effort = effort.clone();
        }
        if self.backend != Backend::Codex
            && let Some(session) = self.selected.clone()
        {
            self.submit(Command::SetModel { session, model: id });
        }
        self.show_tab(Tab::Chat);
        (!already_selected, effort)
    }

    pub(super) fn choose_effort(&mut self, effort: String) {
        if let Some(session) = self.current_mut() {
            session.effort = Some(effort.clone());
            session.pending_restore.effort = None;
        }
        self.default_effort = Some(effort.clone());
        self.saved_preferences.effort = Some(effort.clone());
        if self.backend != Backend::Codex
            && let Some(session) = self.selected.clone()
        {
            self.submit(Command::SetEffort { session, effort });
        }
        self.show_tab(Tab::Chat);
    }

    pub(super) fn choose_mode(&mut self, mode: String) {
        if let Some(session) = self.current_mut() {
            session.mode = Some(mode.clone());
            session.pending_restore.mode = None;
        }
        self.saved_preferences.mode = Some(mode.clone());
        if let Some(session) = self.selected.clone() {
            self.submit(Command::SetMode { session, mode });
        }
    }

    pub(super) fn seed_title(&mut self, id: &str, prompt: &str) {
        if self
            .conversations
            .get(id)
            .is_some_and(|session| session.session.title.trim().is_empty())
        {
            self.title_received(id, prompt_title(prompt));
        }
    }

    pub(super) fn title_received(&mut self, id: &str, title: Option<String>) {
        let Some(title) = title.filter(|title| !title.trim().is_empty()) else {
            return;
        };
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
                session.flush_stream();
                session.transcript.reset_follow_state();
                session.draft = std::mem::replace(&mut self.field, TextField::multiline());
                session.attachment = self.attachment.take();
                session.image = self.image.take();
            }
            if let Some(session) = self.conversations.get_mut(&id) {
                // The first submitted prompt starts in the unattached composer.
                if had_selection || self.field.text().is_empty() {
                    std::mem::swap(&mut self.field, &mut session.draft);
                }
                if had_selection || session.image.is_some() {
                    self.image = session.image.take();
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
        if tab != Tab::Chat {
            self.flush_all_streams();
        }
        if tab == Tab::History && self.tab != Tab::History && self.connection == Connection::Ready {
            self.refresh_history();
        }
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
            queued: false,
            id: 9,
            steer: false,
            text: "first prompt".into(),
            attachment: None,
            image: None,
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
