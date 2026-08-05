# Actime policy pack: coding-agent-baseline
#
# The default guardrail set. It is deliberately boring: it stops effects that
# no coding agent should produce during ordinary development, and it stays out
# of the way of everything else. `${WORKSPACE}` is substituted by Actime with
# the absolute path of the project directory as seen by the running agent.
#
# Every rule matches on real OS effects, so it holds whether the agent used a
# tool call, a bash one-liner, a Python subprocess, or a subagent.
#
# Note on ActPlane 0.1.8: file open/write sink rules and some path-matcher
# classes currently fail to load as runtime policy (engine feature budget
# omits FEAT_OPEN_RULES / FEAT_WRITE_RULES / path contains-suffix at pin
# time). This pack keeps the rules that load and enforce today — exec kill —
# so the product thesis (a force-push is stopped) is demonstrable. File-path
# fences return when ActPlane loads those features from the initial policy.

source AGENT = exec "**/claude"
source AGENT = exec "**/codex"
source AGENT = exec "**/gemini"
source AGENT = exec "**/opencode"
source AGENT = exec "**/openclaw"
source AGENT = exec "**/aider"
source AGENT = exec "**/cursor-agent"
source AGENT = exec "**/actime-demo-agent"

# 1. History rewriting and forced pushes destroy work that is not the agent's.
rule destructive-vcs:
  kill exec "git" "--force" if AGENT
  kill exec "git" "--hard" if AGENT
  kill exec "git" "clean" if AGENT
  because "Force-pushing, hard-resetting, and cleaning discard work that cannot be recovered from the agent's own history. Use a non-destructive git command, or ask the user to run this."

# 2. Whole-tree deletion is never a step in a coding task.
#    Path-scoped `unless target` requires ActPlane path-matcher features that
#    0.1.8 does not enable on the initial pin; the unrestricted form is
#    stricter and still loads.
rule mass-deletion:
  kill exec "rm" "-rf" if AGENT
  because "Recursive deletion is not a step in a coding task. Delete specific paths under ${WORKSPACE}, or ask the user."
