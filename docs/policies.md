# Policies

Actime policies are [ActPlane](https://github.com/eunomia-bpf/ActPlane) rules.
They are evaluated in the kernel against real OS effects (`exec`, `open`,
`read`, `write`, `unlink`, `connect`), not against tool calls. That is the
whole point: a rule you write once holds whether the agent used a tool, a bash
one-liner, a Python subprocess, or a subagent it spawned — wherever Actime is
deployed.

## The three modes

```console
actime run --policy off      -- claude   # no policy plane at all
actime run --policy observe  -- claude   # every match is recorded, nothing is stopped
actime run --policy enforce  -- claude   # matches are blocked or killed
```

`--policy` also works on `actime attach`, for a target that is already
running.

**Start in `observe`.** Run it for a week across your team. Read
`actime report` and see which rules would have fired and how often. Then
promote rules to `enforce` one at a time. A policy that blocks something
legitimate on day one is a policy your engineers will turn off.

`enforce` fails closed: if the policy plane cannot load (no root, no BTF, a
kernel that is too old, `actplane` not installed or too old), the run aborts
rather than silently proceeding unprotected:

```text
error: policy.mode is `enforce` but the policy plane could not start: actplane is not installed; run `cargo install actplane`

Actime fails closed rather than running an agent unprotected.
Fix one of:
• re-run with privileges:  sudo actime run --policy enforce -- <agent>
• learn first without blocking:  actime run --policy observe -- <agent>
• diagnose this machine:  actime doctor
```

`enforce` also fails closed when the engine starts fine but a requested rule is
**not enforceable** on this host — when the rule needs engine features the
installed ActPlane does not provide (see
[Rule enforceability](#rule-enforceability-what-this-host-can-actually-do)).
Silent partial enforcement is the worst outcome, so the run aborts before the
agent starts, with the rules and the missing features named:

```text
error: policy.mode is `enforce` but these rules cannot be enforced on this host:

  • credential-access (notify) — engine missing features required on attach/delta path: path contains matches, path suffix matches
  • evidence-integrity (block) — engine missing features required on attach/delta path: path contains matches, write sink rules
  • no-secret-egress (kill) — engine missing features required on attach/delta path: path contains matches, path suffix matches
  • system-fence (block) — engine missing features required on attach/delta path: write sink rules

Actime fails closed rather than running an agent with a silent subset.
Options:
• drop those packs from policy.packs (e.g. use coding-agent-baseline only)
• learn without blocking:  actime run --policy observe -- <agent>
• inspect the gap:         actime policy check
• wait for ActPlane to enable the missing file-sink / path-matcher features
```

(exit code 1, nothing launched). In `observe` the same conditions degrade the
plane and the run continues: the unenforceable rules are stored on the manifest
and printed in the report under **Unenforceable rules**, so the record says
exactly what the run did *not* watch for. This asymmetry is deliberate.

## Shipped packs

```console
actime policy list
actime policy show coding-agent-baseline
```

The packs live in `policies/` and are embedded into the binary, so
`policy list` / `policy show` work with nothing else on disk.

### `coding-agent-baseline`

The default. Deliberately boring: it stops effects no coding agent should
produce during ordinary development and stays out of the way of everything
else. Two rules:

| Rule | Effect | What it stops |
|---|---|---|
| `destructive-vcs` | kill | `git` with `--force`, `--hard`, or `clean` |
| `mass-deletion` | kill | `rm -rf` (unrestricted; see below) |

An honest limitation, straight from the pack's own header: with ActPlane
0.1.8, file open/write sink rules and some path-matcher classes do not load as
runtime policy, so the file-path fences (system fence, evidence integrity,
credential reporting) live in the `information-flow` pack instead, and the
workspace-scoped form of `rm -rf` waits for the same engine support. This pack
keeps only the rules that load and enforce today — exec `kill` — so
`actime policy check` reports 2/2 enforceable and `--policy enforce` installs
cleanly. The unrestricted `rm -rf` form is stricter than the planned
path-scoped one, deliberately.

### `no-vcs-write`

The agent edits, the human publishes. Three rules:

| Rule | Effect | What it stops |
|---|---|---|
| `no-publish` | kill | `git push`, `git tag` |
| `no-branch-churn` | kill | `git branch`, `git worktree` |
| `gated-commit` | kill | `git commit`, until the user approves, and again after each commit |

`gated-commit` is worth reading closely, because it is a kind of rule a
permission prompt cannot express:

```
rule gated-commit:
  kill exec "git" "commit"
    if AGENT unless after write "${WORKSPACE}/.actime/commit-approved"
      since exec "git" "commit"
```

`after` opens the gate once the approval file is written. `since` makes the
gate **go stale** again after each commit, exactly the way a build system
treats an object file as stale once its source changes. So approval means "this
commit", not "commits forever".

### `information-flow`

Four rules around one idea: the policy language can express **where data came
from**, not just which call was made. **Read this section as a design
statement, not a shipping capability.** Every rule in this pack needs engine
features — open/write sink rules, path contains/suffix matchers, file-source
label propagation — that released ActPlane 0.1.8 does not provide on the
attach / runtime-delta path Actime uses. `actime policy check` says so, per
rule:

```text
ok policy compiled from information-flow · 0/4 rules enforceable on this host

RULE                     EFFECT   ENFORCEABLE  REASON
credential-access        notify   no           engine missing features required on attach/delta path: path contains matches, path suffix matches
evidence-integrity       block    no           engine missing features required on attach/delta path: path contains matches, write sink rules
no-secret-egress         kill     no           engine missing features required on attach/delta path: path contains matches, path suffix matches
system-fence             block    no           engine missing features required on attach/delta path: write sink rules
```

If you put this pack in `policy.packs` with `mode: enforce`, the run fails
closed before the agent starts. With `mode: observe` the run proceeds but the
policy plane is disabled with these rules recorded as unenforceable. No
default profile includes this pack, for exactly that reason. When ActPlane
enables the missing rule classes, the same pack becomes enforceable unchanged.

The four rules:

| Rule | Effect | What it expresses |
|---|---|---|
| `system-fence` | block | the agent may not write or unlink under `/etc`, `/usr`, `/bin`, `/sbin`, `/boot` |
| `evidence-integrity` | block | the agent may not rewrite Actime's own run records |
| `credential-access` | notify | credential reads (`~/.ssh/id_*`, `~/.aws/credentials`, …) are reported, not blocked |
| `no-secret-egress` | kill | data labeled from a secret file may not reach the network |

`no-secret-egress` is the rule no syscall allowlist can replace. The full
source list:

```
source SECRET = file "**/.env"
source SECRET = file "**/.env.*"
source SECRET = file "**/.ssh/id_*"
source SECRET = file "**/.aws/credentials"
source SECRET = file "**/.config/gh/hosts.yml"
source SECRET = file "**/.netrc"
source SECRET = file "**/secrets/**"
source SECRET = file "**/*.pem"
source SECRET = file "**/*.key"

declassify SECRET by exec "**/actime-redact"

rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
```

Secret-shaped files carry a label. The label propagates with the data (through
reads, writes, forks, and execs), so a value read from `.env`, written to a
temp file, piped through `jq`, and posted by a Python subprocess is *still*
labeled when it reaches the socket — and the connection is refused there.
Reading a secret is allowed; reaching the network afterwards is not.

The offending syscall is an ordinary `connect`. What makes it a violation is
its history, and history is what the label tracks. This is the most
interesting idea in the design — and until the engine supports it on the
attach path, it is also exactly the kind of claim `actime policy check`
exists to keep honest.

`declassify` keeps this usable: run the data through a scrubber you trust
(`actime-redact` in this pack) and the label clears, so a reviewed release path
stays open.

## Writing your own

Policies are plain text. Put yours next to the project and point `actime.yaml`
at it:

```yaml
policy:
  mode: enforce
  packs:
    - coding-agent-baseline
  files:
    - ./team-policy.dsl
```

```
# team-policy.dsl
source AGENT = exec "**/claude"
source AGENT = exec "**/codex"

# Nothing gets committed until the tests have passed since the last source edit.
rule tests-before-commit:
  kill exec "git" "commit"
    if AGENT unless after exec "**/pytest" exits 0
      since write "src/**"
  because "Run pytest and get it green, then commit. The gate re-arms whenever you touch src/."

# Generated code goes stale when the schema changes.
rule regenerate-after-schema:
  notify write file "src/generated/**" if AGENT
  because "You changed the schema. The generated client and the docs are now stale: regenerate them before you finish."

# The staging deployer may only be reached through the review script.
rule mediated-deploy:
  kill connect endpoint "10.20.0.7" if AGENT unless lineage-includes exec "**/review-gate"
  because "Deploys to staging go through ./scripts/review-gate. Run that instead of calling the endpoint directly."
```

`${WORKSPACE}` is substituted once, at compose time, with the absolute path of
the project directory as the running agent sees it: your real cwd on the host,
or the guest path when Actime runs inside a container
([deployment position B](./deployment.md)). Never hardcode a path in a policy;
use `${WORKSPACE}` so the same file works in both.

One caveat on the examples above: `tests-before-commit` and
`regenerate-after-schema` use file sinks (`since write "src/**"`,
`notify write file …`), which released ActPlane 0.1.8 cannot install on the
attach path — the same gap that gates the `information-flow` pack. They are
shown because they are the honest way to *express* those constraints in the
language. Run `actime policy check` against your own files before relying on
them; it tells you per rule what this host will actually enforce.

### Rule enforceability: what this host can actually do

```console
actime policy check      # per rule: enforceable on this host, or not, and why
actime policy explain    # how each clause lowers to kernel matchers
```

`check` is the authoritative verdict. It composes the configured policy, calls
the installed `actplane` for `compile --json` (kernel op, target kind, path
patterns per clause), combines that with the feature budget of the installed
engine version, and prints one line per rule. It never loads anything into the
kernel and needs no privileges, so it belongs in CI — gate on it before you
gate on `enforce`. Unenforceable rules do not fail `check` (exit code stays
0); failing closed on them is `enforce`'s job. Without `actplane` installed,
`check` still composes the policy and warns that it could not compile.

`explain` prints ActPlane's clause-level review of the composed policy against
this host: which backend each clause lowers to and whether its effect is
pre-operation or post-event. It matters because `block` is a pre-operation
denial only where BPF-LSM can see the arguments; for argv-sensitive rules like
`git commit`, `kill` is the honest effect, and `explain` tells you which is
which. Note the difference in what the two commands answer: `explain` reviews
how a clause *would* lower, `check` reports whether this host's engine will
actually *install* the rule. When they disagree, `check` is the one that
matches what a run will do. Both subcommands require an `actplane` new enough
to support them (`check` needs `compile --json`, which landed in 0.1.8; on
older engines `check` falls back to a plain compile with no JSON report and
tells you to upgrade).

## Corrective feedback

The `because` clause is not a comment. Actime writes a feedback block into the
generated `policy.yaml` for each run, and when a rule fires ActPlane delivers
the clause's text to the agent through its feedback hook, so the agent sees a
plain-language instruction instead of a bare `EPERM`. The agent reads that,
understands the constraint, and takes a different path, instead of retrying the
same blocked action until it gives up.

Write `because` clauses as instructions to a capable colleague, not as error
codes. Say what to do instead. For example, the `destructive-vcs` clause:

> Force-pushing, hard-resetting, and cleaning discard work that cannot be
> recovered from the agent's own history. Use a non-destructive git command,
> or ask the user to run this.

`policy.feedback: false` records the preference and marks the plane
`feedback off` in the report; in 0.1.0 the generated policy still contains the
feedback block (see [configuration.md](./configuration.md#policy----policy-plane-actplane)).

## Full rule language

Sources, labels, propagation, declassification, `lineage-includes`, `after`,
`exits`, `since`, and the pattern syntax are documented in the ActPlane
[rule language reference](https://github.com/eunomia-bpf/ActPlane/blob/main/docs/rule-language.md).
