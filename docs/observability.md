# Observability and reports

Every `actime run` produces a run record. The policy and observability parts of it
are written by the kernel-side engines, not by the agent — so it is not
something the agent authored. Whether the agent *can* tamper with it depends on
where you deployed Actime: outside the sandbox (position A) the record is
across the container boundary from the agent; inside the sandbox
(position B) it is not, and root in the container can interfere. See
[deployment.md](./deployment.md) before you rely on a record for incident
response.

## The run record

Layout (some files exist only when the plane that writes them ran):

```
~/.local/share/actime/runs/20260804-153012-a3f1/
  manifest.json          what ran, which planes were active, the summary counters
  actime.yaml            the fully resolved config for this run
  report.md              the rendered Markdown report
  policy.yaml            the ActPlane project file the engine loaded   (policy plane)
  policy.dsl             the composed policy, human-readable           (policy plane)
  violations.jsonl       harvested policy violations, one per line     (policy plane)
  actplane/              ActPlane-owned feedback tree                  (policy plane)
    feedback.txt         corrective feedback the agent can read
    audit.jsonl          ActPlane's audit log
    runs/run-<pid>-<ts>/ per-invocation dir written by ActPlane 0.1.x
      events.jsonl       raw ActPlane violation events (source of the harvest)
  observability.db            AgentSight's SQLite store                     (observability plane)
  policy-engine.log      engine stderr                                 (when attempted)
  observability-engine.log    engine stderr                                 (when attempted)
```

`manifest.json`, `actime.yaml`, and `report.md` are written for every run,
even one where every plane was disabled. The agent's own stdout and stderr are
streamed to your terminal, not captured to files in 0.1.0.

About `violations.jsonl`: ActPlane 0.1.x re-scopes its event output under
`actplane/runs/run-<pid>-<ts>/events.jsonl`, and events only flush when the
engine exits. Actime therefore stops the engine gently first (natural exit,
then SIGTERM with a grace period, then SIGKILL) and **harvests** the raw
events into `violations.jsonl` after the engine has fully exited. A tool that
enforces a kill and then loses the event is worse than useless.

Override the store root with `ACTIME_HOME`. Nothing here leaves your machine:
Actime has no telemetry and makes no outbound connections of its own.

## Reading it

```console
actime runs                      # every run, newest first (default limit 20)
actime status                    # runs still in progress
actime report                    # the latest run
actime report 20260804-153012-a3f1
actime report --json             # for a SIEM or a script
actime report --markdown         # for a PR comment or a ticket
```

`actime report` is designed to answer, in one screen, the five questions a
platform or security team actually asks after an agent run:

- **What ran?** the agent, the argv, the duration, the exit code
- **What was it attached to?** the target: a command, a pid, a container, a pod
- **What was actually on?** each plane, `Active` / `Degraded` / `Disabled`, with the reason
- **What did it touch?** processes, files written, endpoints, model calls, tokens, peak RSS, CPU
- **What was stopped?** every policy match, with the rule, the effect, the target, and the reason
- **What now?** the commands to go deeper

The text report prints the header, the target, the plane table, the summary
counters, and the violation table. The Markdown report (`--markdown`, and
`report.md` on disk) adds a **Timeline** section when the run produced
violations or events. The JSON report (`--json`) emits `{manifest, summary,
violations, timeline}`.

When a run was configured with policy rules this host's engine cannot enforce,
the report also prints an **Unenforceable rules** section — rule, effect, and
the missing engine feature for each — and the manifest carries the same list
as `unenforceable_rules`. In `observe` mode the run proceeds with those rules
unwatched and this section is how the record says so; in `enforce` mode the
run aborts before the agent starts instead. `actime policy check` prints the
same verdict before you run, without privileges.

## `violations.jsonl`

One JSON object per line. The fields are exactly the `Violation` struct Actime
reads back:

```json
{"ts":"2026-08-04T15:33:02Z","rule":"destructive-vcs","effect":"kill","op":"exec","target":"/usr/bin/git","pid":41221,"comm":"git","reason":"Force-pushing, hard-resetting, and cleaning discard work that cannot be recovered..."}
{"ts":"2026-08-04T15:33:41Z","rule":"no-secret-egress","effect":"kill","op":"connect","target":"203.0.113.9:443","pid":41307,"comm":"python3","reason":"This process holds data derived from a secret file and tried to open a network connection..."}
```

The second line illustrates the format, not a violation you will see today:
the `no-secret-egress` rule (pack `information-flow`) needs engine features
released ActPlane 0.1.8 does not provide, so it cannot fire yet. Lines like
the first — exec-level `kill` violations from `coding-agent-baseline` or
`no-vcs-write` — are what the policy plane produces now. See
[policies.md](./policies.md) for the enforceability status of each pack.

`effect` is `notify`, `block`, or `kill`. In `observe` mode matches are
recorded and nothing is stopped, which is how you see what `enforce` *would*
have done before you turn it on. Malformed lines are skipped rather than
failing the report.

## `observability.db`

AgentSight's SQLite store: model calls with token counts, process lineage,
file activity, network endpoints, and resource samples. Actime aggregates the
summary counters from it defensively: it queries `sqlite_master` and
`PRAGMA table_info` first, and a schema it does not recognize degrades to zero
counters rather than failing the report. If your counters are zero but
`observability.db` has rows, that is the cause; file an issue with the AgentSight
version. Because the schema is not a stable interface, check it before writing
your own queries:

```console
sqlite3 ~/.local/share/actime/runs/<run-id>/observability.db ".tables"
```

The counters Actime knows how to read in 0.1.0: row counts and token sums from
an `llm_calls` table (`tokens_in`/`input_tokens`, `tokens_out`/`output_tokens`),
process counts, peak RSS, and CPU seconds from `process_nodes`, and file/network
counts from `audit_events`.

## Export and capture settings

`actime.yaml` accepts three observability knobs:

```yaml
observability:
  enabled: true
  capture: [process, file, network, ssl, resource]
  export: [otlp]
  redact: true
```

Honest status in 0.1.0: `enabled` is fully wired (it turns the plane on and
off, and `--no-observability` overrides it). `capture`, `export`, and `redact` are
parsed, validated into the effective config, and recorded in the run's
`actime.yaml`, but Actime does not yet pass them to AgentSight; the engine
runs as `agentsight record --no-server --db <run-dir>/observability.db` with its own
defaults. The `strict` profile sets `export: [otlp]` so the intent is on record
for when the wiring lands. Treat any export pipeline as engine-side
configuration for now, and check `actime doctor` and the run manifest rather
than assuming a sink is live.

## Session backup

The backup plane is [Akeep](https://github.com/eunomia-bpf/akeep). At the end
of a run it commits the agent's own session files (Claude Code transcripts,
Codex history, and the other providers Akeep supports) into a deduplicated,
versioned repository:

```console
actime keep log                       # versions, newest first (delegates to `akeep log`)
actime keep restore latest            # restore a run's session backup to a scratch dir
actime keep restore latest --to ./restored
actime keep commit -m "before the migration"
```

`keep restore` works for runs whose backup plane actually committed; the
commit id is stored in the manifest as `akeep_commit`. If the plane was
disabled or degraded, the command says so instead of guessing. `actime attach`
never commits a backup — there is no run exit to commit on — so attached runs
show the backup plane `Disabled` with the reason
`attach does not commit a backup`.

This is a different thing from the observability store, and both matter. The
observability store is what the kernel saw. The session history is what the agent
*decided*: the prompts, the reasoning, the tool calls. Correlating the two is
how you answer "why did it do that", not just "what did it do".

Provider transcripts are mutable and providers may clean them up on their own
schedules. If you care about being able to reconstruct a run months later,
leave the backup plane on.

## Retention

Actime does not delete run records on its own, and 0.1.0 has no prune command.
Delete run directories directly when you want the space back:

```console
actime runs --limit 50      # look before you delete
rm -rf ~/.local/share/actime/runs/<run-id>
```

For fleet use, ship `report --json` to your SIEM at the end of each run and
treat the local record as a short-lived working copy. For regulated
repositories, keep the whole run directory: `policy.yaml` / `policy.dsl` plus
`violations.jsonl` plus `manifest.json` is a self-contained account of what was
enforced and what happened, with the policy text included rather than
referenced.
