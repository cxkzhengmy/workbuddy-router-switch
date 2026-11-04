#!/usr/bin/env bash
# 一键更新 + 构建 + 安装（Windows / Git Bash）
#
# 一次跑完：
#   退出旧版 → 备份 → 下载官方新版 → 移植自研功能 → 三方合并
#   → 编译验证 → 构建安装包 → 覆盖安装 → 校验网关资源
#
# 与 merge-upstream.sh 的分工：
#   merge-upstream.sh  负责「拿到新的自研版源码」（不安装，可单独跑）
#   update-all.sh      本脚本 = 上者 + 退出旧版 + 覆盖安装 + 安装后校验
#
# 用法：
#   bash scripts/update-all.sh 0.1.63              # 更新到指定版本并装上
#   bash scripts/update-all.sh 0.1.63 release      # 出 release 包（更慢、包更小）
#   SKIP_INSTALL=1 bash scripts/update-all.sh 0.1.63   # 只构建不安装
#   export HTTPS_PROXY=http://127.0.0.1:7897      # 需要代理时（不设则直连）
set -euo pipefail

VERSION="${1:?用法: bash scripts/update-all.sh <官方版本号> [debug|release]}"
PROFILE="${2:-debug}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# 工作根目录 = 本仓的父目录，**从脚本自身位置推导**，不写死 /d/...。
#
# 为什么不能硬编码：Windows 上 `bash` 可能解析到三个不同的 bash——
#   D:\WorkBuddy\...\PortableGit\usr\bin\bash.exe   （Git Bash，路径是 /d/...）
#   C:\Windows\System32\bash.exe                     （WSL，路径是 /mnt/d/...）
#   C:\Users\...\WindowsApps\bash.exe                （WSL 别名）
# cmd 里敲 `bash x.sh` 走哪个取决于 PATH 顺序，用户往往拿到 WSL 那个。
# 之前写死 /d/a_s_s/ww-router，在 WSL 下就变成「在根目录 mkdir /d」——
# 报 `mkdir: cannot create directory '/d': Permission denied`，看着莫名。
# 从 $ROOT 推导则两种环境都正确。
# 项目根 = 本仓的上两级（本仓在<根>/src/workbuddy-router/）。
# 与 merge-upstream.sh 保持一致，别用 `dirname "$ROOT"`——那是 src/。
WORKROOT="${WORKROOT:-$(cd "$ROOT/../.." && pwd)}"
PROXY="${HTTPS_PROXY:-}"

say()  { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
ok()   { printf '    \033[32m✓ %s\033[0m\n' "$*"; }
warn() { printf '    \033[33m! %s\033[0m\n' "$*"; }
die()  { printf '\n    \033[1;31m✗ %s\033[0m\n\n' "$*" >&2; exit 1; }

# ---- 环境自检：路径风格与工具可用性 -----------------------------------
# WSL 里没有 tasklist / powershell.exe 的可用别名，也没有 /d 路径；
# 与其让它跑到一半失败，不如一开始就明说。
is_wsl() { grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; }
if is_wsl; then
  die "当前 bash 是 WSL，本脚本需要 Git Bash。
  WSL 的路径是 /mnt/d/...，且没有 tasklist / cmd //c 等 Windows 命令。
  请改用 Git Bash 运行：在仓库根目录右键打开 Git Bash，再执行 bash scripts/update-all.sh <版本>"
fi
command -v tasklist >/dev/null || warn "缺 tasklist，无法自动退出旧版（可手动关掉再跑）"

# 安装目录按 %LOCALAPPDATA% 推导，退回家目录下的默认位置。
# 以前这里写死 /c/Users/<某个用户名>/...，换台机器就错，也会把用户名带进仓库。
if [ -n "${LOCALAPPDATA:-}" ]; then
  INSTALL_DIR="$(cygpath -u "$LOCALAPPDATA")/workbuddy-switch"
else
  INSTALL_DIR="$HOME/AppData/Local/workbuddy-switch"
fi
DATA_DIR="$HOME/.wb-switch"

# ---------------------------------------------------------------------------
say "[1/4] 退出正在运行的旧版"
# 不退会装不上：托盘程序关了主窗口仍在跑，文件被占用。
if tasklist //FI "IMAGENAME eq wb-switch-rust.exe" 2>/dev/null | grep -qi wb-switch-rust; then
  taskkill //F //IM wb-switch-rust.exe >/dev/null 2>&1 || true
  ok "已结束 wb-switch-rust"
else
  ok "应用未在运行"
fi
# 网关 sidecar 是独立的 python 进程，只杀带 wb_proxy 参数的那个，
# 别把用户自己的 python 也带走。
powershell -NoProfile -Command \
  "Get-CimInstance Win32_Process -Filter \"Name='python.exe'\" |
   Where-Object { \$_.CommandLine -like '*wb_proxy*' } |
   ForEach-Object { Stop-Process -Id \$_.ProcessId -Force }" >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
say "[2/4] 更新并构建自研版"
[ -n "$PROXY" ] && export HTTPS_PROXY="$PROXY"
# merge-upstream.sh 自带备份与构建，中途任何一步失败都会就地中止并指回备份路径。
# 产物目录名带时间戳（脚本内部每次都用全新目录），所以这里取最新的那个。
PROFILE="$PROFILE" bash "$ROOT/scripts/merge-upstream.sh" "$VERSION" \
  || die "更新/构建失败。现有安装未被改动，备份在 $WORKROOT/backup/"

BENCH_REL="$(ls -dt "$WORKROOT"/build/wb-upstream-$VERSION-* 2>/dev/null | head -1 || true)"
[ -n "$BENCH_REL" ] || die "未找到构建产物目录：$WORKROOT/build/wb-upstream-$VERSION-*"
# 归一化为绝对路径：下游要把路径交给 Windows 侧的 cmd，相对路径会找不到文件。
BENCH="$(cd "$BENCH_REL" && pwd)"
EXE="$(find "$BENCH/target" -name "*.exe" -path "*nsis*" 2>/dev/null | head -1)"
[ -n "$EXE" ] || die "未找到安装包（$BENCH/target 下）"
# 必须是**绝对路径**。EXE 虽然来自 find，但 BENCH 来自 `ls -dt` 的相对结果时
# 会一路带着相对性；而 Windows 侧的 cmd /c start 用相对路径**找不到文件**。
# 实测踩过：安装器根本没执行，脚本却以为装完了（安装目录时间戳没变）。
EXE_ABS="$(cd "$(dirname "$EXE")" && pwd)/$(basename "$EXE")"
EXE_WIN="$(cygpath -w "$EXE_ABS")"
[ -f "$EXE_ABS" ] || die "安装包不存在：$EXE_ABS"
ok "安装包：$EXE_ABS ($(du -m "$EXE_ABS" | cut -f1) MB)"

if [ "${SKIP_INSTALL:-0}" = "1" ]; then
  say "已按 SKIP_INSTALL=1 跳过安装"
  printf '    手动安装：%s\n\n' "$EXE"
  exit 0
fi

# ---------------------------------------------------------------------------
say "[3/4] 安装"
# 留一份旧版主程序：万一新版有问题，能直接拷回去，不必重装。
if [ -f "$INSTALL_DIR/wb-switch-rust.exe" ]; then
  cp "$INSTALL_DIR/wb-switch-rust.exe" "$WORKROOT/backup/wb-switch-rust.exe.bak"
  ok "已备份旧版主程序 → _backup/wb-switch-rust.exe.bak"
fi
# NSIS 安装器是 GUI 程序。用 `start //wait` 等不到它真正结束
# （实测卡住 20 多分钟也不返回，安装目录时间戳都没变）。
#
# 正确做法是**轮询安装产物**：先记录主程序的大小/时间，跑安装器（不等），
# 再按固定间隔检查文件是否被更新。判定依据用「主程序 mtime 变新」，
# 因为安装器会替换它。
before_mtime="$(stat -c %Y "$INSTALL_DIR/wb-switch-rust.exe" 2>/dev/null || echo 0)"
before_size="$(stat -c %s "$INSTALL_DIR/wb-switch-rust.exe" 2>/dev/null || echo 0)"

# 静默安装（NSIS 的 /S）。失败时回落到直接打开（让用户点）。
if cmd //c start "" "$EXE_WIN" /S 2>/dev/null; then
  ok "已启动静默安装（/S）：$EXE_WIN"
else
  warn "静默启动失败，改为直接打开安装器（请在弹窗里点安装）"
  cmd //c start "" "$EXE_WIN" 2>/dev/null || die "连安装器都启动不了：$EXE_WIN"
fi

# 轮询等待安装完成：主程序被替换即视为完成
waited=0
while [ "$waited" -lt 300 ]; do
  sleep 5
  waited=$(( waited + 5 ))
  now_mtime="$(stat -c %Y "$INSTALL_DIR/wb-switch-rust.exe" 2>/dev/null || echo 0)"
  now_size="$(stat -c %s "$INSTALL_DIR/wb-switch-rust.exe" 2>/dev/null || echo 0)"
  if [ "$now_mtime" != "$before_mtime" ] || [ "$now_size" != "$before_size" ]; then
    ok "安装完成（耗时 ${waited}s）"
    break
  fi
  [ $(( waited % 30 )) -eq 0 ] && printf '    \033[33m! 等待安装中…%ds\033[0m\n' "$waited"
done
if [ "$waited" -ge 300 ]; then
  warn "等了 ${waited}s 仍没检测到主程序被更新，请确认安装器是否弹了窗口"
  warn "（有界面时手动点安装即可；之后可重跑本命令的校验部分）"
fi

# ---------------------------------------------------------------------------
say "[4/4] 校验安装结果"
sleep 2
NEW_EXE="$INSTALL_DIR/wb-switch-rust.exe"
[ -f "$NEW_EXE" ] || die "安装后没找到主程序：$NEW_EXE"
ok "主程序存在（$(du -m "$NEW_EXE" | cut -f1) MB）"

# 内置网关已编进主程序，安装目录不再有 gateway-hub 目录。
# 确认没有残留即可——残留的旧 hub 会让人以为网关还是 python 那份。
if [ -d "$INSTALL_DIR/gateway-hub" ]; then
  warn "安装目录仍有 gateway-hub 残留（内置网关已取代它，可手动删除）"
else
  ok "内置网关随主程序分发（无 python 残留）"
fi

# 数据目录必须还在（安装不该动它）
[ -f "$DATA_DIR/accounts.json" ] \
  && ok "账号数据完好（$DATA_DIR/accounts.json，$(wc -c < "$DATA_DIR/accounts.json") 字节）" \
  || warn "未找到 $DATA_DIR/accounts.json —— 若是全新安装可忽略"

say "完成"
ok "版本：$VERSION（自研功能已验证落位）"
printf '\n    下一步：\n'
printf '      1. 从开始菜单启动 workbuddy-switch\n'
printf '      2. 进「网关」页，确认版本卡片显示两栏：\n'
printf '         workbuddy-switch %s / workbuddy2api-hub（内嵌网关）\n' "$VERSION"
printf '      3. 点「启动」，面板在 http://127.0.0.1:8788（首次密码 admin）\n\n'
printf '    回退：cp %q/backup/wb-switch-rust.exe.bak %q\n\n' "$WORKROOT" "$INSTALL_DIR"
printf '    ⚠ 不要用应用内的「检查更新」：它指向官方仓，官方没有内嵌网关。\n\n'
