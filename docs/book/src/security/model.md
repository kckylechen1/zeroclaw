# The security model

ZeroClaw's security model gates what the agent is allowed to do at runtime. There are six layers. From outer to inner:

## Channel pairing and access control

Before a message from a channel reaches the agent, the channel's pairing and allow-list are checked. `allowed_users`, `allowed_chats`, IP allowlists for webhooks, all enforced at the channel adapter, before the runtime sees the event.

Docs: each channel's page under [Channels](../channels/overview.md).

## Autonomy level

The coarse-grained knob. Three settings:

- **ReadOnly**: the agent can observe (read files, query memory, fetch URLs it's allowed to fetch) but cannot write or execute commands.
- **Supervised** (default): low-risk ops run; medium-risk ask the operator; high-risk block.
- **Full**: no approval gates; `workspace_only` is implicitly disabled. `forbidden_paths`, `forbidden_commands`, and the OS sandbox still enforce.

Docs: [Autonomy levels](./autonomy.md).

## Workspace boundary and path rules

The agent operates within a configured workspace directory. `file_read`, `file_write`, and `shell` (for commands that touch the filesystem) refuse paths outside it unless `workspace_only = false`.

**Per-session sandbox roots (ACP and gateway WebSocket):** When a session is opened via ACP (`session/new` with a `cwd` parameter) or via the gateway WebSocket (connect-time `cwd` parameter), that path becomes the `SecurityPolicy` workspace boundary for all file and shell tools for the lifetime of the session. The daemon's global `workspace_dir` remains the data directory for memory, identity, cron, and other persistent state. The model is: `session cwd` = project boundary the agent can touch; `workspace_dir` = where ZeroClaw stores its own files. Note: the agent's system prompt currently reflects the daemon's `workspace_dir` rather than the session `cwd`; enforcement is correct but the model's self-reported location may differ.

**Important:** the `cwd` parameter changes which directory on the **ZeroClaw host** the agent is sandboxed to, it does not affect which machine tools run on. Tool use (shell commands, file reads/writes) always executes on the machine running ZeroClaw. If you connect to a remote ZeroClaw instance over the gateway WebSocket, tool calls operate on the remote machine's filesystem, not on your local machine. For localhost-only deployments this distinction does not matter, but remote setups should account for it.

Beyond the workspace, a `forbidden_paths` list (default: `/etc`, `/sys`, `/boot`, `~/.ssh`, …) is always blocked regardless of workspace setting.

## Shell command policy

For shell invocations:

- `allowed_commands`: if non-empty, shell only runs commands whose basename is in this list
- `forbidden_commands`: explicit denylist (`rm -rf /`, `shutdown`, kernel operations)
- `validate_command_execution`: a pattern-matching pass that looks for dangerous flags, pipelines, and argument shapes

The validator runs *before* the command hits the shell. A blocked command surfaces as a tool error the model sees and can react to.

## OS-level sandbox

When a sandbox backend is available, tool invocations run inside it:

| Platform | Default backend |
|---|---|
| Linux | Landlock (kernel) / Bubblewrap / Firejail / Docker, auto-detected |
| macOS | Seatbelt (native) |
| Windows | AppContainer (experimental) |
| Any | Docker (if the daemon is reachable) |

The sandbox confines filesystem access to the workspace, drops network reachability except what the tool explicitly needs, and removes access to the parent process's secrets.

Docs: [Sandboxing](./sandboxing.md).

## Tool receipts

Tool receipts provide HMAC evidence that a successful tool call and its result passed through the runtime. When receipts are enabled, successful tool outputs receive an HMAC-SHA256 receipt over the call and result, and the receipt is fed back into the conversation with the tool result.

Receipts help catch fabricated tool claims. They are not a chained or durable audit log today: receipt keys are ephemeral, receipts are not cross-signed with the conversation hash, and persistent receipt storage is still future work.

Docs: [Tool receipts](./tool-receipts.md).

## Additional gates

Beyond the six layers:

- **OTP (authentication only)**: `[security.otp]` configures TOTP authentication for the e-stop resume challenge. It is not an action-authorization policy; see [OTP scope and action authority](#otp-scope-and-action-authority).
- **Emergency stop**: `zeroclaw estop` halts all in-flight tool calls. Resuming requires an OTP when both `[security.estop] enabled = true` and `require_otp_to_resume = true`.
- **Prompt injection guard**: scans model output for known injection patterns before tool calls are validated.
- **Leak detector**: scans outbound channel responses for credentials and redacts matches before delivery. It covers deterministic credential patterns and can also run a standalone high-entropy-token heuristic.
- **Pairing guard**: device pairing for channel auth; prevents stolen credentials from working on a new device.

## OTP scope and action authority

The live OTP fields have a narrow, inspectable purpose:

| Field | Runtime consumer | Effect |
|---|---|---|
| `enabled` | CLI startup and `zeroclaw estop resume` | Initializes the TOTP secret when enabled. An e-stop resume that requires OTP is refused when this is false. |
| `token_ttl_secs` | `OtpValidator` | Sets the TOTP time step and enrollment URI period. |
| `cache_valid_secs` | `OtpValidator` | Sets the in-memory reuse window after a code validates. |
| `security.estop.require_otp_to_resume` | `EstopManager::resume` and the CLI resume path | Requires the trusted operator to supply a valid code before clearing e-stop state. |

`security.otp.method` is parsed for forward compatibility; only TOTP is
implemented. It is not an action-policy selector.

Four older fields are different: `gated_actions`, `gated_domains`,
`gated_domain_categories`, and `challenge_max_attempts` have no action-execution
consumer. During the compatibility window, a non-default value is still parsed
and produces the structured `otp_action_gating_unsupported` warning, which says
that the setting is not enforced. Defaults and absent fields do not warn. None
of these fields can authorize, deny, or rate-limit an action.

The names historically listed under `gated_actions` map to the real authority
boundaries as follows:

| Action category | Authority owner and required enforcement | Current production status |
|---|---|---|
| Direct local tools such as `shell`, `file_write`, `browser_open` / `browser`, and `memory_forget` | The selected risk profile owns tool admission and `SecurityPolicy` owns the relevant workspace, command, URL/domain, and autonomy checks. Calls that resolve to `Prompt` require a trusted operator through the CLI or an attributed channel approval. | These checks and approval surfaces are wired. For prompted calls, the kernel-local `ApprovalStore` binds a one-shot approval to the boot, run, tool, and exact arguments. Calls do not become Tachi-authorized merely because a deprecated OTP field names them. |
| Tachi-managed durable or specialist work | Tachi owns admitted execution and grant truth. ZeroClaw may submit semantic `TaskIntentV1` content, but that content cannot mint authority. | No production Tachi bridge transport ships in ZeroClaw yet. Until the host interface and production wiring land, this path is unavailable rather than protected by OTP or a local fallback. |
| Remote Node capability invocation | Tachi owns grant authority. The execution target must revalidate its local capability, permission, revision, arguments, expiry, and replay policy before the physical action; the Gateway may only carry admitted proof. | `GrantProof` is currently a reserved wire shape; verification and the production Tachi-to-Gateway grant/claim interface are not wired. Do not extend `approvals.db` for Node grants: that store is limited to kernel-local tool approvals. |
| E-stop resume | The local operator-facing CLI challenge authenticates the person clearing e-stop state when OTP is required. | The challenge is wired. It does not approve any later tool, Tachi task, Node invocation, or domain access. |

A model-provided `approved=true`, OTP-looking text, a matching tool/domain name,
or an `approval_requirement` value in task content is not a grant. Until a
production authority path exists for an action, keep that action unavailable or
within its current local policy boundary; do not treat the retired OTP fields as
a compatibility fallback.

## Leak detector configuration

Configure outbound leak detection in its own TOML section:

```toml
[security.leak_detection]
enabled = true
sensitivity = 0.7
high_entropy_tokens = true
```

`enabled = false` disables the entire outbound leak detector.
`high_entropy_tokens = false` disables only the standalone entropy heuristic;
deterministic credential patterns still run. `sensitivity` accepts `0.0`
through `1.0`; higher values are more aggressive.

The complete field table and defaults are in the
[Config reference](../reference/config.md#securityleak_detection).

## When things go wrong

A blocked tool call doesn't silently fail:

1. The security validator returns an error
2. The runtime wraps it as a `ToolResult::Err` and hands it back to the model
3. The model sees "Error: Shell command blocked by policy: forbidden pattern `rm -rf /`" and can retry, apologise, or ask the user

If a tool is excluded from the channel via `[autonomy].non_cli_excluded_tools` (which gates non-CLI channels as a group), it simply isn't advertised to the model on those channels. Model never sees a tool it can't use.

## Default posture

Out of the box:

- Autonomy: `Supervised`
- Workspace-only: `true`
- Sandbox: auto-detect (uses whatever the OS provides)
- Audit logging: `false` (enable explicitly)
- OTP: `false`
- E-stop: `false`

This is a reasonable middle ground, safe enough for a laptop, permissive enough to not frustrate. For production, enable audit, restrict tools, and enable OTP where e-stop recovery needs operator authentication. For a dev box, see [YOLO](../getting-started/yolo.md).
