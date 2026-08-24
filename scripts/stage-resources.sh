#!/bin/bash
# Stage the self-contained runtime resources for the desktop bundle:
#   1. the official Node binary (NODE_ARCH, default darwin-arm64) under resources/node
#   2. the npm-installed @deepseek-ai/dsh harness under resources/harness
# Run from anywhere; the resource dir is derived from this script's own
# location (scripts/ sits next to src-tauri/).
set -euo pipefail

NODE_VERSION="${NODE_VERSION:-v22.23.2}"
DSH_VERSION="${DSH_VERSION:-0.1.1-rc.1}"
NODE_ARCH="${NODE_ARCH:-darwin-arm64}"
PNPM_VERSION="${PNPM_VERSION:-11.22.0}"
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

# bin/npm 与 bin/npx 在官方 tarball 里是指向 lib/node_modules/npm/bin/
# 的符号链接；Tauri 打包 resources 时会解引用符号链接，其内部相对
# require('../lib/cli.js') 随之断裂，npm/npx 一运行就 MODULE_NOT_FOUND。
# 这里把符号链接替换为指向真实入口的 wrapper 脚本（npm-cli.js 是自执行
# 脚本，require 即可）。注意仓库根 package.json 为 type: module，无扩展名
# 的 wrapper 在仓库内会被当作 ESM，所以下面的 npm 调用一律走 npm-cli.js。
# 必须先删除符号链接再写文件：shell 重定向会跟随符号链接，不删的话 wrapper
# 会被写进 lib/node_modules/npm/bin/npm-cli.js 真身，npm 一运行就递归 require。
rm -f "$RES/node/bin/npm" "$RES/node/bin/npx"
for pair in "npm:../lib/node_modules/npm/bin/npm-cli.js" "npx:../lib/node_modules/npm/bin/npx-cli.js"; do
  tool="${pair%%:*}"
  target="${pair#*:}"
  printf '#!/usr/bin/env node\nrequire(%s)\n' "'$target'" > "$RES/node/bin/$tool"
  chmod +x "$RES/node/bin/$tool"
done

if [ ! -f "$RES/harness/package.json" ]; then
  echo ">> installing @deepseek-ai/dsh@$DSH_VERSION + pnpm into resources/harness"
  # 直接走 npm-cli.js 真实入口：bin/npm 已是无扩展名 wrapper，在仓库内
  # 会被 ESM 解析而 require 不可用。
  # npm resolving the dsh tree peaks over the default 2GB V8 heap on CI;
  # raise the limit for the install process only.
  PATH="$RES/node/bin:$PATH" NODE_OPTIONS="--max-old-space-size=4096" "$RES/node/bin/node" "$RES/node/lib/node_modules/npm/bin/npm-cli.js" install --prefix "$RES/harness" "@deepseek-ai/dsh@$DSH_VERSION" "pnpm@$PNPM_VERSION" --no-audit --no-fund
fi

# pnpm/pnpx shim 在 .bin 下是指向 pnpm 包内 bin/pnpm.mjs 的符号链接；
# Tauri 打包 resources 时会解引用符号链接，ESM shim 的 ../dist 相对导入
# 就从 pnpm/bin/ 基准偏移到 .bin/ 基准而断裂（node_modules/dist/pnpm.mjs
# 不存在）。替换为 CJS wrapper：require 包内 pnpm.cjs，其内部的动态
# import 都在 pnpm 包内解析，打包解引用不影响。
rm -f "$RES/harness/node_modules/.bin/pnpm" "$RES/harness/node_modules/.bin/pnpx"
for pair in "pnpm:../pnpm/bin/pnpm.cjs" "pnpx:../pnpm/bin/pnpx.cjs"; do
  tool="${pair%%:*}"
  target="${pair#*:}"
  printf '#!/usr/bin/env node\nrequire(%s)\n' "'$target'" > "$RES/harness/node_modules/.bin/$tool"
  chmod +x "$RES/harness/node_modules/.bin/$tool"
done

echo ">> resources staged:"
du -sh "$RES/node" "$RES/harness"

