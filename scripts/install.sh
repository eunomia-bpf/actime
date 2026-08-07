#!/bin/sh
# Actime installer.
#
# Downloads a prebuilt actime binary for Linux (x86_64 or aarch64) from the
# GitHub release page, verifies the .sha256 sidecar when present, and installs
# it into ACTIME_INSTALL_DIR (default ~/.local/bin). Safe to re-run.
#
# Environment:
#   ACTIME_VERSION       pin to a release tag, e.g. v0.1.0. Default: latest.
#   ACTIME_INSTALL_DIR   where to install the binary. Default: ~/.local/bin.
#
# Actime itself needs no privileges and sends nothing to the network at runtime.
# The optional policy and observability engines (actplane, agentsight) need root or
# CAP_BPF; Actime degrades cleanly without them. See docs/faq.md.
set -eu

OWNER_REPO="eunomia-bpf/actime"

die() {
    echo "actime install: error: $*" >&2
    exit 1
}

note() {
    echo "actime install: $*"
}

download() {
    # download <url> <output-file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need 'curl' or 'wget' to download the release"
    fi
}

compute_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "need 'sha256sum' or 'shasum' to verify the download"
    fi
}

verify_sha256() {
    # verify_sha256 <file> <sha256-sidecar>
    _file=$1
    _sidecar=$2
    _expected=$(awk 'NR==1{print $1; exit}' "$_sidecar")
    [ -n "$_expected" ] || die "could not read expected checksum from $_sidecar"
    _actual=$(compute_sha256 "$_file")
    if [ "$_actual" != "$_expected" ]; then
        die "checksum mismatch for $(basename "$_file") (expected $_expected, got $_actual)"
    fi
    note "checksum OK ($_expected)"
}

# --- platform check -------------------------------------------------------

_os=$(uname -s)
case "$_os" in
    Linux) ;;
    *) die "actime prebuilt binaries are Linux-only (got $_os). On macOS only the isolation plane works; the eBPF planes are Linux-only. See docs/faq.md." ;;
esac

_arch=$(uname -m)
case "$_arch" in
    x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
    aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
    *) die "unsupported architecture: $_arch. Prebuilt binaries exist for x86_64 and aarch64. Build from source instead: cargo install actime-cli." ;;
esac

# --- version + URLs -------------------------------------------------------

if [ -n "${ACTIME_VERSION:-}" ]; then
    version="$ACTIME_VERSION"
    base_url="https://github.com/${OWNER_REPO}/releases/download/${version}"
else
    version="latest"
    base_url="https://github.com/${OWNER_REPO}/releases/latest/download"
fi

tarball_name="actime-${target}.tar.gz"
sha256_name="actime-${target}.tar.gz.sha256"

# --- install dir ----------------------------------------------------------

install_dir="${ACTIME_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"

ACTIME_TMP=$(mktemp -d 2>/dev/null || mktemp -d -t actime-install)
trap 'rm -rf "$ACTIME_TMP"' EXIT HUP INT TERM

note "downloading actime ${version} (${target})"

download "${base_url}/${tarball_name}" "${ACTIME_TMP}/${tarball_name}"

if download "${base_url}/${sha256_name}" "${ACTIME_TMP}/${sha256_name}"; then
    verify_sha256 "${ACTIME_TMP}/${tarball_name}" "${ACTIME_TMP}/${sha256_name}"
else
    note "no .sha256 sidecar found at ${base_url}/${sha256_name}; skipping verification."
fi

# --- extract + install ----------------------------------------------------

tar -xzf "${ACTIME_TMP}/${tarball_name}" -C "$ACTIME_TMP"

if [ -f "${ACTIME_TMP}/actime" ]; then
    src_bin="${ACTIME_TMP}/actime"
else
    src_bin=$(find "$ACTIME_TMP" -type f -name actime 2>/dev/null | head -n 1)
    [ -n "$src_bin" ] || die "release tarball did not contain an 'actime' binary"
fi

install -m 0755 "$src_bin" "${install_dir}/actime"

# --- done -----------------------------------------------------------------

note "installed actime to ${install_dir}/actime"

if "${install_dir}/actime" --version >/dev/null 2>&1; then
    "${install_dir}/actime" --version
else
    note "could not run '${install_dir}/actime --version' (a shared-library mismatch on this distro? build from source with 'cargo install actime-cli')."
fi

case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
        echo
        echo "NOTE: '${install_dir}' is not on your PATH."
        echo "      Add it now:  export PATH=\"${install_dir}:\$PATH\""
        echo "      or add that line to your shell profile (~/.bashrc, ~/.zshrc)."
        ;;
esac

echo
echo "Actime runs with no privileges and no telemetry. The optional planes are"
echo "separate binaries; install only what you need (Actime degrades cleanly):"
echo "  cargo install actplane     # policy plane (needs root or CAP_BPF)"
echo "  cargo install agentsight   # observability plane (needs root or CAP_BPF)"
echo "  cargo install akeep        # backup plane (no privileges needed)"
echo
echo "Next: run 'actime doctor' to check your setup, then 'actime demo'."
