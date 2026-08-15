use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::sync::{broadcast, mpsc};

use crate::attachment::{PendingAttachment, build_attachments_json, cleanup_attachment_temps};
use crate::chat_render_overlay::{render_session_list_overlay, session_list_overlay_area};
use crate::client::{
    ApprovalDecision, RpcClient, RpcNotification, SessionEntry, SessionUpdate, TurnEndOutcome,
    method, parse_session_update,
};
use crate::diff;
use crate::file_explorer::{ExplorerAction, FileExplorerState};
use crate::input_bar::{InputBarAction, InputBarState};
use crate::jsonrpc::RpcOutbound;
use crate::mouse;
use crate::theme;
use crate::turn_status::TurnStatus;

// Height of the approval popup anchored to the bottom of the content area.
// Used both in render_approval_overlay and to pad diffs so they aren't covered.
const APPROVAL_OVERLAY_HEIGHT: u16 = 7;

mod input;
mod markdown;
mod render;
pub(crate) use markdown::markdown_to_lines;
pub(crate) use render::{
    borrow_line, centered_copy_feedback_rect, centered_message_copy_rect, chord_label, copy_region,
    fenced_text, header_fence_lang, label_cells, message_copied_label, message_copy_label,
    model_picker_overlay_area, queue_sidebar_help_entries, render, render_entry_into,
    truncate_utf8, wrapped_rows,
};
#[cfg(test)]
pub(crate) use render::{
    carve_todo_area, render_approval_overlay, render_queue_sidebar, render_transcript_copy_overlay,
};

/// How often the cwd line re-polls the daemon for the current git branch.
const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const CANCEL_WATCHDOG: Duration = Duration::from_secs(30);
const COPY_FEEDBACK_TTL: Duration = Duration::from_secs(1);

// ── Chat pane (tab mode) ─────────────────────────────────────────

enum ChatPhase {
    /// Showing agent picker (or loading the list).
    PickAgent {
        agents: Vec<String>,
        list_state: ListState,
        loading: bool,
    },
    /// Showing saved Code sessions before any new session has been created.
    PickSession {
        sessions: Vec<SessionEntry>,
        list_state: ListState,
        agents: Vec<String>,
    },
    /// WSS only: user picks the remote working directory before session starts.
    PickCwd {
        /// The agent alias already chosen.
        agent_alias: String,
        /// Interactive directory picker.
        explorer: FileExplorerState,
    },
    /// Active chat session.
    Active(Box<ChatState>),
    /// Unrecoverable error.
    Error(String),
}

/// Distinguishes which kind of chat pane this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneKind {
    Chat,
    Acp,
}

impl PaneKind {
    /// Short name for this pane (no padding — callers format as needed).
    pub(crate) fn name(self) -> String {
        crate::i18n::t(self.fluent_key())
    }

    /// Stable Fluent key for this pane's display name.
    pub(crate) fn fluent_key(self) -> &'static str {
        match self {
            PaneKind::Chat => "zc-chat-pane-chat",
            PaneKind::Acp => "zc-chat-pane-acp",
        }
    }
}

pub(crate) struct Chat {
    rpc: Arc<RpcClient>,
    rpc_out: Arc<RpcOutbound>,
    notif_rx: broadcast::Receiver<RpcNotification>,
    inbound_rx: broadcast::Receiver<crate::client::RpcInboundRequest>,
    /// Background-fetched git status updates: (session_id, branch, hash).
    git_branch_tx: mpsc::Sender<GitStatusUpdate>,
    git_branch_rx: mpsc::Receiver<GitStatusUpdate>,
    /// In-flight git_branch refresh; gates repeat fetches until result arrives.
    git_branch_inflight: bool,
    /// Background model-catalog fetch result, routed back so the Loading
    /// picker can swap to the populated list without blocking the draw loop.
    model_fetch_tx: mpsc::Sender<ModelFetchResult>,
    model_fetch_rx: mpsc::Receiver<ModelFetchResult>,
    phase: ChatPhase,
    pane_kind: PaneKind,
    /// One-shot session id to reattach to on the next session start, set by
    /// the app layer across a reconnect so the rebuilt pane resumes the
    /// pre-disconnect session (the daemon retains it) instead of
    /// minting a fresh one. Cleared once consumed by `start_session`.
    resume_session_id: Option<String>,
    /// The agent the resumed session belongs to. A multi-agent reconnect must
    /// reattach to this agent automatically; the resume id is only dropped when
    /// the user manually picks a different agent.
    resume_agent_alias: Option<String>,
    /// List rect of the agent picker, recorded each draw so mouse clicks in the
    /// PickAgent phase can map a row to a selection. Default until first draw.
    pick_agent_list_area: Rect,
    /// Double-click tracker for the agent picker: a second click on the same row
    /// confirms (enters the session), matching the keyboard Enter.
    pick_agent_double_click: crate::mouse::DoubleClickTracker,
    /// Double-click tracker for the session picker: a second click on the same row
    /// resumes that saved session, matching the keyboard Enter.
    session_list_double_click: crate::mouse::DoubleClickTracker,
    /// Parsed `[todotracker]` config, fetched once (lazily, on first
    /// session start) and applied to every `ChatState` this pane
    /// constructs. Defaults until fetched.
    todo_settings: crate::todo_tracker::TodoTrackerSettings,
    /// Guards the one-shot `[todotracker]` config fetch so it doesn't
    /// repeat on every session start.
    todo_settings_loaded: bool,
    /// One-shot app-level Help request, set by the `/help` slash command and
    /// drained immediately by `app.rs` after this pane handles the key.
    help_requested: bool,
    deferred_elicitations: Vec<DeferredInboundRequest>,
}

const ELICITATION_ROUTE_GRACE: Duration = Duration::from_secs(2);

/// An inbound server-initiated request buffered for a retry pass because it
/// could not be installed on arrival. Carries the arrival instant so the drain
/// loop can enforce [`ELICITATION_ROUTE_GRACE`].
struct DeferredInboundRequest {
    req: crate::client::RpcInboundRequest,
    first_seen: Instant,
}

/// Outcome of attempting to route one inbound `elicitation/create` to the
/// active session. See `Chat::try_install_elicitation`.
enum ElicitationRouting {
    /// Modal installed on the active session; it owns the request id.
    Installed,
    /// Schema/params could not be decoded; caller must answer `cancel`.
    Unparseable(serde_json::Value),
    /// Parsed but does not target the active session yet; retry briefly.
    Defer(crate::client::RpcInboundRequest),
}

/// Result of one background `session/git_branch` poll, routed back to the UI
/// thread over `git_branch_tx`.
struct GitStatusUpdate {
    session_id: String,
    branch: Option<String>,
    hash: Option<String>,
}

/// Result of a background model-catalog fetch, routed back so the Loading
/// picker swaps to the populated list (or surfaces an error) on the draw loop.
struct ModelFetchResult {
    session_id: String,
    family: String,
    model_provider_ref: String,
    models: Vec<String>,
    current: Option<String>,
}

fn should_retry_on_entry(phase: &ChatPhase) -> bool {
    matches!(phase, ChatPhase::Error(_) | ChatPhase::PickAgent { .. })
}

impl Chat {
    pub(crate) fn new(rpc: Arc<RpcClient>, pane_kind: PaneKind) -> Self {
        let (git_branch_tx, git_branch_rx) = mpsc::channel(4);
        let (model_fetch_tx, model_fetch_rx) = mpsc::channel(4);
        Self {
            rpc: rpc.clone(),
            rpc_out: rpc.rpc.clone(),
            notif_rx: rpc.subscribe_notifications(),
            inbound_rx: rpc.subscribe_inbound_requests(),
            git_branch_tx,
            git_branch_rx,
            git_branch_inflight: false,
            model_fetch_tx,
            model_fetch_rx,
            phase: ChatPhase::PickAgent {
                agents: Vec::new(),
                list_state: ListState::default(),
                loading: true,
            },
            pane_kind,
            resume_session_id: None,
            resume_agent_alias: None,
            pick_agent_list_area: Rect::default(),
            pick_agent_double_click: crate::mouse::DoubleClickTracker::new(),
            session_list_double_click: crate::mouse::DoubleClickTracker::new(),
            todo_settings: crate::todo_tracker::TodoTrackerSettings::default(),
            todo_settings_loaded: false,
            help_requested: false,
            deferred_elicitations: Vec::new(),
        }
    }

    /// Seed a session id to reattach to on the next session start. Used by the
    /// app layer right before `init()` on a reconnect rebuild so the new pane
    /// resumes the prior session rather than starting a new one. One-shot:
    /// consumed by the first `start_session`.
    pub(crate) fn set_resume_session_id(&mut self, sid: Option<String>) {
        self.resume_session_id = sid;
    }

    /// Seed the agent the resumed session belongs to so a multi-agent reconnect
    /// can reattach automatically instead of dropping the carried session.
    pub(crate) fn set_resume_agent_alias(&mut self, alias: Option<String>) {
        self.resume_agent_alias = alias;
    }

    /// The active session id, if a session is live. Read by the app layer
    /// before a reconnect rebuild to carry the session across.
    pub(crate) fn current_session_id(&self) -> Option<&str> {
        match &self.phase {
            ChatPhase::Active(state) => Some(state.session_id.as_str()),
            _ => None,
        }
    }

    /// The active session's agent alias, if live. Read by the app layer before a
    /// reconnect rebuild so the resumed session reattaches to its own agent.
    pub(crate) fn current_agent_alias(&self) -> Option<&str> {
        match &self.phase {
            ChatPhase::Active(state) => Some(state.agent_alias.as_str()),
            _ => None,
        }
    }

    /// Fetch agent list. If exactly one enabled agent, auto-start a session (or
    /// show the CWD picker first on WSS ACP connections).
    pub(crate) async fn init(&mut self) -> anyhow::Result<()> {
        let agents = match self.rpc.agents_status().await {
            Ok(result) => result
                .agents
                .into_iter()
                .filter(|a| a.enabled)
                .map(|a| a.alias)
                .collect::<Vec<_>>(),
            Err(e) => {
                self.phase = ChatPhase::Error(crate::i18n::t_args(
                    "zc-chat-error-fetch-agents",
                    &[("error", &e.to_string())],
                ));
                return Ok(());
            }
        };

        if agents.is_empty() {
            self.phase = ChatPhase::Error(crate::i18n::t("zc-chat-no-agents"));
            return Ok(());
        }

        // Multi-agent reconnect: if a resumed session was carried across the
        // rebuild and its agent is still present, reattach to it automatically
        // rather than forcing the user back through the picker and minting a
        // fresh session. The resume id is consumed by `start_session`.
        if let Some(prior) = self.resume_agent_alias.take()
            && self.resume_session_id.is_some()
        {
            if agents.iter().any(|a| a == &prior) {
                self.pick_or_start_session(&prior).await;
                return Ok(());
            }
            self.resume_session_id = None;
        }

        if agents.len() == 1 {
            if self.resume_session_id.is_some() {
                self.pick_or_start_session(&agents[0]).await;
                return Ok(());
            }
            if self.try_show_recent_acp_session_picker(&agents).await {
                return Ok(());
            }
            self.pick_or_start_session(&agents[0]).await;
            return Ok(());
        }

        if self.try_show_recent_acp_session_picker(&agents).await {
            return Ok(());
        }

        self.show_agent_picker(agents);
        Ok(())
    }

    fn show_agent_picker(&mut self, agents: Vec<String>) {
        let prior_alias = match &self.phase {
            ChatPhase::PickAgent {
                agents: prev,
                list_state,
                ..
            } => list_state.selected().and_then(|i| prev.get(i)).cloned(),
            _ => None,
        };
        let selected = prior_alias
            .and_then(|alias| agents.iter().position(|a| a == &alias))
            .unwrap_or(0);
        let mut list_state = ListState::default();
        list_state.select(Some(selected));
        // No carried session matched: a manual pick of a different agent must
        // not bleed a stale resume id into a mismatched agent's session.
        self.resume_session_id = None;
        self.resume_agent_alias = None;
        self.phase = ChatPhase::PickAgent {
            agents,
            list_state,
            loading: false,
        };
    }

    async fn try_show_recent_acp_session_picker(&mut self, agents: &[String]) -> bool {
        if self.pane_kind != PaneKind::Acp || self.resume_session_id.is_some() || agents.is_empty()
        {
            return false;
        }

        let Ok(list) = self.rpc.acp_session_list().await else {
            return false;
        };

        let sessions = list
            .sessions
            .into_iter()
            .filter(|entry| {
                entry
                    .agent_alias
                    .as_ref()
                    .is_some_and(|alias| agents.iter().any(|enabled| enabled == alias))
            })
            .collect::<Vec<_>>();

        if sessions.is_empty() {
            return false;
        }

        let mut list_state = ListState::default();
        list_state.select(Some(0));
        self.phase = ChatPhase::PickSession {
            sessions,
            list_state,
            agents: agents.to_vec(),
        };
        true
    }

    async fn resume_session_entry(&mut self, entry: SessionEntry) {
        let Some(agent_alias) = entry.agent_alias else {
            return;
        };
        self.resume_session_id = Some(entry.session_id);
        self.resume_agent_alias = Some(agent_alias.clone());
        self.pick_or_start_session(&agent_alias).await;
    }

    async fn start_fresh_from_picker(&mut self, agents: Vec<String>) {
        self.resume_session_id = None;
        self.resume_agent_alias = None;
        if agents.len() == 1 {
            self.pick_or_start_session(&agents[0]).await;
        } else {
            self.show_agent_picker(agents);
        }
    }

    /// Decide whether to show the CWD picker (WSS ACP) or start the session
    /// immediately (Unix, or non-ACP pane).
    async fn pick_or_start_session(&mut self, agent_alias: &str) {
        // A carried resume id means we are reattaching a daemon-retained session
        // across a reconnect: it already has a cwd, so skip the picker and
        // resume directly instead of forcing the user to re-pick a directory.
        if self.resume_session_id.is_some() {
            self.start_session(agent_alias, None).await;
            return;
        }
        if self.pane_kind == PaneKind::Acp && self.rpc.transport() == crate::client::Transport::Wss
        {
            // Remote ACP: start from the daemon root, not a local path.
            let start_dir = std::path::PathBuf::from("/");
            self.phase = ChatPhase::PickCwd {
                agent_alias: agent_alias.to_string(),
                explorer: FileExplorerState::new_dir_picker_remote(
                    start_dir,
                    Arc::clone(&self.rpc),
                ),
            };
        } else {
            self.start_session(agent_alias, None).await;
        }
    }

    /// Public entry point for "start a session against this specific
    /// agent." Used by the Quickstart pane on Stage 2 to route the
    /// user into the freshly-created agent's chat.
    pub(crate) async fn focus_agent(&mut self, agent_alias: &str) {
        self.pick_or_start_session(agent_alias).await;
    }

    pub(crate) async fn refresh_if_inactive(&mut self) {
        if should_retry_on_entry(&self.phase) {
            let _ = self.init().await;
        }
    }

    async fn start_session(&mut self, agent_alias: &str, cwd_override: Option<&str>) {
        if !self.todo_settings_loaded {
            self.todo_settings_loaded = true;
            if let Ok(fields) = self.rpc.config_list(Some("todotracker")).await {
                self.todo_settings =
                    crate::todo_tracker::TodoTrackerSettings::from_config_fields(&fields);
            }
        }

        // Reattach to a carried-over session on reconnect (one-shot); else a
        // fresh session. `session_new_with_id`/`_acp` with Some(id) restores
        // the daemon-retained session, its persisted history, and its cwd.
        let resume = self.resume_session_id.take();
        // A resume must not re-point the session at the TUI's launch directory:
        // pass no cwd so the daemon keeps the retained session's own cwd. Only
        // a fresh session derives a cwd from the transport / caller.
        let cwd_str: Option<String> = if resume.is_some() {
            None
        } else if self.rpc.transport() == crate::client::Transport::Local {
            // Over Unix socket, pass local CWD so the agent works in the
            // directory the TUI was launched from.
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
        } else {
            // Over WSS the server uses the agent's workspace dir unless the
            // user supplies one.
            cwd_override
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        let result = if self.pane_kind == PaneKind::Acp {
            self.rpc
                .session_new_acp(agent_alias, cwd_str.as_deref(), resume.as_deref())
                .await
        } else {
            self.rpc
                .session_new_with_id(agent_alias, cwd_str.as_deref(), resume.as_deref())
                .await
        };
        match result {
            Ok(session) => {
                let resumed_sid = resume.as_deref().map(|_| session.session_id.clone());
                let mut state = ChatState::new(
                    session.session_id,
                    agent_alias.to_string(),
                    self.todo_settings,
                );
                state.cwd = session.workspace_dir;
                Self::refresh_model_identity(&self.rpc, &mut state).await;
                // On a resume, replay the daemon-retained transcript so the
                // reattached pane shows the prior conversation rather than an
                // empty history. Fresh sessions have nothing to load.
                if let Some(sid) = resumed_sid
                    && let Ok(msgs) = self.rpc.session_messages(&sid).await
                {
                    state.load_history(msgs.messages);
                }
                self.phase = ChatPhase::Active(Box::new(state));
            }
            Err(e) => {
                self.phase = ChatPhase::Error(crate::i18n::t_args(
                    "zc-chat-error-create-session",
                    &[("error", &e.to_string())],
                ));
            }
        }
    }

    async fn confirm_model_picker_selection(rpc: &Arc<RpcClient>, state: &mut ChatState) {
        // Resolve the selection, then act. The final switch needs async + `rpc`,
        // so extract owned values before replacing the overlay.
        match &state.model_picker {
            ModelPickerOverlay::Model(p) => {
                let choice = p.selected().map(str::to_string);
                state.model_picker = ModelPickerOverlay::None;
                if let Some(model) = choice {
                    Self::apply_session_override(
                        rpc,
                        state,
                        crate::client::SessionOverrides {
                            model: Some(model),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            ModelPickerOverlay::ConfiguredProviderStage(p) => {
                let choice = p.selected().map(str::to_string);
                state.model_picker = ModelPickerOverlay::None;
                if let Some(model_provider) = choice {
                    Self::apply_session_override(
                        rpc,
                        state,
                        crate::client::SessionOverrides {
                            model_provider: Some(model_provider),
                            ..Default::default()
                        },
                    )
                    .await;
                } else {
                    state.mark_dirty_full();
                }
            }
            ModelPickerOverlay::Loading | ModelPickerOverlay::None => {}
        }
    }

    async fn restart_session_for_state(
        rpc: &Arc<RpcClient>,
        pane_kind: PaneKind,
        state: &mut ChatState,
    ) -> Option<ChatPhase> {
        let alias = state.agent_alias.clone();
        if pane_kind == PaneKind::Acp && rpc.transport() == crate::client::Transport::Wss {
            // For WSS ACP, go through the CWD picker for new sessions too.
            let _ = rpc.session_close(&state.session_id).await;
            // Remote ACP picker must start from a path the daemon understands.
            let start_dir = std::path::PathBuf::from("/");
            return Some(ChatPhase::PickCwd {
                agent_alias: alias,
                explorer: FileExplorerState::new_dir_picker_remote(start_dir, Arc::clone(rpc)),
            });
        }

        let local_cwd = if rpc.transport() == crate::client::Transport::Local {
            std::env::current_dir().ok()
        } else {
            None
        };
        let cwd_str = local_cwd.as_deref().and_then(|p| p.to_str());
        let new_session = if pane_kind == PaneKind::Acp {
            rpc.session_new_acp(&alias, cwd_str, None).await
        } else {
            rpc.session_new(&alias, cwd_str).await
        };
        match new_session {
            Ok(s) => {
                let old_session_id = state.session_id.clone();
                let _ = rpc.session_close(&old_session_id).await;
                state.reset_for_session(s.session_id, None);
                state.cwd = s.workspace_dir;
                Self::refresh_model_identity(rpc, state).await;
                state.set_info_notice(crate::i18n::t("zc-chat-session-restarted"));
            }
            Err(e) => {
                state.set_info_notice(crate::i18n::t_args(
                    "zc-chat-session-restart-error",
                    &[("error", &e.to_string())],
                ));
            }
        }
        None
    }

    // ── Drain channels (called from draw) ────────────────────────

    fn drain_notifications(&mut self) {
        let mut applied = false;
        loop {
            match self.notif_rx.try_recv() {
                Ok(notif) if notif.method == "session/update" => {
                    if let ChatPhase::Active(ref mut state) = self.phase
                        && let Some(update) = parse_session_update(&notif.params)
                    {
                        state.apply_update(update);
                        applied = true;
                    }
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                _ => break,
            }
        }
        if applied {
            self.pump_queue();
        }
    }

    fn drain_inbound_requests(&mut self) {
        loop {
            let req = match self.inbound_rx.try_recv() {
                Ok(req) => req,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    if let ChatPhase::Active(ref mut state) = self.phase {
                        state
                            .entries
                            .push(ChatEntry::SystemMessage(Arc::<str>::from(crate::i18n::t(
                                "zc-chat-elicitation-dropped",
                            ))));
                        state.mark_dirty_append();
                    }
                    continue;
                }
                Err(_) => break,
            };
            match req.method.as_str() {
                "elicitation/create" => self.route_inbound_elicitation(req),
                other => {
                    let method = other.to_string();
                    let id = req.id.clone();
                    let rpc = self.rpc.clone();
                    tokio::spawn(async move {
                        let _ = rpc
                            .respond_to_inbound_request(
                                id,
                                Err(crate::jsonrpc::JsonRpcError {
                                    code: crate::jsonrpc::error_codes::METHOD_NOT_FOUND,
                                    message: format!("Method not found: {method}"),
                                    data: None,
                                }),
                            )
                            .await;
                    });
                }
            }
        }

        // Retry any elicitations that arrived before their session was
        // installable, and cancel the ones whose grace window has elapsed.
        self.drain_deferred_elicitations();
    }

    /// Route one inbound `elicitation/create`: install it if its session is
    /// active, defer it if the pane is mid-transition (so a legitimately-owned
    /// prompt is not dropped during a session switch/resume), or cancel it
    /// outright if its schema is unparseable.
    fn route_inbound_elicitation(&mut self, req: crate::client::RpcInboundRequest) {
        match self.try_install_elicitation(req) {
            ElicitationRouting::Installed => {}
            ElicitationRouting::Unparseable(id) => Self::answer_cancel(&self.rpc, id),
            ElicitationRouting::Defer(req) => {
                self.deferred_elicitations.push(DeferredInboundRequest {
                    req,
                    first_seen: Instant::now(),
                });
            }
        }
    }

    /// Retry deferred elicitations. Each is re-attempted once per drain; when
    /// its session becomes active the modal installs, and when its grace
    /// deadline lapses it is answered `cancel` so the daemon's tool call never
    /// stalls indefinitely on a session that never materialised in this pane.
    fn drain_deferred_elicitations(&mut self) {
        if self.deferred_elicitations.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.deferred_elicitations);
        for entry in pending {
            let expired = entry.first_seen.elapsed() >= ELICITATION_ROUTE_GRACE;
            match self.try_install_elicitation(entry.req) {
                ElicitationRouting::Installed => {}
                ElicitationRouting::Unparseable(id) => Self::answer_cancel(&self.rpc, id),
                ElicitationRouting::Defer(req) => {
                    if expired {
                        Self::answer_cancel(&self.rpc, req.id);
                    } else {
                        self.deferred_elicitations.push(DeferredInboundRequest {
                            req,
                            first_seen: entry.first_seen,
                        });
                    }
                }
            }
        }
    }

    /// Answer an inbound request with `{"action":"cancel"}`, which the daemon's
    /// `RpcApprovalChannel::request_choice` collapses to `Ok(None)` so the
    /// calling tool takes its non-channel fallback path.
    fn answer_cancel(rpc: &Arc<RpcClient>, id: serde_json::Value) {
        let rpc = rpc.clone();
        tokio::spawn(async move {
            let _ = rpc
                .respond_to_inbound_request(id, Ok(serde_json::json!({ "action": "cancel" })))
                .await;
        });
    }

    fn try_install_elicitation(
        &mut self,
        req: crate::client::RpcInboundRequest,
    ) -> ElicitationRouting {
        let params: Option<crate::wire::ElicitationRequestParams> =
            serde_json::from_value(req.params.clone()).ok();
        let shape = params
            .as_ref()
            .and_then(|p| crate::wire::ElicitationShape::from_schema(&p.requested_schema));

        // A request we can't decode (missing params or an unknown schema)
        // can never install — cancel it immediately, no retry.
        let (params, shape) = match (params, shape) {
            (Some(p), Some(s)) => (p, s),
            _ => return ElicitationRouting::Unparseable(req.id),
        };

        // Must target THIS pane's active session. If not, it may simply be
        // that the pane is mid resume/reset/switch — defer and retry rather
        // than cancel a prompt this pane will shortly own.
        let matches_active = matches!(
            &self.phase,
            ChatPhase::Active(state) if state.session_id == params.session_id
        );
        if !matches_active {
            return ElicitationRouting::Defer(req);
        }

        let pending = match shape {
            crate::wire::ElicitationShape::Single { choices, .. } => PendingElicitation {
                request_id: req.id,
                session_id: params.session_id,
                message: params.message,
                choices: choices.into_iter().map(|c| c.title).collect(),
                multi: false,
                min_items: 1,
                max_items: 1,
                cursor: 0,
                selected: Vec::new(),
            },
            crate::wire::ElicitationShape::Multi {
                choices,
                min_items,
                max_items,
                ..
            } => {
                let n = choices.len();
                PendingElicitation {
                    request_id: req.id,
                    session_id: params.session_id,
                    message: params.message,
                    choices: choices.into_iter().map(|c| c.title).collect(),
                    multi: true,
                    min_items,
                    max_items,
                    cursor: 0,
                    selected: vec![false; n],
                }
            }
        };

        if let ChatPhase::Active(ref mut state) = self.phase {
            state.set_pending_elicitation(pending);
        }
        ElicitationRouting::Installed
    }

    fn settle_stuck_cancel(&mut self) {
        let expired = matches!(
            self.phase,
            ChatPhase::Active(ref s) if s.cancel_watchdog_expired()
        );
        if !expired {
            return;
        }
        if let ChatPhase::Active(ref mut state) = self.phase {
            state
                .entries
                .push(ChatEntry::SystemMessage(Arc::<str>::from(crate::i18n::t(
                    "zc-cancel-timed-out",
                ))));
            state.mark_dirty_append();
            state.commit_turn(String::new(), false);
        }
        self.pump_queue();
    }

    fn after_enqueue(&mut self, enq: Result<(), String>) {
        match enq {
            Ok(()) => {
                if let ChatPhase::Active(ref mut state) = self.phase {
                    state.ensure_queue_selection();
                }
                self.pump_queue();
            }
            Err(msg) => {
                if let ChatPhase::Active(ref mut state) = self.phase {
                    state
                        .entries
                        .push(ChatEntry::SystemMessage(Arc::<str>::from(msg)));
                    state.mark_dirty_append();
                }
            }
        }
    }

    fn pump_queue(&mut self) {
        let next = match self.phase {
            ChatPhase::Active(ref mut state) => state.take_next_dispatchable(),
            _ => None,
        };
        let Some(msg) = next else { return };
        let sid = match self.phase {
            ChatPhase::Active(ref state) => state.session_id.clone(),
            _ => return,
        };

        let transport = self.rpc.transport();
        let attachments_json = if msg.attachments.is_empty() {
            Vec::new()
        } else {
            match build_attachments_json(&msg.attachments, transport) {
                Ok(json) => json,
                Err(e) => {
                    if let ChatPhase::Active(ref mut state) = self.phase {
                        state
                            .entries
                            .push(ChatEntry::SystemMessage(Arc::<str>::from(
                                crate::i18n::t_args(
                                    "zc-queue-dispatch-failed",
                                    &[("error", &e.to_string())],
                                ),
                            )));
                        state.mark_dirty_append();
                    }
                    return;
                }
            }
        };

        if let ChatPhase::Active(ref mut state) = self.phase {
            let att_names: Vec<String> =
                msg.attachments.iter().map(|a| a.filename.clone()).collect();
            let text = if msg.text.is_empty() {
                None
            } else {
                Some(msg.text.clone())
            };
            state.push_user_message(text, att_names);
        }
        self.spawn_prompt(sid, msg.text, attachments_json);
    }

    fn spawn_prompt(&self, sid: String, prompt: String, attachments_json: Vec<serde_json::Value>) {
        let rpc_arc = self.rpc_out.clone();
        tokio::spawn(async move {
            let mut params = serde_json::json!({
                "session_id": sid,
                "prompt": prompt,
            });
            if !attachments_json.is_empty() {
                params["attachments"] = serde_json::Value::Array(attachments_json);
            }
            rpc_arc.notify(method::SESSION_PROMPT, params).await;
        });
    }

    fn drain_git_branch_results(&mut self) {
        while let Ok(update) = self.git_branch_rx.try_recv() {
            self.git_branch_inflight = false;
            if let ChatPhase::Active(ref mut state) = self.phase
                && state.session_id == update.session_id
            {
                state.git_branch = update.branch;
                state.git_hash = update.hash;
                state.git_branch_last_fetch = Some(Instant::now());
            }
        }
    }

    fn drain_model_fetch_results(&mut self) {
        while let Ok(res) = self.model_fetch_rx.try_recv() {
            self.apply_model_fetch(res);
        }
    }

    /// Spawn a background `session/git_branch` poll when the cache is stale.
    /// Gated by `git_branch_inflight` so we never have more than one fetch
    /// outstanding per Chat — the daemon walks the filesystem each call and
    /// the user only sees one result at a time anyway.
    fn maybe_refresh_git_branch(&mut self) {
        if self.git_branch_inflight {
            return;
        }
        let ChatPhase::Active(ref state) = self.phase else {
            return;
        };
        if state.cwd.is_none() {
            return;
        }
        let due = state
            .git_branch_last_fetch
            .is_none_or(|t| t.elapsed() >= GIT_BRANCH_REFRESH_INTERVAL);
        if !due {
            return;
        }
        self.git_branch_inflight = true;
        let sid = state.session_id.clone();
        let rpc = self.rpc.clone();
        let tx = self.git_branch_tx.clone();
        tokio::spawn(async move {
            let result = rpc.session_git_branch(&sid).await.ok();
            let (branch, hash) = match result {
                Some(r) => (r.branch, r.hash),
                None => (None, None),
            };
            let _ = tx
                .send(GitStatusUpdate {
                    session_id: sid,
                    branch,
                    hash,
                })
                .await;
        });
    }

    // ── Drawing ──────────────────────────────────────────────────

    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.drain_notifications();
        self.drain_inbound_requests();
        self.settle_stuck_cancel();
        self.drain_git_branch_results();
        self.drain_model_fetch_results();
        self.maybe_refresh_git_branch();

        match &mut self.phase {
            ChatPhase::PickAgent {
                agents,
                list_state,
                loading,
            } => {
                let list_area = draw_agent_picker(
                    frame,
                    area,
                    agents,
                    list_state,
                    *loading,
                    &self.pane_kind.name(),
                );
                self.pick_agent_list_area = list_area;
            }
            ChatPhase::PickSession {
                sessions,
                list_state,
                ..
            } => {
                render_session_list_overlay(
                    frame,
                    area,
                    sessions,
                    list_state,
                    crate::i18n::t("zc-chat-session-list-resume-title"),
                );
            }
            ChatPhase::PickCwd { explorer, .. } => {
                explorer.render(frame, area);
            }
            ChatPhase::Active(state) => {
                render(frame, state, area, self.pane_kind);
            }
            ChatPhase::Error(msg) => {
                draw_error(frame, area, msg, &self.pane_kind.name());
            }
        }
    }

    // ── Key handling ─────────────────────────────────────────────

    /// Handle a bracketed paste event.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        let ChatPhase::Active(state) = &mut self.phase else {
            return;
        };
        if state.turn_in_flight {
            return;
        }
        let action = state.input_bar.handle_paste(text);
        if let InputBarAction::StatusMessage(msg) = action {
            state.set_info_notice(msg);
        }
    }

    pub(crate) fn ctx_tokens(&self) -> (Option<u64>, Option<u64>) {
        match &self.phase {
            ChatPhase::Active(s) => (s.context_input_tokens, s.context_max_tokens),
            _ => (None, None),
        }
    }

    /// The agent alias this pane is currently focused on, if any. Used to
    /// resolve a per-agent theme override while this pane is active. Returns
    /// `None` in the agent-picker phase, where no agent is yet chosen.
    pub(crate) fn selected_agent(&self) -> Option<&str> {
        match &self.phase {
            ChatPhase::Active(s) => Some(s.agent_alias.as_str()),
            ChatPhase::PickCwd { agent_alias, .. } => Some(agent_alias.as_str()),
            _ => None,
        }
    }

    /// Working directory for the active conversation, if a session is running.
    pub(crate) fn current_cwd(&self) -> Option<&str> {
        match &self.phase {
            ChatPhase::Active(s) => s.cwd.as_deref(),
            _ => None,
        }
    }

    /// Active info-bar message for the app-level `InfoBar`, expiring it first if
    /// it has outlived [`crate::widgets::INFO_BAR_TTL`] so the bar auto-hides.
    pub(crate) fn info_message(&mut self) -> Option<&crate::widgets::InfoMessage> {
        if let ChatPhase::Active(s) = &mut self.phase {
            if s.info_message.as_ref().is_some_and(|m| m.is_expired()) {
                s.clear_info_notice();
            }
            return s.info_message.as_ref();
        }
        None
    }

    /// Whether the active chat session is in browse mode.
    pub(crate) fn in_browse_mode(&self) -> bool {
        match &self.phase {
            ChatPhase::Active(s) => s.in_browse_mode(),
            _ => false,
        }
    }

    /// Exit browse / selection mode if active. No-op otherwise.
    pub(crate) fn exit_browse_mode(&mut self) {
        if let ChatPhase::Active(s) = &mut self.phase {
            s.exit_browse_mode();
        }
    }

    /// Clear the input bar text (called when Ctrl+C arms the quit modal).
    pub(crate) fn clear_input(&mut self) {
        if let ChatPhase::Active(s) = &mut self.phase {
            s.input_bar.reset();
            s.mark_dirty_full();
        }
    }

    pub(crate) fn wants_quit_chord(&self) -> bool {
        match &self.phase {
            ChatPhase::Active(s) => {
                s.turn_in_flight && !matches!(s.turn_status, TurnStatus::Cancelling)
            }
            _ => false,
        }
    }

    pub(crate) fn take_help_request(&mut self) -> bool {
        std::mem::take(&mut self.help_requested)
    }

    pub(crate) fn wants_text_input(&self) -> bool {
        match &self.phase {
            // CWD picker always captures text input.
            ChatPhase::PickCwd { .. } => true,
            ChatPhase::PickSession { .. } => false,
            ChatPhase::Active(s) => {
                // The model picker is modal: claim text-input so global keys
                // (`?`, reload) are suppressed; its own handler swallows keys.
                if s.model_picker.is_open() {
                    return true;
                }
                if s.pending_elicitation().is_some() {
                    return true;
                }
                if !matches!(s.session_overlay, SessionOverlay::None) {
                    return false;
                }
                // Browse mode: single-char bindings active.
                if s.in_browse_mode() {
                    return false;
                }
                // Command mode when input is empty; text mode when typing.
                s.input_bar.wants_text_input()
            }
            _ => false,
        }
    }
}

impl crate::widgets::HelpContext for Chat {
    fn help_context(&self) -> crate::widgets::HelpNode {
        use crate::keymap::{ChatTabAction, RebindableActions};
        use crate::widgets::{HelpEntry as E, HelpNode};
        match &self.phase {
            ChatPhase::PickAgent { loading, .. } => {
                use crate::keymap::{
                    ChatTabAction as C, GlobalAction, ModalAction, action_key_labels,
                };
                if *loading {
                    HelpNode::entries(vec![E::key("", crate::i18n::t("zc-chat-loading-agents"))])
                } else {
                    let nav = action_key_labels(C::BrowseUp)
                        .into_iter()
                        .chain(action_key_labels(C::BrowseDown))
                        .chain(action_key_labels(C::BrowseUpVim))
                        .chain(action_key_labels(C::BrowseDownVim));
                    HelpNode::entries(vec![
                        E::new(nav, crate::i18n::t("zc-chat-help-navigate")),
                        E::new(
                            action_key_labels(ModalAction::Confirm),
                            crate::i18n::t("zc-chat-help-select-agent"),
                        ),
                        E::new(
                            action_key_labels(GlobalAction::Quit),
                            crate::i18n::t("zc-chat-help-quit"),
                        ),
                    ])
                }
            }
            ChatPhase::PickCwd { explorer, .. } => explorer.help_context(),
            ChatPhase::PickSession { .. } => {
                use crate::keymap::{ChatTabAction as C, ModalAction as M, action_key_labels};
                let nav = action_key_labels(M::Up)
                    .into_iter()
                    .chain(action_key_labels(M::Down));
                HelpNode::entries(vec![
                    E::new(nav, crate::i18n::t("zc-chat-help-navigate")),
                    E::new(
                        action_key_labels(M::Confirm),
                        crate::i18n::t("zc-chat-help-switch-session"),
                    ),
                    E::new(
                        action_key_labels(M::Cancel)
                            .into_iter()
                            .chain(action_key_labels(C::NewSession)),
                        crate::i18n::t("zc-chat-help-new-session"),
                    ),
                ])
            }
            ChatPhase::Error(_) => {
                use crate::keymap::{ChatTabAction as C, GlobalAction, action_key_labels};
                let keys = action_key_labels(C::ErrorDismiss)
                    .into_iter()
                    .chain(action_key_labels(GlobalAction::Quit));
                HelpNode::entries(vec![E::new(keys, crate::i18n::t("zc-chat-help-quit"))])
            }
            ChatPhase::Active(state) => {
                match &state.session_overlay {
                    SessionOverlay::List { .. } => {
                        use crate::keymap::{ModalAction as M, action_key_labels};
                        let nav = action_key_labels(M::Up)
                            .into_iter()
                            .chain(action_key_labels(M::Down));
                        return HelpNode::entries(vec![
                            E::new(nav, crate::i18n::t("zc-chat-help-navigate")),
                            E::new(
                                action_key_labels(M::Confirm),
                                crate::i18n::t("zc-chat-help-switch-session"),
                            ),
                            E::new(
                                action_key_labels(M::Cancel),
                                crate::i18n::t("zc-chat-help-close"),
                            ),
                        ]);
                    }
                    SessionOverlay::None => {}
                }
                if state.pending_elicitation().is_some() {
                    // The elicitation modal is keyboard-driven; source its
                    // hints from the ModalAction registry so they track any
                    // rebind. Multi-select adds the toggle line.
                    use crate::keymap::{ModalAction as M, action_key_labels};
                    let multi = state
                        .pending_elicitation()
                        .map(|e| e.multi)
                        .unwrap_or(false);
                    let mut entries = vec![E::new(
                        action_key_labels(M::Up)
                            .into_iter()
                            .chain(action_key_labels(M::Down)),
                        crate::i18n::t("zc-chat-help-move-up"),
                    )];
                    if multi {
                        entries.push(E::new(
                            action_key_labels(M::Toggle),
                            crate::i18n::t("zc-elicit-help-toggle"),
                        ));
                    }
                    entries.push(E::new(
                        action_key_labels(M::Confirm),
                        crate::i18n::t("zc-elicit-help-confirm"),
                    ));
                    entries.push(E::new(
                        action_key_labels(M::Cancel),
                        crate::i18n::t("zc-elicit-help-cancel"),
                    ));
                    return HelpNode::entries(entries);
                }
                if state.pending_approval().is_some() {
                    use crate::keymap::{ChatTabAction as C, action_key_labels};
                    return HelpNode::entries(vec![
                        E::new(
                            action_key_labels(C::ApprovalApprove),
                            crate::i18n::t("zc-chat-help-approve"),
                        ),
                        E::new(
                            action_key_labels(C::ApprovalApproveAll),
                            crate::i18n::t("zc-chat-help-always-approve"),
                        ),
                        E::new(
                            action_key_labels(C::CancelTurn),
                            crate::i18n::t("zc-chat-help-deny"),
                        ),
                        E::new(
                            action_key_labels(C::CancelTurn),
                            crate::i18n::t("zc-chat-help-cancel-turn"),
                        ),
                    ]);
                }
                if state.in_browse_mode() {
                    use crate::keymap::{ChatTabAction as C, action_key_labels};
                    let mut return_keys = action_key_labels(C::BrowseExit);
                    return_keys.extend(action_key_labels(C::BrowseExitSelection));
                    return HelpNode::entries(vec![
                        E::new(
                            action_key_labels(C::BrowseUp)
                                .into_iter()
                                .chain(action_key_labels(C::BrowseUpVim)),
                            crate::i18n::t("zc-chat-help-move-up"),
                        ),
                        E::new(
                            action_key_labels(C::BrowseDown)
                                .into_iter()
                                .chain(action_key_labels(C::BrowseDownVim)),
                            crate::i18n::t("zc-chat-help-move-down"),
                        ),
                        E::new(
                            action_key_labels(C::BrowseSelectExtend)
                                .into_iter()
                                .chain(action_key_labels(C::BrowseSelectExtendDown)),
                            crate::i18n::t("zc-chat-help-extend-selection"),
                        ),
                        E::new(
                            action_key_labels(C::CopySelection),
                            crate::i18n::t("zc-chat-help-yank-selection"),
                        ),
                        E::new(return_keys, crate::i18n::t("zc-chat-help-return-to-input")),
                    ]);
                }
                if state.turn_in_flight {
                    use crate::keymap::{ChatTabAction as C, action_key_labels};
                    let mut cancel_keys = action_key_labels(C::CancelTurn);
                    cancel_keys.extend(action_key_labels(C::BrowseExitSelection));
                    let mut entries = vec![
                        E::new(cancel_keys, crate::i18n::t("zc-chat-help-cancel-turn")),
                        E::new(
                            action_key_labels(crate::keymap::InputBarAction::Submit),
                            crate::i18n::t("zc-queue-help-enqueue"),
                        ),
                        E::new(
                            action_key_labels(crate::keymap::InputBarAction::Inject),
                            crate::i18n::t("zc-queue-help-inject"),
                        ),
                    ];
                    // Queue-management keys are only live while the sidebar is
                    // open — surface them here too so a mid-turn open queue is
                    // not left without its own controls.
                    if state.queue_sidebar_open() {
                        entries.extend(queue_sidebar_help_entries());
                    }
                    // The input box stays editable mid-turn for queuing, so its
                    // bindings belong in help too.
                    return HelpNode::entries(entries).with_child(state.input_bar.help_context());
                }
                // Idle: compose pane-level bindings + input bar as child.
                let mut pane_entries = vec![
                    // Browse-mode bindings rendered from the registry so
                    // rebinds always stay in sync — see also the browse-mode
                    // dispatch code in `handle_key`.
                    E::new(
                        ChatTabAction::BrowseEnter
                            .resolved()
                            .iter()
                            .map(|c| c.display().to_string()),
                        crate::i18n::t("zc-chat-help-browse-mode"),
                    ),
                    E::key(
                        "Shift+↑/↓",
                        crate::i18n::t("zc-chat-help-scroll-conversation"),
                    ),
                    E::key("t", crate::i18n::t("zc-chat-help-toggle-thoughts")),
                    E::spacer(),
                    E::key(
                        chord_label(ChatTabAction::NewSession),
                        crate::i18n::t("zc-chat-help-new-session"),
                    ),
                    E::key(
                        chord_label(ChatTabAction::SwitchSession),
                        crate::i18n::t("zc-chat-help-switch-session"),
                    ),
                    E::spacer(),
                    E::key(
                        chord_label(ChatTabAction::PauseResumeQueue),
                        crate::i18n::t("zc-queue-help-resume"),
                    ),
                ];
                pane_entries.extend(queue_sidebar_help_entries());
                let pane = HelpNode::entries(pane_entries);
                pane.with_child(state.input_bar.help_context())
            }
        }
    }
}

// ── Agent picker rendering ───────────────────────────────────────

/// Build the agent-picker nav hint from the live keymap (browse up/down + the
/// modal confirm chord), never hardcoded literals.
fn picker_nav_keys() -> String {
    use crate::keymap::{ChatTabAction, Chord, ModalAction, RebindableActions};
    let mut parts: Vec<String> = Vec::new();
    let mut push = |c: &Chord| {
        let d = c.display();
        if !parts.contains(&d) {
            parts.push(d);
        }
    };
    for c in ChatTabAction::BrowseUp.resolved() {
        push(&c);
    }
    for c in ChatTabAction::BrowseDown.resolved() {
        push(&c);
    }
    for c in ModalAction::Confirm.resolved() {
        push(&c);
    }
    parts.join("/")
}

fn draw_agent_picker(
    frame: &mut Frame,
    area: Rect,
    agents: &[String],
    list_state: &mut ListState,
    loading: bool,
    tab_title: &str,
) -> Rect {
    let block = Block::default()
        .title(Span::styled(format!(" {tab_title} "), theme::title_style()))
        .borders(Borders::ALL)
        .border_style(theme::dim_style());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if loading {
        let p = Paragraph::new(crate::i18n::t("zc-chat-loading-agents-msg"))
            .alignment(Alignment::Center)
            .style(theme::dim_style());
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .split(inner);
        frame.render_widget(p, vert[1]);
        return Rect::default();
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{} ", crate::i18n::t("zc-chat-picker-header")),
            theme::body_style(),
        ),
        Span::styled(
            crate::i18n::t_args(
                "zc-chat-picker-header-hint",
                &[("keys", &picker_nav_keys())],
            ),
            theme::dim_style(),
        ),
    ]));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = agents
        .iter()
        .map(|a| ListItem::new(Span::styled(a.as_str(), theme::body_style())))
        .collect();
    let list = List::new(items).highlight_style(theme::list_highlight_style());
    frame.render_stateful_widget(list, chunks[1], list_state);
    // The list rect is unbordered, but `mouse::list_click_index` assumes a
    // 1-cell top border. Hand back a rect shifted up one row (and one taller) so
    // the helper's border compensation lands on the true first item.
    Rect::new(
        chunks[1].x,
        chunks[1].y.saturating_sub(1),
        chunks[1].width,
        chunks[1].height + 1,
    )
}

// ── Error rendering ──────────────────────────────────────────────

fn draw_error(frame: &mut Frame, area: Rect, msg: &str, tab_title: &str) {
    let block = Block::default()
        .title(Span::styled(format!(" {tab_title} "), theme::title_style()))
        .borders(Borders::ALL)
        .border_style(theme::dim_style());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);

    let p = Paragraph::new(Line::from(Span::styled(msg, theme::error_style())))
        .alignment(Alignment::Center);
    frame.render_widget(p, chunks[1]);
}

// ── ChatState / ChatEntry ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct PendingElicitation {
    /// JSON-RPC request id to respond to. Echoed verbatim.
    pub request_id: serde_json::Value,
    /// Session this elicitation belongs to. Captured at install time so a
    /// future mouse handler (or a cross-session correctness assert) can
    /// confirm the modal still targets the active session. Read indirectly
    /// today via the install-time match in `try_install_elicitation`.
    #[allow(dead_code)]
    pub session_id: String,
    /// Prompt text shown above the choice list.
    pub message: String,
    /// User-visible choice titles, in wire order. The `choice-N` const
    /// is the index into this vec.
    pub choices: Vec<String>,
    /// Whether this is a multi-select (checkbox) prompt.
    pub multi: bool,
    /// Multi-select lower bound (inclusive). Ignored for single-select.
    pub min_items: usize,
    /// Multi-select upper bound (inclusive). Ignored for single-select.
    pub max_items: usize,
    /// Highlighted row.
    pub cursor: usize,
    /// Per-row checkbox state for multi-select. Empty / unused for
    /// single-select.
    pub selected: Vec<bool>,
}

impl PendingElicitation {
    /// Number of currently-checked rows (multi-select only).
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&b| b).count()
    }

    /// Whether the current selection satisfies the multi-select
    /// `min_items`/`max_items` bounds. Always `true` for single-select
    /// (the cursor itself is the answer).
    pub fn selection_valid(&self) -> bool {
        if !self.multi {
            return !self.choices.is_empty();
        }
        let n = self.selected_count();
        n >= self.min_items && n <= self.max_items
    }

    /// Build the `accept` content payload for the current selection, or
    /// `None` if the selection is invalid (e.g. too few boxes checked).
    pub fn accept_content(&self) -> Option<serde_json::Value> {
        if !self.selection_valid() {
            return None;
        }
        if self.multi {
            let consts: Vec<serde_json::Value> = self
                .selected
                .iter()
                .enumerate()
                .filter(|&(_, &on)| on)
                .map(|(i, _)| serde_json::json!(format!("choice-{i}")))
                .collect();
            Some(serde_json::json!({ "choices": consts }))
        } else {
            Some(serde_json::json!({ "choice": format!("choice-{}", self.cursor) }))
        }
    }
}

#[derive(Debug, Clone)]
pub enum ChatEntry {
    AgentMessage(Arc<str>),
    AgentThought(Arc<str>),
    /// Local system/info message (e.g. "Attached: photo.png").
    SystemMessage(Arc<str>),
    UserMessage {
        text: Option<Arc<str>>,
        attachments: Vec<Arc<str>>,
    },
    Tool {
        tool_call_id: Arc<str>,
        name: Arc<str>,
        /// Pre-serialised JSON of the tool input. Storing the
        /// rendered string instead of a `serde_json::Value` tree
        /// drops the per-entry parsed-tree footprint (one
        /// allocation per Value node) to a single `Arc<str>`.
        input_json: Arc<str>,
        /// Tool output. `None` while the call is in flight,
        /// `Some(Arc<str>)` once the result arrives.
        result: Option<Arc<str>>,
    },
}

#[derive(Debug)]
enum SessionOverlay {
    None,
    List {
        sessions: Vec<SessionEntry>,
        list_state: ListState,
    },
}

/// Active model / model_provider picker overlay. `None` when no picker is open.
/// The model_provider variant is two-stage: pick a model_provider, then (after a
/// catalog fetch) pick a model from it.
#[derive(Debug, Clone, Default)]
pub(crate) enum ModelPickerOverlay {
    /// No picker open.
    #[default]
    None,
    /// Catalog fetch in flight — drawn as a modal so the user sees a
    /// waiting state instead of a frozen UI while the models load.
    Loading,
    /// Single-stage model picker over the active model_provider's catalog.
    Model(crate::widgets::PickerState),
    ConfiguredProviderStage(crate::widgets::PickerState),
}

impl ModelPickerOverlay {
    fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn item_count(&self) -> usize {
        match self {
            Self::Model(p) | Self::ConfiguredProviderStage(p) => p.items.len(),
            Self::Loading => 1,
            Self::None => 0,
        }
    }

    fn picker_mut(&mut self) -> Option<&mut crate::widgets::PickerState> {
        match self {
            Self::Model(p) | Self::ConfiguredProviderStage(p) => Some(p),
            Self::Loading | Self::None => None,
        }
    }
}

/// Tracks what kind of update has invalidated the rendered lines cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinesDirty {
    /// Cache is up-to-date.
    Clean,
    /// New entries were appended at the tail; the render window has not shifted.
    /// `rebuild_lines` can extend `cached_lines` instead of rebuilding from scratch,
    /// avoiding re-parsing markdown for unchanged `AgentMessage` entries.
    Appended,
    /// Full rebuild required (entry mutation, selection/thoughts change, reset).
    Full,
}

/// Scrollbar drag captured on mouse-down on the track.
#[derive(Debug, Clone, Copy)]
struct ScrollbarDrag {
    start_scroll: u16,
    start_row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleHitTarget {
    Agent,
    ModelProvider,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TitleHitRect {
    target: TitleHitTarget,
    rect: Rect,
}

/// A clickable copy affordance from the last draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyHitKind {
    Code,
    Message,
    Transcript,
}

#[derive(Debug, Clone)]
pub(crate) struct CopyHitRegion {
    rect: Rect,
    text: String,
    kind: CopyHitKind,
    group: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyFeedbackTarget {
    Code(usize),
    Overlay(Rect),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyFeedback {
    target: CopyFeedbackTarget,
    shown_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellPoint {
    column: u16,
    row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptSelection {
    anchor: CellPoint,
    head: CellPoint,
    dragged: bool,
}

impl TranscriptSelection {
    fn normalized(self) -> (CellPoint, CellPoint) {
        if (self.anchor.row, self.anchor.column) <= (self.head.row, self.head.column) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptCell {
    symbol: String,
    span_start: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptSnapshot {
    area: Rect,
    cells: Vec<TranscriptCell>,
}

impl TranscriptSnapshot {
    fn point_at(&self, column: u16, row: u16) -> Option<CellPoint> {
        if !mouse::in_rect(column, row, self.area) {
            return None;
        }
        Some(CellPoint {
            column: column - self.area.x,
            row: row - self.area.y,
        })
    }

    fn cell(&self, point: CellPoint) -> Option<&TranscriptCell> {
        if point.column >= self.area.width || point.row >= self.area.height {
            return None;
        }
        let index =
            usize::from(point.row) * usize::from(self.area.width) + usize::from(point.column);
        self.cells.get(index)
    }

    fn has_text_at(&self, point: CellPoint) -> bool {
        let Some(cell) = self.cell(point) else {
            return false;
        };
        self.cell(CellPoint {
            column: cell.span_start,
            row: point.row,
        })
        .is_some_and(|origin| !origin.symbol.chars().all(char::is_whitespace))
    }

    fn selection_bounds(&self, selection: TranscriptSelection) -> Option<(CellPoint, CellPoint)> {
        if !selection.dragged {
            return None;
        }
        let (mut start, mut end) = selection.normalized();
        start.column = self.cell(start)?.span_start;
        let end_cell = self.cell(end)?;
        let origin = self.cell(CellPoint {
            column: end_cell.span_start,
            row: end.row,
        })?;
        end.column = end_cell
            .span_start
            .saturating_add(
                (unicode_width::UnicodeWidthStr::width(origin.symbol.as_str()) as u16)
                    .max(1)
                    .saturating_sub(1),
            )
            .min(self.area.width.saturating_sub(1));
        Some((start, end))
    }

    fn selection_contains(&self, selection: TranscriptSelection, point: CellPoint) -> bool {
        let Some((start, end)) = self.selection_bounds(selection) else {
            return false;
        };
        (point.row, point.column) >= (start.row, start.column)
            && (point.row, point.column) <= (end.row, end.column)
    }

    fn selected_text(&self, selection: TranscriptSelection) -> Option<String> {
        if self.cells.is_empty() {
            return None;
        }

        let (start, end) = self.selection_bounds(selection)?;
        let start_row = usize::from(start.row);
        let end_row = usize::from(end.row);
        let mut selected_rows = Vec::with_capacity(end_row.saturating_sub(start_row) + 1);

        for row_idx in start_row..=end_row {
            let first_col = if row_idx == start_row {
                start.column
            } else {
                0
            };
            let last_col = if row_idx == end_row {
                end.column
            } else {
                self.area.width.saturating_sub(1)
            };

            let mut row_text = String::new();
            for column in first_col..=last_col {
                let point = CellPoint {
                    column,
                    row: row_idx as u16,
                };
                let Some(cell) = self.cell(point) else {
                    continue;
                };
                if cell.span_start == column {
                    row_text.push_str(&cell.symbol);
                }
            }
            selected_rows.push(row_text.trim_end_matches(char::is_whitespace).to_string());
        }

        let text = selected_rows.join("\n");
        text.chars().any(|ch| !ch.is_whitespace()).then_some(text)
    }

    fn selection_anchor_rect(&self, selection: TranscriptSelection) -> Option<Rect> {
        if !selection.dragged {
            return None;
        }
        let (start, end) = selection.normalized();
        let y = self.area.y.saturating_add(start.row);
        let height = end.row.saturating_sub(start.row).saturating_add(1);
        Some(Rect::new(self.area.x, y, self.area.width, height))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueItemStatus {
    Pending,
    Injected,
}

#[derive(Debug, Clone)]
pub(crate) struct QueuedMessage {
    pub id: u64,
    pub text: String,
    pub attachments: Vec<PendingAttachment>,
    pub status: QueueItemStatus,
}

#[derive(Debug)]
pub struct ChatState {
    pub session_id: String,
    pub agent_alias: String,
    session_name: Option<String>,
    model_provider_ref: Option<String>,
    model: Option<String>,
    /// Working directory for this session (shown above input bar).
    pub cwd: Option<String>,
    /// Cached git branch for `cwd`, refreshed by the daemon on a polling
    /// interval (`GIT_BRANCH_REFRESH_INTERVAL`). `None` means either "not a
    /// git repo" or "not fetched yet".
    pub git_branch: Option<String>,
    /// First user message of the session, pulled from the persisted message
    /// store. Shown as a pinned recovery row at the top of the panel so the
    /// original ask stays visible across scroll and after a session reload.
    pub first_message: Option<String>,
    /// Cached short commit hash for `cwd`, refreshed alongside `git_branch`.
    /// `None` means "not a git repo", "unborn branch", or "not fetched yet".
    pub git_hash: Option<String>,
    /// Monotonic timestamp of the last completed `session/git_branch` reply,
    /// used to throttle re-fetches.
    pub git_branch_last_fetch: Option<Instant>,
    pub input_bar: InputBarState,
    entries: Vec<ChatEntry>,
    streaming_text: String,
    streaming_thought: String,
    pending_approval: Option<PendingApproval>,
    pending_elicitation: Option<PendingElicitation>,
    pub turn_in_flight: bool,
    /// Set when any streaming text was flushed during the current turn.
    /// Used by `commit_turn` to decide whether `full_text` is a fallback
    /// (no streaming happened) or a duplicate (streaming already committed).
    turn_had_streaming_text: bool,
    /// Set when any `ToolCall` event arrived during the current turn.
    /// Used by `commit_turn` to distinguish "empty completion with tool
    /// calls" (normal — tool output is the visible record) from "empty
    /// completion with nothing at all" (needs a diagnostic row).
    turn_had_tool_calls: bool,
    /// Fine-grained label for the input-bar title while a turn is active.
    /// Lockstep with `turn_in_flight` (`Idle` ↔ `false`) but adds the
    /// thinking / responding / tool-call breakdown for the UI.
    pub turn_status: TurnStatus,
    /// Anchor for the dots animation — reset each time a turn begins so
    /// the pulse starts from phase 0.
    turn_started_at: Instant,
    show_thoughts: bool,
    /// Browse mode cursor (most-recently moved position).
    browse_cursor: Option<usize>,
    /// Anchor for range selection; set when Shift+↑/↓ is first pressed.
    /// Range is `min(anchor, cursor)..=max(anchor, cursor)`.
    browse_anchor: Option<usize>,
    /// Ctrl+click multi-select set, independent of cursor/anchor range.
    browse_multi: std::collections::BTreeSet<usize>,
    /// Entry index where mouse went down while browse mode is active. Used
    /// only to extend in-app range selection during a drag; mouse-up never
    /// copies implicitly.
    mouse_down_entry: Option<usize>,
    /// Visible transcript cells from the last draw. Character-level selection
    /// uses this exact rendered grid so Markdown wrapping has one source of truth.
    transcript_snapshot: Option<TranscriptSnapshot>,
    /// Normal-mode character selection within `transcript_snapshot`.
    transcript_selection: Option<TranscriptSelection>,
    /// Per-entry hit rects from the last draw.
    entry_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Clickable `[Copy]` labels from the last draw.
    copy_hit_regions: Vec<CopyHitRegion>,
    /// Temporary `[Copied]` overlay for copy labels.
    copy_feedback: Option<CopyFeedback>,
    /// Clickable provider/model title spans from the last draw.
    title_hit_rects: Vec<TitleHitRect>,
    /// Scrollbar track rect from the last draw.
    scrollbar_track_rect: Option<ratatui::layout::Rect>,
    /// Active scrollbar drag anchor.
    scrollbar_drag: Option<ScrollbarDrag>,
    session_overlay: SessionOverlay,
    scroll_offset: u16,
    pinned_to_bottom: bool,
    last_total_rows: u16,
    last_inner_height: u16,
    /// Cached rendered lines from committed entries.
    cached_lines: Vec<Line<'static>>,
    /// Per-entry unwrapped-line ranges in `cached_lines` — `(entry_idx,
    /// start, end_exclusive)`. Used by mouse hit-testing.
    cached_line_ranges: Vec<(usize, usize, usize)>,
    /// Per-entry screen-row ranges: `(entry_idx, screen_start, screen_end,
    /// content_width)`. Unlike `cached_line_ranges` (unwrapped line indices),
    /// these account for markdown wrapping so mouse hit-testing (`entry_rects`)
    /// lands on the correct screen rows for agent messages, code blocks, and
    /// tables. `content_width` is the widest rendered column extent of the
    /// entry (clamped to the viewport), so hit-testing ignores the blank space
    /// beside short messages — a click there dismisses the highlight instead of
    /// re-selecting the entry.
    cached_screen_ranges: Vec<(usize, u16, u16, u16)>,
    /// Fine-grained dirty tracking — see [`LinesDirty`].
    dirty: LinesDirty,
    /// How many entries from `entries[cached_render_start..]` are represented in
    /// `cached_lines`.  Valid only when `dirty != Full`.
    cached_entry_count: usize,
    /// The `entries` index where the render window starts for the current cache.
    cached_render_start: usize,
    /// The render width the current `cached_lines` were laid out for.
    /// A width change forces a full rebuild because tables compute their
    /// column budgets from it.
    cached_render_width: u16,
    cached_total_rows: u16,
    /// Cumulative token count for this session: every Usage event from the
    /// provider (input + cached + output) is added on arrival. Cleared on
    /// session reset only.
    pub context_input_tokens: Option<u64>,
    /// Configured context limit for this session's model.
    pub context_max_tokens: Option<u64>,
    /// Outbound message queue; the front dispatches when the session is free.
    message_queue: VecDeque<QueuedMessage>,
    /// Monotonic id source for queued messages.
    next_queue_id: u64,
    /// Set on Cancel/Fail; freezes auto-dispatch until the user resumes.
    queue_paused: bool,
    resume_override: bool,
    cancel_started_at: Option<Instant>,
    queue_sidebar_cols: u16,
    /// Selected queued message id for sidebar edit/delete.
    queue_sel: Option<u64>,
    /// Per-item clickable rects from the last sidebar draw, mapping a queued
    /// message id to its header-row rect. Drives left-click selection.
    queue_item_rects: Vec<(u64, ratatui::layout::Rect)>,
    /// Inner sidebar rect from the last draw, for scroll-wheel hit-testing.
    queue_sidebar_rect: Option<ratatui::layout::Rect>,
    /// Scroll offset (in rendered rows) into the queue sidebar.
    queue_scroll: u16,
    /// Latest info-bar message (queue/attach notices, model-switch op notes,
    /// errors). `None` hides the bar. Auto-cleared in the tick loop once
    /// [`crate::widgets::INFO_BAR_TTL`] elapses.
    pub info_message: Option<crate::widgets::InfoMessage>,
    /// Active model / model_provider picker overlay.
    model_picker: ModelPickerOverlay,
    /// Live TodoWrite tracker panel for this session. Read-only; fed by
    /// `SessionUpdate::Plan`, toggled by the user, laid out per config.
    todo_tracker: crate::todo_tracker::TodoTracker,
}

impl ChatState {
    pub fn new(
        session_id: String,
        agent_alias: String,
        todo_settings: crate::todo_tracker::TodoTrackerSettings,
    ) -> Self {
        Self {
            session_id,
            agent_alias,
            session_name: None,
            model_provider_ref: None,
            model: None,
            cwd: None,
            git_branch: None,
            first_message: None,
            git_hash: None,
            git_branch_last_fetch: None,
            input_bar: InputBarState::new(),
            entries: Vec::new(),
            streaming_text: String::new(),
            streaming_thought: String::new(),
            pending_approval: None,
            pending_elicitation: None,
            turn_in_flight: false,
            turn_had_streaming_text: false,
            turn_had_tool_calls: false,
            turn_status: TurnStatus::Idle,
            turn_started_at: Instant::now(),
            show_thoughts: true,
            browse_cursor: None,
            browse_anchor: None,
            browse_multi: std::collections::BTreeSet::new(),
            mouse_down_entry: None,
            transcript_snapshot: None,
            transcript_selection: None,
            entry_rects: Vec::new(),
            copy_hit_regions: Vec::new(),
            copy_feedback: None,
            title_hit_rects: Vec::new(),
            scrollbar_track_rect: None,
            scrollbar_drag: None,
            session_overlay: SessionOverlay::None,
            scroll_offset: 0,
            pinned_to_bottom: true,
            last_total_rows: 0,
            last_inner_height: 0,
            cached_lines: Vec::new(),
            cached_line_ranges: Vec::new(),
            cached_screen_ranges: Vec::new(),
            dirty: LinesDirty::Full,
            cached_entry_count: 0,
            cached_render_start: 0,
            cached_render_width: 0,
            cached_total_rows: 0,
            context_input_tokens: None,
            context_max_tokens: None,
            message_queue: VecDeque::new(),
            next_queue_id: 0,
            queue_paused: false,
            resume_override: false,
            cancel_started_at: None,
            queue_sidebar_cols: 36,
            queue_sel: None,
            queue_item_rects: Vec::new(),
            queue_sidebar_rect: None,
            queue_scroll: 0,
            info_message: None,
            model_picker: ModelPickerOverlay::None,
            todo_tracker: crate::todo_tracker::TodoTracker::from_settings(todo_settings),
        }
    }

    fn mark_dirty_append(&mut self) {
        if self.dirty == LinesDirty::Clean {
            self.dirty = LinesDirty::Appended;
        }
        // Full is sticky — don't downgrade.
    }

    fn mark_dirty_full(&mut self) {
        self.dirty = LinesDirty::Full;
    }

    fn clear_transcript_selection(&mut self) {
        let changed = self.transcript_selection.is_some()
            || !self.copy_hit_regions.is_empty()
            || self.copy_feedback.is_some();
        self.transcript_selection = None;
        self.copy_hit_regions.clear();
        self.copy_feedback = None;
        if changed {
            self.mark_dirty_full();
        }
    }

    fn begin_transcript_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(snapshot) = &self.transcript_snapshot else {
            return false;
        };
        let Some(point) = snapshot.point_at(column, row) else {
            self.clear_transcript_selection();
            return false;
        };
        if !snapshot.has_text_at(point) {
            self.clear_transcript_selection();
            return false;
        }

        self.copy_feedback = None;
        self.transcript_selection = Some(TranscriptSelection {
            anchor: point,
            head: point,
            dragged: false,
        });
        true
    }

    fn update_transcript_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(anchor) = self.transcript_selection.map(|selection| selection.anchor) else {
            return false;
        };
        let Some(head) = self
            .transcript_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.point_at(column, row))
        else {
            return false;
        };

        self.transcript_selection = Some(TranscriptSelection {
            anchor,
            head,
            dragged: head != anchor,
        });
        true
    }

    fn finish_transcript_drag(&mut self) {
        if self
            .transcript_selection
            .is_some_and(|selection| !selection.dragged)
        {
            self.transcript_selection = None;
        }
    }

    #[cfg(test)]
    fn transcript_selected_text(&self) -> Option<String> {
        self.transcript_snapshot
            .as_ref()?
            .selected_text(self.transcript_selection?)
    }

    fn set_transcript_snapshot(&mut self, snapshot: TranscriptSnapshot) {
        if self
            .transcript_snapshot
            .as_ref()
            .is_some_and(|current| current != &snapshot)
        {
            self.clear_transcript_selection();
        }
        self.transcript_snapshot = Some(snapshot);
    }

    fn clear_mouse_highlight(&mut self) {
        let had_mouse_down = self.mouse_down_entry.take().is_some();
        self.clear_transcript_selection();
        if had_mouse_down {
            self.mark_dirty_full();
        }
    }

    fn clear_browse_selection(&mut self) {
        if self.mouse_down_entry.is_some()
            || self.browse_cursor.is_some()
            || self.browse_anchor.is_some()
            || !self.browse_multi.is_empty()
            || self.copy_feedback.is_some()
        {
            self.mouse_down_entry = None;
            self.browse_cursor = None;
            self.browse_anchor = None;
            self.browse_multi.clear();
            self.copy_feedback = None;
            self.mark_dirty_full();
        }
    }

    // ── Browse-mode helpers ───────────────────────────────────────

    /// True when browse mode is active (cursor is set).
    fn in_browse_mode(&self) -> bool {
        self.browse_cursor.is_some()
    }

    /// True when anything is selected — cursor, range, or multi.
    fn has_selection(&self) -> bool {
        self.browse_cursor.is_some() || !self.browse_multi.is_empty()
    }

    /// Yank a single entry's body text for explicit copy actions.
    fn yank_single_entry(&self, idx: usize) -> String {
        self.entries
            .get(idx)
            .map(clipboard_text)
            .unwrap_or_default()
    }

    /// Build the clipboard string. Single = body. Multi = role-prefixed.
    fn yank_selection(&self) -> String {
        let sel = self.selected_entries();
        let count = sel.len();
        if count == 0 {
            return String::new();
        }
        let with_label = count > 1;
        sel.into_iter()
            .filter_map(|i| self.entries.get(i))
            .map(|e| {
                if with_label {
                    labelled_clipboard_text(e)
                } else {
                    clipboard_text(e)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Enter browse mode: jump cursor to last entry, clear anchor.
    fn enter_browse_mode(&mut self) {
        self.clear_transcript_selection();
        if !self.entries.is_empty() {
            self.browse_cursor = Some(self.entries.len() - 1);
            self.browse_anchor = None;
            self.mark_dirty_full();
        }
    }

    /// Leave browse mode: clear both cursor and anchor, return to input.
    fn exit_browse_mode(&mut self) {
        self.browse_cursor = None;
        self.mouse_down_entry = None;
        self.browse_anchor = None;
        self.copy_hit_regions.clear();
        self.copy_feedback = None;
        self.mark_dirty_full();
    }

    /// Move the cursor up by `n` entries (older messages).  Clamps at 0.
    /// If `extend` is true, sets/keeps the anchor for range selection.
    /// Scrolls so the cursor entry is at the top of the viewport.
    fn browse_move_up(&mut self, n: usize, extend: bool) {
        let len = self.entries.len();
        if len == 0 {
            return;
        }
        let cur = self.browse_cursor.unwrap_or(len - 1);
        if extend && self.browse_anchor.is_none() {
            self.browse_anchor = Some(cur);
        } else if !extend {
            self.browse_anchor = None;
        }
        self.browse_cursor = Some(cur.saturating_sub(n));
        self.scroll_entry_into_view(self.browse_cursor.unwrap());
        self.pinned_to_bottom = false;
        self.mark_dirty_full();
    }

    /// Move the cursor down by `n` entries (newer messages).  Clamps at last entry.
    /// If `extend` is true, sets/keeps the anchor for range selection.
    /// Scrolls so the cursor entry is at the top of the viewport.
    fn browse_move_down(&mut self, n: usize, extend: bool) {
        let len = self.entries.len();
        if len == 0 {
            return;
        }
        let cur = self.browse_cursor.unwrap_or(0);
        if extend && self.browse_anchor.is_none() {
            self.browse_anchor = Some(cur);
        } else if !extend {
            self.browse_anchor = None;
        }
        self.browse_cursor = Some((cur + n).min(len - 1));
        self.scroll_entry_into_view(self.browse_cursor.unwrap());
        self.pinned_to_bottom =
            self.scroll_offset >= self.last_total_rows.saturating_sub(self.last_inner_height);
        self.mark_dirty_full();
    }

    /// Adjust `scroll_offset` so the entry at `entry_idx` is visible at the
    /// top of the viewport. If the entry is taller than the viewport, its
    /// top is shown.  Does nothing when `cached_screen_ranges` is empty
    /// (pre-render path).
    fn scroll_entry_into_view(&mut self, entry_idx: usize) {
        let Some(&(_, lo, _hi, _)) = self
            .cached_screen_ranges
            .iter()
            .find(|(idx, _, _, _)| *idx == entry_idx)
        else {
            return;
        };
        let inner_h = self.last_inner_height;
        if inner_h == 0 {
            return;
        }
        let total = self.last_total_rows;
        let max = total.saturating_sub(inner_h);

        // Align the entry's top with the viewport top.
        self.scroll_offset = lo.min(max);
    }

    /// The selected range as `(lo, hi)` indices, inclusive.
    /// Returns `None` when not in browse mode.
    fn browse_range(&self) -> Option<(usize, usize)> {
        let cur = self.browse_cursor?;
        let anchor = self.browse_anchor.unwrap_or(cur);
        let lo = cur.min(anchor);
        let hi = cur.max(anchor);
        Some((lo, hi))
    }

    /// True when `idx` falls inside the current browse selection range.
    fn is_in_browse_range(&self, idx: usize) -> bool {
        self.browse_range()
            .is_some_and(|(lo, hi)| idx >= lo && idx <= hi)
    }

    /// True when `idx` should render highlighted in browse mode.
    fn is_entry_highlighted(&self, idx: usize) -> bool {
        if self.browse_multi.contains(&idx) {
            return true;
        }
        if self.is_in_browse_range(idx) {
            return true;
        }
        self.browse_cursor == Some(idx)
    }

    /// Total selection: multi-select set ∪ browse range ∪ lone cursor.
    fn selected_entries(&self) -> std::collections::BTreeSet<usize> {
        let mut out = self.browse_multi.clone();
        if let Some((lo, hi)) = self.browse_range() {
            for i in lo..=hi {
                out.insert(i);
            }
        } else if let Some(c) = self.browse_cursor {
            out.insert(c);
        }
        out
    }

    fn rebuild_lines(&mut self, width: u16) {
        if self.cached_render_width != width {
            self.dirty = LinesDirty::Full;
            self.cached_render_width = width;
        }
        const MAX_RENDERED_ENTRIES: usize = 1_000;
        let total = self.entries.len();
        let natural_start = total.saturating_sub(MAX_RENDERED_ENTRIES);
        let start = if let Some((lo, _hi)) = self.browse_range() {
            natural_start.min(lo)
        } else {
            natural_start
        };

        // Incremental append path.
        if self.dirty == LinesDirty::Appended && start == self.cached_render_start {
            let render_from = start + self.cached_entry_count;
            let show_thoughts = self.show_thoughts;
            let mut new_lines = Vec::new();
            let mut new_ranges = Vec::new();
            for (rel_idx, entry) in self.entries[render_from..].iter().enumerate() {
                let abs_idx = render_from + rel_idx;
                let before = new_lines.len();
                render_entry_into(
                    entry,
                    self.is_entry_highlighted(abs_idx),
                    show_thoughts,
                    width,
                    &mut new_lines,
                );
                let after = new_lines.len();
                if after > before {
                    let base = self.cached_lines.len();
                    new_ranges.push((abs_idx, base + before, base + after));
                }
            }
            let appended_rows =
                Paragraph::new(new_lines.iter().map(borrow_line).collect::<Vec<_>>())
                    .wrap(Wrap { trim: false })
                    .line_count(width) as u16;
            self.cached_lines.extend(new_lines);
            self.cached_line_ranges.extend(new_ranges);
            self.cached_entry_count = total - start;
            self.dirty = LinesDirty::Clean;
            self.cached_total_rows = self.cached_total_rows.saturating_add(appended_rows);
            self.rebuild_screen_ranges(width);
            return;
        }

        // Full rebuild path.
        let mut lines = Vec::new();
        let mut ranges = Vec::new();
        let show_thoughts = self.show_thoughts;
        for (rel_idx, entry) in self.entries[start..].iter().enumerate() {
            let abs_idx = start + rel_idx;
            let before = lines.len();
            render_entry_into(
                entry,
                self.is_entry_highlighted(abs_idx),
                show_thoughts,
                width,
                &mut lines,
            );
            let after = lines.len();
            if after > before {
                ranges.push((abs_idx, before, after));
            }
        }
        self.cached_lines = lines;
        self.cached_line_ranges = ranges;
        self.cached_entry_count = total - start;
        self.cached_render_start = start;
        self.dirty = LinesDirty::Clean;
        self.cached_total_rows = self.compute_cached_rows(width);
        self.rebuild_screen_ranges(width);
    }

    fn visible_line_slice(&self, scroll: u16, height: u16) -> (Vec<Line<'static>>, u16) {
        if self.cached_screen_ranges.is_empty() || self.cached_line_ranges.is_empty() {
            return (self.cached_lines.clone(), scroll);
        }
        let view_end = scroll.saturating_add(height);
        let mut first: Option<usize> = None;
        let mut last: usize = 0;
        for (i, &(_, screen_lo, screen_hi, _)) in self.cached_screen_ranges.iter().enumerate() {
            if screen_hi > scroll && screen_lo < view_end {
                if first.is_none() {
                    first = Some(i);
                }
                last = i;
            }
        }
        let Some(first) = first else {
            return (self.cached_lines.clone(), scroll);
        };
        let line_lo = self.cached_line_ranges[first].1;
        let line_hi = self.cached_line_ranges[last].2;
        let local_scroll = scroll.saturating_sub(self.cached_screen_ranges[first].1);
        (self.cached_lines[line_lo..line_hi].to_vec(), local_scroll)
    }

    fn visible_copy_scan(&self, scroll: u16, height: u16) -> (Vec<Line<'static>>, u16) {
        if self.cached_screen_ranges.is_empty() || self.cached_line_ranges.is_empty() {
            return (self.cached_lines.clone(), 0);
        }
        let view_end = scroll.saturating_add(height);
        let mut first: Option<usize> = None;
        let mut last: usize = 0;
        for (i, &(_, screen_lo, screen_hi, _)) in self.cached_screen_ranges.iter().enumerate() {
            if screen_hi > scroll && screen_lo < view_end {
                if first.is_none() {
                    first = Some(i);
                }
                last = i;
            }
        }
        let Some(first) = first else {
            return (self.cached_lines.clone(), 0);
        };
        let line_lo = self.cached_line_ranges[first].1;
        let line_hi = self.cached_line_ranges[last].2;
        (
            self.cached_lines[line_lo..line_hi].to_vec(),
            self.cached_screen_ranges[first].1,
        )
    }

    /// Recompute `cached_screen_ranges` from `cached_line_ranges` by wrapping
    /// each entry's `Line`s individually, so screen row positions reflect
    /// markdown wrapping (code blocks, tables, etc.). Called after every
    /// cache rebuild so mouse hit-testing in `entry_rects` stays accurate.
    fn rebuild_screen_ranges(&mut self, width: u16) {
        self.cached_screen_ranges.clear();
        let mut screen_cursor = 0u16;
        for &(entry_idx, lo, hi) in &self.cached_line_ranges {
            let entry_lines = self.cached_lines[lo..hi]
                .iter()
                .map(borrow_line)
                .collect::<Vec<_>>();
            if entry_lines.is_empty() {
                continue;
            }
            // Widest rendered column extent of the entry, clamped to the
            // viewport. Lines wider than `width` wrap to full-width rows, so the
            // clamp yields the true on-screen extent. Hit-testing uses this so
            // the blank space beside a short message is treated as outside the
            // entry.
            let content_width = entry_lines
                .iter()
                .map(|l| l.width() as u16)
                .max()
                .unwrap_or(0)
                .min(width);
            let wrapped = Paragraph::new(entry_lines)
                .wrap(Wrap { trim: false })
                .line_count(width) as u16;
            let screen_lo = screen_cursor;
            screen_cursor += wrapped;
            self.cached_screen_ranges
                .push((entry_idx, screen_lo, screen_cursor, content_width));
        }
    }

    fn rebuild_copy_regions(&mut self, width: u16, scroll: u16, body: Rect) {
        let copy_lbl = " [Copy] ";
        let mut regions: Vec<CopyHitRegion> = Vec::new();
        let (lines, mut screen_cursor) = self.visible_copy_scan(scroll, body.height);
        let mut pending: Option<(u16, u16, u16, usize, Option<String>, String)> = None;
        for line in &lines {
            let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            if first.starts_with('\u{250c}') {
                let lang = header_fence_lang(line);
                pending = label_cells(line, copy_lbl).map(|(col, cells)| {
                    (
                        screen_cursor,
                        col,
                        cells,
                        screen_cursor as usize,
                        lang,
                        String::new(),
                    )
                });
            } else if first.starts_with('\u{2514}') {
                if let Some((header_row, header_col, header_cells, group, lang, acc)) =
                    pending.take()
                {
                    let text = fenced_text(lang.as_deref(), &acc);
                    if let Some(r) = copy_region(
                        header_row,
                        header_col,
                        header_cells,
                        scroll,
                        body,
                        &text,
                        group,
                    ) {
                        regions.push(r);
                    }
                    if let Some((footer_col, footer_cells)) = label_cells(line, copy_lbl)
                        && let Some(r) = copy_region(
                            screen_cursor,
                            footer_col,
                            footer_cells,
                            scroll,
                            body,
                            &text,
                            group,
                        )
                    {
                        regions.push(r);
                    }
                }
            } else if let Some((_, _, _, _, _, acc)) = pending.as_mut() {
                let full: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let body_text = full.strip_prefix("  ").unwrap_or(&full).to_string();
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(&body_text);
            }

            screen_cursor += wrapped_rows(line, width);
        }
        self.copy_hit_regions = regions;
    }

    fn message_copy_region(&self, body: Rect) -> Option<CopyHitRegion> {
        let selected = self.selected_entries();
        let idx = if selected.len() == 1 {
            *selected.iter().next()?
        } else {
            return None;
        };
        let (_, rect) = self
            .entry_rects
            .iter()
            .find(|(entry_idx, _)| *entry_idx == idx)?;
        if rect.height == 0 {
            return None;
        }
        let text = self.yank_single_entry(idx);
        if text.is_empty() {
            return None;
        }
        let label = message_copy_label();
        Some(CopyHitRegion {
            rect: centered_message_copy_rect(&label, *rect, body)?,
            text,
            kind: CopyHitKind::Message,
            group: idx,
        })
    }

    fn rebuild_message_copy_region(&mut self, body: Rect) {
        if let Some(region) = self.message_copy_region(body) {
            self.copy_hit_regions.push(region);
        }
    }

    fn compute_cached_rows(&self, width: u16) -> u16 {
        Paragraph::new(
            self.cached_lines
                .iter()
                .map(borrow_line)
                .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.clear_transcript_selection();
        self.pinned_to_bottom = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.clear_transcript_selection();
        let max = self.last_total_rows.saturating_sub(self.last_inner_height);
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(max);
        if self.scroll_offset >= max {
            self.pinned_to_bottom = true;
        }
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.last_inner_height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.last_inner_height.max(1));
    }

    pub fn scroll_to_top(&mut self) {
        self.clear_transcript_selection();
        self.pinned_to_bottom = false;
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.clear_transcript_selection();
        let max = self.last_total_rows.saturating_sub(self.last_inner_height);
        self.scroll_offset = max;
        self.pinned_to_bottom = true;
    }

    pub fn title(&self) -> String {
        self.title_parts()
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn title_parts(&self) -> Vec<(Option<TitleHitTarget>, String)> {
        let short = self.session_id.get(..7).unwrap_or(self.session_id.as_str());
        let mut parts: Vec<(Option<TitleHitTarget>, String)> = Vec::with_capacity(5);
        parts.push((Some(TitleHitTarget::Agent), self.agent_alias.clone()));
        if let Some(ref name) = self.session_name {
            parts.push((None, format!("— {name}")));
        }
        parts.push((None, short.to_string()));
        if let Some(ref provider) = self.model_provider_ref {
            parts.push((Some(TitleHitTarget::ModelProvider), provider.clone()));
        }
        if let Some(ref model) = self.model {
            parts.push((Some(TitleHitTarget::Model), model.clone()));
        }
        parts
    }

    fn refresh_title_hit_rects(&mut self, area: Rect) {
        self.title_hit_rects.clear();
        let mut x = area.x.saturating_add(2);
        let right = area.x.saturating_add(area.width);
        for (idx, (target, text)) in self.title_parts().into_iter().enumerate() {
            if idx > 0 {
                x = x.saturating_add(2);
            }
            let width = crate::display_width::display_width(text.as_str()) as u16;
            if let Some(target) = target
                && width > 0
                && x < right
            {
                self.title_hit_rects.push(TitleHitRect {
                    target,
                    rect: Rect::new(x, area.y, width.min(right.saturating_sub(x)), 1),
                });
            }
            x = x.saturating_add(width);
        }
    }

    fn title_hit_target_at(&self, col: u16, row: u16) -> Option<TitleHitTarget> {
        self.title_hit_rects
            .iter()
            .find(|hit| mouse::in_rect(col, row, hit.rect))
            .map(|hit| hit.target)
    }

    pub fn set_model_identity(&mut self, model_provider_ref: Option<&str>, model: Option<&str>) {
        if let Some(r) = model_provider_ref {
            self.model_provider_ref = Some(r.to_string());
        }
        if let Some(m) = model {
            self.model = Some(m.to_string());
        }
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[ChatEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub fn current_agent_text(&self) -> &str {
        &self.streaming_text
    }

    #[cfg(test)]
    pub fn current_thought_text(&self) -> &str {
        &self.streaming_thought
    }

    pub fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_approval.as_ref()
    }

    pub fn take_pending_approval(&mut self) -> Option<PendingApproval> {
        self.pending_approval.take()
    }

    pub fn pending_elicitation(&self) -> Option<&PendingElicitation> {
        self.pending_elicitation.as_ref()
    }

    #[cfg(test)]
    pub fn take_pending_elicitation(&mut self) -> Option<PendingElicitation> {
        self.pending_elicitation.take()
    }

    /// Install a pending elicitation modal. Replaces any prior one (the
    /// daemon serializes elicitations per session, so a second arrival
    /// before the first is answered is a protocol anomaly we resolve by
    /// keeping the newest).
    pub fn set_pending_elicitation(&mut self, e: PendingElicitation) {
        self.pending_elicitation = Some(e);
        self.mark_dirty_full();
    }

    /// Commit any accumulated streaming thought as an entry. Called at the two
    /// natural flush points: when a tool call interrupts thinking, and when the
    /// first response text chunk arrives after a thinking phase.
    fn flush_streaming_thought(&mut self) {
        let thought = std::mem::take(&mut self.streaming_thought);
        if !thought.is_empty() {
            self.entries
                .push(ChatEntry::AgentThought(Arc::<str>::from(thought)));
            self.mark_dirty_append();
        }
    }

    /// Commit any accumulated streaming text as an `AgentMessage` entry.
    /// Called when a tool call interrupts the text stream so that pre-tool
    /// text is committed in conversation order before the `Tool` entry.
    /// Returns `true` if any text was flushed.
    fn flush_streaming_text(&mut self) -> bool {
        let text = std::mem::take(&mut self.streaming_text);
        if !text.is_empty() {
            self.entries
                .push(ChatEntry::AgentMessage(Arc::<str>::from(text)));
            self.mark_dirty_append();
            true
        } else {
            false
        }
    }

    pub fn apply_update(&mut self, update: SessionUpdate) {
        // Ignore notifications that belong to a different session.
        let update_sid = match &update {
            SessionUpdate::AgentMessageChunk { session_id, .. }
            | SessionUpdate::AgentThoughtChunk { session_id, .. }
            | SessionUpdate::ToolCall { session_id, .. }
            | SessionUpdate::ToolResult { session_id, .. }
            | SessionUpdate::ApprovalRequest { session_id, .. }
            | SessionUpdate::ContextUsage { session_id, .. }
            | SessionUpdate::HistoryTrimmed { session_id, .. }
            | SessionUpdate::TurnComplete { session_id, .. }
            | SessionUpdate::Plan { session_id, .. } => session_id.as_str(),
        };
        if update_sid != self.session_id {
            return;
        }

        match update {
            SessionUpdate::AgentMessageChunk { text, .. } => {
                // Flush any accumulated thought before the response text begins
                // so it appears inline at the right position, not piled at the end.
                if self.streaming_text.is_empty() {
                    self.flush_streaming_thought();
                }
                self.streaming_text.push_str(&text);
                // Guard: don't mutate turn_status after commit_turn has already
                // set us back to Idle. Late-arriving notifications (broadcast
                // channel lag) can otherwise flip the input bar back to the
                // working animator even though the turn is done.
                if self.turn_in_flight {
                    self.turn_status = TurnStatus::Responding;
                }
            }
            SessionUpdate::AgentThoughtChunk { text, .. } => {
                self.streaming_thought.push_str(&text);
                if self.turn_in_flight {
                    self.turn_status = TurnStatus::Thinking;
                }
            }
            SessionUpdate::ToolCall {
                tool_call_id,
                name,
                raw_input,
                ..
            } => {
                // Flush any accumulated text and thought before the tool call
                // so that pre-tool agent text and thinking both appear in
                // conversation order before the Tool entry.
                if self.flush_streaming_text() {
                    self.turn_had_streaming_text = true;
                }
                self.flush_streaming_thought();
                self.turn_had_tool_calls = true;
                if self.turn_in_flight {
                    self.turn_status = TurnStatus::CallingTool(name.clone());
                }
                self.entries.push(ChatEntry::Tool {
                    tool_call_id: Arc::<str>::from(tool_call_id),
                    name: Arc::<str>::from(name),
                    input_json: Arc::<str>::from(
                        serde_json::to_string(&raw_input).unwrap_or_default(),
                    ),
                    result: None,
                });
                self.mark_dirty_append();
            }
            SessionUpdate::ToolResult {
                tool_call_id,
                raw_output,
                ..
            } => {
                // Cap stored output so large tool responses (bash, file reads) don't
                // accumulate unboundedly.  The renderer already truncates to 200 chars
                // for display; 16 KB gives clipboard users a generous but bounded copy.
                const MAX_RAW_OUTPUT: usize = 16 * 1024;
                let raw_output = if raw_output.len() > MAX_RAW_OUTPUT {
                    format!("{}…[truncated]", truncate_utf8(&raw_output, MAX_RAW_OUTPUT))
                } else {
                    raw_output
                };
                for entry in self.entries.iter_mut().rev() {
                    if let ChatEntry::Tool {
                        tool_call_id: id,
                        result,
                        ..
                    } = entry
                        && id.as_ref() == tool_call_id.as_str()
                    {
                        *result = Some(Arc::<str>::from(raw_output));
                        self.mark_dirty_full(); // mutation of existing entry
                        break;
                    }
                }
                if self.turn_in_flight && matches!(self.turn_status, TurnStatus::CallingTool(_)) {
                    self.turn_status = TurnStatus::Working;
                }
            }
            SessionUpdate::ApprovalRequest {
                request_id,
                tool_name,
                arguments_summary,
                timeout_secs,
                ..
            } => {
                self.pending_approval = Some(PendingApproval {
                    request_id,
                    tool_name,
                    arguments_summary,
                    timeout_secs,
                });
                if self.turn_in_flight {
                    self.turn_status = TurnStatus::WaitingForApproval;
                }
            }
            SessionUpdate::ContextUsage {
                input_tokens,
                max_context_tokens,
                ..
            } => {
                if input_tokens.is_some() {
                    self.context_input_tokens = input_tokens;
                }
                if max_context_tokens.is_some() {
                    self.context_max_tokens = max_context_tokens;
                }
            }
            SessionUpdate::HistoryTrimmed {
                dropped_messages,
                kept_turns,
                reason,
                ..
            } => {
                let dropped = dropped_messages.to_string();
                let kept = kept_turns.to_string();
                let notice = crate::i18n::t_args(
                    "zc-chat-history-trimmed",
                    &[("reason", &reason), ("dropped", &dropped), ("kept", &kept)],
                );
                self.entries
                    .push(ChatEntry::SystemMessage(Arc::<str>::from(notice)));
                self.mark_dirty_append();
            }
            SessionUpdate::TurnComplete {
                outcome, content, ..
            } => match outcome {
                TurnEndOutcome::Completed => {
                    self.commit_turn(content, true);
                }
                TurnEndOutcome::Cancelled | TurnEndOutcome::Failed => {
                    self.entries
                        .push(ChatEntry::SystemMessage(Arc::<str>::from(content.as_str())));
                    self.mark_dirty_append();
                    self.commit_turn(String::new(), false);
                }
            },
            // Whole-list replace: hand the authoritative plan to the
            // tracker, which runs the auto-pop rule. Session routing is
            // already enforced by the session_id check above.
            SessionUpdate::Plan { entries, .. } => {
                self.todo_tracker.set_plan(entries);
            }
        }
    }

    pub fn commit_turn(&mut self, full_text: String, clean: bool) {
        if self.flush_streaming_text() {
            self.turn_had_streaming_text = true;
        }
        self.flush_streaming_thought();
        // If no streaming text was accumulated during this turn, use the
        // daemon-provided final text as a fallback so the turn is never
        // invisible to the user.
        if !self.turn_had_streaming_text && !full_text.is_empty() {
            self.entries
                .push(ChatEntry::AgentMessage(Arc::<str>::from(full_text)));
            self.mark_dirty_append();
        } else if clean
            && !self.turn_had_streaming_text
            && !self.turn_had_tool_calls
            && full_text.is_empty()
        {
            // Clean completion with no streamed text, no tool calls, and
            // no final content — render a diagnostic so the user knows the
            // turn finished rather than silently vanishing.
            self.entries
                .push(ChatEntry::SystemMessage(Arc::<str>::from(crate::i18n::t(
                    "zc-turn-no-output",
                ))));
            self.mark_dirty_append();
        }
        self.turn_had_streaming_text = false;
        self.turn_had_tool_calls = false;
        self.mark_dirty_append();
        self.turn_in_flight = false;
        self.turn_status = TurnStatus::Idle;
        self.cancel_started_at = None;
        self.input_bar.cleanup_temps();
        if !clean && !self.resume_override && !self.message_queue.is_empty() {
            self.queue_paused = true;
        }
        self.resume_override = false;
    }

    pub fn enter_cancelling(&mut self) {
        self.turn_status = TurnStatus::Cancelling;
        self.cancel_started_at = Some(Instant::now());
    }

    pub fn cancel_watchdog_expired(&self) -> bool {
        matches!(self.turn_status, TurnStatus::Cancelling)
            && self
                .cancel_started_at
                .is_some_and(|t| t.elapsed() >= CANCEL_WATCHDOG)
    }

    pub fn push_user_message(&mut self, text: Option<String>, attachments: Vec<String>) {
        if self.first_message.is_none()
            && let Some(ref t) = text
            && !t.trim().is_empty()
        {
            self.first_message = Some(t.clone());
        }
        self.entries.push(ChatEntry::UserMessage {
            text: text.map(Arc::<str>::from),
            attachments: attachments.into_iter().map(Arc::<str>::from).collect(),
        });
        self.mark_dirty_append();
        self.turn_in_flight = true;
        self.turn_had_streaming_text = false;
        self.turn_had_tool_calls = false;
        // Start a fresh status + animation anchor. We're `Working` until the
        // first chunk (thought / message / tool-call) tells us otherwise.
        self.turn_status = TurnStatus::Working;
        self.turn_started_at = Instant::now();
    }

    const QUEUE_CAP: usize = 32;
    const QUEUE_SIDEBAR_COLS_MIN: u16 = 24;
    const QUEUE_SIDEBAR_COLS_MAX: u16 = 80;
    const QUEUE_SIDEBAR_COLS_STEP: u16 = 4;
    const QUEUE_CHAT_COLS_MIN: u16 = 20;

    fn alloc_queue_id(&mut self) -> u64 {
        let id = self.next_queue_id;
        self.next_queue_id = self.next_queue_id.wrapping_add(1);
        id
    }

    pub fn enqueue_message(
        &mut self,
        text: String,
        attachments: Vec<PendingAttachment>,
    ) -> Result<(), String> {
        if text.trim().is_empty() && attachments.is_empty() {
            return Err(crate::i18n::t("zc-queue-empty"));
        }
        let pending = self.message_queue.len();
        if pending >= Self::QUEUE_CAP {
            return Err(crate::i18n::t_args(
                "zc-queue-full",
                &[("cap", &Self::QUEUE_CAP.to_string())],
            ));
        }
        let id = self.alloc_queue_id();
        self.message_queue.push_back(QueuedMessage {
            id,
            text,
            attachments,
            status: QueueItemStatus::Pending,
        });
        Ok(())
    }

    pub fn inject_message(
        &mut self,
        text: String,
        attachments: Vec<PendingAttachment>,
    ) -> Result<(), String> {
        if text.trim().is_empty() && attachments.is_empty() {
            return Err(crate::i18n::t("zc-queue-empty"));
        }
        if self.message_queue.len() >= Self::QUEUE_CAP {
            return Err(crate::i18n::t_args(
                "zc-queue-full",
                &[("cap", &Self::QUEUE_CAP.to_string())],
            ));
        }
        let id = self.alloc_queue_id();
        let insert_at = self
            .message_queue
            .iter()
            .position(|m| m.status == QueueItemStatus::Pending)
            .unwrap_or(self.message_queue.len());
        self.message_queue.insert(
            insert_at,
            QueuedMessage {
                id,
                text,
                attachments,
                status: QueueItemStatus::Injected,
            },
        );
        // An inject is the force-send-now intent: resume the queue and let it
        // survive a cancel auto-pause, unlike a plain queued submission.
        self.queue_paused = false;
        if self.turn_in_flight {
            self.resume_override = true;
        }
        Ok(())
    }

    fn next_dispatch_index(&self) -> Option<usize> {
        if self.turn_in_flight {
            return None;
        }
        if let Some(idx) = self
            .message_queue
            .iter()
            .position(|m| m.status == QueueItemStatus::Injected)
        {
            return Some(idx);
        }
        if self.queue_paused {
            return None;
        }
        self.message_queue
            .iter()
            .position(|m| m.status == QueueItemStatus::Pending)
    }

    pub fn take_next_dispatchable(&mut self) -> Option<QueuedMessage> {
        let idx = self.next_dispatch_index()?;
        let msg = self.message_queue.remove(idx)?;
        self.resume_override = false;
        if self.queue_sel == Some(msg.id) {
            self.queue_sel = None;
        }
        Some(msg)
    }

    /// Flip the queue pause state. Returns the new paused value so the caller
    /// can pump on resume and surface the right notice.
    pub fn toggle_queue_pause(&mut self) -> bool {
        self.queue_paused = !self.queue_paused;
        self.queue_paused
    }

    pub fn queue_paused(&self) -> bool {
        self.queue_paused
    }

    /// Clear an explicit pause without bypassing the cancel auto-pause: a
    /// cancelled turn settles into the paused state and the backlog waits for a
    /// deliberate resume. Returns true if the queue was paused.
    pub fn resume_queue(&mut self) -> bool {
        let was_paused = self.queue_paused;
        self.queue_paused = false;
        was_paused
    }

    pub fn queue_len(&self) -> usize {
        self.message_queue.len()
    }

    /// Store a transient note for the info bar (queue/attach/detach feedback).
    /// Routes through the shared `info_message` bar so it inherits TTL auto-clear
    /// and consistent rendering with model-switch notes.
    pub fn set_info_notice(&mut self, msg: String) {
        self.info_message = Some(crate::widgets::InfoMessage::note(msg));
        self.mark_dirty_full();
    }

    fn set_overlay_copy_feedback(&mut self, anchor: Rect) {
        if let Some(rect) = centered_copy_feedback_rect(&message_copied_label(), anchor) {
            self.set_copy_feedback(CopyFeedbackTarget::Overlay(rect));
        }
    }

    fn set_copy_feedback(&mut self, target: CopyFeedbackTarget) {
        self.copy_feedback = Some(CopyFeedback {
            target,
            shown_at: Instant::now(),
        });
        self.mark_dirty_full();
    }

    /// Drop the active info-bar message (on submit, inject, or turn start).
    pub fn clear_info_notice(&mut self) {
        if self.info_message.take().is_some() || self.copy_feedback.take().is_some() {
            self.mark_dirty_full();
        }
    }

    fn expire_copy_feedback(&mut self) {
        let expired = self
            .copy_feedback
            .is_some_and(|feedback| feedback.shown_at.elapsed() >= COPY_FEEDBACK_TTL);
        if expired {
            self.copy_feedback = None;
            self.mark_dirty_full();
        }
    }

    /// The queue sidebar is open exactly when the queue is non-empty. There is
    /// no manual toggle: it appears with the first queued message and closes
    /// when the queue drains, so its presence always reflects real state.
    pub fn queue_sidebar_open(&self) -> bool {
        !self.message_queue.is_empty()
    }

    /// Default the sidebar selection to the front item when nothing is selected
    /// yet (e.g. the first message just opened the sidebar). Keeps keyboard
    /// delete/edit working without a manual open step.
    pub fn ensure_queue_selection(&mut self) {
        if self.queue_sel.is_none()
            && let Some(front) = self.message_queue.front()
        {
            self.queue_sel = Some(front.id);
        }
    }

    /// Select a queued item by id (mouse left-click in the sidebar). Ignores
    /// ids no longer present. Returns true when the selection changed.
    pub fn select_queued_by_id(&mut self, id: u64) -> bool {
        if self.message_queue.iter().any(|m| m.id == id) && self.queue_sel != Some(id) {
            self.queue_sel = Some(id);
            self.mark_dirty_full();
            true
        } else {
            false
        }
    }

    /// Hit-test a screen point against the last sidebar draw and select the
    /// queued item under it, if any. Returns true when something was selected.
    pub fn queue_click_at(&mut self, col: u16, row: u16) -> bool {
        let hit = self
            .queue_item_rects
            .iter()
            .find(|(_, r)| mouse::in_rect(col, row, *r))
            .map(|(id, _)| *id);
        match hit {
            Some(id) => self.select_queued_by_id(id),
            None => false,
        }
    }

    /// True when the point lies within the last drawn sidebar inner rect.
    pub fn point_in_queue_sidebar(&self, col: u16, row: u16) -> bool {
        self.queue_sidebar_rect
            .is_some_and(|r| mouse::in_rect(col, row, r))
    }

    /// Scroll the queue sidebar by `delta` rows (negative = up). Clamped to the
    /// content overflow recorded on the last draw.
    pub fn queue_scroll_by(&mut self, delta: i16) {
        let new = (self.queue_scroll as i32 + delta as i32).max(0) as u16;
        if new != self.queue_scroll {
            self.queue_scroll = new;
            self.mark_dirty_full();
        }
    }

    pub fn widen_queue_sidebar(&mut self) {
        self.queue_sidebar_cols = (self.queue_sidebar_cols + Self::QUEUE_SIDEBAR_COLS_STEP)
            .min(Self::QUEUE_SIDEBAR_COLS_MAX);
        self.mark_dirty_full();
    }

    pub fn narrow_queue_sidebar(&mut self) {
        self.queue_sidebar_cols = self
            .queue_sidebar_cols
            .saturating_sub(Self::QUEUE_SIDEBAR_COLS_STEP)
            .max(Self::QUEUE_SIDEBAR_COLS_MIN);
        self.mark_dirty_full();
    }

    /// Queue sidebar width in columns for a given chat area width. The stored
    /// column width is clamped to the absolute range, then to whatever leaves
    /// the chat column its floor on a terminal too narrow for both.
    pub fn queue_sidebar_width(&self, area_width: u16) -> u16 {
        let upper =
            Self::QUEUE_SIDEBAR_COLS_MAX.min(area_width.saturating_sub(Self::QUEUE_CHAT_COLS_MIN));
        let lower = Self::QUEUE_SIDEBAR_COLS_MIN.min(upper);
        self.queue_sidebar_cols.clamp(lower, upper)
    }

    fn editable_ids(&self) -> Vec<u64> {
        self.message_queue.iter().map(|m| m.id).collect()
    }

    pub fn queue_select_step(&mut self, delta: isize) {
        let ids = self.editable_ids();
        if ids.is_empty() {
            self.queue_sel = None;
            return;
        }
        let cur = self
            .queue_sel
            .and_then(|id| ids.iter().position(|&x| x == id))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(ids.len() as isize) as usize;
        self.queue_sel = Some(ids[next]);
        self.mark_dirty_full();
    }

    pub fn delete_selected_queued(&mut self) {
        let Some(id) = self.queue_sel else { return };
        if let Some(pos) = self.message_queue.iter().position(|m| m.id == id) {
            if let Some(msg) = self.message_queue.remove(pos) {
                cleanup_attachment_temps(&msg.attachments);
            }
            let ids = self.editable_ids();
            self.queue_sel = ids.get(pos.min(ids.len().saturating_sub(1))).copied();
            self.mark_dirty_full();
        }
    }

    pub fn take_selected_for_edit(&mut self) -> Option<(String, Vec<PendingAttachment>)> {
        let id = self.queue_sel?;
        let pos = self.message_queue.iter().position(|m| m.id == id)?;
        let msg = self.message_queue.remove(pos)?;
        self.queue_sel = self.editable_ids().first().copied();
        self.mark_dirty_full();
        Some((msg.text, msg.attachments))
    }

    /// Slash-command queue removal. `None` clears the whole queue; `Some(n)`
    /// removes the 1-based item shown in the sidebar. Returns a user-facing
    /// info-bar message. `Some(0)` is the invalid-index sentinel from a
    /// malformed `/clear-queue` arg.
    pub fn clear_queue_cmd(&mut self, index: Option<usize>) -> String {
        let count = self.message_queue.len();
        match index {
            None => {
                if count == 0 {
                    return crate::i18n::t("zc-queue-clear-empty");
                }
                self.clear_queue();
                self.mark_dirty_full();
                crate::i18n::t_args("zc-queue-cleared-all", &[("count", &count.to_string())])
            }
            Some(n) => {
                if count == 0 {
                    return crate::i18n::t("zc-queue-clear-empty");
                }
                if n == 0 || n > count {
                    return crate::i18n::t_args(
                        "zc-queue-clear-invalid",
                        &[("index", &n.to_string()), ("count", &count.to_string())],
                    );
                }
                let pos = n - 1;
                if let Some(msg) = self.message_queue.remove(pos) {
                    cleanup_attachment_temps(&msg.attachments);
                    if self.queue_sel == Some(msg.id) {
                        let ids = self.editable_ids();
                        self.queue_sel = ids.get(pos.min(ids.len().saturating_sub(1))).copied();
                    }
                }
                self.mark_dirty_full();
                crate::i18n::t_args("zc-queue-cleared-one", &[("index", &n.to_string())])
            }
        }
    }

    fn clear_queue(&mut self) {
        for msg in self.message_queue.drain(..) {
            cleanup_attachment_temps(&msg.attachments);
        }
        self.next_queue_id = 0;
        self.queue_paused = false;
        self.resume_override = false;
        self.queue_sel = None;
    }

    fn load_history(&mut self, messages: Vec<crate::client::MessageEntry>) {
        for m in messages {
            match m.role() {
                crate::client::MessageRole::User => {
                    if self.first_message.is_none() {
                        self.first_message = Some(m.content.clone());
                    }
                    self.entries.push(ChatEntry::UserMessage {
                        text: Some(Arc::<str>::from(m.content)),
                        attachments: vec![],
                    });
                }
                crate::client::MessageRole::Assistant => {
                    self.entries
                        .push(ChatEntry::AgentMessage(Arc::<str>::from(m.content)));
                }
                crate::client::MessageRole::System | crate::client::MessageRole::Other => {}
            }
        }
        self.mark_dirty_full();
    }
    /// Reset conversational state for a new or switched session.
    pub fn reset_for_session(&mut self, session_id: String, name: Option<String>) {
        self.session_id = session_id;
        self.session_name = name;
        self.model_provider_ref = None;
        self.model = None;
        self.input_bar.reset();
        self.entries.clear();
        self.streaming_text.clear();
        self.streaming_thought.clear();
        self.cached_lines.clear();
        self.entry_rects.clear();
        self.copy_hit_regions.clear();
        self.copy_feedback = None;
        self.dirty = LinesDirty::Full;
        self.cached_entry_count = 0;
        self.cached_render_start = 0;
        self.cached_render_width = 0;
        self.pending_approval = None;
        self.pending_elicitation = None;
        self.turn_in_flight = false;
        self.turn_status = TurnStatus::Idle;
        self.cancel_started_at = None;
        self.browse_cursor = None;
        self.browse_anchor = None;
        self.mouse_down_entry = None;
        self.transcript_snapshot = None;
        self.transcript_selection = None;
        self.browse_multi.clear();
        // Reset branch cache: new session may have a different cwd.
        self.git_branch = None;
        self.first_message = None;
        self.git_hash = None;
        self.git_branch_last_fetch = None;
        // Context usage is per-session; clear so we don't show stale numbers
        // from the previous session before the first LLM call fires a new
        // ContextUsage event.
        self.context_input_tokens = None;
        self.context_max_tokens = None;
        self.clear_queue();
    }
}

/// Body-only clipboard text.
fn clipboard_text(entry: &ChatEntry) -> String {
    match entry {
        ChatEntry::UserMessage { text, attachments } => {
            let base = text.as_deref().unwrap_or("");
            if attachments.is_empty() {
                base.to_string()
            } else {
                let label = attachments
                    .iter()
                    .map(|a| a.as_ref())
                    .collect::<Vec<&str>>()
                    .join(", ");
                format!("{base} [{label}]")
            }
        }
        ChatEntry::AgentMessage(t) => t.to_string(),
        ChatEntry::AgentThought(t) => format!("(thinking) {t}"),
        ChatEntry::SystemMessage(t) => t.to_string(),
        ChatEntry::Tool {
            name,
            input_json,
            result,
            ..
        } => match result {
            Some(r) => format!("[tool: {name}] {input_json}\n  \u{2514}\u{2500} {r}"),
            None => format!("[tool: {name}] {input_json}"),
        },
    }
}

/// Role-prefixed clipboard text. Used when ≥2 entries are yanked.
fn labelled_clipboard_text(entry: &ChatEntry) -> String {
    match entry {
        ChatEntry::UserMessage { .. } => {
            crate::i18n::t_args("zc-chat-clipboard-you", &[("text", &clipboard_text(entry))])
        }
        ChatEntry::AgentMessage(_) => crate::i18n::t_args(
            "zc-chat-clipboard-agent",
            &[("text", &clipboard_text(entry))],
        ),
        _ => clipboard_text(entry),
    }
}

/// Suspend the TUI, open `$VISUAL` / `$EDITOR` with `content`, return the edited text.
/// Restores raw mode and alternate screen before returning.
/// Falls back to `content` unchanged if no editor is available or the process fails.
pub async fn open_editor_for_content(content: &str) -> String {
    let Some(editor) = crate::editor::editor_from_env_or_path() else {
        return content.to_string();
    };

    let tmp = match tempfile::NamedTempFile::new() {
        Ok(f) => f,
        Err(_) => return content.to_string(),
    };
    if std::fs::write(tmp.path(), content).is_err() {
        return content.to_string();
    }

    crossterm::terminal::disable_raw_mode().ok();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::terminal::LeaveAlternateScreen
    );

    let path = tmp.path().to_owned();
    let status = tokio::process::Command::new(&editor)
        .arg(&path)
        .status()
        .await;

    crossterm::terminal::enable_raw_mode().ok();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
    );
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            )
        );
    }

    if status.map(|s| s.success()).unwrap_or(false) {
        std::fs::read_to_string(&path).unwrap_or_else(|_| content.to_string())
    } else {
        content.to_string()
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
