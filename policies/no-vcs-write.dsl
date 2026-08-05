# Actime policy pack: no-vcs-write
#
# The agent may read the repository and edit the working tree, but it may not
# publish. Commits and pushes stay a human decision. Used by the `strict`
# profile and by review-only agent runs.

source AGENT = exec "**/claude"
source AGENT = exec "**/codex"
source AGENT = exec "**/gemini"
source AGENT = exec "**/opencode"
source AGENT = exec "**/openclaw"
source AGENT = exec "**/aider"
source AGENT = exec "**/actime-demo-agent"

rule no-publish:
  kill exec "git" "push" if AGENT
  kill exec "git" "tag" if AGENT
  because "This run may change the working tree but may not publish. Leave the changes staged and report what you did; the user pushes."

rule no-branch-churn:
  kill exec "git" "branch" if AGENT
  kill exec "git" "worktree" if AGENT
  because "This run does not create branches or worktrees. Work on the current checkout, or ask the user to prepare the branch."

# Commits are gated, not forbidden: the user writes the approval file when they
# have reviewed the diff, and the gate goes stale again after each commit.
rule gated-commit:
  kill exec "git" "commit"
    if AGENT unless after write "${WORKSPACE}/.actime/commit-approved"
      since exec "git" "commit"
  because "Commits need explicit approval. Show the user the diff; once they approve, they (or you, on their instruction) write ${WORKSPACE}/.actime/commit-approved, and the next commit is allowed."
