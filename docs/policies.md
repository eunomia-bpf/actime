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

In `observe` the same conditions degrade the plane and the run continues. This
asymmetry is deliberate.

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
runtime policy, so the file-path fences this pack will eventually carry
(system fence, evidence integrity, credential reporting, workspace-scoped
`rm -rf`) are not in it yet. The pack keeps the rules that load and enforce
today — exec `kill` — so the product thesis (a force-push is stopped) is
demonstrable. The unrestricted `rm -rf` form is stricter than the planned
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

### `no-secret-egress`

The pack that no syscall allowlist can replace. The full source list:

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
labeled when it reaches the socket. The connection is refused there. Reading a
secret is allowed; reaching the network afterwards is not.

The offending syscall is an ordinary `connect`. What makes it a violation is
its history, and history is what Actime tracks.

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

### Validate before you run

```console
actime policy check      # compile every pack and file; report errors and warnings
actime policy explain    # what your kernel can enforce before the fact vs. after
```

Both call the installed `actplane` binary. `check` compiles without loading
anything and needs no privileges, so it belongs in CI. `explain` prints
ActPlane's review of the composed policy against this host: which sources and
rules the kernel can enforce pre-operation. It matters because `block` is a
pre-operation denial only where BPF-LSM can see the arguments; for
argv-sensitive rules like `git commit`, `kill` is the honest effect, and
`explain` tells you which is which. Both subcommands require an `actplane`
new enough to support them (`check` needs `compile --json`, which landed in
0.1.8; on older engines `check` falls back to a plain compile with no JSON
report and tells you to upgrade).

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
