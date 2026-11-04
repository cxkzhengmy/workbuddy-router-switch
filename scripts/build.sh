#!/usr/bin/env bash
# 构建本项目（**不拉任何上游代码**）并产出安装包。
#
# 与 update-all.sh 的区别是本脚本的核心：
#   update-all.sh  下载上游 workbuddy-switch 源码 → 三方合并自研改动 → 构建
#   build.sh       直接构建当前这份源码
#
# 本项目已于 2026-10 彻底脱离上游 workbuddy-switch —— 代码不再从上游获取，
# `update-all.sh` 仅作为「可选：想跟进上游时才用」的历史工具保留。
# **日常构建走本脚本**，不联网、不做合并，快很多。
#
# 用法：
#   bash scripts/build.sh                # debug 构建 + 安装包
#   bash scripts/build.sh release        # release 构建（更慢、包更小）
#   SKIP_BUILD=1 bash scripts/build.sh   # 只跑前置检查
set -euo pipefail

# 关于 set -e 的一个坑（本脚本踩过）：
# **`cmd | head` 这类管道在 set -e 下会以非 0 结束** —— 上游进程被 SIGPIPE
# 打断，退出码 141/1，与「命令是否成功」无关。
# 所以凡是「纯展示用、不参与成败判定」的管道，一律写成：
#     { cmd || true; } | head -N
# 否则会出现「明明构建成功、脚本却退出 1」的假失败（实测踩过，
# 现象是卡在最后一行「完成。产物：」）。

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# 本仓库有两种放置方式，产物目录要分别对待：
#   1) 开发时的嵌套布局  <wrapper>/src/workbuddy-router  → 安装包放 <wrapper>/release
#   2) 别人 clone 下来直接构建  <repo>                    → 安装包放 <repo>/release
# 判断依据是「上两级的 src/ 下是否还指回本目录」。写死 $ROOT/../.. 的话，
# 第二种情况下安装包会被扔到仓库外面的随机目录，构建完找不到产物。
WORKROOT="$ROOT"
if [ -d "$ROOT/../../src/workbuddy-router" ] \
   && [ "$(cd "$ROOT/../../src/workbuddy-router" && pwd)" = "$ROOT" ]; then
  WORKROOT="$(cd "$ROOT/../.." && pwd)"
fi
cd "$ROOT"

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_R=$'\033[31m'; C_G=$'\033[32m'; C_Y=$'\033[33m'; C_B=$'\033[1m'; C_0=$'\033[0m'
else
  C_R=""; C_G=""; C_Y=""; C_B=""; C_0=""
fi
say()  { printf '%s==>%s %s\n' "$C_B" "$C_0" "$1"; }
ok()   { printf '    %s✓%s %s\n' "$C_G" "$C_0" "$1"; }
warn() { printf '    %s!%s %s\n' "$C_Y" "$C_0" "$1"; }
die()  { printf '\n%s✗ %s%s\n\n' "$C_R" "$1" "$C_0" >&2; exit 1; }

say "版本自检"
ver="$(grep -m1 '^version = ' src-tauri/Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
ok "workbuddy-router $ver"
conf_ver="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json | head -1)"
[ "$ver" = "$conf_ver" ] || die "版本不一致：Cargo.toml=$ver tauri.conf.json=$conf_ver
   两个文件都要改，否则界面显示的版本与安装包文件名会对不上。"
ok "tauri.conf.json 版本一致"

say "工作区检查"
if ! git diff --quiet 2>/dev/null; then
  warn "有未提交改动（构建不受影响，但建议先 commit 以便回退）"
  git status --short | head -5 | sed 's/^/      /'
fi


if [ "${SKIP_BUILD:-0}" = "1" ]; then
  say "SKIP_BUILD=1，只跑前置检查，不构建"
  exit 0
fi

say "构建"
mkdir -p "$WORKROOT/release"

# 清上一次的 dist-embed 让位备份。
# build-exe-local.sh 里把旧 dist-embed 改名成 dist-embed.old-<时间戳>，
# 是为了绕开受管环境的批量删除保护（vite 需要空 outDir）。改名之后
# **没人清理**，跑 8 次构建就积 8 份（实测每份 1.1MB），而它们在
# 源码目录里会污染 git status。
# 用 mv 挪到 build/ 而不是 rm —— 受管环境不允许脚本递归删除。
moved=0
for d in vendor/agent-companion/dist-embed.old-*; do
  [ -d "$d" ] || continue
  mkdir -p "$WORKROOT/build"
  mv "$d" "$WORKROOT/build/$(basename "$d")" && moved=$((moved + 1))
done
[ "$moved" -gt 0 ] && ok "已挪走 $moved 个上次的 dist-embed 让位备份"

PROFILE="$PROFILE" bash scripts/build-exe-local.sh "$PROFILE"

say "完成"
# 按**版本号**精确定位，不能用 `find ... | head -1`——那会拿到 target/nsis/
# 里残留的旧版本安装包（实测：0.1.60 与 1.0.0 并存时选中了前者，
# 因为 find 的遍历顺序不保证按版本）。文件名由 productName + version 组成。
#
# productName 从 tauri.conf.json 读，不在这里写死 —— 写死过一次，改名后
# 脚本仍找旧名，报「未产出安装包」而其实包已经生成了。
product="$(sed -n 's/.*"productName"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json | head -1)"
[ -n "$product" ] || die "读不到 tauri.conf.json 的 productName"
exe="$ROOT/target/$PROFILE/bundle/nsis/${product}_${ver}_x64-setup.exe"
if [ ! -f "$exe" ]; then
  # 退而求其次：按 mtime 找最新，但要排除与当前版本不符的
  # `find` 无结果时退出码非 0，在 set -e 下会直接中止脚本，所以必须兜住。
  exe="$(find "$ROOT/target/$PROFILE/bundle/nsis" -name '*.exe' 2>/dev/null | head -1 || true)"
fi
[ -n "$exe" ] && [ -f "$exe" ] || die "未产出安装包（看上方日志）"
case "$(basename "$exe")" in
  *"_${ver}_"*) : ;;
  *) die "找到的安装包版本与预期不符：$(basename "$exe")（应为 ${ver}）
   target/$PROFILE/bundle/nsis/ 里可能有旧版本残留，删掉后重试。" ;;
esac

dest="$WORKROOT/release/workbuddy-router-${ver}-setup.exe"
cp "$exe" "$dest" || die "复制安装包失败"
ok "安装包：$dest ($(du -m "$dest" | cut -f1) MB)"
printf '    安装：双击它。应用内自动更新已开启，发版时记得同时上传 latest.json。\n'
printf '    生成清单：UPDATE_OS=windows UPDATE_ARCH=x86_64 UPDATE_JSON_NAME=latest.json \\\n'
printf '              UPDATE_ARCHIVE_NAME=workbuddy-router-%s-setup.exe \\\n' "$ver"
printf '              bash scripts/gen-update-json.sh cxkzhengmy workbuddy-router-switch\n\n'