# Real DSH delegation proof

Run this operator check directly with Python 3.9 or newer. It is outside the
Cargo suite because it reaches a real external harness and may need provider
authentication. The ZeroClaw body model is a scripted loopback OpenAI-compatible
HTTP provider with a synthetic dummy key. The installed ZeroClaw CLI, production
Agent/tool registry, Tachi daemon, and DSH worker remain real.

Use a dedicated loopback Tachi daemon with its own `TACHI_HOME` and
`TACHI_RUN_ROOT` inside that home, an empty launch workspace, and an admitted
native `dsh_executor` profile. Install the candidate ZeroClaw and Tachi binaries
before running. The script does not start a daemon, provision credentials, or
read existing user configurations. DSH authentication belongs to the operator's
daemon configuration. Serialize runs against that daemon.

```sh
python3 tests/manual/tachi/test_dsh_delegation.py \
  --endpoint http://127.0.0.1:26919/mcp \
  --tachi-home /tmp/zeroclaw-dsh-e2e/tachi-home
```

The fresh ZeroClaw config uses `composition = "minimal"`, memory `none`, one
synthetic body provider, and only the five Tachi delegation tools. The scripted
body calls start, replays the same request id, watches to terminal, then reads
status and result. It verifies model visibility of all five tools; cancellation
is not exercised. The replay must return the same dispatch and create no second
worker. A 300-second CLI deadline bounds the check; timeout does not cancel a
separate Tachi worker, so the operator must inspect the retained dispatch receipt.

Success requires canonical completed state, exact `137 * 29 = 3973` result,
`untrusted_external_report` with `accepted_by_body = false`, trusted trajectory
metadata identifying backend `dsh` / transport `dsh_headless`, child exit 0,
and DSH's opening session, completed terminal turn, and exact final answer.
The arithmetic worker must make no tool call. Profile names and answer text
alone are insufficient. The DSH profile's workspace authority is advisory;
this check makes no claim about native read/network sandbox enforcement.

For a daemon deliberately lacking DSH credentials, append
`--expect-worker-failure`. That mode requires a real DSH session ending with
`MISSING_CREDENTIAL`, child exit 1, and Tachi failed state. It prints
`EXPECTED_WORKER_FAILURE (not successful E2E)` and cannot establish successful
DSH delegation. The default mode always requires success.

The script prints a private, retained temporary evidence directory containing
the generated config, CLI stdout/stderr, and `proof.json` with observed tool
receipts. Tachi's receipt directory must resolve inside `--tachi-home`; canonical
`status.json`, `trajectory.jsonl`, `result.md`, and `dsh-events.jsonl` stay there.
It reads only these artifacts and status-directory names under that dedicated
home. Review both directories when reporting the result. No user credential,
persona, or existing workspace content belongs in the synthetic task.
