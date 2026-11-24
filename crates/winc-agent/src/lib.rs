//! Provider commands and events used by the editor. The client owns the provider process.

mod codex;
mod protocol;

pub use codex::CodexProvider;

use std::path::PathBuf;

/// Maximum UTF-8 size of a single prompt, including attached editor context.
pub const MAX_PROMPT: usize = 256 * 1024;

/// Takes at most the requested number of UTF-8 bytes without splitting a character.
pub fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A workspace-scoped connection to an agent. All calls return immediately.
pub trait AgentProvider {
    /// Queues an operation, returning its local correlation ID.
    ///
    /// # Errors
    /// The connection is closed, its queue is full, or the input is invalid.
    fn submit(&mut self, command: Command) -> Result<u64, SubmitError>;

    /// Takes one batch of queued events in protocol order.
    fn drain(&mut self) -> Vec<Event>;
}

/// A command rejected before it was queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The connection no longer accepts commands.
    Disconnected,
    /// There are too many commands in flight.
    Busy,
    /// The prompt is too large.
    PromptTooLarge,
}

/// Agent operations supported by Winc.
#[derive(Debug)]
pub enum Command {
    /// Create a session in the project's root.
    NewSession,
    /// Load a saved session and its recent turns.
    Resume(String),
    /// List historical sessions scoped to the project, with an optional page cursor.
    History(Option<String>),
    /// Discover models and supported reasoning efforts, with an optional page cursor.
    Models(Option<String>),
    /// Start a turn in a loaded session.
    Send {
        /// Loaded session ID.
        session: String,
        /// User prompt with optional editor context.
        text: String,
        /// Model ID from discovery.
        model: Option<String>,
        /// Reasoning effort from the selected model.
        effort: Option<String>,
    },
    /// Interrupt a specific session's active turn.
    Stop(TurnKey),
    /// Answer one pending approval.
    Approve {
        /// Local handle to the server request.
        approval: u64,
        /// Whether the user approved the displayed operation.
        accept: bool,
    },
}

/// Agent session metadata.
#[derive(Clone, Debug)]
pub struct Session {
    /// Provider identifier.
    pub id: String,
    /// Project the session belongs to.
    pub cwd: PathBuf,
    /// Session name or preview.
    pub title: String,
    /// Model stored with the session, when available.
    pub model: Option<String>,
    /// Reasoning effort stored with the session, when available.
    pub effort: Option<String>,
}

/// A model advertised by the provider.
#[derive(Debug)]
pub struct Model {
    /// Identifier passed to a turn.
    pub id: String,
    /// Model name shown to users.
    pub name: String,
    /// Selectable reasoning efforts.
    pub efforts: Vec<String>,
    /// Default effort, if advertised.
    pub default_effort: Option<String>,
    /// Whether this is the provider's default model.
    pub is_default: bool,
}

/// A turn is identified by both IDs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TurnKey {
    /// Session ID.
    pub session: String,
    /// Turn ID.
    pub turn: String,
}

/// Current or final turn state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStatus {
    /// The turn is in progress.
    Running,
    /// The turn completed.
    Completed,
    /// The turn was interrupted.
    Interrupted,
    /// The turn failed.
    Failed,
}

/// The turn and its persisted items.
#[derive(Debug)]
pub struct Turn {
    /// Turn identity.
    pub key: TurnKey,
    /// Turn state.
    pub status: TurnStatus,
    /// Items included in the response, in conversation order.
    pub items: Vec<Item>,
}

/// A message or operation reported by the provider.
#[derive(Debug)]
pub struct Item {
    /// Stable item ID within the turn.
    pub id: String,
    /// Kind of item.
    pub kind: ItemKind,
    /// Message, command, or tool name.
    pub text: String,
    /// Captured tool output, bounded in size.
    pub output: String,
    /// Operation status reported by the provider.
    pub status: String,
    /// Paths explicitly reported by a file-change item.
    pub files: Vec<PathBuf>,
}

/// Message or tool-card presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A user message.
    User,
    /// An agent message.
    Message,
    /// A shell command.
    Command,
    /// A file change.
    FileChange,
    /// A provider tool call.
    Tool,
}

/// An operation requiring explicit user review.
#[derive(Debug)]
pub struct Approval {
    /// Local handle.
    pub id: u64,
    /// Turn that requested the operation.
    pub turn: TurnKey,
    /// Associated item ID.
    pub item: String,
    /// Review category.
    pub kind: ApprovalKind,
    /// Operation details displayed for review.
    pub details: String,
    /// Whether the complete requested operation is available for review.
    pub reviewable: bool,
}

/// Approval categories supported by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Execute a command or send input to a running command.
    Command,
    /// Write files.
    Files,
    /// Grant additional permissions for the turn.
    Permissions,
}

/// Target of a streamed text update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    /// Markdown message body.
    Message,
    /// Tool output.
    Output,
}

/// Connection failures with safe diagnostics. Raw server error bodies stay out of logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The executable could not be found.
    ExecutableNotFound,
    /// Unable to start the executable or a transport thread.
    Start,
    /// The workspace must be an existing directory with a UTF-8 path.
    Workspace,
    /// The server closed its connection.
    Exited,
    /// Malformed or excessively large protocol data.
    Protocol,
    /// The request expired.
    Timeout,
    /// The server rejected an operation, with a JSON-RPC error code.
    Rejected(i64),
}

/// Events delivered in wire order, with adjacent text deltas batched.
#[derive(Debug)]
pub enum Event {
    /// The handshake completed and the account was checked.
    Ready {
        /// Whether Codex requires authentication and has no active account.
        login_required: bool,
    },
    /// The process or connection ended.
    Disconnected(Failure),
    /// An operation failed and may be retried by the caller.
    RequestFailed(u64, Failure),
    /// Response to a new or resumed session.
    SessionOpened(u64, Session, Vec<Turn>),
    /// Response to a historical session listing.
    History(u64, Vec<Session>, Option<String>),
    /// Response to a model listing.
    Models(u64, Vec<Model>, Option<String>),
    /// A turn started, with a correlation ID for client-initiated sends when available.
    TurnStarted(Option<u64>, TurnKey),
    /// A turn reached a final state.
    TurnEnded(TurnKey, TurnStatus),
    /// A message or tool operation started or completed.
    Item(TurnKey, Item, bool),
    /// Text deltas batched by item and turn.
    Delta(TurnKey, String, DeltaKind, String),
    /// A file-change item published a patch and paths before completion.
    Files(TurnKey, String, Vec<PathBuf>, String),
    /// A request is waiting for a user decision.
    Approval(Approval),
    /// An approval is no longer waiting for a decision.
    ApprovalResolved(u64),
    /// Successfully accepted an acknowledgement-only operation.
    Acknowledged(u64),
    /// Cumulative and most recent turn total token counts.
    Tokens(TurnKey, u64, u64),
}
