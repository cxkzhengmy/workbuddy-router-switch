# WorkBuddy Router

管理多个 WorkBuddy / CodeBuddy 账号，并把它们的模型能力开放成本地 OpenAI 兼容接口的桌面应用。

一个安装包解决两件事：**账号**（切换、会话迁移、用量统计）和**网关**（把账号变成任何客户端都能调的 API）。项目由 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch) 与 [workbuddy2api-hub](https://github.com/ardeyouxipianyi/workbuddy2api-hub) 合并而成，网关重写为 Rust 后运行在应用进程内，不再需要 Python。

支持 Windows 10 / 11（x64）。

## 功能

| 模块 | 说明 |
| --- | --- |
| 账号管理 | OAuth 扫码登录、导入 / 导出、备注与显示字段自定义 |
| 一键切换 | 在 WorkBuddy、CodeBuddy CLI / IDE、VS Code 插件、JetBrains 插件之间切换，过程显示进度 |
| 会话复制 | 把当前账号勾选的会话复制给目标账号，云端归属目标账号，源账号不受影响 |
| 关联会话 | 复制过的会话自动建立跨账号关联，查看副本状态、增量同步、处理分歧。分组按 CPU 核数并行加载，账号多时不用排队等 |
| 积分统计 | 官方请求用量汇总：总览、近 30 天趋势、模型分类、账号消耗、请求明细；附「积分计费核算」，用本地逐条实扣积分反推每模型每百万 token 单价、对照官方倍率、与官方账单交叉核对 |
| Token 统计 | 按来源统计构成占比、活跃热力图、项目 / 模型 Top 10、会话排行。结果缓存在内存和磁盘，数据没变直接返回，出现新对话自动失效重算 |
| 积分到期 | 自动查询各账号余额与到期时间，7 天内到期高亮并按紧迫度排序 |
| 自动轮换 | 后台把积分最紧迫的账号设为 CLI 默认账号，检测到会话运行时跳过 |
| 内置网关 | OpenAI 兼容服务，API Key 管理、模型白名单、逐请求用量与速度统计 |
| 外部模型 | 把网关接到本机 doubao2api（豆包反代），模型以 `doubao/<id>` 暴露，只代理聊天，见下方说明 |
| 会话悬浮窗 | 桌面常驻状态栏，显示 Codex / WorkBuddy / CodeBuddy 会话状态，点击回到原会话 |
| 应用内更新 | 自动检查本项目 Release，下载签名包并校验后一键重启升级，不用再去网页下载安装包 |

## 安装

到 [Releases](https://github.com/cxkzhengmy/workbuddy-router-switch/releases/latest) 下载 `workbuddy-router-<版本>-setup.exe`，双击运行。

装好之后就不用再回来下载了：应用启动 15 秒后会自动检查更新，之后每 30 分钟一次，也可以在「设置 → 自动更新」里手动检查。下载回来的安装包会用内置公钥校验签名，校验不通过不会执行。

> 1.0.7 及更早的版本是在自动更新关闭时构建的，它们**无法**自己升上来——需要手动装一次 1.0.8，之后的版本就都是应用内更新了。

安装包未做代码签名，Windows SmartScreen 会提示「未知发布者」，点「更多信息」→「仍要运行」即可。macOS / Linux 需自行构建，见文末。

## 快速上手

1. **加账号**：账号页 →「OAuth 扫码登录」。已有备份的话用「导入备份」。
2. **切账号**：账号卡片 →「切换」，可勾选把当前会话一并复制过去。
3. **看用量**：「积分统计」看余额与消耗趋势，「Token 统计」看 token 花在哪个模型上。
4. **开网关**：网关页确认状态为运行中 → 新建 API Key → 填进你的客户端。
5. **管会话**：「关联会话」页按客户端看副本状态，增量同步或处理分歧。
6. **自动轮换**：设置 → CodeBuddy CLI 自动轮换，按积分紧迫度设默认账号。

## 内置网关

网关随应用启动，监听 `127.0.0.1:8788`，只在本机可见。默认使用账号管理里当前激活的账号——界面切号，下一条请求立即用新账号，不需要重启网关。

需要给手机或同网段的另一台电脑用时，在网关页打开「允许局域网访问」开关：服务会改绑 `0.0.0.0` 并按新地址自动重启，页面同时给出形如 `http://192.168.x.x:8788/v1` 的地址。这一项默认关闭，因为网关没有用户体系，只有 API Key 一道门——绑到 `0.0.0.0` 意味着同网段拿到 Key 的人都能用本机账号的额度。首次开启时 Windows 会弹防火墙提示，需要勾选「专用网络」并允许，否则其它设备仍然连不上。

接入步骤：网关页新建 API Key（可限定该 Key 能用哪些模型），然后把地址和 Key 填进任意 OpenAI 兼容客户端，Cline、Cherry Studio、Open WebUI、自己写的脚本都行。

```bash
curl http://127.0.0.1:8788/v1/chat/completions \
  -H "Authorization: Bearer sk-wb-xxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4.1-flash",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8788/v1", api_key="sk-wb-xxxxxxxx")

for chunk in client.chat.completions.create(
    model="deepseek-v4.1-flash",
    messages=[{"role": "user", "content": "你好"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="")
```

接口：`/v1/models`、`/v1/chat/completions`、`/health`。

> 端口沿用 workbuddy2api-hub 的 `8788`，原来指向 hub 的客户端不用改任何配置。
> 与 hub 的区别是不做账号池轮询（账号跟界面当前激活的那个走），也不带独立 Web 看板（看板职责由桌面端界面承担）。

### 外部模型（豆包反代）

1.0.14 起，外部通道从「OpenCode 公网免费层」换成**本机 [doubao2api](https://github.com/wangchuxiaoji-oss/doubao2api)**。旧通路在 1.0.12 已判定不可用：免费模型只允许官方客户端、要 Bun 的 TLS/HTTP2 指纹、UA `<1.18` 先返回 `426 UpgradeRequired`，且与官方 CLI 共享每日额度；普通 OpenAI 客户端怎么配都拿不到 200。

新上游是跑在本机 `http://127.0.0.1:9090/v1` 的 Python/FastAPI 服务，用 Playwright 维持豆包网页版登录态，对外提供标准 OpenAI 接口。**服务在跑不等于能用**：首次使用、Cookie 过期或命中风控时，需要到 `/admin` 用豆包 App 扫码 / 处理验证码。页面顶部的「服务状态」会直接探测 `/health` 的 `logged_in` 与 `needs_captcha`。

- 模型以 `doubao/<id>` 出现在网关 `/v1/models` 里，前缀用于和 WorkBuddy 自带模型区分路由。推荐 `doubao`、`doubao-pro`、`doubao-think`、`doubao-expert` 四个对话模型，页面有「一键填入」。
- **只代理聊天**：`/v1/chat/completions` 原样转发。图片 / 视频 / 音乐生成端点不在网关代理范围（超时与缓冲策略完全不同），生成类模型在界面上标注且不可勾选，Rust 侧 `save` / `forward` 也会二次拒绝；要用生成能力请直连 9090。
- 上游目录里的千问系模型属于另一条可选通道，需单独设置 `QIANWEN_ENABLED=true`，默认部署不开启，界面只做提示。
- 上游用 `DOUBAO_API_KEY` 保护 `/v1/*`；在页面填同一个 Key。`adminLink` 会自动带上 `?key=`，配置视图本身不回显明文 Key。
- 设置存在 `proxy.db` 的 `external_doubao` 键，**不继承**旧的 `external_opencode` 配置，避免显示一个永远连不上的地址。
- 调用会记入网关请求台账，账号标签为 `doubao`；用量取上游返回的 `usage`。

本机部署步骤（仓库外层，不随本仓库发布）见维护手册「豆包反代本机部署」一节。

## 积分计费核算

「积分统计」页往下能看到「积分计费核算」。官方只公布了各模型的**积分消耗倍率**，没给基准单价，也没给缓存 token 的计价规则，所以这里不假装知道官方单价，而是把可复核的本地账本摊开：

- 从本机会话日志里逐条读取 `providerData.rawUsage.credit`（实扣积分）与 token 计数，按日期 × 模型聚合。复制/派生会话重放的历史会去重，不会被重复计费。
- 两种口径可切换：**全量 token**（输入 + 输出 + 缓存写入）与**有效 token**（未缓存输入 + 输出）。官方没说缓存怎么算，所以两种都保留，只在**同一种口径内**做横向比较。
- 「积分 / 百万 token」= 实扣积分 ÷ 所选口径 token × 100 万；再除以官方倍率得到**归一基准价**。倍率正确时各模型应收敛到同一基准，单个模型明显偏高才值得怀疑；全体一起偏高更可能是计价口径或官方调价。倍率表可在卡片里编辑，存到 `~/.wb-switch/credit-rate-multipliers.json`。
- 「官方账单 vs 本地记录」把官方 `rangeStart` ~ `rangeEnd` 区间内的按模型积分，与同一区间的本地实扣积分逐项相减。官方接口按账号统计（可能含其他设备/客户端），本地只覆盖这台机器，所以差额用于定位**漏记 / 口径不一致**，不能单独当作「多扣积分」的结论。

账本和 Token 统计共用同一套数据指纹缓存，会话文件没变就直接复用，不会反复扫几百 MB 日志。页面只展示聚合数字，不返回消息正文或绝对路径。

## 支持的工具

| 工具 | 账号切换 | 会话复制 | 自动关闭重开 | 自动轮换 | 悬浮窗监听 |
| --- | :---: | :---: | :---: | :---: | :---: |
| WorkBuddy | ✅ | ✅ | ✅ | — | ✅ |
| CodeBuddy IDE | ✅ | ✅ | ✅ | — | ✅ |
| CodeBuddy CLI | ✅ | — | — | ✅ | — |
| VS Code CodeBuddy 插件 | ✅ | ✅ | ✅ | — | ✅ |
| JetBrains IDE 插件 | ✅ | — | ✅ | — | — |

「设置 → 支持工具」可按客户端逐个开关入口。关掉后该端从账号页隐藏、不再轮询状态，不影响账号库。JetBrains 端默认关闭。

## 数据与隐私

- 账号凭据、积分快照、网关台账、统计缓存全部存放在本机 `~/.wb-switch/`
- 网关默认只监听 `127.0.0.1`，API Key 只在本机校验；只有手动打开「允许局域网访问」后才会绑 `0.0.0.0`，此时同网段设备可凭 Key 访问
- 除登录、积分查询、模型请求这些必要的官方接口调用外，不上传任何数据
- 内置更新：启动 15 秒后自动检查一次，之后每 30 分钟一次；只访问本项目的 GitHub Release，不上传任何数据
- 卸载不会删除 `~/.wb-switch/`，需要时手动清理

## macOS 权限

切换账号需要写入认证文件，macOS 要求授权「App 管理」或「完全磁盘访问」：

1. 首次切换报「无权限」时，点「打开系统设置」
2. 优先在 **App 管理** 打开本应用开关；列表里没有，再到 **完全磁盘访问** 添加
3. 授权后重启应用生效；设置页「权限检测」可随时验证

## 从源码构建

### 环境要求

| 依赖 | 版本 | 说明 |
| --- | --- | --- |
| Node.js | 20 或 22 | 22 是 CI 用的版本 |
| Rust | stable 起步，建议 nightly | 见下方「Windows 链接器」 |
| Tauri CLI | 随 `npm install` 装上 | 在 devDependencies 里 |
| WebView2 | Windows 10/11 自带 | 旧系统需单独装 Runtime |
| Visual Studio Build Tools | C++ 工作负载 | Windows 编译必需 |

Linux 还需 `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`。

### 开发与打包

```bash
npm install
npx tauri dev                   # 开发模式，热更新

bash scripts/build.sh           # 打安装包（debug，快）
bash scripts/build.sh release   # release 版，包更小、更慢
SKIP_BUILD=1 bash scripts/build.sh   # 只跑前置检查，不构建
```

产物在仓库根目录的 `release/`，文件名 `workbuddy-router-<版本>-setup.exe`。`build.sh` 会先校验 `src-tauri/Cargo.toml` 与 `src-tauri/tauri.conf.json` 的版本号一致，再注入 Agent Companion sidecar、构建前端、打 NSIS 包。

### 首次构建会自动准备的东西

- **updater 签名密钥**：`~/.wb-switch/wb-switch-updater.key` 不存在时自动生成一份自签名密钥，仅供本地自测。正式发布要换成自己的密钥对，并把公钥写进 `tauri.conf.json`。
- **`.cargo/config.toml`**：内容是本机 rustup 的绝对路径，不入库。脚本检测到 nightly 的 `lld-link.exe` 时按当前机器生成；装了 Visual Studio Build Tools 的机器不生成，直接走默认 `link.exe`。需要手动配置时参考 `.cargo/config.toml.example`。

### Windows 链接器

没装 MSVC 构建工具、又在 Git Bash 里构建的机器，默认链接器会被 Git Bash 自带的 `/usr/bin/link` 抢名字，报 `link: extra operand '*.rlib'`。装 nightly 并复制一份 `lld-link.exe` 即可，构建脚本会自动接管：

```bash
rustup toolchain install nightly-x86_64-pc-windows-msvc --profile minimal
cp "$HOME/.rustup/toolchains/nightly-x86_64-pc-windows-msvc/lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe" \
   "$HOME/.rustup/toolchains/nightly-x86_64-pc-windows-msvc/bin/lld-link.exe"
```

### 代码结构

```
crates/wb-switch-core/      账号、切换、会话、统计、网关（Rust）
crates/wb-switch-server/    无头服务，供浏览器版使用
src/                        React 前端（桌面版与浏览器版共用）
src-tauri/                  Tauri 外壳与命令注册
vendor/agent-companion/     会话悬浮窗，第三方组件快照
scripts/                    构建与发版脚本
```

## 浏览器版

代码在仓库里（`crates/wb-switch-server` + `npm/`），但 npm 包尚未发布到本项目名下，`npm i -g workbuddy-switch` 装到的是上游的包，两者不要混用。要自己跑：

```bash
cargo build -p wb-switch-server --release
./target/release/wb-switch                # serve：起服务并打开浏览器
./target/release/wb-switch serve --port 8790 --no-open
./target/release/wb-switch status         # 终端查看当前账号
```

界面与桌面版一致，但不提供会话悬浮窗。macOS 下的权限由启动服务的终端进程决定。

## 免责声明

本项目是第三方工具，与腾讯及 WorkBuddy / CodeBuddy 官方无关联。它通过读写本机客户端数据与调用官方接口工作，**可能因官方调整而失效**；使用非官方客户端也可能触发账号风控、限流或封禁。请勿用于商业转售，建议使用小号并自担风险。

## 致谢与许可

桌面端来自 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch)，网关协议与设计来自 [workbuddy2api-hub](https://github.com/ardeyouxipianyi/workbuddy2api-hub)，会话悬浮窗由 [Agent Companion](https://github.com/changexbc/agent-companion) 提供。感谢 [Linux.do](https://linux.do) 社区。

本项目以 MIT 许可发布，详见 [LICENSE](./LICENSE)。
