#!/usr/bin/env bash
# 补齐 cargo 缓存里缺失的 .crate 文件。
#
# 为什么需要：全局 ~/.cargo/config.toml 把 crates-io 镜像到 rsproxy，而
# rsproxy 的 download 端点返回 307 重定向且 cargo 不跟进，于是只要
# Cargo.toml 新增依赖（本次是 tar/flate2 带来的 filetime、xattr 等）就会卡：
#
#   error: failed to download from
#   `https://rsproxy.cn/api/v1/crates/filetime/0.2.29/download`
#
# static.crates.io 直连正常，所以这里的做法是：**先让 cargo 报出缺哪个，
# 再从官方源把那个 .crate 放进 cargo 缓存**，绕过镜像的下载端点。
# 索引仍走 rsproxy（国内快）。
#
# 用法：
#   bash scripts/fetch-missing-crates.sh              # 循环补，最多 12 轮
#   bash scripts/fetch-missing-crates.sh 3.4.46       # 直接补指定版本
#
# 注意：本机环境里设了 http_proxy=127.0.0.1:5897 但代理未运行，
# 所以 curl 必须加 --noproxy '*' 直连，否则同样连不上。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CACHE=""
for d in "$HOME"/.cargo/registry/cache/*/; do
  [ -d "$d" ] && CACHE="${d%/}"
done
[ -n "$CACHE" ] || { echo "找不到 cargo 缓存目录"; exit 1; }
echo "缓存目录：$CACHE"

fetch_one() {
  local name="$1" ver="$2" dest="$CACHE/$1-$2.crate"
  if [ -s "$dest" ]; then
    echo "  ✓ $name-$ver 已在缓存"
    return 0
  fi
  if curl -sSL --noproxy '*' -o "$dest" --max-time 60 \
       "https://static.crates.io/crates/$name/$name-$ver.crate" 2>/dev/null \
     && [ -s "$dest" ]; then
    echo "  ✓ 已下载 $name-$ver"
    return 0
  fi
  rm -f "$dest"
  echo "  ✗ $name-$ver 下载失败"
  return 1
}

# 指定版本模式
if [ $# -ge 1 ]; then
  fetch_one "$1" "$2" || exit 1
  exit 0
fi

# 循环模式：让 cargo 自己说出缺什么
for round in $(seq 1 12); do
  err="$(CARGO_INCREMENTAL=0 cargo fetch 2>&1 | grep -oE 'crates/[a-zA-Z0-9_-]+/[0-9][^/]*/download' | head -1)"
  if [ -z "$err" ]; then
    echo "第 $round 轮：没有缺失的包了"
    exit 0
  fi
  spec="${err#crates/}"; spec="${spec%/download}"
  name="${spec%%/*}"; ver="${spec##*/}"
  echo "第 $round 轮：缺 $name-$ver"
  fetch_one "$name" "$ver" || exit 1
done
echo "补了 12 轮仍有问题，请手动检查"
exit 1