#!/bin/sh
# Rootbeer installer
# Usage: sh -c "$(curl -fsSL rbpkg.com/rb.sh)" -- init tale/dotfiles
set -e

BASE_URL="https://rbpkg.com/bootstrap/v1"
PROFILE_BIN="${XDG_STATE_HOME:-${HOME}/.local/state}/rootbeer/profiles/user/current/bin"

detect_platform() {
	os=$(uname -s)
	arch=$(uname -m)

	case "$os" in
		Darwin) os="macos" ;;
		Linux)  os="linux" ;;
		*)
			echo "error: unsupported OS: $os" >&2
			exit 1
			;;
	esac

	case "$arch" in
		x86_64|amd64)  arch="x86_64" ;;
		arm64|aarch64) arch="aarch64" ;;
		*)
			echo "error: unsupported architecture: $arch" >&2
			exit 1
			;;
	esac

	echo "${arch}-${os}"
}

download() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto '=https' --proto-redir '=https' --max-time "$3" "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -T "$3" "$1" -O "$2"
	else
		echo "error: curl or wget is required" >&2
		exit 1
	fi
}

field() {
	tr ',' '\n' | sed -n "/^[{[:space:]]*\"$1\"[[:space:]]*:/{s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p;q;}"
}

decode() {
	base64 -d 2>/dev/null || base64 -D
}

digest() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum < "$1" | cut -d ' ' -f 1
	else
		shasum -a 256 < "$1" | cut -d ' ' -f 1
	fi
}

# Piped installs (curl | sh) still reach the terminal for the one-time sudo prompt.
interactive() {
	if [ -t 0 ] || ! (: </dev/tty) 2>/dev/null; then
		"$@"
	else
		"$@" </dev/tty
	fi
}

main() {
	for tool in tar base64 sed tr; do
		if ! command -v "$tool" >/dev/null 2>&1; then
			echo "error: $tool is required" >&2
			exit 1
		fi
	done

	if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
		echo "error: sha256sum or shasum is required" >&2
		exit 1
	fi

	platform=$(detect_platform)
	if [ "$platform" = "x86_64-macos" ]; then
		echo "error: Intel macOS is unsupported; use Apple silicon macOS or Linux ARM64/x86-64." >&2
		exit 1
	fi

	tmpdir=$(mktemp -d)
	trap 'rm -rf "$tmpdir"' EXIT
	trap 'exit 130' INT TERM HUP

	download "${BASE_URL}/${platform}.json" "${tmpdir}/release.json" 60
	payload=$(field payload < "${tmpdir}/release.json" | decode)
	url=$(printf '%s' "$payload" | field url)
	sha256=$(printf '%s' "$payload" | field sha256)
	version=$(printf '%s' "$payload" | field version)
	case "$url" in
		https://*) ;;
		*)
			echo "error: the ${platform} release names ${url:-no archive}" >&2
			exit 1
			;;
	esac

	case "$sha256" in
		*[!0-9a-f]*) sha256= ;;
	esac
	if [ "${#sha256}" -ne 64 ]; then
		echo "error: the ${platform} release has no valid digest" >&2
		exit 1
	fi

	echo "downloading rootbeer ${version} for ${platform}..."
	download "$url" "${tmpdir}/rb.tar.gz" 600
	if [ "$(digest "${tmpdir}/rb.tar.gz")" != "$sha256" ]; then
		echo "error: ${url} doesn't match the release's digest" >&2
		exit 1
	fi

	tar -xzf "${tmpdir}/rb.tar.gz" -C "${tmpdir}"
	chmod +x "${tmpdir}/rb"
	if [ -f "${tmpdir}/rb-store" ]; then
		chmod +x "${tmpdir}/rb-store"
	fi

  # Rootbeer immediately installs itself via the real catalog
	interactive "${tmpdir}/rb" use rootbeer

	echo ""
	echo "add this to your shell profile:"
	echo "  eval \"\$(\"${PROFILE_BIN}/rb\" env)\""

	if [ $# -gt 0 ]; then
		echo ""
		echo "running: rb $*"
		PATH="${PROFILE_BIN}:$PATH" interactive "${PROFILE_BIN}/rb" "$@"
	fi
}

main "$@"
