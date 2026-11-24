use super::*;
use tcode_core::notice::{
    CHILD_REPORT_REMINDER, ORCHESTRATE_APPROVAL_PHRASE, ORCHESTRATE_FALLBACK_LABEL,
    ORCHESTRATE_OUTPUT_TAIL_LABEL, ORCHESTRATE_QUESTION_PHRASE, ORCHESTRATE_REPORT_LABEL,
    callback_header,
};
use tcode_core::project::ChildExpectation;
use tcode_core::settings::{OrchestrateChildModel, orchestrate_efforts};
use tcode_core::settlement::{AgentDelivery, AgentReport};

/// Appended to every dispatched brief so the report contract reaches the child
/// regardless of what the orchestrator wrote.
pub(super) const CHILD_REPORT_FOOTER: &str = "\n\n---\nThe tcode_report report_result tool is the only channel through which your work reaches the orchestrator that dispatched you; it cannot see your transcript, and your own turns, waits, background tasks and wake-ups never reach it. When your work is complete, send your complete final report through it once, then end your turn. Make the report self-contained: for a discussion, give your reasoning, recommendations, disagreements, and open questions; for execution, include files changed and commands actually run with their outcomes. Include the evidence behind your conclusions. Only if the tool call fails, write that same complete report as your final message instead — it is sent back as the fallback, truncated when long.";

/// A thread's pull request tool registration.
pub(super) struct PullRequestRegistration {
    pub(super) registration: agent::McpRegistration,
    /// The host names the tools were described with.
    pub(super) hosts: String,
    /// The linking instructions the provider is given with the tools.
    pub(super) instructions: String,
}

#[derive(Default)]
pub(super) struct McpWiring {
    pub(super) pull_request_url: Option<String>,
    pub(super) pull_request_tokens: Option<pull_request_mcp::TokenRegistry>,
    /// The hosts the pull request tools' descriptions name.
    pub(super) pull_request_hosts: Option<pull_request_mcp::HostNames>,
    pub(super) pull_request_registrations: HashMap<String, PullRequestRegistration>,
    pub(super) preview_url: Option<String>,
    pub(super) preview_tokens: Option<preview_mcp::TokenRegistry>,
    pub(super) preview_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) orchestrate_url: Option<String>,
    pub(super) orchestrate_tokens: Option<orchestrate_mcp::TokenRegistry>,
    pub(super) orchestrate_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) orchestrate_child_url: Option<String>,
    pub(super) orchestrate_child_tokens: Option<orchestrate_mcp::ChildTokenRegistry>,
    pub(super) orchestrate_child_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) orchestrate_requests:
        Option<smol::channel::Receiver<orchestrate_mcp::BrokerRequest>>,
    pub(super) computer_use_url: Option<String>,
    pub(super) computer_use_tokens: Option<computer_use_mcp::TokenRegistry>,
    pub(super) computer_use_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) control_url: Option<String>,
    pub(super) control_tokens: Option<control_mcp::TokenRegistry>,
    pub(super) control_registrations: HashMap<String, agent::McpRegistration>,
    pub(super) control_requests: Option<smol::channel::Receiver<control_mcp::BrokerRequest>>,
    /// Each registered thread's effective grant, read by the control server when a
    /// connection opens to list the tools that grant allows.
    pub(super) control_grants:
        Arc<std::sync::Mutex<HashMap<String, tcode_core::control::ControlGrant>>>,
    /// The control server's record of the tools each thread's connection received.
    pub(super) control_source: Option<control_mcp::GrantSource>,
}

impl AppState {
    pub(crate) fn pump_preview_requests(
        &mut self,
        requests: Option<async_channel::Receiver<preview_mcp::BrokerRequest>>,
        cx: &mut HostCx,
    ) {
        let Some(requests) = requests else {
            return;
        };
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(request) = requests.recv().await {
                host.enqueue(move |state, cx| state.route_preview(request, cx));
            }
        });
    }

    pub(crate) fn route_preview(&mut self, broker: preview_mcp::BrokerRequest, cx: &mut HostCx) {
        self.route_preview_with_timeout(broker, Duration::from_secs(60), cx);
    }

    pub(crate) fn route_preview_with_timeout(
        &mut self,
        broker: preview_mcp::BrokerRequest,
        timeout: Duration,
        cx: &mut HostCx,
    ) {
        // The agent's thread need not be on anyone's screen: any client with a
        // preview browser answers. Without one, waiting out the timeout only
        // delays the same answer.
        if !self.subscriptions.contains(&Topic::Preview) {
            let _ = broker.reply.try_send(Err(
                "preview is unavailable: no connected client has a preview browser".into(),
            ));
            return;
        }
        self.next_preview_request += 1;
        let request_id = self.next_preview_request;
        self.preview_pending.insert(request_id, broker.reply);
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::Preview,
            event: ServerEvent::PreviewRequest {
                request_id,
                session_id: broker.session_id,
                request: broker.op,
            },
        }));
        let host = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(timeout).await;
            host.enqueue(move |state, _| {
                state.resolve_preview(
                    request_id,
                    Err("preview operation timed out (no responding client)".into()),
                )
            });
        });
    }

    pub(crate) fn resolve_preview(
        &mut self,
        request_id: u64,
        response: Result<tcode_protocol::PreviewResponse, String>,
    ) {
        if let Some(reply) = self.preview_pending.remove(&request_id) {
            let _ = reply.try_send(response);
        }
    }

    /// Attach the serializable registration half of the preview MCP server.
    /// Its broker receiver is drained by the host reverse-RPC pump.
    pub fn attach_preview_mcp(&mut self, url: String, tokens: preview_mcp::TokenRegistry) {
        self.mcp.preview_url = Some(url);
        self.mcp.preview_tokens = Some(tokens);
    }

    pub fn attach_orchestrate_mcp(&mut self, server: orchestrate_mcp::OrchestrateMcpServer) {
        self.mcp.orchestrate_url = Some(server.url);
        self.mcp.orchestrate_tokens = Some(server.tokens);
        self.mcp.orchestrate_child_url = Some(server.child_url);
        self.mcp.orchestrate_child_tokens = Some(server.child_tokens);
        self.mcp.orchestrate_requests = Some(server.requests);
    }

    pub fn attach_computer_use_mcp(
        &mut self,
        url: String,
        tokens: computer_use_mcp::TokenRegistry,
    ) {
        self.mcp.computer_use_url = Some(url);
        self.mcp.computer_use_tokens = Some(tokens);
    }

    /// Every MCP tool offered to this thread's provider, and every text Tcode gives its model
    /// outside the conversation, as delivered; `None` for a provider that takes no MCP server.
    pub(super) fn injected_tools(
        &self,
        session: &ActiveSession,
    ) -> Option<tcode_protocol::InjectedTools> {
        use tcode_protocol::{InjectedChannel, InjectedInstructions};
        let meta = &session.meta;
        let provider = meta.provider;
        if !provider.caps().mcp_servers {
            return None;
        }
        let pull_requests = self.pull_request_tools_offered(provider);
        let preview = self.mcp.preview_url.is_some();
        let orchestrate = meta.orchestrate_enabled && self.mcp.orchestrate_url.is_some();
        let report = meta.parent_session_id.is_some() && self.mcp.orchestrate_child_url.is_some();
        let computer_use =
            computer_use_attaches(provider, &self.settings) && self.mcp.computer_use_url.is_some();
        let control = self.control_offered(meta);
        if !(pull_requests || preview || orchestrate || report || computer_use || control) {
            return None;
        }
        let id = meta.id.as_str();
        let attached = session.mcp_servers_attached;
        let mut instructions = Vec::new();
        let channel = provider.caps().instruction_channel;
        if pull_requests && attached && channel != agent::InstructionChannel::None {
            instructions.push(InjectedInstructions {
                channel: InjectedChannel::Linking(channel),
                text: self.pull_request_instructions(id),
            });
        }
        if provider == ProviderKind::Codex {
            instructions.push(InjectedInstructions {
                channel: InjectedChannel::DeveloperInstructions,
                text: agent::codex::PREVIEW_BROWSER_INSTRUCTIONS.to_owned(),
            });
        }
        let servers = [
            (
                agent::McpRegistration::SERVER_NAME_PULL_REQUESTS,
                pull_request_mcp::INSTRUCTIONS,
                self.mcp.pull_request_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_PREVIEW,
                preview_mcp::INSTRUCTIONS,
                self.mcp.preview_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_ORCHESTRATE,
                orchestrate_mcp::INSTRUCTIONS,
                self.mcp.orchestrate_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_ORCHESTRATE_REPORT,
                orchestrate_mcp::REPORT_INSTRUCTIONS,
                self.mcp.orchestrate_child_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_COMPUTER_USE,
                computer_use_mcp::tools::INSTRUCTIONS,
                self.mcp.computer_use_registrations.contains_key(id),
            ),
            (
                agent::McpRegistration::SERVER_NAME_CONTROL,
                control_mcp::INSTRUCTIONS,
                self.mcp.control_registrations.contains_key(id),
            ),
        ];
        instructions.extend(
            servers
                .into_iter()
                .filter(|(_, _, registered)| attached && *registered)
                .map(|(name, text, _)| InjectedInstructions {
                    channel: InjectedChannel::McpServer(name.to_owned()),
                    text: text.to_owned(),
                }),
        );
        let mut tools = Vec::new();
        let mut servers = Vec::new();
        let mut offer = |name: &str, listed: Vec<(String, String)>| {
            servers.push(tcode_protocol::InjectedServer {
                name: name.to_owned(),
                tools: listed.len(),
            });
            tools.extend(listed);
        };
        if pull_requests {
            offer(
                agent::McpRegistration::SERVER_NAME_PULL_REQUESTS,
                pull_request_mcp::tool_descriptions(&self.pull_request_tool_hosts(id)),
            );
        }
        if preview {
            offer(
                agent::McpRegistration::SERVER_NAME_PREVIEW,
                preview_mcp::tool_descriptions(),
            );
        }
        if orchestrate {
            offer(
                agent::McpRegistration::SERVER_NAME_ORCHESTRATE,
                orchestrate_mcp::tool_descriptions(),
            );
        }
        if computer_use {
            offer(
                agent::McpRegistration::SERVER_NAME_COMPUTER_USE,
                computer_use_mcp::tools::tool_descriptions(),
            );
        }
        if control {
            offer(
                agent::McpRegistration::SERVER_NAME_CONTROL,
                self.mcp
                    .control_source
                    .as_ref()
                    .and_then(|source| source.offered(id))
                    .unwrap_or_else(|| control_mcp::tool_descriptions(&self.caller_grant(meta).0)),
            );
        }
        Some(tcode_protocol::InjectedTools {
            tools,
            servers,
            attached,
            instructions,
        })
    }

    pub(super) fn computer_use_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.mcp.computer_use_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.computer_use_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_COMPUTER_USE.into(),
            url: self.mcp.computer_use_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .computer_use_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    pub(super) fn cancel_computer_use_feedback(&self, session_id: &str) {
        if let Some(tokens) = &self.mcp.computer_use_tokens {
            tokens.cancel(session_id);
        }
    }

    pub(super) fn revoke_computer_use_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.computer_use_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.computer_use_tokens
        {
            tokens.revoke(session_id, &registration.bearer_token);
        }
    }

    /// Pump orchestrator requests through the runtime on the host executor.
    ///
    /// Taking the receiver makes repeated calls harmless: exactly one pump can
    /// own the request stream.
    pub fn pump_orchestrate_requests(&mut self, cx: &mut HostCx) {
        let Some(requests) = self.mcp.orchestrate_requests.take() else {
            return;
        };
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            while let Ok(request) = requests.recv().await {
                let orchestrate_mcp::BrokerRequest { op, reply } = request;
                host_cx.enqueue(move |state, cx| state.handle_orchestrate_op(op, reply, cx));
            }
        });
    }

    /// Persistently opt a session into native orchestration. Callers restart a
    /// currently-live provider so its next spawn receives the MCP registration.
    pub(crate) fn enable_orchestrate(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        let Some(mut meta) = self.find_meta(session_id) else {
            return Err("unknown session".into());
        };
        meta.orchestrate_enabled = true;
        meta.updated_at = now_secs();
        if let Some(live_meta) = self.meta_mut(session_id) {
            live_meta.orchestrate_enabled = true;
            live_meta.updated_at = meta.updated_at;
        }
        self.persist_meta(&meta, cx);
        let _ = self.orchestrate_registration_for(&meta);
        Ok(())
    }

    /// Enable orchestration on first use, restart so the MCP registration is
    /// present, and submit the provider-specific guidance plus the user's text.
    pub fn orchestrate_turn(
        &mut self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let (text, attachments) = self.assemble_user_message(target_id, text, attachment_paths);
        self.orchestrate_turn_assembled(target_id, text, attachments, cx);
        self.clear_consumed_draft_context(target_id, cx);
    }

    pub(super) fn orchestrate_turn_assembled(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        let enabling = !active.meta.orchestrate_enabled;
        let session_id = active.meta.id.clone();
        // The composed text is [workflow] + [configuration] + [user text] joined
        // by "\n\n", with the user's words last. `context_len` is the byte length
        // of everything before them (prefix + its trailing "\n\n") — the split the
        // timeline records so it can show the prefix as a disclosure and the
        // bubble only the user's words. The provider still receives all of `text`.
        let user_len = text.len();
        let caller_model = active
            .meta
            .model
            .as_deref()
            .or(active.live_model.as_deref())
            .or_else(|| {
                self.models_for(active.meta.provider)
                    .iter()
                    .find(|model| model.is_default)
                    .map(|model| model.id.as_str())
            });
        let text = compose_orchestrate_text(
            &self.settings.orchestrate,
            &text,
            Some((active.meta.provider, caller_model)),
            &self.providers.model_catalogs,
        );
        let context_len = text.len().saturating_sub(user_len);

        if enabling {
            if let Err(message) = self.enable_orchestrate(&session_id, cx) {
                self.report_error(RuntimeError::External(message), cx);
                return;
            }
            if let Some(active) = self.resident_mut(target_id) {
                active.shutdown_to_idle();
            }
        }

        // Stage the split so the next `push_queued` records it on the user
        // message. (A mid-turn steer clears it instead — see `steer` — so the
        // annotation never leaks onto an unrelated later message.)
        if let Some(active) = self.resident_mut(target_id) {
            active.pending_context_len = Some(context_len);
        }

        // `steer` sends ordinarily when idle and injects into a live turn. On
        // first enable the restart above intentionally makes this an ordinary
        // queued send for the resumed, MCP-enabled process.
        self.steer_assembled(target_id, text, attachments, cx);
    }

    pub(super) fn orchestrate_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !meta.orchestrate_enabled {
            return None;
        }
        if let Some(registration) = self.mcp.orchestrate_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.orchestrate_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
            url: self.mcp.orchestrate_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .orchestrate_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    /// The `report_result` half of orchestration, registered with child threads
    /// so they can push their full RESULT text up instead of relying on the
    /// tail of their last message.
    pub(super) fn orchestrate_child_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        meta.parent_session_id.as_ref()?;
        if let Some(registration) = self.mcp.orchestrate_child_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self
            .mcp
            .orchestrate_child_tokens
            .as_ref()?
            .register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE_REPORT.into(),
            url: self.mcp.orchestrate_child_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .orchestrate_child_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    pub(super) fn revoke_orchestrate_child_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.orchestrate_child_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.orchestrate_child_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    pub(super) fn preview_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(registration) = self.mcp.preview_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.preview_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: self.mcp.preview_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .preview_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }

    #[allow(clippy::too_many_arguments)] // Dispatch accepts the provider launch and child lifecycle settings.
    pub(crate) fn create_child_session(
        &mut self,
        parent_id: &str,
        provider: ProviderKind,
        model: Option<String>,
        effort: Option<String>,
        fast: bool,
        profile_id: Option<String>,
        permission_selection: Option<OptionSelection>,
        title: String,
        cwd: Option<PathBuf>,
        brief: String,
        result_max_chars: Option<u32>,
        cx: &mut HostCx,
    ) -> Result<String, String> {
        let parent = self
            .find_meta(parent_id)
            .ok_or_else(|| "unknown parent session".to_string())?;
        let cwd = cwd.unwrap_or_else(|| parent.cwd.clone());
        let mut meta = build_child_meta(
            &parent,
            provider,
            model,
            effort,
            fast,
            profile_id,
            permission_selection,
            cwd,
            result_max_chars,
        );
        meta.title = title;
        self.install_child_session(meta, brief, cx)
    }

    fn install_child_session(
        &mut self,
        meta: SessionMeta,
        brief: String,
        cx: &mut HostCx,
    ) -> Result<String, String> {
        // The report contract rides with every brief: the child sees nothing
        // of the orchestration, and the tool description alone yields missing
        // or one-line reports. Skipped for providers without MCP attachment
        // (pi), where the tool does not exist.
        let brief = if meta.provider.caps().mcp_servers {
            format!("{brief}{CHILD_REPORT_FOOTER}")
        } else {
            brief
        };
        let mut meta = meta;
        meta.expectation = Some(ChildExpectation::opened(now_secs()));
        Ok(self.install_background_thread(meta, brief, Vec::new(), cx))
    }

    /// Persist a new thread and start it in the background on `first_message`, written by an
    /// agent. Nobody's selection or draft is touched.
    pub(super) fn install_background_thread(
        &mut self,
        meta: SessionMeta,
        first_message: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) -> String {
        self.thread_activity
            .insert(meta.id.clone(), Default::default());
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: true,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());
        let id = meta.id.clone();
        let provider_commands = self.cached_provider_commands_for(&meta);
        let mut session = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            None,
            provider_commands,
        );
        session.meta = meta;
        session.draft = false;
        session.push_queued(first_message, attachments);
        session.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
        self.residents.parked.insert(id.clone(), session);
        self.reactivate_session(&id, cx);
        self.ensure_session_started(&id, cx);
        id
    }

    /// Switch a child's fast mode and persist it. Fast mode is a launch-time
    /// option, so a live child restarts before its next turn (see
    /// `options_changed_while_live`); a turn already running is unaffected.
    fn set_child_fast(&mut self, thread_id: &str, fast: bool, cx: &mut HostCx) {
        let Some(mut meta) = self
            .resident(thread_id)
            .map(|child| child.meta.clone())
            .or_else(|| self.find_meta(thread_id))
        else {
            return;
        };
        apply_fast_selection(&mut meta.option_selections, meta.provider, fast);
        if let Some(child) = self.resident_mut(thread_id) {
            child.meta.option_selections = meta.option_selections.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Resolve one MCP operation on the host owner thread.
    pub(crate) fn handle_orchestrate_op(
        &mut self,
        op: orchestrate_mcp::OrchestrateOp,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        use orchestrate_mcp::OrchestrateOp;
        let mut authored_cx = cx.clone();
        authored_cx.origin = Some(MessageOrigin::Agent);
        authored_cx.author = None;
        let cx = &mut authored_cx;

        match op {
            orchestrate_mcp::OrchestrateOp::Status {
                parent_id,
                thread_id,
            } => self.handle_orchestrate_status(parent_id, thread_id, reply, cx),
            orchestrate_mcp::OrchestrateOp::Result {
                parent_id,
                thread_id,
            } => self.handle_orchestrate_result(parent_id, thread_id, reply, cx),
            orchestrate_mcp::OrchestrateOp::Dispatch {
                purpose,
                parent_id,
                provider,
                model,
                effort,
                profile,
                permission,
                title,
                brief,
                cwd,
                worktree,
                result_max_chars,
                fast: fast_override,
            } => {
                let collaboration = purpose == orchestrate_mcp::ThreadPurpose::Collaboration;
                let resolve = if collaboration {
                    resolve_orchestrate_collaboration
                } else {
                    resolve_orchestrate_dispatch
                };
                let resolved = (|| {
                    let (provider, model, effort, fast, profile_id) = resolve(
                        &self.settings.orchestrate,
                        &provider,
                        model.as_deref(),
                        effort.as_deref(),
                        profile.as_deref(),
                        &self.providers.model_catalogs,
                    )?;
                    if let Some(id) = profile_id.as_deref()
                        && self.settings.resolved_profile(id).is_none()
                    {
                        return Err(format!("unknown profile: {id}"));
                    }
                    let permission_selection = resolve_child_permission(
                        permission_control(provider),
                        self.settings.orchestrate.child_approval,
                        permission.as_deref(),
                    )?;
                    let fast = fast_override.unwrap_or(fast);
                    Ok((
                        provider,
                        model,
                        effort,
                        fast,
                        profile_id,
                        permission_selection,
                    ))
                })();
                let (provider, model, effort, fast, profile_id, permission_selection) =
                    match resolved {
                        Ok(resolved) => resolved,
                        Err(err) => {
                            let _ = reply.try_send(Err(err));
                            return;
                        }
                    };
                let resolved_permission = permission_selection
                    .as_ref()
                    .map(|selection| selection.value.clone());
                let brief = if collaboration {
                    compose_collaboration_brief(
                        &self.settings.orchestrate,
                        provider,
                        &model,
                        &brief,
                    )
                } else {
                    brief
                };
                let isolate =
                    !collaboration && worktree.unwrap_or(self.settings.orchestrate.child_worktrees);
                if cwd.is_none() && !isolate {
                    let result = self
                        .create_child_session(
                            &parent_id,
                            provider,
                            Some(model),
                            effort,
                            fast,
                            profile_id,
                            permission_selection,
                            title,
                            None,
                            brief,
                            result_max_chars,
                            cx,
                        )
                        .map(|id| serde_json::json!({ "thread_id": id, "permission": resolved_permission }));
                    let _ = reply.try_send(result);
                    return;
                }
                let Some(parent) = self.find_meta(&parent_id) else {
                    let _ = reply.try_send(Err("unknown parent session".to_string()));
                    return;
                };
                let path = PathBuf::from(cwd.unwrap_or_default());
                let path = if path.as_os_str().is_empty() {
                    parent.cwd.clone()
                } else if path.is_absolute() {
                    path
                } else {
                    parent.cwd.join(path)
                };
                let mut meta = build_child_meta(
                    &parent,
                    provider,
                    Some(model),
                    effort,
                    fast,
                    profile_id,
                    permission_selection,
                    path.clone(),
                    result_max_chars,
                );
                meta.title = title;
                let child_id = meta.id.clone();
                let data_dir = self.store.root().clone();
                let host_cx = cx.clone();
                HostCx::spawn_detached(cx, async move {
                    let resolved_cwd = host_cx
                        .unblock(move || {
                            let canonical = path
                                .canonicalize()
                                .map_err(|_| format!("invalid cwd: {}", path.display()))?;
                            if !canonical.is_dir() {
                                return Err(format!("invalid cwd: {}", canonical.display()));
                            }
                            if isolate {
                                Ok(resolve_child_worktree(canonical, &child_id, &data_dir))
                            } else {
                                Ok((canonical, None, None))
                            }
                        })
                        .await;
                    let result = match resolved_cwd {
                        Ok((cwd, worktree, warning)) => host_cx
                            .enqueue_and_wait(move |state, cx| {
                                meta.cwd = cwd;
                                meta.worktree = worktree;
                                let worktree_info = meta.worktree.clone();
                                let worktree_path =
                                    worktree_info.as_ref().map(|_| meta.cwd.clone());
                                state
                                    .install_child_session(meta, brief, cx)
                                    .map(|id| (id, worktree_info, worktree_path, warning))
                            })
                            .await
                            .unwrap_or_else(|_| Err("application closed".to_string()))
                            .map(|(id, worktree, worktree_path, warning)| {
                                let mut response = serde_json::json!({ "thread_id": id, "permission": resolved_permission });
                                if let Some(worktree) = worktree {
                                    response["worktree_path"] = serde_json::json!(
                                        worktree_path.expect("worktree path").display().to_string()
                                    );
                                    response["worktree_branch"] =
                                        serde_json::json!(worktree.branch);
                                }
                                if let Some(warning) = warning {
                                    response["warning"] = serde_json::json!(warning);
                                }
                                response
                            }),
                        Err(err) => Err(err),
                    };
                    let _ = reply.try_send(result);
                });
            }
            OrchestrateOp::Send {
                parent_id,
                thread_id,
                message,
                fast,
            } => {
                let result = (|| {
                    let archived = self
                        .require_child(&parent_id, &thread_id)?
                        .archived_at
                        .is_some();
                    if let Some(fast) = fast {
                        self.set_child_fast(&thread_id, fast, cx);
                    }
                    // A follow-up revives an archived child: it returns to its
                    // lead's Agents view so the user can watch the retry.
                    if archived {
                        self.unarchive_session(&thread_id, cx);
                    }
                    // A follow-up is new work the lead is owed a report for: a
                    // report not yet delivered for the previous message is not
                    // the answer to this one.
                    let superseded = self
                        .find_meta(&thread_id)
                        .is_some_and(|meta| meta.report().pending());
                    let mut expectation = ChildExpectation::opened(now_secs());
                    // A report made before this message answers the previous
                    // one, not this one.
                    expectation.delivered_item = self
                        .loaded_child_timeline(&thread_id)
                        .and_then(Timeline::last_report)
                        .map(|(item, _)| item.to_owned());
                    self.set_child_expectation(&thread_id, Some(expectation), cx);
                    let delivery = self.deliver_to_child(&thread_id, message, cx)?;
                    Ok(serde_json::json!({
                        "ok": true,
                        "delivery": delivery,
                        "expectation": if superseded { "replaced" } else { "opened" },
                    }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Cancel {
                parent_id,
                thread_id,
            } => {
                let result = self.require_child(&parent_id, &thread_id).map(|_| ());
                let result = result.map(|()| {
                    self.cancel_child(&thread_id, cx);
                    serde_json::json!({ "ok": true, "delivery": AgentDelivery::NotDelivered })
                });
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Settle {
                parent_id,
                thread_id,
            } => {
                let result = (|| {
                    self.require_child(&parent_id, &thread_id)?;
                    self.validate_command_target(&tcode_protocol::Command::SettleSession {
                        session_id: thread_id.clone(),
                    })
                    .map_err(|error| error.message)?;
                    self.settle_session(&thread_id, cx);
                    Ok(serde_json::json!({ "ok": true, "delivery": AgentDelivery::Settled }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::SettleAll { parent_id } => {
                let result = self
                    .validate_command_target(&tcode_protocol::Command::SettleChildren {
                        session_id: parent_id.clone(),
                    })
                    .map_err(|error| error.message)
                    .map(|()| serde_json::to_value(self.settle_children(&parent_id, cx)).unwrap());
                let _ = reply.try_send(result);
            }
            OrchestrateOp::ReportResult { child_id, text } => {
                let result = (|| {
                    let is_child = self
                        .sessions
                        .iter()
                        .any(|meta| meta.id == child_id && meta.parent_session_id.is_some());
                    if !is_child {
                        return Err("unknown thread or not an orchestrated child".into());
                    }
                    // The report is read back from this call's own tool item when
                    // the turn ends (`Timeline::last_report`), so nothing is
                    // stored here: the transcript is the one copy.
                    Ok(serde_json::json!({
                        "ok": true,
                        "chars": text.chars().count(),
                        "delivered_when": "turn_ends",
                    }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Approve {
                parent_id,
                thread_id,
                request_id,
                option,
                cancel,
            } => {
                let result = (|| {
                    self.require_child(&parent_id, &thread_id)?;
                    let pending = self.approval_requests(&thread_id);
                    let request = match request_id {
                        Some(request_id) => pending
                            .iter()
                            .find(|request| request.id == request_id)
                            .cloned()
                            .ok_or_else(|| {
                                "no pending approval with that request_id".to_string()
                            })?,
                        None => match pending {
                            [request] => request.clone(),
                            [] => return Err("no pending approval".into()),
                            _ => {
                                return Err(
                                    "multiple pending approvals; request_id is required".into()
                                );
                            }
                        },
                    };
                    let decision = resolve_approval_decision(option.as_deref(), cancel, &request)?;
                    let request_id = request.id;
                    self.respond_session_approval(&thread_id, request_id.clone(), decision)?;
                    Ok(serde_json::json!({ "ok": true, "request_id": request_id }))
                })();
                let _ = reply.try_send(result);
            }
            OrchestrateOp::Answer {
                parent_id,
                thread_id,
                request_id,
                answers,
            } => {
                let result = (|| {
                    self.require_child(&parent_id, &thread_id)?;
                    let pending = self
                        .resident(&thread_id)
                        .and_then(|child| child.timeline.pending_user_input.clone())
                        .ok_or_else(|| "no pending question".to_string())?;
                    if request_id.is_some_and(|request_id| request_id != pending.request_id) {
                        return Err("no pending question with that request_id".into());
                    }
                    let ids = pending
                        .questions
                        .iter()
                        .map(|question| question.id.as_str())
                        .collect::<Vec<_>>();
                    if let Some(unknown) = answers.keys().find(|key| !ids.contains(&key.as_str())) {
                        return Err(format!(
                            "unknown question id {unknown:?}; the question ids are {ids:?}"
                        ));
                    }
                    let missing = ids
                        .iter()
                        .filter(|id| !answers.contains_key(**id))
                        .copied()
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        return Err(format!("missing answers for question ids {missing:?}"));
                    }
                    self.respond_user_input(&thread_id, pending.request_id.clone(), answers, cx)
                        .map_err(|error| error.message)?;
                    Ok(serde_json::json!({ "ok": true, "request_id": pending.request_id }))
                })();
                let _ = reply.try_send(result);
            }
        }
    }

    /// Stop a dispatched child without delivering its result: it keeps its
    /// transcript and no longer holds its lead.
    pub(crate) fn cancel_child(&mut self, thread_id: &str, cx: &mut HostCx) {
        self.clear_approvals(thread_id);
        if self.residents.live.contains_key(thread_id) {
            if let Some(child) = self.resident_mut(thread_id) {
                child.queue.clear();
                child.timeline.mark_idle();
                child.shutdown_to_idle();
            }
        } else {
            self.drop_background(thread_id, cx);
        }
        let Some(mut meta) = self.find_meta(thread_id) else {
            return;
        };
        meta.cancelled_at = Some(now_secs());
        meta.expectation = None;
        meta.updated_at = now_secs();
        if let Some(child) = self.resident_mut(thread_id) {
            child.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    pub fn settle_children(
        &mut self,
        parent_id: &str,
        cx: &mut HostCx,
    ) -> tcode_protocol::SettleChildrenResult {
        let children: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.is_dispatched() && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .filter_map(|meta| {
                self.agent_status(meta)
                    .map(|status| (meta.id.clone(), status.delivery))
            })
            .collect();
        let mut result = tcode_protocol::SettleChildrenResult::default();
        for (thread_id, delivery) in children {
            if delivery == AgentDelivery::AwaitingSettle && !self.settle_thread_busy(&thread_id) {
                self.settle_session(&thread_id, cx);
                result.settled.push(tcode_protocol::ChildSettlement {
                    thread_id,
                    delivery: AgentDelivery::Settled,
                });
            } else {
                result.skipped.push(tcode_protocol::ChildSettlement {
                    thread_id,
                    delivery,
                });
            }
        }
        result
    }

    pub(super) fn require_child(
        &self,
        parent_id: &str,
        thread_id: &str,
    ) -> Result<&SessionMeta, String> {
        self.sessions
            .iter()
            .find(|meta| {
                meta.id == thread_id
                    && meta.parent_session_id.as_deref() == Some(parent_id)
                    && meta.native_subagent.is_none()
            })
            .ok_or_else(|| "unknown thread or not a child of this parent".into())
    }

    pub(super) fn ensure_child_loaded(
        &mut self,
        thread_id: &str,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        if self.residents.parked.contains_key(thread_id) {
            return Ok(());
        }
        let meta = self
            .sessions
            .iter()
            .find(|meta| meta.id == thread_id)
            .cloned()
            .ok_or_else(|| "unknown thread".to_string())?;
        if self.residents.live.contains_key(thread_id) {
            return Err("child thread is currently open in the foreground".into());
        }
        self.load_background_session(meta, cx);
        Ok(())
    }

    pub(super) fn load_background_session(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        let thread_id = meta.id.clone();
        let commands = self.cached_provider_commands_for(&meta);
        let mut child = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            meta.acp_agent_id.clone(),
            commands,
        );
        child.meta = meta;
        child.draft = false;
        self.residents.parked.insert(thread_id.clone(), child);
        self.schedule_timeline_load(thread_id, TimelineLoadTarget::Background, cx);
    }

    pub(super) fn handle_orchestrate_status(
        &mut self,
        parent_id: String,
        thread_id: Option<String>,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let children: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.parent_session_id.as_deref() == Some(&parent_id))
            .filter(|meta| meta.native_subagent.is_none())
            .filter(|meta| thread_id.as_ref().is_none_or(|id| id == &meta.id))
            .cloned()
            .collect();
        if thread_id.is_some() && children.is_empty() {
            let _ = reply.try_send(Err("unknown thread or not a child of this parent".into()));
            return;
        }
        let unloaded: Vec<_> = children
            .iter()
            .filter(|meta| self.loaded_child_timeline(&meta.id).is_none())
            .map(|meta| meta.id.clone())
            .collect();
        if unloaded.is_empty() {
            let result = self.orchestrate_status_json(&children, &HashMap::new());
            let _ = reply.try_send(Ok(result));
            return;
        }
        let folds: Vec<_> = unloaded
            .into_iter()
            .map(|id| {
                let fold = self.folded_log(&id, cx);
                (id, fold)
            })
            .collect();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let mut timelines = Ok(HashMap::new());
            for (id, fold) in folds {
                match fold.await {
                    Ok(timeline) => {
                        if let Ok(timelines) = &mut timelines {
                            timelines.insert(id, timeline);
                        }
                    }
                    Err(error) => timelines = Err(format!("could not read thread {id}: {error}")),
                }
            }
            let result = match timelines {
                Ok(timelines) => host_cx
                    .enqueue_and_wait(move |state, _| {
                        state.orchestrate_status_json(&children, &timelines)
                    })
                    .await
                    .map_err(|_| "tcode orchestrator is not available".to_string()),
                Err(error) => Err(error),
            };
            let _ = reply.send(result).await;
        });
    }

    pub(super) fn handle_orchestrate_result(
        &mut self,
        parent_id: String,
        thread_id: String,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let meta = match self.require_child(&parent_id, &thread_id) {
            Ok(meta) => meta.clone(),
            Err(error) => {
                let _ = reply.try_send(Err(error));
                return;
            }
        };
        if let Some(timeline) = self.loaded_child_timeline(&thread_id) {
            let result = self.orchestrate_result_json(&meta, timeline);
            let _ = reply.try_send(result);
            return;
        }
        let fold = self.folded_log(&thread_id, cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match fold.await {
                Ok(timeline) => host_cx
                    .enqueue_and_wait(move |state, _| {
                        let timeline = state.loaded_child_timeline(&thread_id).unwrap_or(&timeline);
                        state.orchestrate_result_json(&meta, timeline)
                    })
                    .await
                    .unwrap_or_else(|_| Err("tcode orchestrator is not available".to_string())),
                Err(error) => Err(format!("could not read thread {thread_id}: {error}")),
            };
            let _ = reply.send(result).await;
        });
    }

    /// The pure fold of a session's whole log. A cached log already holds
    /// every record accepted for it, appends still queued for the store
    /// included; otherwise the store is read once everything queued before now
    /// has committed.
    pub(super) fn folded_log(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
    ) -> HostTask<Result<Timeline, String>> {
        if let Some(log) = self.event_records.get(session_id) {
            let fold = log.fold().clone();
            return cx.spawn_background(async move { Ok(fold) });
        }
        let barrier = self.store_write_barrier(cx);
        let store = self.store.clone();
        let read_id = session_id.to_string();
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            match barrier.recv().await {
                Ok(Ok(())) => host_cx
                    .unblock(move || store.read_events(&read_id).map(Timeline::fold_events))
                    .await
                    .map_err(|error| error.to_string()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err("the session store writer has stopped".to_string()),
            }
        })
    }

    pub(super) fn loaded_child_timeline(&self, session_id: &str) -> Option<&Timeline> {
        self.resident(session_id).map(|child| &child.timeline)
    }

    /// What the lead can learn about a child from its thread: whether it is
    /// still running, what it owes, its latest report and its final message.
    pub(super) fn child_result(&self, meta: &SessionMeta, timeline: &Timeline) -> ChildResult {
        let running = self
            .resident(&meta.id)
            .is_some_and(ActiveSession::is_unfinished);
        let state = if running || meta.report().pending() {
            "running"
        } else if trailing_start_error(timeline).is_some() {
            "failed"
        } else {
            match meta.report() {
                AgentReport::Reported => "reported",
                AgentReport::Unreported => "unreported",
                AgentReport::Failed => "failed",
                // Children dispatched before expectations were recorded.
                AgentReport::Expected | AgentReport::None => match timeline.last_turn_status {
                    Some(TurnStatus::Completed) => "completed",
                    Some(TurnStatus::Failed | TurnStatus::Interrupted) => "failed",
                    None => "idle",
                },
            }
        };
        ChildResult {
            state,
            report: meta.report(),
            reported: timeline.last_report().map(|(_, text)| text.to_owned()),
            final_message: final_assistant_message(timeline),
            usage: timeline.usage,
        }
    }

    pub(super) fn orchestrate_result_json(
        &self,
        meta: &SessionMeta,
        timeline: &Timeline,
    ) -> Result<serde_json::Value, String> {
        let result = self.child_result(meta, timeline);
        let mut json = match &result.reported {
            Some(report) => serde_json::json!({
                "state": result.state,
                "report": result.report,
                "source": "report",
                "text": report,
                "final_message": result.final_message,
            }),
            None => serde_json::json!({
                "state": result.state,
                "report": result.report,
                "source": "fallback",
                "text": result.final_message,
                "final_message": result.final_message,
            }),
        };
        if let Some(usage) = result.usage.as_ref() {
            json["tokens"] = token_usage_json(usage);
        }
        Ok(json)
    }

    pub(super) fn orchestrate_status_json(
        &self,
        children: &[SessionMeta],
        disk_timelines: &HashMap<String, Timeline>,
    ) -> serde_json::Value {
        let mut children: Vec<_> = children
            .iter()
            .filter_map(|meta| {
                self.loaded_child_timeline(&meta.id)
                    .or_else(|| disk_timelines.get(&meta.id))
                    .map(|timeline| self.child_status_json(meta, timeline))
            })
            .collect();
        children.sort_by_key(|value| value["updated_at"].as_u64().unwrap_or_default());
        children.reverse();
        serde_json::Value::Array(children)
    }

    pub(super) fn child_status_json(
        &self,
        meta: &SessionMeta,
        timeline: &Timeline,
    ) -> serde_json::Value {
        let result = self.child_result(meta, timeline);
        let approval = self.first_approval(&meta.id);
        let waiting_approval = approval.map(approval_request_summary);
        let approval_request_id = approval.map(|request| request.id.as_str());
        let mut status = serde_json::json!({
            "thread_id": meta.id,
            "title": meta.title,
            "provider": provider_name(meta.provider),
            "state": result.state,
            "report": result.report,
            "delivery": self.agent_status(meta).map(|status| status.delivery),
            "archived": meta.archived_at.is_some(),
            "waiting_approval": waiting_approval,
            "approval_request_id": approval_request_id,
            "last_output_tail": tail_chars(&result.final_message, 600),
            "updated_at": meta.updated_at,
        });
        if let Some(error) = trailing_start_error(timeline) {
            status["start_error"] = serde_json::json!(error);
        }
        if let Some(usage) = result.usage.as_ref() {
            status["tokens"] = token_usage_json(usage);
        }
        status
    }

    /// Record what a child owes its lead, on the thread and in the store.
    pub(super) fn set_child_expectation(
        &mut self,
        thread_id: &str,
        expectation: Option<ChildExpectation>,
        cx: &mut HostCx,
    ) {
        let Some(mut meta) = self.find_meta(thread_id) else {
            return;
        };
        if meta.expectation == expectation {
            return;
        }
        meta.expectation = expectation;
        meta.updated_at = now_secs();
        if let Some(child) = self.resident_mut(thread_id) {
            child.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Hand the lead's (or tcode's own) message to a child: steered into a
    /// live turn, otherwise queued as the child's next turn. Reports which.
    pub(super) fn deliver_to_child(
        &mut self,
        thread_id: &str,
        message: String,
        cx: &mut HostCx,
    ) -> Result<&'static str, String> {
        // A live turn accepts the message right away — same routing as
        // parent callbacks. Queueing a mid-turn correction until the
        // turn ends would deliver it after the work it was meant to
        // redirect (and never, if the turn hangs).
        let can_steer = self
            .resident(thread_id)
            .is_some_and(|child| child.turn_in_flight && child.can_steer());
        if can_steer {
            let request_id = self.record_steer_request(thread_id, &message, &[], cx);
            let sent = self.resident_mut(thread_id).is_some_and(|child| {
                child
                    .steer_now(request_id, message.clone(), Vec::new())
                    .is_ok()
            });
            if sent {
                return Ok("steered");
            }
            // Provider channel gone: fall through so the text survives
            // in the queue for the wake-up path.
        }
        if !can_steer {
            self.reactivate_session(thread_id, cx);
        }
        if self.residents.live.contains_key(thread_id) {
            let default_window = self.default_context_window(thread_id);
            let child = self.resident_mut(thread_id).unwrap();
            child.push_queued(message, Vec::new());
            child.stamp_context_window_change(default_window);
            child.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
            let idle = matches!(child.runtime, Runtime::Idle);
            if self.dispatch_next_queued(thread_id, cx).is_err() {
                return Err("child provider is unavailable".into());
            }
            if idle {
                self.ensure_started(thread_id, cx);
            }
            return Ok("queued");
        }
        self.ensure_child_loaded(thread_id, cx)?;
        let child = self.resident_mut(thread_id).unwrap();
        child.push_queued(message, Vec::new());
        child.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
        let idle = matches!(child.runtime, Runtime::Idle);
        if !idle && !child.turn_in_flight {
            self.on_background_turn_completed(thread_id, cx);
        }
        if idle {
            self.ensure_session_started(thread_id, cx);
        }
        Ok("queued")
    }

    /// Nothing is left inside the thread that could still produce a report
    /// on its own: no turn, nothing queued, no background task or wake-up,
    /// no request waiting on an answer.
    pub(super) fn thread_quiescent(&self, id: &str) -> bool {
        !self.settle_thread_busy(id)
            && self.resident(id).is_none_or(|session| {
                session.queue.is_empty()
                    && session.background_task_count == 0
                    && matches!(session.runtime, Runtime::Idle | Runtime::Live(_))
            })
    }

    /// The child meta and its lead, when the child still owes or has owed a
    /// report and both sides are current.
    fn expectant_child(&self, child_id: &str) -> Option<(SessionMeta, String, ChildExpectation)> {
        let child = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .filter(|meta| meta.archived_at.is_none() && meta.cancelled_at.is_none())?
            .clone();
        let expectation = child.expectation.clone()?;
        let parent_id = child.parent_session_id.clone()?;
        if !self
            .find_meta(&parent_id)
            .is_some_and(|meta| meta.archived_at.is_none())
        {
            return None;
        }
        Some((child, parent_id, expectation))
    }

    /// A child's turn ended (or its background work did): decide whether the
    /// lead is owed anything now. A report the child made is delivered; a
    /// child that owes one and has gone quiet is reminded once, then its
    /// final message is delivered in the report's place. Turns that end for
    /// the child's own reasons — wake-ups, monitors, background tasks —
    /// deliver nothing.
    pub(super) fn settle_child_expectation(
        &mut self,
        child_id: &str,
        status: TurnStatus,
        cx: &mut HostCx,
    ) {
        let Some((child, parent_id, mut expectation)) = self.expectant_child(child_id) else {
            return;
        };
        let Some(resident) = self.resident(child_id) else {
            return;
        };
        if resident.turn_in_flight {
            return;
        }
        let timeline = &resident.timeline;
        let usage = timeline.usage;
        let report = timeline
            .last_report()
            .map(|(item, text)| (item.to_owned(), text.to_owned()));
        // The child chose this text for its lead: deliver it whatever else the
        // child is doing, once per report.
        if let Some((item, text)) = report
            && expectation.delivered_item.as_deref() != Some(item.as_str())
        {
            let state = if expectation.delivered_item.is_some() {
                "updated"
            } else {
                "reported"
            };
            let text = assemble_report_callback(
                child_id,
                &child.title,
                state,
                CallbackBody::Report(&text),
                usage.as_ref(),
            );
            expectation.state = AgentReport::Reported;
            expectation.delivered_item = Some(item);
            self.set_child_expectation(child_id, Some(expectation), cx);
            self.deliver_orchestrate_callback_to_parent(&parent_id, text, cx);
            return;
        }
        if !expectation.state.pending() {
            return;
        }
        match status {
            TurnStatus::Completed => {}
            // The user stopped the child: they have it now.
            TurnStatus::Interrupted => return,
            TurnStatus::Failed => {
                let error = trailing_error(timeline)
                    .unwrap_or("the turn failed")
                    .to_owned();
                self.fail_child_expectation(child_id, &error, cx);
                return;
            }
        }
        if !self.thread_quiescent(child_id) {
            return;
        }
        if !expectation.reminded && child.provider.caps().mcp_servers {
            expectation.reminded = true;
            self.set_child_expectation(child_id, Some(expectation), cx);
            if let Err(error) = self.deliver_to_child(child_id, CHILD_REPORT_REMINDER.into(), cx) {
                self.fail_child_expectation(child_id, &error, cx);
            }
            return;
        }
        let final_message = final_assistant_message(timeline);
        let text = assemble_report_callback(
            child_id,
            &child.title,
            "unreported",
            CallbackBody::Fallback {
                final_message: &final_message,
                max_chars: child.result_max_chars,
            },
            usage.as_ref(),
        );
        expectation.state = AgentReport::Unreported;
        self.set_child_expectation(child_id, Some(expectation), cx);
        self.deliver_orchestrate_callback_to_parent(&parent_id, text, cx);
    }

    /// The child can no longer deliver what it owes: its lead hears why, once.
    pub(super) fn fail_child_expectation(&mut self, child_id: &str, error: &str, cx: &mut HostCx) {
        let Some((child, parent_id, mut expectation)) = self.expectant_child(child_id) else {
            return;
        };
        if !expectation.state.pending() {
            return;
        }
        let usage = self
            .resident(child_id)
            .and_then(|child| child.timeline.usage);
        let text = assemble_report_callback(
            child_id,
            &child.title,
            "failed",
            CallbackBody::Failure(error),
            usage.as_ref(),
        );
        expectation.state = AgentReport::Failed;
        self.set_child_expectation(child_id, Some(expectation), cx);
        self.deliver_orchestrate_callback_to_parent(&parent_id, text, cx);
    }

    pub(super) fn deliver_child_approval_callback(
        &mut self,
        child_id: &str,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        let Some(request) = self
            .approval_requests(child_id)
            .iter()
            .find(|request| request.id == request_id)
            .cloned()
        else {
            return;
        };
        if self.settings.orchestrate.child_approval == ChildApprovalMode::AlwaysAllow
            && let Some(option) = native_allow_option(&request)
        {
            match self.respond_session_approval(
                child_id,
                request.id.clone(),
                ApprovalDecision::Option(option.id.clone()),
            ) {
                Ok(()) => return,
                Err(err) => log::warn!("failed to answer child {child_id} approval: {err}"),
            }
        }
        if !self
            .callback_approval_requests
            .insert((child_id.to_string(), request.id.clone()))
        {
            return;
        }
        let parent_id = child.parent_session_id.as_deref().unwrap();
        let options = request.options.iter().map(|option| {
            serde_json::json!({ "id": option.id, "label": option.label, "kind": option.kind })
        }).collect::<Vec<_>>();
        let instruction = if self.settings.orchestrate.child_approval == ChildApprovalMode::Manual {
            "The user answers in the child thread."
        } else {
            "You are the approver: use the approve tool with option: <exact id> or cancel: true; reject anything outside the brief's scope. To allow, prefer an allow_always option over allow_once when one is offered."
        };
        let text = format!(
            "{} {ORCHESTRATE_APPROVAL_PHRASE}{} (request_id: {}). Native options: {}. {}",
            callback_header(child_id, &child.title),
            approval_request_summary(&request),
            request.id,
            serde_json::to_string(&options).expect("approval options serialize"),
            instruction,
        );
        self.deliver_orchestrate_callback_to_parent(parent_id, text, cx);
    }

    /// A child's question goes to its lead, as an approval does; in Manual
    /// mode the lead is only told that the user answers it. Delivered once per
    /// request, whether the child blocks on it or keeps working.
    pub(super) fn deliver_child_user_input_callback(
        &mut self,
        child_id: &str,
        request_id: &str,
        cx: &mut HostCx,
    ) {
        let Some(child) = self
            .sessions
            .iter()
            .find(|meta| meta.id == child_id && meta.parent_session_id.is_some())
            .cloned()
        else {
            return;
        };
        let Some(pending) = self
            .resident(child_id)
            .and_then(|child| child.timeline.pending_user_input.as_ref())
            .filter(|pending| pending.request_id == request_id)
            .cloned()
        else {
            return;
        };
        if !self
            .callback_user_input_requests
            .insert((child_id.to_string(), pending.request_id.clone()))
        {
            return;
        }
        let parent_id = child.parent_session_id.as_deref().unwrap();
        let delivery = if pending.delivery.is_blocking() {
            "the child is blocked until it is answered"
        } else {
            "the child keeps working and reads the answer when it arrives; the question lapses when its turn ends"
        };
        let instruction = if self.settings.orchestrate.child_approval == ChildApprovalMode::Manual {
            "The user answers in the child thread."
        } else {
            "You are the decider: use the answer tool with answers keyed by question id, a string (an option label or free text) for a single-choice question and an array of strings for a multi_select one, deciding from the brief and the user's instructions."
        };
        let text = format!(
            "{} {ORCHESTRATE_QUESTION_PHRASE}request_id: {}; {delivery}). Questions: {}. {instruction}",
            callback_header(child_id, &child.title),
            pending.request_id,
            serde_json::to_string(&pending.questions).expect("questions serialize"),
        );
        self.deliver_orchestrate_callback_to_parent(parent_id, text, cx);
    }

    /// Deliver a child result into the orchestrator's current reasoning turn.
    ///
    /// A foreground parent already used `steer`, but a parked parent used to put
    /// callbacks into its ordinary queue. Parallel children could therefore
    /// leave results stranded while the orchestrator planned from only the first
    /// completion. Steering is session lifecycle behavior, not UI focus behavior,
    /// so foreground and parked parents follow the same routing here.
    pub(super) fn deliver_orchestrate_callback_to_parent(
        &mut self,
        parent_id: &str,
        text: String,
        cx: &mut HostCx,
    ) {
        if !self
            .find_meta(parent_id)
            .is_some_and(|meta| meta.archived_at.is_none())
        {
            return;
        }
        let mut callback_cx = cx.clone();
        callback_cx.origin = Some(MessageOrigin::Agent);
        callback_cx.author = None;
        let cx = &mut callback_cx;
        let can_steer = self
            .resident(parent_id)
            .is_some_and(|parent| parent.turn_in_flight && parent.can_steer());
        if can_steer {
            // A steered callback is already part of this turn, so persist it just
            // like a user-triggered steer before handing it to the provider.
            let request_id = self.record_steer_request(parent_id, &text, &[], cx);
            let sent = self
                .resident_mut(parent_id)
                .is_some_and(|parent| parent.steer_now(request_id, text, Vec::new()).is_ok());
            if !sent {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            return;
        }

        self.reactivate_session(parent_id, cx);
        self.queue_automatic_turn(
            parent_id,
            |parent| {
                parent.push_or_merge_orchestrate_callback(text);
            },
            cx,
        );
    }
}

pub(super) const ORCHESTRATE_GUIDANCE: &str =
    include_str!("../../../../assets/orchestrate/workflow.md");

/// What the lead can learn about a child (see `AppState::child_result`).
pub(super) struct ChildResult {
    pub(super) state: &'static str,
    pub(super) report: AgentReport,
    /// The child's latest `report_result` text, if it ever reported.
    pub(super) reported: Option<String>,
    pub(super) final_message: String,
    pub(super) usage: Option<agent::TokenUsage>,
}
pub(super) const COLLABORATION_GUIDANCE: &str =
    include_str!("../../../../assets/orchestrate/collaboration.md");

pub(super) fn compose_orchestrate_text(
    settings: &OrchestrateSettings,
    user_text: &str,
    caller: Option<(ProviderKind, Option<&str>)>,
    catalogs: &HashMap<ProviderKind, Vec<agent::ModelSpec>>,
) -> String {
    let configuration = render_orchestrate_configuration(settings, caller, catalogs);
    let mut sections = Vec::with_capacity(3);
    // Refresh the workflow on explicit /orchestrate messages, including sessions
    // enabled before the old model-identity instructions were removed.
    sections.push(ORCHESTRATE_GUIDANCE.trim());
    sections.push(configuration.trim());
    if !user_text.is_empty() {
        sections.push(user_text);
    }
    sections.join("\n\n")
}

pub(super) fn compose_collaboration_brief(
    settings: &OrchestrateSettings,
    provider: ProviderKind,
    model: &str,
    brief: &str,
) -> String {
    let recognition = settings
        .decision_models
        .iter()
        .find(|entry| entry.provider == provider && entry.model == model)
        .map(|entry| entry.guidance(true).trim())
        .unwrap_or_default();
    format!(
        "{}\n\n## Your collaboration guidance\n\n{recognition}\n\n## Discussion\n\n{brief}",
        COLLABORATION_GUIDANCE.trim()
    )
}

pub(super) fn render_orchestrate_configuration(
    settings: &OrchestrateSettings,
    caller: Option<(ProviderKind, Option<&str>)>,
    catalogs: &HashMap<ProviderKind, Vec<agent::ModelSpec>>,
) -> String {
    let mut text = String::from(
        "## Current orchestrator configuration\n\nUse tcode_orchestrate for collaboration and execution. Compare enabled profiles across all providers by their task fit, strengths, limitations, and cost; provider family is not a routing preference.\n",
    );
    for (heading, tool, profiles) in [
        (
            "Collaboration models",
            "collaborate",
            &settings.decision_models,
        ),
        ("Execution models", "dispatch", &settings.child_models),
    ] {
        text.push_str(&format!("\n### {heading} — `{tool}`\n\n"));
        text.push_str("Choose the model across providers, then set the tool's effort parameter from that model's available values according to its description and task difficulty. Effort is a per-call choice; omitted effort uses medium when available, otherwise the provider default. Headings list provider / model: pass these as provider and model. Set profile only when the entry explicitly lists a profile ID; otherwise omit it for the built-in endpoint. profile selects an endpoint configuration, not a model. Fast mode follows the configuration unless the user explicitly requests an override. Descriptions under peer headings belong to those peers, not to the main thread. The main thread's own peer entry is omitted.\n");
        let collaboration = tool == "collaborate";
        let available: Vec<_> = profiles
            .iter()
            .filter(|entry| {
                entry.enabled
                    && !(collaboration
                        && caller.is_some_and(|(provider, model)| {
                            // Before discovery identifies the lead, withhold that
                            // provider's peer identities rather than guessing its model.
                            provider == entry.provider
                                && model.is_none_or(|model| model == entry.model)
                        }))
            })
            .map(|entry| {
                let catalog = catalogs
                    .get(&entry.provider)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                (
                    entry,
                    model_missing_from_loaded_catalog(catalog, &entry.model),
                    orchestrate_efforts(entry.provider, &entry.model, catalog, collaboration),
                )
            })
            .filter(|(_, unavailable, choices)| {
                *unavailable || !collaboration || !choices.is_empty()
            })
            .collect();
        if available.is_empty() {
            text.push_str(&format!(
                "No eligible configured models; `{tool}` is unavailable with the current configuration.\n"
            ));
        }
        for (entry, unavailable, choices) in available {
            let mut permission_values = String::new();
            if let Some(descriptor) = permission_control(entry.provider) {
                permission_values.push_str(
                    "\nNative `permission` values (omit to use the child approval setting):\n",
                );
                match descriptor {
                    OptionDescriptor::Select {
                        options,
                        recommended,
                        ..
                    } => {
                        for option in options.iter().filter(|option| option.unavailable.is_none()) {
                            let marker = if recommended.as_deref() == Some(&option.value) {
                                " (recommended)"
                            } else {
                                ""
                            };
                            permission_values.push_str(&format!(
                                "- {} — {}: {}{marker}\n",
                                option.value,
                                option.label,
                                option.description.as_deref().unwrap_or_default()
                            ));
                        }
                    }
                    OptionDescriptor::Boolean { .. } => {}
                }
            } else {
                permission_values.push_str("\nNo native permission control; omit `permission`.\n");
            }
            let profile = entry
                .profile_id
                .as_ref()
                .map(|id| format!(" — profile `{}`", escape_markdown_inline(id)))
                .unwrap_or_default();
            let fast = if entry.fast { " — fast mode" } else { "" };
            if unavailable {
                text.push_str(&format!(
                    "\n#### `{}` / `{}` — unavailable{fast}{profile}\n\nUnavailable: model `{}` is not present in the loaded `{}` catalog.\n\n{}\n",
                    provider_name(entry.provider),
                    escape_markdown_inline(&entry.model),
                    escape_markdown_inline(&entry.model),
                    provider_name(entry.provider),
                    entry.guidance(collaboration).trim()
                ));
                text.push_str(&permission_values);
                continue;
            }
            let efforts = if choices.is_empty() {
                "omit (provider default)".to_string()
            } else {
                choices
                    .iter()
                    .map(|effort| format!("`{}`", escape_markdown_inline(effort)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            text.push_str(&format!(
                "\n#### `{}` / `{}` — available `effort`: {efforts}{fast}{profile}\n\n{}\n",
                provider_name(entry.provider),
                escape_markdown_inline(&entry.model),
                entry.guidance(collaboration).trim()
            ));
            text.push_str(&permission_values);
        }
    }
    text
}

pub(super) fn escape_markdown_inline(value: &str) -> String {
    value.replace('`', "\\`").replace(['\r', '\n'], " ")
}

/// `(provider, model, effort, fast, profile_id)` of the child profile a
/// dispatch resolved to.
pub(super) type ResolvedDispatch = (ProviderKind, String, Option<String>, bool, Option<String>);

/// Validate an MCP dispatch against the configured child-model allow list and
/// fill in its model/default effort. The main model is unrestricted; this gate
/// applies only to newly-created child sessions.
pub(super) fn resolve_orchestrate_dispatch(
    settings: &OrchestrateSettings,
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    profile: Option<&str>,
    catalogs: &HashMap<ProviderKind, Vec<agent::ModelSpec>>,
) -> Result<ResolvedDispatch, String> {
    resolve_orchestrate_profiles(
        &settings.child_models,
        provider,
        model,
        effort,
        profile,
        catalogs,
        false,
    )
}

pub(super) fn resolve_orchestrate_collaboration(
    settings: &OrchestrateSettings,
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    profile: Option<&str>,
    catalogs: &HashMap<ProviderKind, Vec<agent::ModelSpec>>,
) -> Result<ResolvedDispatch, String> {
    resolve_orchestrate_profiles(
        &settings.decision_models,
        provider,
        model,
        effort,
        profile,
        catalogs,
        true,
    )
}

/// A native provider as a model names it: `claude`, `codex`, `opencode`, …
pub(super) fn native_provider_named(name: &str) -> Option<ProviderKind> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" | "claude-code" => Some(ProviderKind::ClaudeCode),
        "codex" => Some(ProviderKind::Codex),
        "pi" => Some(ProviderKind::Pi),
        "opencode" | "open_code" | "open-code" => Some(ProviderKind::OpenCode),
        "cursor" => Some(ProviderKind::Cursor),
        "grok" => Some(ProviderKind::Grok),
        _ => None,
    }
}

fn resolve_orchestrate_profiles(
    profiles: &[OrchestrateChildModel],
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
    profile: Option<&str>,
    catalogs: &HashMap<ProviderKind, Vec<agent::ModelSpec>>,
    collaboration: bool,
) -> Result<ResolvedDispatch, String> {
    let provider = match native_provider_named(provider) {
        Some(provider) => provider,
        None if provider.trim().eq_ignore_ascii_case("acp") => {
            return Err(
                "ACP child dispatch is not available yet; configure a native-provider child model"
                    .into(),
            );
        }
        None => return Err(format!("unknown provider: {}", provider.trim())),
    };
    let requested_model = model.map(str::trim).filter(|model| !model.is_empty());
    let requested_effort = effort.map(str::trim).filter(|effort| !effort.is_empty());
    let requested_profile = profile.map(str::trim).filter(|profile| !profile.is_empty());
    let candidates: Vec<_> = profiles
        .iter()
        .filter(|entry| {
            entry.enabled
                && entry.provider == provider
                && requested_model.is_none_or(|model| entry.model == model)
                && !entry.model.trim().is_empty()
        })
        .filter(|entry| {
            requested_profile.is_none_or(|requested| {
                entry
                    .profile_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(requested))
            })
        })
        .collect();
    let child = if requested_profile.is_some() {
        candidates.first().copied()
    } else {
        candidates
            .iter()
            .find(|entry| entry.profile_id.is_none())
            .copied()
            .or_else(|| candidates.first().copied())
    }
    .ok_or_else(|| {
        let enabled = profiles.iter()
            .filter(|entry| entry.enabled && entry.provider == provider)
            .map(|entry| {
                match entry.profile_id.as_deref() {
                    Some(profile_id) => format!("model={} (profile {profile_id})", entry.model),
                    None => format!("model={} (built-in endpoint; omit profile)", entry.model),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let requested = requested_model.unwrap_or("provider default model");
        let profile = requested_profile
            .map(|profile| format!(" under profile {profile}"))
            .unwrap_or_default();
        format!(
            "no enabled profile matches {requested}{profile} under {}; enabled model/endpoint combinations: {}. profile selects a provider endpoint, not a model; pass the model name as model and omit profile unless the configuration lists an explicit profile ID. Effort has not been validated yet.",
            provider_name(provider),
            if enabled.is_empty() { "none" } else { &enabled }
        )
    })?;
    let catalog = catalogs
        .get(&provider)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if model_missing_from_loaded_catalog(catalog, &child.model) {
        return Err(format!(
            "model `{}` is unavailable for provider `{}`: not present in the loaded catalog",
            child.model,
            provider_name(provider)
        ));
    }
    let available = orchestrate_efforts(provider, &child.model, catalog, collaboration);
    let selected_effort = match requested_effort {
        Some(requested) => Some(
            available
                .iter()
                .find(|effort| effort.eq_ignore_ascii_case(requested))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "unsupported effort {requested} for {}; available effort values: {}",
                        child.model,
                        if available.is_empty() {
                            "none; omit effort".into()
                        } else {
                            available.join(", ")
                        }
                    )
                })?,
        ),
        None => available
            .iter()
            .find(|effort| effort.as_str() == "medium")
            .or_else(|| collaboration.then(|| available.first()).flatten())
            .cloned(),
    };
    if collaboration && selected_effort.is_none() {
        return Err(format!(
            "{} has no available medium/high collaboration effort",
            child.model
        ));
    }
    Ok((
        provider,
        child.model.clone(),
        selected_effort,
        child.fast,
        child.profile_id.clone(),
    ))
}

fn model_missing_from_loaded_catalog(catalog: &[agent::ModelSpec], model: &str) -> bool {
    !catalog.is_empty() && !catalog.iter().any(|spec| spec.id == model)
}

fn resolve_child_permission(
    descriptor: Option<OptionDescriptor>,
    mode: ChildApprovalMode,
    explicit: Option<&str>,
) -> Result<Option<OptionSelection>, String> {
    let Some(descriptor) = descriptor else {
        return if explicit.is_some() {
            Err("this provider has no permission control; valid permission values: none (omit permission)".into())
        } else {
            Ok(None)
        };
    };
    let selection = match descriptor {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            recommended,
            permissive,
            ..
        } => {
            let value = if let Some(value) = explicit {
                if !options
                    .iter()
                    .any(|option| option.value == value && option.unavailable.is_none())
                {
                    let valid = options
                        .iter()
                        .filter(|option| option.unavailable.is_none())
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "unknown permission: {value}; valid permission values: {valid}"
                    ));
                }
                Some(value.to_string())
            } else {
                match mode {
                    ChildApprovalMode::Auto => recommended.or(default_value),
                    ChildApprovalMode::AlwaysAllow => permissive.or(default_value),
                    ChildApprovalMode::Orchestrator | ChildApprovalMode::Manual => default_value,
                }
            };
            value.map(|value| OptionSelection {
                id,
                value: serde_json::Value::String(value),
            })
        }
        OptionDescriptor::Boolean { .. } => {
            return Err("this provider has no selectable permission values".into());
        }
    };
    Ok(selection)
}

fn native_allow_option(request: &agent::ApprovalRequest) -> Option<&agent::ApprovalOption> {
    use agent::ApprovalOptionKind::{AllowAlways, AllowOnce};
    request
        .options
        .iter()
        .find(|option| option.kind == AllowAlways)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|option| option.kind == AllowOnce)
        })
}

fn resolve_approval_decision(
    option: Option<&str>,
    cancel: bool,
    request: &agent::ApprovalRequest,
) -> Result<ApprovalDecision, String> {
    match (option, cancel) {
        (Some(id), false) if request.options.iter().any(|option| option.id == id) => {
            Ok(ApprovalDecision::Option(id.to_string()))
        }
        (Some(id), false) => Err(format!(
            "unknown option: {id}; valid option ids: {}",
            request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        (None, true) => Ok(ApprovalDecision::Cancel),
        _ => Err("supply option or cancel: true, never both".into()),
    }
}

pub(super) fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Cursor => "cursor",
        ProviderKind::Grok => "grok",
        ProviderKind::Acp => "acp",
    }
}

fn resolve_child_worktree(
    cwd: PathBuf,
    child_id: &str,
    data_dir: &Path,
) -> (PathBuf, Option<WorktreeInfo>, Option<String>) {
    match provision(&cwd, child_id, data_dir) {
        Ok(created) => {
            let info = WorktreeInfo {
                root_project_path: cwd,
                base: created.base,
                branch: created.branch,
            };
            (created.path, Some(info), None)
        }
        Err(ProvisionError::NotRepositoryRoot { path }) => {
            let warning = format!(
                "worktree isolation unavailable: {} is not a Git repository root; using the plain cwd",
                path.display()
            );
            log::warn!("orchestrate dispatch: {warning}");
            (cwd, None, Some(warning))
        }
        Err(error) => {
            let warning = format!("worktree isolation failed ({error}); using the plain cwd");
            log::warn!("orchestrate dispatch: {warning}");
            (cwd, None, Some(warning))
        }
    }
}

#[allow(clippy::too_many_arguments)] // Dispatch accepts the provider launch and child lifecycle settings.
pub(super) fn build_child_meta(
    parent: &SessionMeta,
    provider: ProviderKind,
    model: Option<String>,
    effort: Option<String>,
    fast: bool,
    profile_id: Option<String>,
    permission_selection: Option<OptionSelection>,
    cwd: PathBuf,
    result_max_chars: Option<u32>,
) -> SessionMeta {
    let mut meta = SessionMeta::new(provider, cwd, model);
    meta.project_id = parent.project_id.clone();
    meta.parent_session_id = Some(parent.id.clone());
    meta.profile_id = profile_id;
    meta.option_selections.extend(permission_selection);
    meta.result_max_chars = result_max_chars;
    if let Some(effort) = effort {
        meta.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::Value::String(effort),
        });
    }
    apply_fast_selection(&mut meta.option_selections, provider, fast);
    meta
}

/// Set or clear the provider's fast-mode selection in `selections`. Other
/// selections (a Codex `flex` tier, say) are left alone.
fn apply_fast_selection(selections: &mut Vec<OptionSelection>, provider: ProviderKind, fast: bool) {
    let Some((id, value)) = fast_selection(provider) else {
        return;
    };
    selections.retain(|selection| !(selection.id == id && selection.value == value));
    if fast {
        selections.push(OptionSelection {
            id: id.into(),
            value,
        });
    }
}

/// The option selection that turns on a provider's fast mode: Claude's
/// `fastMode` launch setting, Codex's `fast` service tier. `None` for
/// providers without one.
fn fast_selection(provider: ProviderKind) -> Option<(&'static str, serde_json::Value)> {
    match provider {
        ProviderKind::ClaudeCode => Some(("fastMode", serde_json::Value::Bool(true))),
        ProviderKind::Codex => Some(("serviceTier", serde_json::Value::String("fast".into()))),
        _ => None,
    }
}

pub(super) fn final_assistant_message(timeline: &Timeline) -> String {
    let Some((last_index, last)) = timeline
        .entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { .. })
            )
        })
    else {
        return String::new();
    };

    // One provider message may contain several adjacent text blocks. They are
    // separate timeline entries, but together form the final assistant output.
    // Stop at the first non-assistant item so tool preambles from earlier in the
    // turn are not mistaken for part of the final answer.
    let mut parts = Vec::new();
    for entry in timeline.entries[..=last_index].iter().rev() {
        if entry.turn != last.turn {
            break;
        }
        match &entry.content {
            EntryContent::Item(ItemContent::AssistantMessage { text }) => parts.push(text.as_str()),
            _ => break,
        }
    }
    parts.reverse();
    parts.concat()
}

/// The error entry a failed turn ended on, when it is the thread's last entry.
fn trailing_error(timeline: &Timeline) -> Option<&str> {
    match &timeline.entries.last()?.content {
        EntryContent::Error { message, .. } => Some(message),
        EntryContent::ProviderStartError { error } => Some(error),
        _ => None,
    }
}

/// The error of a provider start that failed after the thread's last entry.
fn trailing_start_error(timeline: &Timeline) -> Option<&str> {
    match &timeline.entries.last()?.content {
        EntryContent::ProviderStartError { error } => Some(error),
        _ => None,
    }
}

pub(super) fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max)).collect()
}

pub(super) fn approval_request_summary(request: &agent::ApprovalRequest) -> String {
    let detail = match &request.kind {
        agent::ApprovalKind::ExecCommand { command, .. } => format!("command `{command}`"),
        agent::ApprovalKind::FileRead { detail } => format!("file read `{detail}`"),
        agent::ApprovalKind::FileChange { changes, .. } => match changes.as_slice() {
            [change] => format!("file change `{}`", change.path),
            changes => format!("{} file changes", changes.len()),
        },
        agent::ApprovalKind::ToolUse { name, .. } => format!("tool `{name}`"),
    };
    let one_line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = one_line.chars().take(180).collect();
    if one_line.chars().count() > 180 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

pub(super) fn token_usage_json(usage: &agent::TokenUsage) -> serde_json::Value {
    let mut value = serde_json::Map::new();
    for (key, count) in [
        ("input_tokens", usage.input_tokens),
        ("cached_input_tokens", usage.cached_input_tokens),
        ("output_tokens", usage.output_tokens),
        ("used_tokens", usage.used_tokens),
        ("total_processed_tokens", usage.total_processed_tokens),
    ] {
        if let Some(count) = count {
            value.insert(key.into(), count.into());
        }
    }
    serde_json::Value::Object(value)
}

pub(super) enum CallbackBody<'a> {
    /// The child's own report, delivered verbatim.
    Report(&'a str),
    /// The child never reported: its final message stands in, bounded.
    Fallback {
        final_message: &'a str,
        max_chars: Option<u32>,
    },
    Failure(&'a str),
}

pub(super) fn assemble_report_callback(
    child_id: &str,
    title: &str,
    state: &str,
    body: CallbackBody<'_>,
    usage: Option<&agent::TokenUsage>,
) -> String {
    let mut token_parts = Vec::new();
    if let Some(usage) = usage {
        if let Some(input) = usage.input_tokens {
            let cached = usage
                .cached_input_tokens
                .filter(|cached| *cached > 0)
                .map(|cached| format!(" (+{cached} cached)"))
                .unwrap_or_default();
            token_parts.push(format!("input {input}{cached}"));
        }
        if let Some(output) = usage.output_tokens {
            token_parts.push(format!("output {output}"));
        }
        if let Some(total) = usage.total_processed_tokens.or(usage.used_tokens) {
            token_parts.push(format!("total {total}"));
        }
    }
    let token_segment = if token_parts.is_empty() {
        String::new()
    } else {
        format!(" tokens: {}.", token_parts.join(", "))
    };
    let body = match body {
        CallbackBody::Report(report) => format!("{ORCHESTRATE_REPORT_LABEL}{report}"),
        CallbackBody::Fallback {
            final_message,
            max_chars,
        } => {
            let digest = if final_message.is_empty() {
                "(no assistant output)".to_string()
            } else {
                let count = final_message.chars().count();
                let cap = max_chars.unwrap_or(1200) as usize;
                if cap == 0 || count <= cap {
                    final_message.to_string()
                } else {
                    format!(
                        "{ORCHESTRATE_OUTPUT_TAIL_LABEL}{count} chars total; result {child_id} has the full text):\n{}",
                        tail_chars(final_message, 600.min(cap))
                    )
                }
            };
            format!("{ORCHESTRATE_FALLBACK_LABEL}{digest}")
        }
        CallbackBody::Failure(error) => error.to_string(),
    };
    format!(
        "{} {state}.{token_segment}\n{body}",
        callback_header(child_id, title)
    )
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn missing_permission_hints_fall_back_and_control_less_providers_get_no_selection() {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::OpenCode] {
            let mut descriptor = permission_control(provider).unwrap();
            match &mut descriptor {
                OptionDescriptor::Select {
                    recommended,
                    permissive,
                    ..
                } => {
                    *recommended = None;
                    *permissive = None;
                }
                OptionDescriptor::Boolean {
                    recommended,
                    permissive,
                    ..
                } => {
                    *recommended = None;
                    *permissive = None;
                }
            }
            let expected = if provider == ProviderKind::ClaudeCode {
                serde_json::json!("default")
            } else {
                serde_json::json!("normal")
            };
            for mode in [ChildApprovalMode::Auto, ChildApprovalMode::AlwaysAllow] {
                assert_eq!(
                    resolve_child_permission(Some(descriptor.clone()), mode, None)
                        .unwrap()
                        .unwrap()
                        .value,
                    expected
                );
            }
        }
        for provider in [ProviderKind::Pi, ProviderKind::Acp] {
            for mode in [
                ChildApprovalMode::Orchestrator,
                ChildApprovalMode::Auto,
                ChildApprovalMode::AlwaysAllow,
                ChildApprovalMode::Manual,
            ] {
                assert_eq!(
                    resolve_child_permission(permission_control(provider), mode, None).unwrap(),
                    None
                );
                assert!(
                    resolve_child_permission(permission_control(provider), mode, Some("true"))
                        .unwrap_err()
                        .contains("valid permission values: none")
                );
            }
        }
    }
}
