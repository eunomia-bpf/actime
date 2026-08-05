# Actime policy pack: no-secret-egress
#
# This is the pack that a syscall allowlist cannot express. It is not "which
# calls may this process make" but "where may this *data* go".
#
# Secret-shaped files are labeled at the source. The label follows the data
# through reads, writes, forks, and execs, so a value copied into a temp file,
# piped through jq, and posted by a Python subprocess is still labeled when it
# reaches the socket. At that point the connection is refused.
#
# Reading a secret is allowed. Reaching the network afterwards is not.

source AGENT = exec "**/claude"
source AGENT = exec "**/codex"
source AGENT = exec "**/gemini"
source AGENT = exec "**/opencode"
source AGENT = exec "**/openclaw"
source AGENT = exec "**/aider"
source AGENT = exec "**/actime-demo-agent"

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

rule no-secret-egress:
  kill connect endpoint "*" if AGENT and SECRET
  because "This process holds data derived from a secret file and tried to open a network connection. Actime tracks the label across reads, pipes, and subprocesses, so copying the value elsewhere first does not clear it. Do the network call from a process that has not read the secret, or run the data through actime-redact."
