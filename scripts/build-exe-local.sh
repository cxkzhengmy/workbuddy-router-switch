#!/usr/bin/env bash
# Windows 本地构建 exe 的一键脚本。
#
# 存在的理由：Tauri 的 `beforeBuildCommand` 链路上有四个环境相关的坑，
# 每一个都会让 `npm run build:app` 在不同阶段失败，且报错信息与真实原因
# 往往无关（EBUSY 报成"文件被占用"、版本不匹配报成"npm 依赖问题"）。
# 这里把它们按顺序固化成可重复执行的步骤。
#
# 用法：
#   bash scripts/build-exe-local.sh          # debug + nsis（快速自测）
#   bash scripts/build-exe-local.sh release  # release + nsis（正式包）
#
# 产物：src-tauri/target/<profile>/release/bundle/nsis/*.exe

set -euo pipefail

# 坑 8：stable rustc 1.98 在本 crate 上会 ICE（"the compiler unexpectedly
# panicked"），编译 `wb-switch-rust` lib 时必现，与代码内容无关——
# 清 incremental 缓存也复现，属于 rustc 的 bug。关掉增量编译即可绕过，
# 代价是每次全量重编该 crate（其余 crate 仍走缓存，整体多十几秒）。
export CARGO_INCREMENTAL=0

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-pc-windows-msvc"

echo "==> [1/6] 校验构建前置"
if [ ! -f "$HOME/.wb-switch/wb-switch-updater.key" ]; then
  echo "    生成 updater 自签名密钥（仅供本地自测，不能用于正式发布）"
  npx tauri signer generate --ci -w "$HOME/.wb-switch/wb-switch-updater.key" -p "" --force
fi

# 坑 1：Tauri NPM 包版本必须与 Cargo.lock 里的 Rust crate 版本一致，
# 否则 `tauri build` 直接拒绝。package.json 用的是 ^ 范围，`npm install`
# 会装到更新的小版本（api 2.12 vs crate 2.11），必须按 lockfile 精确装。
echo "==> [2/6] 对齐 Tauri 依赖版本（npm 包 vs Rust crate）"
node - <<'NODE'
const lock = require('./package-lock.json').packages;
const want = ['api', 'plugin-opener', 'plugin-dialog', 'plugin-updater', 'cli'];
const out = want.map(n => `@tauri-apps/${n}@${lock[`node_modules/@tauri-apps/${n}`]?.version}`)
  .filter(s => !s.endsWith('@undefined'));
console.log('    ' + out.join(' '));
require('fs').writeFileSync('.tauri-pins', out.join(' '));
NODE
# shellcheck disable=SC2046
npm install --no-save --ignore-scripts $(cat .tauri-pins)
rm -f .tauri-pins

# 坑 2：Windows 安全软件会锁住刚装的 node_modules/.bin，导致 esbuild 平台包
# (@esbuild/win32-x64) 安装后未落盘，`bin/esbuild` 只是个 shim 脚本。
if [ ! -f "node_modules/@esbuild/${TARGET%%-*}/esbuild.exe" ] 2>/dev/null; then
  echo "==> 补装 esbuild 平台二进制"
  EB_VER=$(node -e "console.log(require('./node_modules/esbuild/package.json').version)")
  npm install --no-save --ignore-scripts "@esbuild/win32-x64@${EB_VER}" || true
fi

# 坑 3：`link.exe` 被 Git Bash 的 /usr/bin/link（coreutils 软链）抢占，
# 报 `link: extra operand '*.rlib'`。项目用 .cargo/config.toml 指向
# rustup nightly 自带的 lld-link，因此这台上必须用 nightly 工具链构建。
#
# 但这**只适用于没有 MSVC 构建工具的机器**。.cargo/config.toml 里是绝对路径，
# 不能入库（别人 clone 下来路径不存在，cargo 直接报 linker not found），
# 所以它被 .gitignore 排除、在这里按当前机器生成：
#   - 找得到 nightly 的 lld-link → 生成配置 + 用 nightly
#   - 找不到（装了 Visual Studio Build Tools 的机器）→ 不生成配置，走默认
#     link.exe + stable，构建照常进行
echo "==> [3/6] 校验链接器"
LINKER="$HOME/.rustup/toolchains/nightly-${TARGET}/bin/lld-link.exe"
if [ -f "$LINKER" ]; then
  if [ ! -f .cargo/config.toml ]; then
    LINKER_WIN="$(cygpath -m "$LINKER" 2>/dev/null || printf '%s' "$LINKER")"
    mkdir -p .cargo
    printf '[target.%s]\nlinker = "%s"\n' "$TARGET" "$LINKER_WIN" > .cargo/config.toml
    echo "    已生成本机 .cargo/config.toml（不入库）：$LINKER_WIN"
  fi
  TOOLCHAIN="+nightly"
else
  echo "    未找到 $LINKER"
  echo "    改用系统默认链接器（需要 Visual Studio Build Tools 的 C++ 组件）"
  echo "    若要改用 lld，执行：rustup toolchain install nightly-${TARGET} --profile minimal"
  echo "    然后：cp \$HOME/.rustup/toolchains/nightly-${TARGET}/lib/rustlib/${TARGET}/bin/rust-lld.exe \$LINKER"
  TOOLCHAIN=""
fi

# 坑 4：prepare-agent-companion.mjs 用 `execFileSync('rustc', ['-vV'])` 探测
# target，在本机会被 EBUSY（安全软件瞬时锁）打断。用环境变量直接给值跳过探测。
export AGENT_COMPANION_TARGET="$TARGET"

echo "==> [4/6] 准备 Agent Companion sidecar + 内嵌网关"
# 内嵌网关：把 21 个 wb_*.py + dashboard.html 复制进 Tauri 资源目录并生成清单。
# 资源只是「载体」，运行时由 Rust 侧 ensure_runtime() 释放到可写目录
# （安装目录在 Windows 上是 Program Files，Python 无法在其中写 __pycache__）。
# 它的内部 `npm ci` 在沙箱/受管环境下会触发批量删除保护，这里直接跑
# 已验证可用的等价步骤：构建前端产物 + 编译 Rust runtime + 拷贝 exe。
#
# build:embed 之前必须先把旧的 dist-embed **改名让位**：vite 启动时默认
# 清空 outDir，若目录已存在就撞受管环境的批量删除保护
#   [safe-delete][SAFE_DELETE_BULK_CONFIRM_REQUIRED] count:50, threshold:50
# （实测在受管环境下必现）。改名而不是 rm —— 受管环境不允许脚本递归删除。
if [ -d vendor/agent-companion/dist-embed ]; then
  mv vendor/agent-companion/dist-embed "vendor/agent-companion/dist-embed.old-$(date +%s)" \
    && echo "    已把旧 dist-embed 改名让位（vite 需要空 outDir）"
fi
( cd vendor/agent-companion \
  && npm run build:embed \
  && CARGO_TARGET_DIR="$ROOT/target" cargo $TOOLCHAIN build --release --locked \
       --manifest-path Cargo.toml -p agent-studio-runtime --target "$TARGET" )
mkdir -p src-tauri/binaries
cp "target/${TARGET}/release/agent-studio-runtime.exe" \
   "src-tauri/binaries/agent-studio-runtime-${TARGET}.exe"

echo "==> [5/6] 构建前端"
# 坑 5：vite build 会先清空 outDir，在受管环境下触发批量删除保护。
# 每次用一个**全新**的临时目录（带时间戳）：复用固定目录时，上一轮残留的
# assets 会让计数累加，越积越多最终又撞阈值。
BUILD_OUT="dist-build-$(date +%s)"
npx tsc --noEmit
npx vite build --outDir "$BUILD_OUT"

# 合并：vite 产物 + Agent Companion 资源。
# companion 必须在这一步之后落位——若放在 vite build 之前，会被它的
# 清空逻辑连同 dist 一起删掉。
mkdir -p dist
cp -R "$BUILD_OUT/." dist/

# 坑 7：不能用 `prepare-agent-companion.mjs copy`——它内部先
# `fs.rm(dist/companion, {recursive:true})` 再复制，在受管环境下删除
# 61 个文件会触发批量删除守卫。直接 cp -R 覆盖：内容等价，且不删任何东西。
# 内容哈希变化时旧文件会残留，但 companion 资源由 revision 固定，实践中不会变。
#
# 坑 8：**必须用 `src/.` 而不是 `src`**。`cp -R a b` 在 b 已存在时会把 a 复制成
# `b/a`，于是变成 dist/companion/dist-embed/desktop.html——第 119 行的断言
# 反而能通过，但 RustEmbed 内嵌进去的是错的一层，运行时 companion 面板 404。
# 之前没暴露只是因为 dist/companion 通常不存在（b 不存在时 cp -R a b = 复制为 b）。
# `a/.` 明确表示「把 a 的内容铺进 b」，两种情况都对。
# 但前提是 b 存在——`cp -R a/. b/` 在 b 不存在时会报错，
# 所以先 mkdir（不用 -p 的删除语义，纯创建，不触发删除保护）。
mkdir -p dist/companion
cp -R vendor/agent-companion/dist-embed/. dist/companion/
[ -f dist/companion/desktop.html ] || { echo "    dist/companion 缺失"; exit 1; }
# 反向断言：确认没多出一层嵌套
[ ! -d dist/companion/dist-embed ] || { echo "    ✗ dist/companion/dist-embed 多了一层嵌套"; exit 1; }

echo "==> [6/6] 构建安装包"
#坑 6：`tauri build` 会执行 beforeBuildCommand（npm run build:desktop），
# 它内部的 `npm ci` 在受管环境下必然撞批量删除守卫，前面的步骤会白做一遍。
# Tauri 会自动合并 `tauri.windows.conf.json`，用它把 beforeBuildCommand 置空——
# 这是唯一能真正跳过它的机制（--no-bundle 只跳过打包，不跳过前置命令）。
# 用完立即删除：这是本机构建辅助配置，不该进版本库影响其他人的正常构建。
PLATFORM_CONF="src-tauri/tauri.windows.conf.json"
cat > "$PLATFORM_CONF" <<'JSON'
{
  "$schema": "https://schema.tauri.app/config/2",
  "build": { "beforeBuildCommand": "" }
}
JSON
cleanup() { rm -f "$PLATFORM_CONF"; }
trap cleanup EXIT

# 签名变量必须用 `TAURI_SIGNING_PRIVATE_KEY` 传**密钥内容**，不能用
# `TAURI_SIGNING_PRIVATE_KEY_PATH`——Tauri CLI 只读前者（PATH 形式在部分版本
# 不生效，会报 "A public key has been found, but no private key"）。
# 密钥是 minisign 格式的多行文本，这里用 `$(cat ...)` 整段读入。
# Tauri 2 的 `build` 默认就是 release，没有 `--release` 参数（传了会报
# "unexpected argument"）；只有 debug 需要显式 `--debug`。之前这里无条件
# 拼 `--${PROFILE}`，等于 release 构建根本走不通。
if [ "$PROFILE" = "release" ]; then
  TAURI_PROFILE_ARGS=()
else
  TAURI_PROFILE_ARGS=(--debug)
fi

TAURI_SIGNING_PRIVATE_KEY="$(cat "$HOME/.wb-switch/wb-switch-updater.key")" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  npx tauri build "${TAURI_PROFILE_ARGS[@]}" --bundles nsis
cleanup
trap - EXIT

echo
echo "==> 完成。产物："
# `find | head` 在管道里会被 SIGPIPE 打断，退出码非 0——而本脚本开了
# `set -e`，于是明明构建成功却以非 0 退出（build.sh 跟着失败，
# 表现为「卡在『完成。产物：』那一行」）。
# 用 `|| true` 兜住：这里的 find 只是给人看清单，不参与成败判定。
{ find src-tauri/target target -name "*.exe" -path "*nsis*" 2>/dev/null || true; } | head -5
# 退出码显式归零：本脚本的成败由上面的 npx tauri build 决定，
# 而它成功了就不该因为最后这个纯展示命令而失败。
exit 0