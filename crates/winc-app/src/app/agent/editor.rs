//! Editor context and local file navigation for the Codex panel.

use std::path::{Component, Path};

use super::*;

const MAX_CHANGES: usize = 256;

pub(crate) struct Attachment {
    path: String,
    first: usize,
    last: usize,
    text: String,
    dirty: bool,
}

impl Attachment {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(super) fn append_to(&self, prompt: &mut String) {
        use std::fmt::Write;
        let source = if self.dirty {
            "unsaved editor buffer"
        } else {
            "editor buffer"
        };
        let _ = write!(
            prompt,
            "\n\nEditor context: {}:{}–{} ({source}, {} UTF-8 bytes)\n{}",
            self.path,
            self.first,
            self.last,
            self.text.len(),
            self.text,
        );
    }
}

/// A project-local path reference in an agent response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    path: PathBuf,
    line: Option<usize>,
    column: Option<usize>,
}

impl Link {
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            line: None,
            column: None,
        }
    }

    pub(crate) fn parse(raw: &str, explicit: bool) -> Option<Self> {
        let raw = raw.trim_matches(|c: char| c.is_whitespace() || "\u{60}[]()<>,;\"'".contains(c));
        let raw = if let Some(url) = raw.strip_prefix("file://") {
            if !url.starts_with('/') {
                return None;
            }
            url
        } else {
            raw
        };
        if raw.is_empty() || raw.contains("://") || raw.starts_with('#') || raw.len() > 4096 {
            return None;
        }
        let (raw, anchor) = raw.split_once('#').map_or((raw, None), |(path, anchor)| {
            let line = anchor
                .strip_prefix('L')
                .and_then(|anchor| anchor.split('-').next())
                .and_then(|line| line.parse::<usize>().ok());
            (path, line)
        });
        let numeric_suffix = |value: &str| {
            let (left, right) = value.rsplit_once(':')?;
            Some((left.len(), right.parse::<usize>().ok()?.checked_sub(1)?))
        };
        let (mut path, mut line, mut column) = (raw, anchor.and_then(|at| at.checked_sub(1)), None);
        if line.is_none()
            && let Some((len, at)) = numeric_suffix(raw)
        {
            path = &raw[..len];
            line = Some(at);
            if let Some((len, at)) = numeric_suffix(path) {
                path = &path[..len];
                column = line;
                line = Some(at);
            }
        }
        if !explicit && line.is_none() && !path.contains(['/', '\\']) {
            return None;
        }
        let decoded = crate::app::preview::markdown::percent_decode(path);
        let path = PathBuf::from(decoded.trim_end_matches('.'));
        (!path.as_os_str().is_empty()).then_some(Self { path, line, column })
    }
}

fn project_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let full = root.join(path);
    let relative = full.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|c| c == Component::ParentDir)
        || winc_backend::is_remote(&full)
    {
        return None;
    }
    Some(full)
}

fn attachment(doc: &winc_editor::Document, root: &Path, selection: bool) -> Option<Attachment> {
    let path = doc
        .path
        .as_ref()?
        .strip_prefix(root)
        .ok()?
        .to_str()?
        .to_owned();
    let rope = doc.text().rope()?;
    let (start, end) = if selection {
        (doc.selection.from(), doc.selection.to())
    } else {
        (0, rope.len_chars())
    };
    if start == end || rope.char_to_byte(end) - rope.char_to_byte(start) >= MAX_BODY {
        return None;
    }
    Some(Attachment {
        path,
        first: rope.char_to_line(start) + 1,
        last: rope.char_to_line(end - 1) + 1,
        text: rope.slice(start..end).to_string(),
        dirty: doc.is_dirty(),
    })
}

#[derive(Default)]
pub(crate) struct Change {
    pub(crate) reported: bool,
    pub(crate) stat: Option<LineChanges>,
}

impl Conversation {
    pub(super) fn note_change(
        &mut self,
        turn: &TurnKey,
        path: PathBuf,
        reported: bool,
        stat: Option<LineChanges>,
    ) {
        if self.change_turn.as_ref() != Some(turn) {
            self.change_turn = Some(turn.clone());
            self.changes.clear();
        }
        if let Some(current) = self.changes.get_mut(&path) {
            current.reported |= reported;
            if let Some(stat) = stat {
                let total = current.stat.get_or_insert_default();
                total.added += stat.added;
                total.removed += stat.removed;
            }
        } else if self.changes.len() < MAX_CHANGES {
            self.changes.insert(path, Change { reported, stat });
        }
    }
}

impl App {
    pub(in crate::app) fn agent_attach(&mut self, selection: bool) {
        let attached = self
            .editor
            .workspace
            .as_ref()
            .filter(|ws| !winc_backend::is_remote(&ws.root))
            .and_then(|ws| {
                self.editor
                    .active()
                    .and_then(|doc| attachment(doc, &ws.root, selection))
            });
        let Some(attached) = attached else {
            self.flash(t!("agent-no-context"));
            return;
        };
        if !self.agent.as_ref().is_some_and(|panel| panel.shown) {
            self.toggle_agent();
        }
        if self.agent_project_matches()
            && let Some(panel) = &mut self.agent
        {
            panel.attachment = Some(std::rc::Rc::new(attached));
            self.focus = Focus::Agent;
            self.request_redraw();
        }
    }

    pub(in crate::app) fn agent_workspace_changes(&mut self, paths: &[PathBuf]) {
        let Some(panel) = &mut self.agent else { return };
        let Some(root) = &panel.root else { return };
        if !panel.any_active() {
            return;
        }
        let paths: Vec<_> = paths
            .iter()
            .filter_map(|path| project_path(root, path))
            .collect();
        for session in panel.conversations.values_mut() {
            if let Some(turn) = session.running.clone() {
                for path in &paths {
                    if path
                        .strip_prefix(root)
                        .is_ok_and(|name| !name.starts_with(".git"))
                    {
                        session.note_change(&turn, path.clone(), false, None);
                    }
                }
            }
        }
    }

    pub(super) fn agent_open_link(&mut self, link: Link, diff: bool) {
        let Some(root) = self.agent.as_ref().and_then(|panel| panel.root.clone()) else {
            return;
        };
        let Some(path) = project_path(&root, &link.path) else {
            self.flash(t!("agent-invalid-path"));
            return;
        };
        let Some(proxy) = self.link.proxy.clone() else {
            self.finish_agent_link(root, secure_path(path), link, diff);
            return;
        };
        winc_editor::background::spawn_blocking(move || {
            let _ = proxy.send_event(UserEvent::AgentPath(root, secure_path(path), link, diff));
        });
    }

    pub(in crate::app) fn finish_agent_link(
        &mut self,
        root: PathBuf,
        real: Option<PathBuf>,
        link: Link,
        diff: bool,
    ) {
        let matches_agent = self
            .agent
            .as_ref()
            .is_some_and(|panel| panel.root.as_deref() == Some(root.as_path()))
            || self
                .agent_peer
                .as_ref()
                .is_some_and(|panel| panel.root.as_deref() == Some(root.as_path()));
        if !matches_agent && self.editor.workspace.as_ref().map(|ws| &ws.root) != Some(&root) {
            self.flash(t!("agent-project-changed"));
            return;
        }
        let Some(path) = real.filter(|p| project_path(&root, p).is_some()) else {
            self.flash(t!("agent-invalid-path"));
            return;
        };
        if diff {
            self.fetch_changes(&path, super::super::git::Target::Open);
        } else {
            self.open_path_then(&path, move |app| {
                if let Some(line) = link.line {
                    app.go_to(line, link.column.unwrap_or(0), 0);
                }
            });
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "agent links are restricted to the local project"
)]
fn secure_path(path: PathBuf) -> Option<PathBuf> {
    if let Ok(real) = winc_config::files::canonicalize(&path) {
        return (!real.is_dir()).then_some(real);
    }
    // A removed file still has a Git diff. Resolve the existing parent.
    let parent = winc_config::files::canonicalize(path.parent()?).ok()?;
    if path.symlink_metadata().is_ok() {
        return None;
    }
    Some(parent.join(path.file_name()?))
}

pub(super) fn reported_changes(root: &Path, item: &Item) -> Vec<(PathBuf, Option<LineChanges>)> {
    item.files
        .iter()
        .enumerate()
        .filter_map(|(i, path)| {
            Some((
                project_path(root, path)?,
                item.file_stats.get(i).copied().flatten(),
            ))
        })
        .collect()
}

pub(super) fn reported_files(root: Option<&Path>, files: &[PathBuf]) -> Vec<PathBuf> {
    root.map_or_else(Vec::new, |root| {
        files
            .iter()
            .filter_map(|path| project_path(root, path))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_references_and_rejects_remote_urls_and_traversal() {
        let link = Link::parse("src/lib.rs:15:3", false).unwrap();
        assert_eq!(link.path, Path::new("src/lib.rs"));
        assert_eq!((link.line, link.column), (Some(14), Some(2)));
        let link = Link::parse("file:///winc/src/a%20b.rs#L23-L25", true).unwrap();
        assert_eq!(link.path, Path::new("/winc/src/a b.rs"));
        assert_eq!(link.line, Some(22));
        assert!(Link::parse("https://example.org/src/lib.rs", true).is_none());
        assert!(Link::parse("file://host/winc/a", true).is_none());
        assert!(project_path(Path::new("/winc"), Path::new("../escape")).is_none());
    }
}
