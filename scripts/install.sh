#!/bin/sh
# Run by herdr as this plugin's [[build]] step during `herdr plugin install`.
# herdr invokes argv commands directly (no shell expansion), and build steps
# get none of the runtime plugin env vars, so version and repository are read
# from the checked-out manifest instead of relying on injected context.
set -eu

repo="m1sk9/herdr-worktree-hooks-plugin"
version="$(grep -m1 '^version = ' herdr-plugin.toml | cut -d'"' -f2)"
tag="v${version}"

os="$(uname -s)"
arch="$(uname -m)"

case "${os}" in
Darwin)
	case "${arch}" in
	arm64) target="aarch64-apple-darwin" ;;
	*)
		echo "error: unsupported macOS architecture: ${arch}" >&2
		exit 1
		;;
	esac
	;;
Linux)
	case "${arch}" in
	x86_64) target="x86_64-unknown-linux-musl" ;;
	aarch64) target="aarch64-unknown-linux-musl" ;;
	*)
		echo "error: unsupported Linux architecture: ${arch}" >&2
		exit 1
		;;
	esac
	;;
*)
	echo "error: unsupported OS: ${os}" >&2
	exit 1
	;;
esac

archive="herdr-worktree-hooks-plugin-${target}.tar.gz"
base_url="https://github.com/${repo}/releases/download/${tag}"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

curl -fsSL -o "${tmp_dir}/${archive}" "${base_url}/${archive}"
curl -fsSL -o "${tmp_dir}/${archive}.sha256" "${base_url}/${archive}.sha256"

# The checksum file holds a bare filename (see release.yaml's `Generate
# checksums` step), so verification must run from the directory it was
# downloaded into.
(
	cd "${tmp_dir}"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "${archive}.sha256"
	else
		shasum -a 256 -c "${archive}.sha256"
	fi
)

tar -xzf "${tmp_dir}/${archive}" -C "${tmp_dir}"

# [[startup]]/[[events]]/[[actions]] in herdr-plugin.toml all shell out to this
# fixed path, matching where `cargo build --release` used to leave it.
mkdir -p target/release
mv "${tmp_dir}/herdr-worktree-hooks-plugin" target/release/herdr-worktree-hooks-plugin
chmod +x target/release/herdr-worktree-hooks-plugin
