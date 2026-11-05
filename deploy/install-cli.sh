#!/bin/sh
# Install the native `rudder` CLI on macOS or Linux.
#
#   curl -fsSL https://github.com/vantanminh/rudder/releases/latest/download/install-cli.sh | sh
#
# Options:
#   --version VERSION  Release tag to install (default: latest)
#   --bin-dir DIR      Directory to install `rudder` into (default: /usr/local/bin as root, ~/.local/bin otherwise)
set -eu

REPO="${RUDDER_GITHUB_REPO:-vantanminh/rudder}"
version="latest"
bin_dir=""

die() {
	printf 'rudder-install: %s\n' "$1" >&2
	exit 1
}

info() {
	printf 'rudder-install: %s\n' "$1"
}

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || die "--version needs a value"
		version="$2"
		shift 2
		;;
	--bin-dir)
		[ $# -ge 2 ] || die "--bin-dir needs a directory"
		bin_dir="$2"
		shift 2
		;;
	*)
		die "unknown option $1"
		;;
	esac
done

command -v curl >/dev/null 2>&1 || die "curl is required"

os_name="$(uname -s)"
arch_name="$(uname -m)"

case "$arch_name" in
x86_64 | amd64) arch="x86_64" ;;
aarch64 | arm64) arch="aarch64" ;;
*) die "unsupported architecture: $arch_name" ;;
esac

case "$os_name" in
Linux) target="${arch}-unknown-linux-musl" ;;
Darwin) target="${arch}-apple-darwin" ;;
*) die "unsupported OS: $os_name (use install-cli.ps1 on Windows)" ;;
esac

asset="rudder-${target}"

if [ -z "$bin_dir" ]; then
	if [ "$(id -u)" -eq 0 ] || [ -w "/usr/local/bin" ]; then
		bin_dir="/usr/local/bin"
	else
		bin_dir="${HOME}/.local/bin"
	fi
fi

if [ "$version" = "latest" ]; then
	base="https://github.com/${REPO}/releases/latest/download"
else
	case "$version" in
	v*) tag="$version" ;;
	*) tag="v$version" ;;
	esac
	base="https://github.com/${REPO}/releases/download/${tag}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "downloading ${asset} from ${base}..."
curl -fsSL --proto '=https,http' --retry 3 -o "$tmp/$asset" "$base/$asset"
curl -fsSL --proto '=https,http' --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected="$(awk -v file="$asset" '$2 == file || $2 == "*"file { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS does not contain a checksum for $asset"

if command -v sha256sum >/dev/null 2>&1; then
	actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
	actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
else
	die "neither sha256sum nor shasum is installed"
fi

[ "$expected" = "$actual" ] || die "SHA-256 mismatch for $asset (expected $expected, got $actual)"

mkdir -p "$bin_dir"
install -m 0755 "$tmp/$asset" "$bin_dir/rudder"
info "installed rudder to $bin_dir/rudder"

case ":${PATH}:" in
*":${bin_dir}:"*) ;;
*)
	info "note: add $bin_dir to your PATH:"
	info "  export PATH=\"$bin_dir:\$PATH\""
	;;
esac

"$bin_dir/rudder" --version || true
