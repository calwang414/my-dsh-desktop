#!/bin/bash
# Stage the self-contained runtime resources for the desktop bundle:
#   1. the official Node binary (NODE_ARCH, default darwin-arm64) under resources/node
#   2. the npm-installed @deepseek-ai/dsh harness under resources/harness
# Run from anywhere; the resource dir is derived from this script's own
# location (scripts/ sits next to src-tauri/).
set -euo pipefail

NODE_VERSION="${NODE_VERSION:-v22.23.2}"
DSH_VERSION="${DSH_VERSION:-0.1.0-rc.6}"
NODE_ARCH="${NODE_ARCH:-darwin-arm64}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RES="$(dirname "$SCRIPT_DIR")/src-tauri/resources"

mkdir -p "$RES"
if [ ! -x "$RES/node/bin/node" ]; then
  echo ">> downloading node $NODE_VERSION (darwin-arm64)"
  curl -fsSL -o /tmp/node.tar.gz "https://nodejs.org/dist/$NODE_VERSION/node-$NODE_VERSION-$NODE_ARCH.tar.gz"
  tar xzf /tmp/node.tar.gz -C "$RES"
  mv "$RES/node-$NODE_VERSION-$NODE_ARCH" "$RES/node"
  # npm/npx ship as symlinks into lib/; keep them and include/ (node-gyp
  # headers) so the bundled toolchain can install plugins standalone.
  # corepack has no use here, and share/ is man pages only.
  rm -f "$RES/node/bin/corepack"
  rm -rf "$RES/node/share"
fi

if [ ! -f "$RES/harness/package.json" ]; then
  echo ">> installing @deepseek-ai/dsh@$DSH_VERSION + pnpm into resources/harness"
  # npm-cli.js resolves node via PATH (env node), so expose the bundled
  # node dir and run it through the bundled node explicitly.
  PATH="$RES/node/bin:$PATH" "$RES/node/bin/node" "$RES/node/bin/npm" install --prefix "$RES/harness" "@deepseek-ai/dsh@$DSH_VERSION" pnpm --no-audit --no-fund
fi

echo ">> resources staged:"
du -sh "$RES/node" "$RES/harness"

