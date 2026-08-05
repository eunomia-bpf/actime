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
# ActPlane 0.1.8 (released): only exec sink rules with argv matching install and
# fire on the attach / runtime-delta path Actime uses. File open/write sink
# rules and path contains/suffix matchers do not load (engine feature budget
# omits those classes). This pack ships only the rules that actually enforce
# today so `actime policy check` reports every rule enforceable and
# `--policy enforce` can install cleanly. File and label-propagation rules live
# in the `information-flow` pack.

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
#    Path-scoped `unless target` matches the exec object (the rm binary), not
#    argv paths, so the unrestricted form is the enforceable shape today.
rule mass-deletion:
  kill exec "rm" "-rf" if AGENT
  because "Recursive deletion is not a step in a coding task. Delete specific paths under ${WORKSPACE}, or ask the user."
