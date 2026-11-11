#!/usr/bin/env bash
# Stage the npm packages: the JS wrapper plus one platform package per target.
#
#   ./scripts/pack-npm.sh                 # build for this machine only
#   ./scripts/pack-npm.sh --cross         # also try cross targets that are installed
#
# Then:  cd npm && npm publish --access public
#        cd npm/platform/linux-x64 && npm publish --access public
#
# If you would rather automate the whole matrix, cargo-dist does exactly this
# (GitHub Releases + npm packages, `npx` included) from a single config file.
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "version $VERSION"

stage() { # stage <npm-arch> <rust-target>
  local arch=$1 target=$2
  local out="npm/platform/linux-$arch"
  local bin="target/$target/release/agentc"
  # native build lands in target/release, cross builds in target/<triple>/release
  [ -f "$bin" ] || bin="target/release/agentc"
  [ -f "$bin" ] || { echo "skip $arch (nothing built)"; return; }
  mkdir -p "$out/bin"
  cp "$bin" "$out/bin/agentc"
  chmod +x "$out/bin/agentc"
  cat > "$out/package.json" <<JSON
{
  "name": "agentc-mcp-linux-$arch",
  "version": "$VERSION",
  "description": "agentc binary for linux $arch (glibc)",
  "os": ["linux"],
  "cpu": ["$arch"],
  "files": ["bin/agentc"],
  "bin": { "agentc": "bin/agentc" },
  "license": "MIT"
}
JSON
  echo "staged $out ($(du -h "$out/bin/agentc" | cut -f1))"
}

cargo build --release
stage x64 x86_64-unknown-linux-gnu
if [ "${1:-}" = "--cross" ]; then
  stage arm64 aarch64-unknown-linux-gnu
fi

# keep versions in sync
python3 - "$VERSION" <<'PY'
import json, sys
v = sys.argv[1]
p = "npm/package.json"
d = json.load(open(p))
d["version"] = v
d["optionalDependencies"] = {k: v for k in d.get("optionalDependencies", {})}
json.dump(d, open(p, "w"), indent=2, ensure_ascii=False)
open(p, "a").write("\n")
PY
echo
echo "next:"
echo "  cd npm && npm publish --access public"
echo "  cd npm/platform/linux-x64 && npm publish --access public"
