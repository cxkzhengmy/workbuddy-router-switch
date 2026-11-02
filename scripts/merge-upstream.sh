#!/usr/bin/env bash
# 把自研功能（内嵌网关等）移植到新的官方版本上。
#
# 为什么需要这个脚本：workbuddy-switch（0.1.x）与 workbuddy2api-hub（1.7.x）
# 是两个独立项目，而「内嵌网关」是我们自己加的——**官方仓库里完全没有**：
#   crates/wb-switch-core/src/modules/gateway.rs
#   src/pages/GatewayPage.tsx
#   crates/wb-switch-core/src/modules/proxy/
#   tauri.conf.json 的 bundle.resources
# 直接装官方新版 = 整目录覆盖 = 网关功能消失。本脚本负责在升级时把它带回来。
#
# 用法：
#   bash scripts/merge-upstream.sh 0.1.63            # 更新到指定版本并构建
#   SKIP_BUILD=1 bash scripts/merge-upstream.sh 0.1.63   # 只准备源码，不构建
#   export HTTPS_PROXY=http://127.0.0.1:7897          # 需要代理时
#
# 合并为什么用 git merge-file 而不是自己写 diff：
#   手写 difflib 合并在「往对象字面量内部插一行」这类场景会算错插入点，
#   实测把网关路由插进了 ROUTES 的闭合括号之后、types.ts 的网关块插进了
#   VscodeSessionList 接口内部——报出来却是 TS1005/TS1131 这类
#   「语法错误」，看不出是合并错位。git merge-file 有 diff3 基线、
#   括号配平检测和标准冲突标记，冲突能精确定位到行、便于人工裁决。
set -euo pipefail

VERSION="${1:?用法: bash scripts/merge-upstream.sh <官方版本号>，例 0.1.63}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# 从脚本自身位置推导工作根，不写死 /d/...。
# Windows 上 `bash` 可能解析到 WSL 的 bash（路径是 /mnt/d/...），
# 写死 /d 会在 WSL 下变成「mkdir /d」并报 Permission denied。
#
# 项目根 = 本仓的上上级：本仓在<根>/src/workbuddy-router/，构建产物与备份
# 都放在<根>/build|backup|hubs|release 下。
# 注意不能只写 `dirname "$ROOT"` —— 那是 src/，而 WORKROOT 要的是**项目根**
#（供备份 tarball、下载缓存、构建目录使用）。
WORKROOT="${WORKROOT:-$(cd "$ROOT/../.." && pwd)}"
CURRENT="$ROOT"
BENCH="$WORKROOT/build/wb-upstream-$VERSION-$(date +%Y%m%d-%H%M%S)"
PROXY="${HTTPS_PROXY:-}"

# 项目根下这四个目录是整套流程的产物落位，先建好再干活：
#   build/临时构建目录（跑完可删）    backup/  备份与下载缓存（要留）
#   hubs/ 独立 hub 版本              release/成品安装包
mkdir -p "$WORKROOT/build" "$WORKROOT/backup" "$WORKROOT/hubs" "$WORKROOT/release"

say() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
ok()  { printf '    \033[32m✓ %s\033[0m\n' "$*"; }
warn() { printf '    \033[33m! %s\033[0m\n' "$*"; }
die() { printf '    \033[1;31m✗ %s\033[0m\n\n' "$*" >&2; exit 1; }

# WSL 下没有 tasklist / cmd //c 之类的 Windows 命令，路径风格也不同，
# 与其跑到一半失败，不如一开始就说清楚。
grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null \
  && die "当前 bash 是 WSL，本脚本需要 Git Bash（路径 /d/...）。
  请用 \"C:\\Program Files\\Git\\bin\\bash.exe\" -lc \"bash scripts/merge-upstream.sh <版本>\""

# 换行符归一化：Windows 上工作区是 CRLF，官方 tarball 是 LF。
# 不归一化的话 git merge-file 会把**整个文件**都判成「每一行都变了」，
# 于是每个文件都报假冲突（实测：归一化前 3 个文件里 2 个假冲突，
# 归一化后 3 个全部零冲突自动合并）。这一步是整个流程能跑通的关键。
norm() { tr -d '\r'; }

# ---------------------------------------------------------------------------
say "[0/6] 备份当前目录"
STAMP="$(date +%Y%m%d-%H%M%S)"
ARCHIVE="$WORKROOT/backup/switch-src-$STAMP.tar.gz"
mkdir -p "$(dirname "$ARCHIVE")"
# tar 要的是**相对 WORKROOT 的路径**，不是 basename。
# 整合后本仓在 <根>/src/workbuddy-router/ 下，basename 出来只有 "workbuddy-router"，
# 而 -C 到了项目根，拼出来 <根>/workbuddy-router 不存在 → 「Cannot stat」。
# 这里显式算出相对路径，以后再挪目录也不会错。
REL_CURRENT="${CURRENT#"$WORKROOT"/}"
[ -n "$REL_CURRENT" ] && [ "$REL_CURRENT" != "$CURRENT" ] \
  || die "无法算出本仓相对项目根的路径：CURRENT=$CURRENT WORKROOT=$WORKROOT"
tar -czf "$ARCHIVE" --exclude=target --exclude=node_modules \
    --exclude='dist-build-*' --exclude=dist -C "$WORKROOT" "$REL_CURRENT"
tar -tzf "$ARCHIVE" >/dev/null || die "备份归档损坏，中止"
ok "已备份并校验通过 → $ARCHIVE"

# git 仓是这套流程的前提：没有它就没有「我的改动」这个可比对的基线。
git -C "$CURRENT" rev-parse --git-dir >/dev/null 2>&1 \
  || die "$CURRENT 不是 git 仓。自研改动无法与官方区分，先建仓并提交基线。"
command -v git >/dev/null || die "缺 git"

# 工作区有改动就中止——三方合并只能看见**已提交**的改动，
# 改了没提交的话，自研改动会被漏掉，合并结果等于丢了功能。
#
# 例外：改动只发生在 scripts/ 下时不必拦。改脚本的人本来就要先跑一次
# 自测，如果每改一次就得先 commit 才能测，等于逼着人不验证——
# 脚本自己的质量正是靠反复实测磨出来的（本次为 0.1.63 的报错就改了 3 轮）。
if ! git -C "$CURRENT" diff --quiet 2>/dev/null || \
   ! git -C "$CURRENT" diff --cached --quiet 2>/dev/null; then
  # 管道里必须带 `|| true`：grep -v 没匹配到任何行时返回 1，
  # 在 set -e 下会把整个脚本掐断——现象是「跑完备份就没了、无任何报错」。
  dirty="$(git -C "$CURRENT" status --porcelain | awk '{print $2}' \
           | grep -v '^scripts/' | head -5 || true)"
  if [ -n "$dirty" ]; then
    printf '    \033[1;31m✗ 代码区有未提交改动，先 commit 或 stash：\033[0m\n' >&2
    printf '%s\n' "$dirty" | sed 's/^/      /' >&2
    printf '    \033[1m三方合并只能看见已提交的改动，否则会漏掉未提交的自研改动。\033[0m\n\n' >&2
    die "中止（备份在 $ARCHIVE）"
  fi
  ok "scripts/ 下有改动（不影响合并结果，继续）"
fi
OURS="$(git -C "$CURRENT" rev-parse HEAD)"
ok "自研基线 = ${OURS:0:8}"

# ---------------------------------------------------------------------------
say "[1/6] 下载官方 v$VERSION"
mkdir -p "$WORKROOT/backup"

# 先确认这个 tag 真的存在。GitHub 对不存在的 tag 返回 404，curl -f 会失败，
# 但那报错是「HTTP 404」，看不出是「版本号写错了」。这里先查一次最新版本，
# 把「你可能想更新到 X」直接说出来——实测就是踩了这个：
# 传了 0.1.63，而上游最新还是 0.1.62。
LATEST="$(curl -fsSL --max-time 30 ${PROXY:+-x "$PROXY"} \
  https://api.github.com/repos/changexbc/workbuddy-switch/releases/latest 2>/dev/null \
  | python -c "import sys,json; print(json.load(sys.stdin).get('tag_name','').lstrip('v'))" 2>/dev/null || true)"
if [ -n "$LATEST" ]; then
  if [ "$LATEST" = "$VERSION" ]; then
    ok "v$VERSION 就是当前最新"
  elif [ "$LATEST" = "$(grep '^version' "$CURRENT/src-tauri/Cargo.toml" 2>/dev/null | head -1 | sed 's/.*"\(.*\)"/\1/')" ]; then
    die "v$VERSION 尚未发布。上游当前最新仍是 v$LATEST（=你当前这版）。
    若只是想重新构建当前版本，用：bash scripts/merge-upstream.sh $LATEST"
  else
    ok "上游最新是 v$LATEST，你指定 v$VERSION"
  fi
else
  warn "查不到上游最新版本（API 限流或离线），继续按 v$VERSION 尝试"
fi

TARBALL="$WORKROOT/backup/upstream-$VERSION.tar.gz"
if [ ! -s "$TARBALL" ]; then
  if ! curl -fSL ${PROXY:+-x "$PROXY"} --retry 3 --max-time 300 -o "$TARBALL" \
    "https://github.com/changexbc/workbuddy-switch/archive/refs/tags/v$VERSION.tar.gz" \
    2>"$WORKROOT/backup/.curl-err"; then
    err="$(tail -1 "$WORKROOT/backup/.curl-err" 2>/dev/null)"
    case "$err" in
      *404*)
        printf '\n    \033[1;31m官方没有 v%s 这个 tag。\033[0m\n\n' "$VERSION"
        printf '    \033[1m最可能：版本号写错了，或该版本尚未发布。\033[0m\n'
        printf '    到 https://github.com/changexbc/workbuddy-switch/releases 核对最新版本号。\n\n'
        printf '    常见是「手滑多写一位」——比如当前最新 0.1.62，写成了 0.1.63。\n\n'
        ;;
      *)
        printf '\n    \033[1;31m下载 v%s 失败。\033[0m\n' "$VERSION"
        printf '    curl 原始报错：%s\n' "$err"
        printf '    \033[1m若是网络问题，设代理后重试：\033[0m\n'
        printf '        export HTTPS_PROXY=http://127.0.0.1:7897\n\n'
        ;;
    esac
    die "中止（备份在 $ARCHIVE）"
  fi
fi
[ -s "$TARBALL" ] || die "下载到 0 字节"

# **每次都重新解压**到全新目录，不复用上一次的工作目录。
#
# 原因：上一轮跑完后，bench 目录里已经是「合并结果」——含自研改动。
# 如果复用它，这轮就把它当作「官方原始版」（theirs）来做三方合并，
# 于是自研改动被误判成「上游的改动」，git merge-file 会把我的类型
# 判成被上游删改 → 假冲突，甚至整个合并崩掉。
# 实测踩过：第二次跑时 bench 里的 types.ts 已含 GatewayStatus（官方原版
# 应该是 0），三个文件全被判为「需合并」而失败。
#
# 受管环境不允许脚本递归删除目录，所以不用 rm -rf，改用带时间戳的
# 全新目录：既保证干净，又不需要删除。
BENCH="$WORKROOT/build/wb-upstream-$VERSION-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$BENCH"
tar -xzf "$TARBALL" -C "$BENCH" --strip-components=1
ok "官方源码解压 → $BENCH"
got="$(grep '^version' "$BENCH/src-tauri/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')"
[ "$got" = "$VERSION" ] || die "解压出的版本是 $got，不是 $VERSION"
ok "官方源码就位：$BENCH"

# ---------------------------------------------------------------------------
say "[2/6] 准备三方合并的 base（官方上一版）"
# 没有 base 就分不清「我加的」与「上游新加的」：只比对「我的目录」和
# 「新版上游」两方，差异里两种来源混在一起，整份覆盖会把上游的新特性
# 连同我加的一起盖掉（实测踩过：types.ts 被整份盖回旧版，丢了上游新增的
# isEnterprise / unlimited，tsc 报一片 Property does not exist）。
# 三方合并的 base = **上游的上一版**，不是我们自己的版本。
#
# 这里原先从 Cargo.toml 的 version 推断，因为那时本项目就是上游的复制品，
# 我们的版本号恰好等于上游版本号（0.1.60）。改成独立的 1.0.x 之后那条假设
# 失效了——推断出 base=1.0.0，去 GitHub 上拉当然404（实测踩过）。
#
# 正确做法：base 版本显式记录在 UPSTREAM_BASE_VERSION，升级并验证通过后
# 手工更新它。绝不从自己的版本号推。
UPSTREAM_BASE_VERSION="0.1.60"
BASE_VERSION="${MERGE_BASE_VERSION:-$UPSTREAM_BASE_VERSION}"
[ -n "$BASE_VERSION" ] || die "MERGE_BASE_VERSION 不能为空"
case "$BASE_VERSION" in
  0.*) : ;;
  *) die "base 版本 \"$BASE_VERSION\" 不像上游版本号（上游是 0.x）。
    本项目版本已是独立的 1.0.x，base 必须填**上游**的版本，用 MERGE_BASE_VERSION 指定。"
     ;;
esac
if [ "$BASE_VERSION" = "$VERSION" ]; then
  BASE_DIR=""          # 当前目录已是目标版本，无需三方
  ok "当前目录已是 v$VERSION，跳过 base"
else
  BASE_DIR="$WORKROOT/src/base-official-$BASE_VERSION"
  if [ ! -d "$BASE_DIR/src-tauri" ]; then
    BASE_TAR="$WORKROOT/backup/upstream-$BASE_VERSION.tar.gz"
    [ -s "$BASE_TAR" ] || curl -fSL ${PROXY:+-x "$PROXY"} --retry 3 --max-time 300 -o "$BASE_TAR" \
      "https://github.com/changexbc/workbuddy-switch/archive/refs/tags/v$BASE_VERSION.tar.gz" \
      || die "base 版本 v$BASE_VERSION 下载失败，用 MERGE_BASE_VERSION= 指定正确的上一版"
    mkdir -p "$BASE_DIR"
    tar -xzf "$BASE_TAR" -C "$BASE_DIR" --strip-components=1
  fi
  got_base="$(grep '^version' "$BASE_DIR/src-tauri/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')"
  [ "$got_base" = "$BASE_VERSION" ] || die "base 版本不符：目录里是 $got_base"
  ok "base = v$BASE_VERSION"
fi

# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# 解决 Cargo.toml 里「本项目版本号 vs 上游版本号」这类机械冲突。
#
# 用 python 而不是 sed/awk 的原因：冲突块的结构不固定（可能多个块、
# 可能行尾带空格），正则要照顾太多边界；而逐行状态机在这种文本上最可靠。
#
# 只在**整个冲突块就是一行 version = "..."** 时才自动解决，否则退出码非 0，
# 交人工——绝不做「看起来像」的猜测性改写。
py_resolve_version() {
  python3 - "$1" "$2" <<'RESOLVE_EOF'
import io, re, sys

target, ours_ref = sys.argv[1], sys.argv[2]

def lines(p):
    return io.open(p, 'rb').read().replace(b'\r\n', b'\n').decode('utf-8', 'replace').split('\n')

# 拿本仓那一侧的 version 值（冲突时 ours 段里的 version 才是我们要的）
# TOML:  version = "1.0.0"
# JSON:  "version": "1.0.0"
mine_ver = None
for l in lines(ours_ref):
    m = re.match(r'^\s*(?:"version"\s*:|version\s*=)\s*"([^"]+)"', l)
    if m:
        mine_ver = m.group(1)
        break
if not mine_ver:
    print('  本仓文件里找不到 version 行')
    sys.exit(1)

out, i, resolved, kept = [], 0, 0, 0
src = lines(target)
while i < len(src):
    if not src[i].startswith('<<<<<<<'):
        out.append(src[i]); i += 1; continue
    # 收集 ours / theirs 段
    j = i + 1
    ours = []
    while j < len(src) and not src[j].startswith('======='):
        ours.append(src[j]); j += 1
    if j >= len(src):
        print('  冲突块没有 ======= 分隔，中止'); sys.exit(1)
    j += 1
    theirs = []
    while j < len(src) and not src[j].startswith('>>>>>>>'):
        theirs.append(src[j]); j += 1
    if j >= len(src):
        print('  冲突块没有 >>>>>>> 结束，中止'); sys.exit(1)

    # 只处理「双方各一行、且都是 version 声明」的块。
    # 覆盖两种语法：TOML 的  version = "x"  与 JSON 的  "version": "x"
    # （tauri.conf.json 是 JSON，它的 version 同样会与上游冲突）。
    VER_PAT = r'^\s*(?:"version"\s*:|version\s*=)\s*"[^"]*"'
    o = [l for l in ours if l.strip()]
    t = [l for l in theirs if l.strip()]
    is_ver = (len(o) == 1 and len(t) == 1
              and re.match(VER_PAT, o[0]) and re.match(VER_PAT, t[0]))
    if is_ver:
        # 保留 ours 那一行的**缩进与引号风格**，只换版本号字符串。
        # 不能直接拼 'version = "x"' —— JSON 需要带引号的 key，否则产出非法 JSON。
        out.append(re.sub(r'"[^"]*"$', f'"{mine_ver}"', o[0].rstrip()))
        resolved += 1
    else:
        out.extend([f'<<<<<<< ours', *ours, '=======', *theirs, f'>>>>>>> theirs'])
        kept += 1
    i = j + 1

io.open(target, 'w', encoding='utf-8', newline='').write('\n'.join(out))
print(f'    {target}: 解决 {resolved} 处，保留 {kept} 处')
sys.exit(0 if kept == 0 else 3)
RESOLVE_EOF
}

say "[3/6] 移植：新增文件 + 三方合并"
# heredoc 里的 python 若抛异常，退出码非 0；不显式检查的话，
# set -e 会掐断脚本，而 python 的 traceback 走的是 stderr ——
# 在只重定向 stdout 的调用方眼里就成了「这行之后没输出，什么也没报」。
# 所以：stderr 也接住，出错时把 traceback 打出来再中止。
if python3 - "$CURRENT" "$BENCH" "$BASE_DIR" <<'PYEOF' 2>&1 | sed 's/^/  /'
import hashlib, io, os, shutil, subprocess, sys

# 参数个数与存在性校验。
# 为什么必须查：`2>&1` 写错位置（比如放在 heredoc 之前）会被 python
# 当成**命令行参数**吃掉，于是 sys.argv 少一个、base_dir 变成 undefined，
# 后面 os.path.isdir(None) 抛 TypeError。而 traceback 走 stderr，
# 若 stderr 没接进管道就一个字都看不到——现象是「这行之后没输出也没报错」。
# 这里把「参数错」变成一句人话。
if len(sys.argv) < 4:
    print(f'[中止] 参数个数不对：收到 {sys.argv[1:]}，需要 3 个'
          f'（CURRENT / BENCH / BASE_DIR）')
    sys.exit(2)

cur, ben, base_dir = sys.argv[1], sys.argv[2], sys.argv[3]
for label, p in (('CURRENT', cur), ('BENCH', ben)):
    if not os.path.isdir(p):
        print(f'[中止] {label} 目录不存在：{p}')
        sys.exit(2)
if not os.path.isdir(base_dir):
    print(f'[中止] base 目录不存在：{base_dir}')
    print('       没有 base 就分不清「我加的」与「上游新加的」，三方合并会误判。')
    sys.exit(2)
SKIP = {'.git', 'node_modules', 'target', 'dist'}

def norm(p):
    d = io.open(p, 'rb').read()
    return d.replace(b'\r\n', b'\n').replace(b'\r', b'\n')

def walk(root):
    out = {}
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if d not in SKIP and not d.startswith('dist-build')]
        for fn in fns:
            full = os.path.join(dp, fn)
            rel = os.path.relpath(full, root).replace(os.sep, '/')
            try:
                out[rel] = hashlib.sha256(norm(full)).hexdigest()
            except Exception:
                pass
    return out

ours, up = walk(cur), walk(ben)
base = walk(base_dir) if base_dir and os.path.isdir(base_dir) else {}

# --- 1) 纯新增文件（上游没有的）：直接拷 -------------------------------
added = sorted(r for r in set(ours) - set(up)
               if not r.startswith('vendor/agent-companion/'))
n = 0
for rel in added:
    src = os.path.join(cur, rel.replace('/', os.sep))
    dst = os.path.join(ben, rel.replace('/', os.sep))
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    shutil.copy2(src, dst)
    n += 1
print(f'  新增文件：{n} 个')

# --- 2) 双方都改过的：先判断归属，再决定是否要合并 --------------------
to_merge = []
skipped_upstream = skipped_ours = 0
for rel in sorted(set(ours) & set(up)):
    if ours[rel] == up[rel]:
        continue
    b = base.get(rel)
    if b is None:
        continue                                   # base 里没有 → 我新增的，上面已拷
    if b == up[rel]:
        # 上游没动过 → 用我的
        shutil.copy2(os.path.join(cur, rel.replace('/', os.sep)),
                     os.path.join(ben, rel.replace('/', os.sep)))
        skipped_upstream += 1
        continue
    if b == ours[rel]:
        continue                                   # 我没动过 → 用上游的（bench 里已是）
    to_merge.append(rel)
print(f'  我改上游没改（取我的）：{skipped_upstream} 个')
print(f'  上游改我没改（取上游的）：{len(set(ours) & set(up)) - skipped_upstream - len(to_merge)} 个')
print(f'  需三方合并：{len(to_merge)} 个')
for r in to_merge:
    print('     ', r)

# --- 3) 用 git merge-file 做三方合并 ---------------------------------
for rel in to_merge:
    mine_p = os.path.join(cur, rel.replace('/', os.sep))
    base_p = os.path.join(base_dir, rel.replace('/', os.sep))
    up_p = os.path.join(ben, rel.replace('/', os.sep))
    if not os.path.exists(base_p):
        print(f'  [跳过] {rel}：base 里没有该文件')
        continue
    def read(p):
        return norm(p).decode('utf-8', errors='replace')
    # 临时目录放在 backup/ 下，**不能放 build/**。
    # cargo 会向上找workspace 根并把沿途所有子目录当成员扫描：
    # build/ 位于项目根之下，而项目根本身有 Cargo.toml，于是
    # build/.merge-tmp/{o,b,t} 会被当成 crate manifest 解析，
    # 报 "failed to load manifest for workspace member ...\.merge-tmp\o"
    # （还夹着未处理的冲突标记文本，看起来莫名其妙）。
    # backup/ 不参与 cargo 扫描，放那儿安全。
    # 注意层级：base_dir = <项目根>/src/base-official-<版本>，所以要上溯**两级**
    # 才是项目根。原先用 dirname(base_dir) 只上溯一级，会在 <根>/src/backup/
    # 建目录——那里也在 cargo 的 workspace 扫描路径上（src/ 有 Cargo.toml），
    # 会引发与 build/.merge-tmp 相同的问题。
    root = os.path.dirname(os.path.dirname(base_dir)) if base_dir \
        else os.path.abspath(os.path.join(os.getcwd(), '..', '..'))
    tmp = os.path.join(root, 'backup', '.merge-tmp')
    os.makedirs(tmp, exist_ok=True)
    b_f, o_f, t_f, m_f = (os.path.join(tmp, n) for n in ('b', 'o', 't', 'm'))
    for src, dst in ((base_p, b_f), (mine_p, o_f), (up_p, t_f)):
        io.open(dst, 'wb').write(norm(src))
    # git 可能不在 Python 继承到的 PATH 里（从 cmd 调 Git Bash 时 PATH 会被
    # 重写）。subprocess 找不到它会抛 FileNotFoundError，脚本直接崩，
    # 现象是「[3/6] 这行之后就没输出」——看不出是 git 找不到。
    # 所以先把 git 的绝对路径解析出来，找不到就明确报出来。
    git_exe = shutil.which('git') or shutil.which('git.exe')
    if not git_exe:
        print('  [中止] 在 Python 环境里找不到 git。'
              'cmd 直接调 Git Bash 时 PATH 会被重写，导致子进程里 git 不在。')
        print('         解决办法：先开 Git Bash 再跑脚本，或临时加 '
              '"C:\\Program Files\\Git\\cmd" 到 PATH。')
        sys.exit(2)
    r = subprocess.run([git_exe, 'merge-file', '-p', o_f, b_f, t_f],
                       capture_output=True)
    merged = r.stdout.decode('utf-8', errors='replace')
    if r.returncode == 0:
        io.open(up_p, 'wb').write(merged.encode('utf-8'))
        print(f'  [合并] {rel}：自动完成')
    else:
        n_conf = merged.count('<<<<<<<')
        if n_conf <= 3:
            # 冲突很少且已在控制下：写入冲突标记，交人工裁决
            io.open(up_p, 'wb').write(merged.encode('utf-8'))
            print(f'  [需人工] {rel}：{n_conf} 处冲突，已写入标记，'
                  f'修完删掉 <<<<<<< ======= >>>>>>> 三行')
        else:
            print(f'  [回退] {rel}：{n_conf} 处冲突，保留上游版本')
PYEOF
  then
    # ---- 版本号冲突：机械冲突，脚本代为裁决 --------------------------------
    # 三个 Cargo.toml 会因「本项目 1.0.0」vs「上游 0.1.62」而冲突。
    # 这是版本脱钩的必然结果，且答案永远是「取我的」——本项目是独立产品，
    # 版本号与上游无关，不存在需要人工判断的语义。
    # 不处理的话每次更新都要在这三个文件上卡一次，而报错
    # （error: key with no value）完全看不出是冲突。
    #
    # 顺带这层检查也替代了原来的「冲突标记扫描」：先扫一遍标记，如果只剩
    # Cargo.toml 的版本号行，就地解决后复查；剩下的才报给人工。
    conflict_files="$(grep -rlE '^<<<<<<< |^>>>>>>> ' "$BENCH" \
                        --include='*.toml' --include='*.lock' --include='*.rs' --include='*.ts' \
                        --include='*.tsx' --include='*.json' 2>/dev/null || true)"
    fixed_ver=0
    for f in $conflict_files; do
      case "$(basename "$f")" in
        Cargo.toml|tauri.conf.json)
          # 用 ours（本仓）那一侧的 version 行覆盖掉整个冲突块
          py_resolve_version "$f" "$CURRENT/${f#$BENCH/}" \
            && { ok "已按本项目版本号解决 ${f#$BENCH/}"; fixed_ver=$((fixed_ver + 1)); } \
            || printf '    \033[33m! %s 的冲突不只版本号，需人工看\033[0m\n' "${f#$BENCH/}"
          ;;
      esac
    done
    if [ "$fixed_ver" -gt 0 ]; then
      ok "共自动解决 $fixed_ver 处版本号冲突"
    fi

    # ---- Cargo.lock：不参与三方合并 ---------------------------------------
    # lock 文件是 cargo 依各 Cargo.toml 生成的派生结果，人工合并毫无意义。
    # 它之所以会带冲突标记，是因为本仓的 lock 随版本号一起被提交过改动，
    # 于是被判为「双方都改过」而进了合并列表。
    #
    # 处置：直接用本仓 HEAD 的 lock 占位——它至少是**合法 TOML**，
    # 而带 <<<<<<< 的 lock 会让 cargo 在解析阶段就失败
    #（failed to parse lock file ... TOML parse error），
    # 报错里只有行号，完全看不出根因是合并冲突。
    # 依赖树随后会被 cargo 按合并后的 Cargo.toml 重新解析。
    if [ -f "$BENCH/Cargo.lock" ] && grep -q '^<<<<<<< ' "$BENCH/Cargo.lock" 2>/dev/null; then
      if git -C "$CURRENT" show HEAD:Cargo.lock > "$BENCH/Cargo.lock" 2>/dev/null; then
        ok "Cargo.lock 含冲突标记，已换成本仓版本（cargo 会重算依赖树）"
      else
        rm -f "$BENCH/Cargo.lock"
        ok "Cargo.lock 含冲突标记且无可用历史版本，已删除（cargo 会重新生成）"
      fi
    fi

    # 复查：还有标记就是真冲突，交人工
    leftover="$(grep -rlE '^<<<<<<< |^>>>>>>> ' "$BENCH" \
                 --include='*.toml' --include='*.lock' --include='*.rs' --include='*.ts' \
                 --include='*.tsx' --include='*.json' 2>/dev/null | head -10 || true)"
    if [ -n "$leftover" ]; then
      printf '    \033[1;31m✗ 合并产生 %d 处冲突，需人工裁决：\033[0m\n' "$(printf '%s\n' "$leftover" | wc -l | tr -d ' ')" >&2
      printf '%s\n' "$leftover" | sed "s|^$BENCH/|      |" >&2
      printf '    \033[1m在上述文件里删除 <<<<<<< / ======= / >>>>>>> 三行标记，\033[0m\n' >&2
      printf '    \033[1m保留你要的那一侧后，重新跑本脚本（备份在 %s）\033[0m\n\n' "$ARCHIVE" >&2
      die "合并冲突未解决"
    fi
    ok "移植与合并完成（无冲突标记）"
  else
    die "移植/合并失败（上面若有 Traceback 就是原因）。备份在 $ARCHIVE"
  fi

# ---------------------------------------------------------------------------
say "[4/6] 补齐官方 tarball 不含的东西"
# 只补 node_modules。**不要补 dist-embed**：companion 的
# build-exe-local.sh 里会跑 `npm run build:embed`（vite --outDir dist-embed）
# 重新生成它；而 vite 启动时会先清空 outDir，若目录已存在就会被受管环境的
# 批量删除保护拦下：
#   [safe-delete][SAFE_DELETE_BULK_CONFIRM_REQUIRED] count:50, threshold:50
# 这个报错出现在 companion 构建阶段，看着像 vite 问题，实际是「多此一举的拷贝」。
# （首次实测：先复制 dist-embed 再构建 → 被拦；不复制 → vite 自己生成，OK。）
for rel in node_modules vendor/agent-companion/node_modules; do
  if [ -e "$BENCH/$rel" ]; then
    ok "$rel 已存在"
  elif [ -e "$CURRENT/$rel" ]; then
    cp -R "$CURRENT/$rel" "$BENCH/$rel"
    ok "$rel 已从旧目录复制"
  elif [ "$rel" = "node_modules" ]; then
    die "两侧都没有 node_modules，请在 $CURRENT 跑一次 npm install 再更新"
  else
    die "$rel 两侧都没有——需先在旧目录跑 npm install"
  fi
done
[ -d "$BENCH/vendor/agent-companion/node_modules" ] \
  || die "companion 的 node_modules 缺失——build:embed 需要它"

# ---------------------------------------------------------------------------
say "[5/6] 编译与验证"
cd "$BENCH"

# ---- cargo 互斥锁 ----------------------------------------------------
# 为什么需要：cargo 的包缓存锁 `~/.cargo/.package-cache` 是**全局共享**的，
# 与 target 目录无关。两个 cargo 进程同时跑会互相阻塞，表现为
#   lld-link: error: failed to write output '...exe': permission denied
# 看着像磁盘权限问题，实际是并发冲突（极难定位，曾为此查了三轮）。
# 构建动辄十几分钟，很容易和另一次运行撞上。
#
# 为什么用 mkdir 而不是 flock：**Git Bash 里没有 flock**
# （实测 command not found），所以 flock 只是走个 warning 等于没加锁。
# mkdir 是原子的：一个进程 mkdir 成功，另一个必然失败。
# 锁目录名必须是**所有进程共享的固定路径**，不能带 $$（PID）：
# 带 PID 的话每个进程都 mkdir 自己的路径，互斥完全失效。
LOCK_DIR="${TMPDIR:-/tmp}/ww-switch-cargo-lock.d"
_lock_acquire() {
  local waited=0
  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    # 别人持有锁就等；若锁已存在但很旧（>10 分钟），接管它。
    # 阈值取 10 分钟而非 1 小时：异常退出留下的目录不该让人干等太久，
    # 而正常构建即使慢也该有心跳更新——所以同时看「年龄」和「是否有 cargo 在跑」。
    if [ -d "$LOCK_DIR" ]; then
      local age=$(( $(date +%s) - $(stat -c %Y "$LOCK_DIR" 2>/dev/null || echo 0) ))
      if [ "$age" -gt 600 ] 2>/dev/null; then
        # 年龄够大且系统里确实没有 cargo 在跑 → 判定为残留，接管
        if ! tasklist //FI "IMAGENAME eq cargo.exe" 2>/dev/null | grep -qi cargo; then
          warn "发现 $(( age / 60 )) 分钟前的残留锁且当前无 cargo 进程，接管"
          rmdir "$LOCK_DIR" 2>/dev/null || true
          continue
        fi
      fi
    fi
    if [ $(( waited % 30 )) -eq 0 ]; then
      printf '    \033[33m! 已有另一次构建在跑（已等 %d 分钟），继续等待…\033[0m\n' "$(( waited / 60 ))"
    fi
    [ "$waited" -ge 1800 ] && die "等待 cargo 锁超过 30 分钟，中止（另一次构建可能卡住了）"
    sleep 10
    waited=$(( waited + 10 ))
  done
  ok "已取得 cargo 互斥锁"
}
_lock_acquire

# 锁心跳：持有期间定期 touch，让别人能靠「年龄」判断这是活跃锁。
# 没有心跳的话，一次正常的长构建（十几分钟）会被后来者当成残留锁接管，
# 两边同时编译 → 又回到 permission denied。
( while :; do sleep 60; [ -d "$LOCK_DIR" ] && touch "$LOCK_DIR" 2>/dev/null; done ) &
_HEARTBEAT_PID=$!
trap 'kill "$_HEARTBEAT_PID" 2>/dev/null || true; rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT
# ---------------------------------------------------------------------------

# 必须先产出 dist/：crates/wb-switch-server 用 RustEmbed 把前端内嵌进二进制。
# 目录不存在时 RustEmbed 不报错，而是静默不生成 Assets::get，于是 api.rs 报
# `no associated function named 'get' found for struct 'Assets'`——看着像 API
# 改动，实际是缺前端产物。
# 用 node_modules/.bin 下的真实可执行文件而非 npx：缺依赖时 npx 会去 registry
# 拉同名包，打印「This is not the tsc command you are looking for」后
# **以 0 退出**，于是 `npx tsc || die` 形同虚设。
[ -x node_modules/.bin/tsc ]  || die "缺 node_modules/.bin/tsc，先 npm install"
[ -x node_modules/.bin/vite ] || die "缺 node_modules/.bin/vite，先 npm install"
node_modules/.bin/tsc --noEmit || die "TypeScript 检查失败（有合并冲突的话先修 conflict 标记）"
ok "tsc 通过"

BUILD_OUT="dist-build-$(date +%s)"
node_modules/.bin/vite build --outDir "$BUILD_OUT" >/dev/null 2>&1 \
  || die "前端构建失败（vite build）"
mkdir -p dist && cp -R "$BUILD_OUT/." dist/
ok "主前端产物已生成（$(ls dist/assets/*.js 2>/dev/null | wc -l | tr -d ' ') 个 js）"

# companion 面板必须在 cargo check **之前**构建好：RustEmbed 抓的是整个
# dist/，而 dist/companion 来自 companion 自己的 vite 构建。
#
# 这里必须让它自己生成而不是拷贝 dist-embed：vite 启动时会清空 outDir，
# 目录已存在就撞受管环境的批量删除保护
# （[safe-delete][SAFE_DELETE_BULK_CONFIRM_REQUIRED] count:50, threshold:50）。
# 用全新目录名同理绕过——但 companion 的 build:embed 写死 dist-embed，
# 所以只能在拷贝之前构建。**不要提前 cp dist-embed**。
(
  cd vendor/agent-companion
  # node_modules 已从旧目录复制过来，vite 可执行文件应该在了
  [ -x node_modules/.bin/vite ] || die "companion 缺 node_modules/.bin/vite"
  node_modules/.bin/vite build --base ./ --outDir dist-embed >"$WORKROOT/backup/companion-build.log" 2>&1 \
    || {
      printf '    \033[31mcompanion 前端构建失败，日志：%s\033[0m\n' "$WORKROOT/backup/companion-build.log" >&2
      tail -12 "$WORKROOT/backup/companion-build.log" >&2
      die "中止（备份在 $ARCHIVE）"
    }
)
[ -f vendor/agent-companion/dist-embed/desktop.html ] \
  || die "companion 构建产物缺失（vendor/agent-companion/dist-embed/desktop.html）"
mkdir -p dist/companion && cp -R vendor/agent-companion/dist-embed/. dist/companion/
ok "companion 面板已生成并拷入 dist/"
ok "前端产物已生成"

# 不能用 `cargo check | grep '^error' && die || true`：无错误时 grep 返回 1，
# 会被 || true 吞掉，编译失败也照样往下走。
LOG="$WORKROOT/backup/merge-check-$VERSION.log"
if ! CARGO_INCREMENTAL=0 cargo check --workspace >"$LOG" 2>&1; then
  printf '    \033[31mcargo check 失败，日志：%s\033[0m\n' "$LOG" >&2
  grep -E "^error" -A 6 "$LOG" | head -20 >&2
  die "中止（备份在 $ARCHIVE）"
fi
ok "cargo check 通过"

# 测试失败必须中止。之前写成 `cargo test | grep ... | head -2`：
# grep 匹配不到（= 测试全挂）时返回 1，set -e 直接掐断脚本，
# 现象是「网关测试这行之后就没输出」，看不出是测试没过。
# 这里显式判 cargo test 自身的退出码。
TEST_LOG="$WORKROOT/backup/merge-test-$VERSION.log"
if ! CARGO_INCREMENTAL=0 cargo test -p wb-switch-core gateway -- --test-threads=1 \
     >"$TEST_LOG" 2>&1; then
  printf '    \033[31m网关测试未通过，日志：%s\033[0m\n' "$TEST_LOG" >&2
  grep -E "FAILED|panicked|test result" "$TEST_LOG" | head -10 >&2
  die "中止（备份在 $ARCHIVE）"
fi
# `grep | head` 在管道里会被 SIGPIPE 打断，退出码非 0——而本脚本开了
# `set -e`，于是「测试通过」之后反而以失败结束。这一行只是给人看摘要，
# 不参与成败判定，`|| true` 兜住。
grep -E "test result" "$TEST_LOG" | head -2 || true
ok "网关测试通过"

ok "网关模块就位"

# ---------------------------------------------------------------------------
if [ "${SKIP_BUILD:-0}" = "1" ]; then
  say "已按 SKIP_BUILD=1 跳过构建"
  printf '    手动构建：cd %q && bash scripts/build-exe-local.sh debug\n\n' "$BENCH"
  exit 0
fi

say "[6/6] 构建安装包"
PROFILE="${PROFILE:-debug}"

# 每次跑都新建一个带时间戳的目录（原因见 [1/6]：复用会被合并结果污染）。
# 跑多了会攒下几个大目录（含 target/，单个几个 GB），给一句清理提示——
# 但不自动删：受管环境不允许脚本递归删除，且你可能还想留某个产物。
_dirn="$(dirname "$BENCH")"
_n="$(ls -d "$_dirn"/wb-upstream-$VERSION-* 2>/dev/null | wc -l | tr -d ' ' || true)"
if [ "${_n:-0}" -gt 2 ] 2>/dev/null; then
  warn "已有 $_n 个 wb-upstream-$VERSION-* 目录（含 target/，单个几个 GB）"
  warn "  旧的可手动清理，只留最新那个："
  warn "    ls -dt $_dirn/wb-upstream-$VERSION-* | tail -n +2 | xargs -I{} mv {} _trash-{}-\$(date +%s)"
fi

BUILD_LOG="$WORKROOT/backup/merge-build-$VERSION.log"
# build-exe-local.sh 末尾是 `find ... | head -5`，find 在管道里被 SIGPIPE
# 打断时会以非 0 退出——**那是正常的**，脚本已打完产物路径。所以不能拿
# 它的退出码当失败判据，改为检查安装包是否真的产出。
if ! bash scripts/build-exe-local.sh "$PROFILE" >"$BUILD_LOG" 2>&1; then
  printf '    \033[33m构建命令返回非 0，先看日志确认（find|SIGPIPE 会误报）\033[0m\n' "$BUILD_LOG"
fi
EXE="$(find target -name "*.exe" -path "*nsis*" 2>/dev/null | head -1)"
[ -n "$EXE" ] || {
  printf '    \033[31m未产出安装包，日志：%s\033[0m\n' "$BUILD_LOG" >&2
  grep -viE "^\s+at |handleInvalidResolvedId|onwarn" "$BUILD_LOG" | tail -15 >&2
  die "构建失败（备份在 $ARCHIVE）"
}
ok "安装包：$EXE"
say "完成：v$VERSION（自研功能已移植并验证）"
printf '    安装：双击 %s\n\n' "$EXE"
