//! Provider commands and events used by the editor. The client owns the provider process.

mod acp;
mod codex;
mod protocol;

pub use codex::CodexProvider;
pub use winc_config::PermissionMode;

use std::path::PathBuf;

/// Agent backend selectable in the editor.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Backend {
    /// OpenAI Codex App Server.
    #[default]
    Codex,
    /// Claude Agent SDK through the ACP adapter.
    Claude,
    /// Gemini CLI in ACP mode.
    Gemini,
    /// Antigravity CLI in ACP mode.
    Antigravity,
}

impl Backend {
    /// Display name used by the backend picker.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::Antigravity => "Antigravity",
        }
    }

    /// Stable identifier used to persist per-backend preferences.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
        }
    }

    /// Starts the selected backend on a supervised connection.
    pub fn connect(
        self,
        root: PathBuf,
        standalone: bool,
        codex_executable: Option<PathBuf>,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Box<dyn AgentProvider>> {
        codex::connect(self, root, standalone, codex_executable, wake)
    }
}

/// One account quota window.
#[derive(Clone, Debug)]
pub struct RateLimitWindow {
    /// Window duration in minutes.
    pub minutes: u64,
    /// Remaining percentage.
    pub remaining: f64,
    /// Next reset, as Unix seconds.
    pub resets_at: Option<u64>,
}

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
    /// Read account usage limits.
    RateLimits,
    /// Create a session in the project's root.
    NewSession,
    /// Load a saved session and its recent turns.
    Resume(String),
    /// Load the next (older) page of turns in a resumed session.
    Older {
        /// Loaded session ID.
        session: String,
        /// Cursor provided by the App Server.
        cursor: String,
    },
    /// List historical sessions scoped to the project, with an optional page cursor.
    History(Option<String>),
    /// Discover models and supported reasoning efforts, with an optional page cursor.
    Models(Option<String>),
    /// Discover available skills for the current workspace (Codex).
    Skills,
    /// Start a turn in a loaded session.
    Send {
        /// Loaded session ID.
        session: String,
        /// User prompt with optional editor context.
        text: String,
        /// Optional clipboard image, encoded as PNG.
        image: Option<std::sync::Arc<[u8]>>,
        /// Model ID from discovery.
        model: Option<String>,
        /// Reasoning effort from the selected model.
        effort: Option<String>,
        /// Permissions for this turn.
        permissions: PermissionMode,
    },
    /// Add guidance to the identified active turn.
    Steer {
        /// Active turn precondition.
        turn: TurnKey,
        /// Guidance and optional editor context.
        text: String,
        /// Optional clipboard image, encoded as PNG.
        image: Option<std::sync::Arc<[u8]>>,
    },
    /// Interrupt a specific session's active turn.
    Stop(TurnKey),
    /// Answer one pending approval.
    Approve {
        /// Local handle to the server request.
        approval: u64,
        /// Explicit user decision.
        decision: ApprovalDecision,
    },
    /// Set a session-scoped execution/permission mode.
    SetMode {
        /// Loaded session ID.
        session: String,
        /// Mode ID advertised by the provider.
        mode: String,
    },
    /// Set a session-scoped model ID.
    SetModel {
        /// Loaded session ID.
        session: String,
        /// Model ID advertised by the provider.
        model: String,
    },
    /// Set a session-scoped reasoning effort / thinking level.
    SetEffort {
        /// Loaded session ID.
        session: String,
        /// Reasoning effort value advertised by the provider.
        effort: String,
    },
    /// Answer one pending structured question / elicitation prompt.
    AnswerElicitation {
        /// Local handle to the server request.
        elicitation: u64,
        /// Explicit user response.
        response: ElicitationResponse,
    },
}

/// A session execution or permission mode advertised by the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeOption {
    /// Mode identifier passed back to the provider.
    pub id: String,
    /// Human-readable mode label.
    pub name: String,
    /// Short description of the mode's behavior.
    pub description: String,
}

/// A slash command or skill advertised by the provider for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    /// Command name without the leading `/`.
    pub name: String,
    /// Human-readable description shown in completion menus.
    pub description: String,
    /// Optional argument hint when the command accepts input.
    pub input_hint: Option<String>,
}

/// One selectable option in a structured prompt field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElicitationOption {
    /// Wire value returned when this option is chosen.
    pub value: String,
    /// Human-readable label shown to the user.
    pub label: String,
    /// Optional description shown beneath the label.
    pub description: String,
}

/// Input control type for one field in an elicitation form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElicitationFieldKind {
    /// Choose one option from the list.
    SingleSelect(Vec<ElicitationOption>),
    /// Choose zero or more options from the list.
    MultiSelect(Vec<ElicitationOption>),
    /// True/false toggle.
    Boolean,
    /// Free-form text input.
    Text,
    /// Numeric input.
    Number {
        /// Whether only whole integers are accepted.
        integer: bool,
    },
}

/// One field in an agent-initiated structured question form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElicitationField {
    /// Property key in the response object.
    pub name: String,
    /// Human-readable question or field title.
    pub title: String,
    /// Optional helper description.
    pub description: String,
    /// Whether this field is required by the schema.
    pub required: bool,
    /// Control type and options.
    pub kind: ElicitationFieldKind,
    /// Optional default text value.
    pub default_text: Option<String>,
    /// Optional default selected option values.
    pub default_values: Vec<String>,
    /// Optional default boolean value.
    pub default_bool: Option<bool>,
}

/// A structured question or selection prompt awaiting user input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Elicitation {
    /// Local handle for answering the request.
    pub id: u64,
    /// Session that originated this request.
    pub session: String,
    /// Prompt heading or explanation from the agent.
    pub message: String,
    /// Ordered form fields to display.
    pub fields: Vec<ElicitationField>,
}

/// One field value submitted in response to an elicitation form.
#[derive(Clone, Debug, PartialEq)]
pub enum ElicitationValue {
    /// Text or single-select value.
    String(String),
    /// Multi-select values.
    Strings(Vec<String>),
    /// Boolean value.
    Bool(bool),
    /// Integer value.
    Integer(i64),
    /// Floating-point number value.
    Number(f64),
}

/// User response to a pending elicitation request.
#[derive(Clone, Debug, PartialEq)]
pub enum ElicitationResponse {
    /// Accept the prompt with field values.
    Accept(Vec<(String, ElicitationValue)>),
    /// Decline the prompt without cancelling the whole turn.
    Decline,
    /// Cancel/dismiss the prompt.
    Cancel,
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug)]
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
    /// Complete line counts from the agent's patch, in file order.
    pub file_stats: Vec<Option<LineChanges>>,
}

/// Counts for one agent-reported patch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineChanges {
    /// Added lines.
    pub added: usize,
    /// Removed lines.
    pub removed: usize,
}

/// Message or tool-card presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A user message.
    User,
    /// An agent message.
    Message,
    /// An interim agent message.
    Commentary,
    /// A shell command.
    Command,
    /// A command reported as reading files.
    Read,
    /// A command reported as searching or listing files.
    Search,
    /// A test command recognized from its executable and arguments.
    Test,
    /// A sub-agent operation.
    Subagent,
    /// Current plan and step status.
    Plan,
    /// A turn failure reported by the server.
    Error,
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
    /// Decisions supported by the server for this request.
    pub decisions: Vec<ApprovalDecision>,
}

/// Interactive Codex approval decisions handled by Winc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Approve the displayed operation.
    Accept,
    /// Approve with session scope.
    AcceptForSession,
    /// Reject while allowing the turn to continue.
    Decline,
    /// Reject and interrupt the turn.
    Cancel,
}

impl ApprovalDecision {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptForSession => "acceptForSession",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }

    pub(crate) fn grants(self) -> bool {
        matches!(self, Self::Accept | Self::AcceptForSession)
    }
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
    /// Markdown body of an interim message.
    Commentary,
    /// Proposed plan text.
    Plan,
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
    /// Account quota windows, as advertised by Codex.
    RateLimits(Vec<RateLimitWindow>),
    /// Whether the backend accepts images in prompts.
    ImageInput(bool),
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
    SessionOpened(u64, Session, Vec<Turn>, Option<String>),
    /// Older turns in chronological order and their next cursor.
    Older(u64, String, Vec<Turn>, Option<String>),
    /// Response to a historical session listing.
    History(u64, Vec<Session>, Option<String>),
    /// Response to a model listing.
    Models(u64, Vec<Model>, Option<String>),
    /// Response to a Codex skills listing.
    Skills(u64, Vec<SlashCommand>),
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
    /// Approval waited past the local review deadline.
    ApprovalExpired(u64),
    /// A loaded session has a new title.
    SessionTitle(String, Option<String>),
    /// An approval is no longer waiting for a decision.
    ApprovalResolved(u64),
    /// Guidance accepted for the active turn.
    Steered(u64, String),
    /// Successfully accepted an acknowledgement-only operation.
    Acknowledged(u64),
    /// Cumulative and most recent turn total token counts.
    Tokens(TurnKey, u64, u64, Option<u64>),
    /// Account plan or tier label reported by the provider.
    AccountTier(Option<String>),
    /// Cumulative session cost reported by the provider.
    SessionCost(String, f64, String),
    /// Session-scoped execution/permission modes and active mode ID.
    SessionModes(String, Vec<ModeOption>, Option<String>),
    /// Session-scoped slash commands and skills advertised by the provider.
    SessionCommands(String, Vec<SlashCommand>),
    /// Session-scoped model catalog and active model/effort selection.
    SessionModels {
        /// Session ID.
        session: String,
        /// Available models.
        models: Vec<Model>,
        /// Active model ID.
        current_model: Option<String>,
        /// Active reasoning effort / thinking level.
        current_effort: Option<String>,
    },
    /// A structured question/selection prompt is waiting for user input.
    Elicitation(Elicitation),
    /// A pending elicitation was resolved.
    ElicitationResolved(u64),
    /// A pending elicitation expired past the local deadline.
    ElicitationExpired(u64),
}
