# History management

The runtime keeps conversation history for each agent session and sends a
provider-facing working history to the model. Two complementary limits operate
on different representations:

1. **Token-budget trimming** acts on the provider-facing `ChatMessage` working
   history and drops oldest whole turns until the estimated context fits the
   token budget.
2. **Structured message-count trimming** mutates `Agent::history`
   (`ConversationMessage`) used by gateway and ACP `Agent` turns when it
   exceeds the structured agent's effective message cap. Daemon channel loops
   that call the legacy `agent::run` path use the separate raw-message cap
   described below.

Token-budget trimming and the structured message-count limit retain turns
atomically. A turn starts at a real user message and includes the assistant
response and any tool calls and tool results before the next user message.
Trimming therefore does not split a tool call from its result.

## Whole-turn retention

`history_trim::trim_to_recent_turns` enforces the token budget, while
`history_trim::trim_conversation_to_recent_turns` enforces the structured
message-count limit. Each keeps the newest complete turn even when that turn by
itself exceeds the relevant limit. This is intentional: preserving a complete
current turn is safer than satisfying a numeric cap by dropping its newest
messages or breaking a tool exchange.

Leading system messages are retained. When no trim is needed, message order and
shape are left unchanged.

## Token budget

The token budget comes from `ResolvedRuntime::effective_context_budget()`:

- When `history_pruning.enabled` is set with a positive
  `history_pruning.max_tokens`, the budget is the lower of that value and
  `max_context_tokens`.
- Otherwise the budget is `max_context_tokens`.

Token counts are estimated by `history::estimate_history_tokens`: roughly four
characters per token plus four framing tokens per message. This is a heuristic,
not a provider tokenizer.

Token-budget trimming runs before the first provider call of a turn when
history already exceeds the effective budget and at provider-call boundaries
between tool-loop iterations, including reactively when a provider reports that
the context window was exceeded. It retains whole turns, so it never splits a
tool exchange.

## Structured message-count limit

`max_history_messages` is the configured value in the agent's runtime profile.
An explicitly configured value is authoritative for both the legacy raw path
and structured agent history, including `0`. Because structured trimming always
retains the newest whole turn, a value of `0` removes older turns but does not
erase the current turn.

When `max_history_messages` is omitted, the legacy raw cap remains `50`. The
structured agent's effective cap is derived from the tool-loop allowance:

```text
max(50, 2 * max_tool_iterations + 2)
```

Each tool iteration can add a tool call and a tool result; the extra two slots
cover the user message and final assistant response. With the default
`max_tool_iterations = 10`, the derived limit remains `50`.

## Visible trimming

Whenever token-budget trimming or the structured message-count limit drops
older turns, the runtime:

1. Inserts a breadcrumb before the first retained turn so the model knows that
   earlier context was omitted.
2. Emits `HistoryTrimmed` with the number of dropped messages, retained turns,
   and a reason identifying the token budget or message limit.

The event is surfaced through the active client transport and through the
observer path used by dashboards and event subscribers. Trimming is therefore
not log-only and is not silent to either the model or connected clients.

The legacy `agent::run` path in `loop_.rs` is an unchanged exception. Its raw
`ChatMessage` cap in `history::trim_history` remains message-level and reports
trimming through logs only, without the breadcrumb or `HistoryTrimmed` event.
This path serves interactive use as well as one-shot and non-interactive daemon,
cron, subagent, and SOP callers.

## Pairing safety

Whole-turn retention is the primary tool-pairing guarantee: a tool call and its
result belong to the same turn and are retained or dropped together. The orphan
sweep remains a final safety net for histories that were already inconsistent,
such as restored or externally modified sessions.

Tool-result length limits are separate. `max_tool_result_chars` bounds an
individual result when it is recorded; it does not trim conversation history.
Provider-side context enforcement is also separate, though a provider overflow
can trigger the runtime's reactive token-budget trim.

## Tool context in model requests

`runtime_profiles.<alias>.keep_tool_context_turns` controls the existing
cache-TTL projection of tool-result text. The current real user turn always
keeps its full tool text; the configured number adds that many prior turns.
An omitted value resolves to `2`.

| Value | Full tool text in a cold request |
| --- | --- |
| `0` | Current turn |
| `1` | Current turn and one prior turn |
| `2` | Current turn and two prior turns |
| `8` | Current turn and eight prior turns |

Older tool results remain in the request. Oversized text uses the existing
4,096-byte, UTF-8-safe head-and-tail projection, preserving native result IDs
and envelope metadata. The byte budget applies to each result's text, not the
entire JSON envelope or a parallel batch. Prompt-mode batches preserve every
result's opening and closing tag, including IDs, names and status; only their
bodies are compacted. Prompt-mode carriers with nested or unparseable block
boundaries remain unabridged. Assistant tool-call rows and opaque reasoning metadata remain
unchanged. Prompt-mode `[Tool results]` rows and tool-parse repair feedback do
not start a new user turn.

The session's 300-second prompt-cache window freezes the compaction boundary
while requests continue within the TTL. A new turn during that warm window can
therefore retain more full text than the configured minimum. Once the window
expires, the next request advances the boundary. Requests without a session
cache apply the cold projection each time. Increasing the retained-turn count
restores newly protected text from source history immediately; lowering it
waits for a cold window. A shifted or changed history prefix also reanchors
the boundary because the previous cache prefix is no longer valid.

This projection operates on a model-request copy; it does not rewrite canonical
history or remove tool-call/result pairs. It shares the existing TTL state and
does not add another history trimmer. Existing image replay limits remain
independent: keeping a turn's full text does not replay all its older images.
The final tool-free summary after exhausting the iteration budget and the
isolated skill-review request retain their existing full-evidence behavior.
This setting is therefore not a hard limit on every model request or on stored
history. Token and message-count trimming still enforce their own limits.
