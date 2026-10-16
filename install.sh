#!/bin/sh
# Install the latest gpuwho release into ~/.local/bin (or $GPUWHO_INSTALL_DIR).
#
#   curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
#
# It downloads one archive from GitHub Releases, checks it against SHA256SUMS, and copies
# one file. No root, nothing else touched.
set -eu

repo=htyvv/gpuwho
dir=${GPUWHO_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64 | Darwin-aarch64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "gpuwho: no prebuilt binary for $(uname -s) $(uname -m); try: cargo install --git https://github.com/$repo gpuwho" >&2; exit 1 ;;
esac

base=https://github.com/$repo/releases/latest/download
file=gpuwho-$target.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  else wget -q "$1" -O "$2"; fi
}

echo "downloading $file"
fetch "$base/$file" "$tmp/$file"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
want=$(grep " $file\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$file" | cut -d' ' -f1)
else got=$(shasum -a 256 "$tmp/$file" | cut -d' ' -f1); fi
[ -n "$want" ] && [ "$want" = "$got" ] || { echo "gpuwho: checksum mismatch, not installing" >&2; exit 1; }

tar xzf "$tmp/$file" -C "$tmp"
mkdir -p "$dir"
install -m 755 "$tmp/gpuwho-$target/gpuwho" "$dir/gpuwho"
echo "installed $("$dir/gpuwho" --version) to $dir/gpuwho"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir is not on your PATH" ;;
esac
