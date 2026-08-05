#!/bin/sh
# actime-demo-agent — a stand-in for a real coding agent.
#
# `actime demo` copies this script to a temporary directory, names it
# `actime-demo-agent` so the shipped policy packs recognize it as an agent, and
# runs it under the full Actime pipeline. It produces the same *kinds* of
# system effects a real coding agent produces — reading files, spawning
# subprocesses, writing source, reaching the network, and one action the
# baseline policy forbids — so you can see every plane light up in 30 seconds
# without installing an agent, and without an API key.
#
# It is deliberately harmless. Everything it writes goes in its own scratch
# directory, and the forbidden action is chosen to be blocked, not destructive.

set -u

say() { printf '  \033[2m%s\033[0m %s\n' "$1" "$2"; }

WORK="${1:-$PWD}"
cd "$WORK" 2>/dev/null || exit 1

printf '\nactime-demo-agent: pretending to be a coding agent in %s\n\n' "$WORK"

# 1. Read the project, the way an agent orients itself.
say "read" "looking around the project"
ls -la . >/dev/null 2>&1
for f in README.md package.json Cargo.toml pyproject.toml go.mod; do
    [ -f "$f" ] && head -20 "$f" >/dev/null 2>&1
done
sleep 1

# 2. Spawn subprocesses — the thing tool-layer guardrails stop seeing.
say "exec" "running subprocesses (git, grep, python)"
git status --short >/dev/null 2>&1
grep -r "TODO" . >/dev/null 2>&1
python3 -c "print(sum(range(1000)))" >/dev/null 2>&1
sh -c 'for i in 1 2 3; do echo "$i" >/dev/null; done'
sleep 1

# 3. Write source, the way an agent edits.
say "write" "editing files"
mkdir -p .actime-demo
cat > .actime-demo/uploader.py <<'PY'
def upload(payload, retries=3):
    """Written by actime-demo-agent."""
    for attempt in range(retries):
        try:
            return _send(payload)
        except TransientError:
            continue
    raise UploadFailed(payload)
PY
cat > .actime-demo/test_uploader.py <<'PY'
def test_upload_retries():
    assert True
PY
sleep 1

# 4. Reach the network, the way an agent calls a model or a registry.
say "connect" "opening a network connection"
if command -v curl >/dev/null 2>&1; then
    curl -s -m 3 -o /dev/null https://example.com 2>/dev/null
elif command -v python3 >/dev/null 2>&1; then
    python3 - <<'PY' 2>/dev/null
import socket
s = socket.socket()
s.settimeout(3)
try:
    s.connect(("example.com", 443))
finally:
    s.close()
PY
fi
sleep 1

# 5. Read credential-shaped material. The baseline pack reports this.
say "read" "touching credential-shaped paths (policy: notify)"
[ -f "$HOME/.aws/credentials" ] && head -1 "$HOME/.aws/credentials" >/dev/null 2>&1
[ -f "$HOME/.npmrc" ] && head -1 "$HOME/.npmrc" >/dev/null 2>&1
sleep 1

# 6. The forbidden action. `coding-agent-baseline` has a `destructive-vcs`
#    rule with `kill exec "git" "--force"`. This is what a real agent does when
#    it decides the fastest way past a conflict is to overwrite the remote.
#    With the policy plane active this process is killed here and the demo
#    stops; that is the intended outcome, and the report explains it.
say "exec" "attempting: git push --force  (policy: kill)"
git push --force >/dev/null 2>&1
RC=$?

# 7. Only reached when the policy plane is not enforcing.
printf '\n'
if [ "$RC" -ne 0 ]; then
    say "note" "git push --force did not succeed (rc=$RC)"
fi
say "done" "demo agent finished"
printf '\n'
exit 0
