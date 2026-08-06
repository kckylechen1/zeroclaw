use crate::tools;
use std::sync::Arc;

// Debug so a failing routing assertion can print which variant and which
// source it actually got; without it the test just says "assertion failed".
#[derive(Debug)]
pub(crate) enum RoutedApproval {
    /// Use this response. `decider` names the channel that answered, for audit
    /// attribution; `None` for a bridge-synthesized fail-closed deny.
    ///
    /// `source` says whether a human actually decided. `decider` cannot answer
    /// that on its own: it is also `None` when a single non-fan-out channel
    /// relays a real operator answer.
    Decided {
        response: zeroclaw_api::channel::ChannelApprovalResponse,
        decider: Option<String>,
        source: zeroclaw_api::channel::ApprovalSource,
    },
    /// Explicit `InheritOriginator` — defer to the originating-channel fan-out.
    Fallthrough,
}

pub(crate) async fn resolve_routed_approval(
    handles: &tools::PerToolChannelHandle,
    route: &zeroclaw_config::autonomy::ApprovalRoute,
    recipient: &str,
    request: &zeroclaw_api::channel::ChannelApprovalRequest,
) -> RoutedApproval {
    let approver: Option<(String, Arc<dyn zeroclaw_api::channel::Channel>)> = handles
        .read()
        .iter()
        .find(|(name, _)| name.as_str() == route.approver_channel)
        .map(|(name, channel)| (name.clone(), Arc::clone(channel)));

    // `source` is tracked alongside `reason` so the fail-closed deny below can
    // say WHY no operator decided, rather than leaving the caller to guess from
    // a missing decider.
    let (reason, source): (&str, zeroclaw_api::channel::ApprovalSource) =
        if let Some((channel_name, channel)) = approver {
            let dur = std::time::Duration::from_secs(route.timeout_secs.max(1));
            // Attributed, not legacy: if the approver channel synthesizes its own
            // `Some(Deny)` (its inner timeout firing before this outer one), that
            // is a runtime denial and must not be relabelled as the approver's
            // decision just because a response came back.
            match tokio::time::timeout(dur, channel.request_approval_attributed(recipient, request))
                .await
            {
                Ok(Ok(Some(attributed))) => {
                    return RoutedApproval::Decided {
                        response: attributed.response,
                        decider: Some(channel_name),
                        source: attributed.source,
                    };
                }
                Ok(Ok(None)) => (
                    "approver returned no decision",
                    zeroclaw_api::channel::ApprovalSource::Unreachable,
                ),
                Ok(Err(_)) => (
                    "approver channel unreachable",
                    zeroclaw_api::channel::ApprovalSource::Unreachable,
                ),
                Err(_) => (
                    "approver timed out",
                    zeroclaw_api::channel::ApprovalSource::TimedOut,
                ),
            }
        } else {
            (
                "approver channel not registered",
                zeroclaw_api::channel::ApprovalSource::Unavailable,
            )
        };

    match route.on_no_approver {
        zeroclaw_config::autonomy::OnNoApprover::Deny => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "tool": request.tool_name,
                        "approver_channel": route.approver_channel,
                        "reason": reason,
                        "policy": "deny",
                    })),
                "approval route fail-closed: denying gated tool"
            );
            RoutedApproval::Decided {
                response: zeroclaw_api::channel::ChannelApprovalResponse::Deny,
                decider: None,
                // The runtime denied this, not a person. Carrying the specific
                // reason lets the tool result say so instead of reporting a
                // user denial that never happened.
                source,
            }
        }
        zeroclaw_config::autonomy::OnNoApprover::InheritOriginator => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "tool": request.tool_name,
                        "approver_channel": route.approver_channel,
                        "reason": reason,
                        "policy": "inherit-originator",
                    })),
                "approval route falling back to originating channel"
            );
            RoutedApproval::Fallthrough
        }
    }
}

pub(crate) struct RoutedApprovalChannel {
    handles: tools::PerToolChannelHandle,
    route: zeroclaw_config::autonomy::ApprovalRoute,
}

impl RoutedApprovalChannel {
    pub(crate) fn new(
        handles: tools::PerToolChannelHandle,
        route: zeroclaw_config::autonomy::ApprovalRoute,
    ) -> Self {
        Self { handles, route }
    }
}

impl ::zeroclaw_api::attribution::Attributable for RoutedApprovalChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(::zeroclaw_api::attribution::ChannelKind::Cli)
    }
    fn alias(&self) -> &str {
        "approval-route"
    }
}

#[async_trait::async_trait]
impl zeroclaw_api::channel::Channel for RoutedApprovalChannel {
    fn name(&self) -> &str {
        "approval-route"
    }

    async fn send(&self, _message: &zeroclaw_api::channel::SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<zeroclaw_api::channel::ChannelMessage>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Non-attributed entry point: delegates to
    /// [`Self::request_approval_attributed`] and drops the attribution so the
    /// routing decision lives in exactly one place.
    async fn request_approval(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|attributed| attributed.response))
    }

    async fn request_approval_attributed(
        &self,
        recipient: &str,
        request: &zeroclaw_api::channel::ChannelApprovalRequest,
    ) -> anyhow::Result<Option<zeroclaw_api::channel::AttributedApprovalResponse>> {
        match resolve_routed_approval(&self.handles, &self.route, recipient, request).await {
            // The deciding approver's name travels on the response itself;
            // `None` for a bridge-synthesized fail-closed deny.
            //
            // Cross-crate construction: `AttributedApprovalResponse` is
            // `#[non_exhaustive]`, so struct-literal syntax is forbidden from
            // here. Build via the dedicated constructors.
            RoutedApproval::Decided {
                response,
                decider,
                source,
            } => Ok(Some(
                zeroclaw_api::channel::AttributedApprovalResponse::from_runtime(response, source)
                    .with_decider_opt(decider),
            )),
            // No originating channel to inherit on this path; let the gate apply
            // the non-interactive default (auto-deny).
            RoutedApproval::Fallthrough => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use zeroclaw_api::channel::{ChannelApprovalRequest, ChannelApprovalResponse};
    use zeroclaw_config::autonomy::{ApprovalRoute, OnNoApprover};

    enum StubBehavior {
        Answer(ChannelApprovalResponse),
        NoDecision,
        Slow,
    }

    struct StubChannel {
        name: String,
        behavior: StubBehavior,
    }

    impl zeroclaw_api::attribution::Attributable for StubChannel {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Channel(zeroclaw_api::attribution::ChannelKind::Cli)
        }
        fn alias(&self) -> &str {
            &self.name
        }
    }

    #[async_trait::async_trait]
    impl zeroclaw_api::channel::Channel for StubChannel {
        fn name(&self) -> &str {
            &self.name
        }
        async fn send(&self, _m: &zeroclaw_api::channel::SendMessage) -> anyhow::Result<()> {
            Ok(())
        }
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<zeroclaw_api::channel::ChannelMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn request_approval(
            &self,
            _recipient: &str,
            _request: &ChannelApprovalRequest,
        ) -> anyhow::Result<Option<ChannelApprovalResponse>> {
            match &self.behavior {
                StubBehavior::Answer(resp) => Ok(Some(resp.clone())),
                StubBehavior::NoDecision => Ok(None),
                StubBehavior::Slow => {
                    // Far exceeds the route timeout; with a paused clock the
                    // timeout fires at +timeout_secs virtual time, instantly.
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Ok(Some(ChannelApprovalResponse::Approve))
                }
            }
        }
    }

    fn registry(channels: Vec<StubChannel>) -> tools::PerToolChannelHandle {
        let mut map: HashMap<String, Arc<dyn zeroclaw_api::channel::Channel>> = HashMap::new();
        for c in channels {
            map.insert(c.name.clone(), Arc::new(c));
        }
        Arc::new(RwLock::new(map))
    }

    fn req() -> ChannelApprovalRequest {
        ChannelApprovalRequest {
            tool_name: "shell".into(),
            arguments_summary: "rm -rf /".into(),
            raw_arguments: None,
        }
    }

    fn route(approver: &str, policy: OnNoApprover) -> ApprovalRoute {
        ApprovalRoute {
            approver_channel: approver.into(),
            on_no_approver: policy,
            timeout_secs: 1,
        }
    }

    #[tokio::test]
    async fn approver_answer_is_used_and_attributed() {
        let h = registry(vec![StubChannel {
            name: "ops".into(),
            behavior: StubBehavior::Answer(ChannelApprovalResponse::Approve),
        }]);
        match resolve_routed_approval(&h, &route("ops", OnNoApprover::Deny), "r", &req()).await {
            RoutedApproval::Decided {
                response,
                decider,
                source,
            } => {
                assert_eq!(response, ChannelApprovalResponse::Approve);
                assert_eq!(
                    decider.as_deref(),
                    Some("ops"),
                    "decider names the approver"
                );
                assert_eq!(
                    source,
                    zeroclaw_api::channel::ApprovalSource::Operator,
                    "an approver's answer is an operator decision"
                );
            }
            RoutedApproval::Fallthrough => panic!("expected a routed decision"),
        }
    }

    #[tokio::test]
    async fn unregistered_approver_fails_closed_by_default() {
        let h = registry(vec![]);
        match resolve_routed_approval(&h, &route("ops", OnNoApprover::Deny), "r", &req()).await {
            RoutedApproval::Decided {
                response,
                decider,
                source,
            } => {
                assert_eq!(response, ChannelApprovalResponse::Deny, "fail-closed deny");
                assert!(decider.is_none(), "synthetic deny has no decider");
                // The regression this guards: a fail-closed deny is Some(Deny)
                // with no decider, so anything inferring "a user decided" from
                // the presence of a response reports a denial nobody made.
                assert_eq!(
                    source,
                    zeroclaw_api::channel::ApprovalSource::Unavailable,
                    "an unregistered approver is a runtime denial, not a user's"
                );
                assert!(source.is_runtime_fail_closed());
            }
            RoutedApproval::Fallthrough => panic!("default policy must NOT fall through"),
        }
    }

    #[tokio::test]
    async fn unregistered_approver_inherits_when_opted_in() {
        let h = registry(vec![]);
        let out = resolve_routed_approval(
            &h,
            &route("ops", OnNoApprover::InheritOriginator),
            "r",
            &req(),
        )
        .await;
        assert!(
            matches!(out, RoutedApproval::Fallthrough),
            "InheritOriginator must fall through to the originating fan-out"
        );
    }

    #[tokio::test]
    async fn no_decision_fails_closed() {
        let h = registry(vec![StubChannel {
            name: "ops".into(),
            behavior: StubBehavior::NoDecision,
        }]);
        let out = resolve_routed_approval(&h, &route("ops", OnNoApprover::Deny), "r", &req()).await;
        assert!(
            matches!(
                out,
                RoutedApproval::Decided {
                    response: ChannelApprovalResponse::Deny,
                    source: zeroclaw_api::channel::ApprovalSource::Unreachable,
                    ..
                }
            ),
            "an approver that returns no decision is a runtime denial: {out:?}"
        );
    }

    // The route timeout (1s) fires and cancels the stub's long sleep, so this
    // resolves in ~1s of real time without needing tokio's `test-util` clock.
    #[tokio::test]
    async fn slow_approver_times_out_and_fails_closed() {
        let h = registry(vec![StubChannel {
            name: "ops".into(),
            behavior: StubBehavior::Slow,
        }]);
        let out = resolve_routed_approval(&h, &route("ops", OnNoApprover::Deny), "r", &req()).await;
        // A timeout is the case most easily mistaken for a user's "no": the
        // route returns Some(Deny) exactly as an operator denial would.
        assert!(
            matches!(
                out,
                RoutedApproval::Decided {
                    response: ChannelApprovalResponse::Deny,
                    source: zeroclaw_api::channel::ApprovalSource::TimedOut,
                    ..
                }
            ),
            "a timed-out approver is a runtime denial, not a user's: {out:?}"
        );
    }

    use zeroclaw_api::channel::Channel as _;

    #[tokio::test]
    async fn routed_channel_returns_and_attributes_approver_decision() {
        let h = registry(vec![StubChannel {
            name: "ops".into(),
            behavior: StubBehavior::Answer(ChannelApprovalResponse::Approve),
        }]);
        let bridge = RoutedApprovalChannel::new(h, route("ops", OnNoApprover::Deny));
        let out = bridge
            .request_approval_attributed("r", &req())
            .await
            .unwrap()
            .expect("the approver decided");
        assert_eq!(out.response, ChannelApprovalResponse::Approve);
        assert_eq!(
            out.decided_by.as_deref(),
            Some("ops"),
            "the gate attributes the approval to the deciding channel"
        );
    }

    #[tokio::test]
    async fn routed_channel_fails_closed_when_approver_unregistered() {
        let bridge = RoutedApprovalChannel::new(registry(vec![]), route("ops", OnNoApprover::Deny));
        let out = bridge
            .request_approval_attributed("r", &req())
            .await
            .unwrap()
            .expect("the fail-closed deny is a decision");
        assert_eq!(
            out.response,
            ChannelApprovalResponse::Deny,
            "unreachable approver denies, not auto-approves"
        );
        assert!(
            out.decided_by.is_none(),
            "a bridge-synthesized fail-closed deny has no deciding channel"
        );
    }

    #[tokio::test]
    async fn routed_channel_inherit_returns_none_on_channelless_path() {
        let bridge = RoutedApprovalChannel::new(
            registry(vec![]),
            route("ops", OnNoApprover::InheritOriginator),
        );
        let out = bridge.request_approval("r", &req()).await.unwrap();
        assert_eq!(
            out, None,
            "no originator to inherit; gate applies the non-interactive auto-deny"
        );
    }
}
