#!/bin/bash
# 统一 bump 版本：usage: bash scripts/bump-version.sh 1.0.6
#
# 不依赖 perl —— 部分 Git Bash / 精简环境里没有 perl（本机就没有），
# 统一改成 sed。只用最小正则替换目标行，不重新序列化整个 JSON，
# 避免把 tauri.conf.json 之类的格式整篇重排。
set -euo pipefail

V=${1:-}
[ -z "$V" ] && { echo "用法: bash scripts/bump-version.sh <新版本>，例如 1.0.6"; exit 1; }
case "$V" in
  [0-9]*.[0-9]*.[0-9]*) : ;;
  *) echo "版本号应形如 x.y.z，收到：$V"; exit 1 ;;
esac

cd "$(dirname "$0")/.."

# JSON：只替换顶层的 "version" 键（依赖名不会撞上这个键名）
for f in package.json src-tauri/tauri.conf.json npm/package.json npm/platform/*/package.json; do
  [ -f "$f" ] || continue
  sed -i -E 's/"version": "[0-9]+\.[0-9]+\.[0-9]+"/"version": "'"$V"'"/' "$f"
done

# npm 主包的 optionalDependencies 指向平台包，版本要跟着走
if [ -f npm/package.json ]; then
  sed -i -E 's/("workbuddy-switch[a-z0-9-]*": ")[0-9]+\.[0-9]+\.[0-9]+(")/\1'"$V"'\2/g' npm/package.json
fi

# TOML：只改 [package] 段第一处 version
for f in src-tauri/Cargo.toml crates/wb-switch-core/Cargo.toml crates/wb-switch-server/Cargo.toml; do
  [ -f "$f" ] || continue
  sed -i -E '0,/^version = "[0-9]+\.[0-9]+\.[0-9]+"/s//version = "'"$V"'"/' "$f"
done

echo "版本已同步为 $V："
grep -m1 '^version = ' src-tauri/Cargo.toml | sed 's/^/  src-tauri\/Cargo.toml: /'
grep -m1 '"version"' src-tauri/tauri.conf.json | sed 's/^/  tauri.conf.json: /'
grep -m1 '"version"' package.json | sed 's/^/  package.json: /'
