# Actime policy pack: information-flow
#
# REQUIRES ActPlane engine support that released 0.1.8 does NOT provide on the
# attach / runtime-delta path Actime uses:
#   - open sink rules
#   - write sink rules
#   - path contains matches
#   - path suffix matches
#   - file-source label propagation (source LABEL = file "…")
#
# Until those features land in the pinned engine budget, `actime policy check`
# reports every rule here as NOT enforceable, and `actime run --policy enforce`
# fails closed before starting the agent if this pack is selected. Use
# `--policy observe` only if you want a dry run that records the gap.
#
# Contents: system fence, run-record integrity, credential access reporting, and
# secret-egress (label from secret files may not reach the network).

source AGENT = exec "**/claude"
source AGENT = exec "**/codex"
source AGENT = exec "**/gemini"
source AGENT = exec "**/opencode"
source AGENT = exec "**/openclaw"
source AGENT = exec "**/aider"
source AGENT = exec "**/cursor-agent"
source AGENT = exec "**/actime-demo-agent"

# Secret-shaped files are labeled at the source. The label follows the data
# through reads, writes, forks, and execs, so a value copied into a temp file,
# piped through jq, and posted by a Python subprocess is still labeled when it
# reaches the socket. At that point the connection is refused.
source SECRET = file "**/.env"
source SECRET = file "**/.env.*"
source SECRET = file "**/.ssh/id_*"
source SECRET = file "**/.aws/credentials"
source SECRET = file "**/.config/gh/hosts.yml"
source SECRET = file "**/.netrc"
source SECRET = file "**/secrets/**"
source SECRET = file "**/*.pem"
source SECRET = file "**/*.key"

# A redaction step clears the label, so a deliberate, reviewed release path
# stays possible: run the data through your scrubber and the flow is allowed.
declassify SECRET by exec "**/actime-redact"

# 1. The agent must not rewrite the host system image.
rule system-fence:
  block write file "/etc/**" if AGENT
  block write file "/usr/**" if AGENT
  block write file "/bin/**" if AGENT
  block write file "/sbin/**" if AGENT
  block write file "/boot/**" if AGENT
  block unlink file "/etc/**" if AGENT
  block unlink file "/usr/**" if AGENT
  block unlink file "/bin/**" if AGENT
  block unlink file "/sbin/**" if AGENT
  block unlink file "/boot/**" if AGENT
  because "System paths under /etc, /usr, /bin, /sbin, and /boot are outside the agent's working tree. Edit files under ${WORKSPACE}, or ask the user to make system changes."

# 2. The agent must not rewrite its own accountability record.
rule run-record-integrity:
  block write file "${WORKSPACE}/.actime/**" if AGENT
  block unlink file "${WORKSPACE}/.actime/**" if AGENT
  block write file "**/.local/share/actime/**" if AGENT
  block unlink file "**/.local/share/actime/**" if AGENT
  because "The agent must not edit or delete Actime run records. Leave run records alone; ask the user if a run needs to be pruned."

# 3. Credential reads are reported, not blocked — the useful control is where
#    the data goes afterwards (see no-secret-egress).
rule credential-access:
  notify open file "**/.ssh/id_*" if AGENT
  notify open file "**/.aws/credentials" if AGENT
  notify open file "**/.config/gh/hosts.yml" if AGENT
  notify open file "**/.npmrc" if AGENT
  notify open file "**/.docker/config.json" if AGENT
  because "The agent read credential material. This is reported, not blocked, so that legitimate tooling keeps working. Review it in the run report."

# 4. Data labeled from a secret file may not reach the network.
rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "This process holds data derived from a secret file and tried to open a network connection. Actime tracks the label across reads, pipes, and subprocesses, so copying the value elsewhere first does not clear it. Do the network call from a process that has not read the secret, or run the data through actime-redact."
