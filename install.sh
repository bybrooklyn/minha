#!/usr/bin/env sh
set -eu

repo="${MINHA_REPO:-bybrooklyn/minha}"
version="${MINHA_VERSION:-latest}"
install_dir="${MINHA_INSTALL_DIR:-${HOME}/.local/bin}"

fail() {
  printf 'minha installer: %s\n' "$*" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v install >/dev/null 2>&1 || fail "install is required"

case "$(uname -s):$(uname -m)" in
  Darwin:arm64 | Darwin:aarch64)
    target="aarch64-apple-darwin"
    ;;
  Darwin:x86_64 | Darwin:amd64)
    target="x86_64-apple-darwin"
    ;;
  Linux:x86_64 | Linux:amd64)
    target="x86_64-unknown-linux-gnu"
    ;;
  *)
    fail "unsupported platform $(uname -s)/$(uname -m); install with cargo instead"
    ;;
esac

artifact="minha-${target}"
if [ "$version" = "latest" ]; then
  release_url="https://github.com/${repo}/releases/latest/download"
else
  case "$version" in
    v*) tag="$version" ;;
    *) tag="v${version}" ;;
  esac
  release_url="https://github.com/${repo}/releases/download/${tag}"
fi

temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/minha-install.XXXXXX")"
staged_path=""
trap 'rm -rf "$temp_dir"; if [ -n "$staged_path" ]; then rm -f "$staged_path"; fi' EXIT HUP INT TERM

printf 'Downloading %s from %s…\n' "$artifact" "$repo"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${temp_dir}/${artifact}" \
  "${release_url}/${artifact}"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  --output "${temp_dir}/${artifact}.sha256" \
  "${release_url}/${artifact}.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$temp_dir" && sha256sum --check "${artifact}.sha256")
elif command -v shasum >/dev/null 2>&1; then
  (cd "$temp_dir" && shasum -a 256 --check "${artifact}.sha256")
else
  fail "sha256sum or shasum is required to verify the download"
fi

mkdir -p "$install_dir"
staged_path="${install_dir}/.minha.install.$$"
install -m 0755 "${temp_dir}/${artifact}" "$staged_path"
mv -f "$staged_path" "${install_dir}/minha"
staged_path=""

printf 'Installed Minha to %s/minha\n' "$install_dir"
case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) printf 'Add %s to PATH to run minha from any shell.\n' "$install_dir" ;;
esac
"${install_dir}/minha" --version
