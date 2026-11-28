#!/usr/bin/env bash
# Download the pinned MediaMTX release for this host, verify its SHA-256 and unpack it. CI uses it
# for the WebRTC tests; use it by hand for the same binary:
#
#   export MEDIAMTX_BIN=$(scripts/mediamtx-fetch.sh)             # cached under ~/.cache/simee
#   export MEDIAMTX_BIN=$(scripts/mediamtx-fetch.sh "$dir")      # or in a directory of your choice
#   MEDIAMTX_BIN=... cargo test -p simee-rtc --features rust-crypto --test mediamtx
#
# Prints the path of the verified `mediamtx` binary on stdout (everything else goes to stderr).
# The archive is checked against the pin below every run, cached or not, and the binary is always
# unpacked from the verified archive. To bump: change VERSION and every hash from the release's
# `checksums.sha256` (https://github.com/bluenviron/mediamtx/releases), and nothing else.
# MEDIAMTX_BASE_URL replaces the download location (the tests of this script use it).
set -euo pipefail

VERSION=v1.19.2
SHA256_LINUX_AMD64=f9c601cc303ceca8fad2883917b022882672c5bc56311e92dbceb16e5f20c60c
SHA256_LINUX_ARM64=562f419912a8668c18216a9e8c95359ec82fbb754e4a44e2953ef62b98eec688
SHA256_DARWIN_AMD64=b8851bf53d1e0d6d078240d54e78517a3a239b40040b22cddddf5a0ef2712c17
SHA256_DARWIN_ARM64=c225e46ab65295f95ee9fcda75703129c07add8852483abda09299a78524391f

base=${MEDIAMTX_BASE_URL:-https://github.com/bluenviron/mediamtx/releases/download/$VERSION}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) asset=linux_amd64 sha=$SHA256_LINUX_AMD64 ;;
  Linux-aarch64 | Linux-arm64) asset=linux_arm64 sha=$SHA256_LINUX_ARM64 ;;
  Darwin-x86_64) asset=darwin_amd64 sha=$SHA256_DARWIN_AMD64 ;;
  Darwin-arm64) asset=darwin_arm64 sha=$SHA256_DARWIN_ARM64 ;;
  *)
    echo "mediamtx-fetch: no MediaMTX $VERSION pin for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

dir=${1:-${XDG_CACHE_HOME:-$HOME/.cache}/simee/mediamtx/$VERSION-$asset}
archive="mediamtx_${VERSION}_$asset.tar.gz"
mkdir -p "$dir"

sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

verified() {
  [[ -f $dir/$archive && $(sha256_of "$dir/$archive") == "$sha" ]]
}

if ! verified; then
  # Download beside the target and move it into place only once it is whole.
  rm -f "$dir/$archive" "$dir/$archive.part"
  echo "mediamtx-fetch: downloading $base/$archive" >&2
  curl -fsSL --retry 3 -o "$dir/$archive.part" "$base/$archive"
  mv "$dir/$archive.part" "$dir/$archive"
  if ! verified; then
    echo "mediamtx-fetch: $archive does not match the pinned SHA-256 $sha (got $(sha256_of "$dir/$archive"))" >&2
    rm -f "$dir/$archive"
    exit 1
  fi
fi

rm -f "$dir/mediamtx"
tar -xzf "$dir/$archive" -C "$dir" mediamtx
echo "$dir/mediamtx"
