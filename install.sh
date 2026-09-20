#!/bin/sh
set -eu

repository="SaehwanPark/rupi"
version=${RUPI_VERSION:-latest}
install_dir=${RUPI_INSTALL_DIR:-"${HOME:-}/.local/bin"}

usage() {
  cat <<'EOF'
Install the latest rupi release archive.

Usage:
  install.sh [--version VERSION] [--install-dir DIRECTORY]

Environment:
  RUPI_VERSION       Release tag to install, for example v0.2.2 (default: latest)
  RUPI_INSTALL_DIR   Destination directory (default: ~/.local/bin)
EOF
}

fail() {
  printf 'rupi installer: %s\n' "$*" >&2
  exit 1
}

download() {
  url=$1
  destination=$2

  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --location --retry 3 --output "$destination" "$url" \
      || fail "could not download $url"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --tries=3 --output-document="$destination" "$url" \
      || fail "could not download $url"
  else
    fail "curl or wget is required"
  fi
}

checksum() {
  file=$1

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$file" | awk '{print $NF}'
  else
    fail "sha256sum, shasum, or openssl is required to verify the release"
  fi
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || fail "--version requires a value"
      version=$2
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || fail "--install-dir requires a value"
      install_dir=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

[ -n "$install_dir" ] || fail "install directory must not be empty"

tmp_dir=$(mktemp -d 2>/dev/null || mktemp -d -t rupi-install)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

if [ "$version" = "latest" ]; then
  metadata="$tmp_dir/release.json"
  download "https://api.github.com/repos/$repository/releases/latest" "$metadata"
  version=$(tr ',' '\n' <"$metadata" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)
  [ -n "$version" ] || fail "GitHub did not return a latest release tag"
fi

case "$version" in
  [0-9]*) version="v$version" ;;
esac

printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' \
  || fail "release version must look like v0.2.2"

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
  Linux:x86_64|Linux:amd64)
    target=x86_64-unknown-linux-gnu
    archive_extension=tar.gz
    ;;
  Darwin:x86_64|Darwin:amd64)
    target=x86_64-apple-darwin
    archive_extension=tar.gz
    ;;
  Darwin:arm64|Darwin:aarch64)
    target=aarch64-apple-darwin
    archive_extension=tar.gz
    ;;
  *)
    fail "no prebuilt release for $os/$arch; use a supported release target or install from source with cargo"
    ;;
esac

archive_name="rupi-${version}-${target}.${archive_extension}"
base_url="https://github.com/$repository/releases/download/$version"
archive="$tmp_dir/$archive_name"
checksum_file="$tmp_dir/$archive_name.sha256"

printf 'Downloading rupi %s for %s...\n' "$version" "$target"
download "$base_url/$archive_name" "$archive"
download "$base_url/$archive_name.sha256" "$checksum_file"

expected=$(awk 'NF {print $1; exit}' "$checksum_file" \
  | tr '[:upper:]' '[:lower:]')
actual=$(checksum "$archive" | tr '[:upper:]' '[:lower:]')
[ "$expected" = "$actual" ] \
  || fail "checksum mismatch for $archive_name"

extracted="$tmp_dir/extracted"
mkdir -p "$extracted"
tar -xzf "$archive" -C "$extracted"
binary="$extracted/rupi"
[ -f "$binary" ] || fail "release archive does not contain rupi"

mkdir -p "$install_dir"
staged="$install_dir/.rupi-install.$$"
rm -f "$staged"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$binary" "$staged"
else
  cp "$binary" "$staged"
  chmod 0755 "$staged"
fi
mv -f "$staged" "$install_dir/rupi" \
  || fail "could not write $install_dir/rupi; choose a writable directory with --install-dir"

printf 'Installed rupi %s to %s/rupi\n' "$version" "$install_dir"
case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *)
    printf 'Add it to your shell PATH, for example:\n  export PATH="%s:\$PATH"\n' "$install_dir"
    ;;
esac
