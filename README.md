# WorkBuddy Router

**把 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch) 和 [workbuddy2api-hub](https://github.com/ardeyouxipianyi/workbuddy2api-hub) 合成一个桌面应用的产物。**

账号管理、会话迁移、用量统计来自前者；把账号变成 OpenAI 兼容接口的网关来自后者。上游是两个各自独立、各自要单独部署的项目，这里把它们并进同一个进程、同一个安装包、同一份账号库。

---

## 一、这是个合并体

先把两个来源摆清楚，因为本项目的每一个设计取舍都来自它们各自的边界。

| | 来源项目 | 它原本是什么 | 在本项目里的形态 |
| --- | --- | --- | --- |
| **A** | `changexbc/workbuddy-switch` | 基于 Tauri 2 的桌面控制台。管 WorkBuddy / CodeBuddy 的多账号：OAuth 登录、一键切换、会话复制、积分与 Token 统计、CLI 自动轮换 | **桌面端主体**。账号页、会话页、两类统计页、切换逻辑、托盘、悬浮窗全部沿用这条线 |
| **B** | `ardeyouxipianyi/workbuddy2api-hub` | 独立的 Python 服务，自带 Web 看板。把账号的模型能力包装成 OpenAI 兼容 API，默认端口 8788，靠 Docker 或 bat 脚本启动 | **被 Rust 原生重写**后跑在应用进程内，成为「网关」页和 `/v1/*` 接口。不再有 Python、不再有子进程 |

名字里的 **Router** 指的是 B 那一半：账号是路由目标，网关把请求路由到当前激活账号背后的模型。

## 二、为什么要合

分开用时，你要同时装两个东西，而且它们互相不知道对方存在：

| 场景 | 两个项目分开跑 | 合并后 |
| --- | --- | --- |
| 安装 | 装一份桌面端，再单独配一份 Python hub | 一个安装包，装完即用 |
| 运行 | hub 是独立进程，要自己拉起来、自己守端口 | 网关随应用启动，跑在同一进程内，无外部运行时 |
| 账号 | 桌面端一份账号库，hub 里再配一遍账号或账号池 | **只有一份账号库**，界面切号，网关下一条请求就用新账号 |
| 用量 | 桌面端统计一套，hub 台账另一套，对不上 | 网关的逐请求台账与桌面端的积分 / Token 统计打通 |
| 升级 | 两边版本各自演进，兼容性自己盯 | 一次发版，两端一致 |

合并**不是简单打包**，B 被刻意做成了子集，见第四节。

## 三、合并后有哪些能力

「来源」一列：**A** = 来自 workbuddy-switch，**B** = 来自 workbuddy2api-hub，**A+B** = 两边打通后才成立的能力。

| 模块 | 说明 | 来源 |
| --- | --- | --- |
| 账号管理 | OAuth 扫码登录、导入 / 导出、备注与显示字段自定义 | A |
| 账号切换 | 一键切换 WorkBuddy / CodeBuddy CLI / CodeBuddy IDE / VS Code 插件 / JetBrains 插件，过程显示进度 | A |
| 会话复制 | 把当前账号勾选的会话复制给目标账号，云端归属目标账号，源账号不受影响 | A |
| 关联会话 | 复制过的会话自动建立跨账号关联，看副本状态、增量同步、处理分歧 | A |
| 积分统计 | 官方请求用量汇总：总览、近 30 天趋势、模型分类、账号消耗、请求明细 | A |
| Token 统计 | 按来源统计构成占比、活跃热力图、项目 / 模型 Top 10、会话排行 | A |
| 积分到期 | 自动查询余额与到期时间，7 天内到期高亮并按紧迫度排序 | A |
| 自动轮换 | 后台把积分最紧迫的账号设为 CLI 默认账号，检测到会话运行时跳过 | A |
| 会话悬浮窗 | 桌面常驻状态栏，显示 Codex / WorkBuddy / CodeBuddy 会话运行状态，点击回到原会话 | A（第三方组件） |
| **内置网关** | OpenAI 兼容服务，API Key 管理、模型白名单、逐请求用量与速度统计 | B → Rust 重写 |
| **账号跟随** | 网关不维护独立账号池，永远跟随界面上的当前激活账号，切号即时生效 | **A+B** |
| **用量闭环** | 网关的请求台账直接并入桌面端的积分与 Token 统计 | **A+B** |

## 四、B 那一半做了哪些减法

这一点必须写明白，否则从 hub 迁过来的人会踩空。内置网关的定位是「够用」，不是「功能对齐」：

| workbuddy2api-hub 的能力 | 本项目 |
| --- | --- |
| OpenAI 兼容反向代理 | **保留**（`/v1/models`、`/v1/chat/completions`、`/health`） |
| API Key 管理、模型白名单 | **保留** |
| 模型目录 | **保留**（按 hub 的规则清理与排序） |
| 逐请求台账：用量、积分、首字延迟 | **保留** |
| 多账号池轮询调度 | **不做**。改为跟随当前激活账号——账号切换在桌面端是显式操作，池化会让「现在用的是谁」变得不可解释 |
| 风控连坐、模型别名魔法、独立 Web 看板 | **不做**。看板职责由桌面端界面承担 |

取舍的原因写在代码里：每多对齐一项 hub 的能力，就多一处与上游协议漂移的维护面。

端口沿用 **8788**，所以原来指向 hub 的客户端 `base_url` 一个字都不用改。

## 五、端口与接口

网关只监听 `127.0.0.1`，不对外暴露。

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

接入步骤：打开「网关」页确认状态为运行中 → 新建一个 API Key（可限定该 Key 能用哪些模型）→ 把地址和 Key 填进任意 OpenAI 兼容客户端（Cline、Cherry Studio、Open WebUI、自己写的脚本都行）。

## 六、下载与安装

目前只提供 **Windows x64** 安装包，在 [Releases](https://github.com/cxkzhengmy/workbuddy-router-switch/releases/latest) 页面下载 `workbuddy-router-<版本>-setup.exe`，双击运行。

- macOS / Linux 需自行从源码构建，见下节。
- 安装包未做代码签名，Windows SmartScreen 会提示「未知发布者」，属正常现象。
- 卸载不会删除 `~/.wb-switch/`，账号与统计数据需手动清理。

## 七、从源码构建

### 环境要求

| 依赖 | 版本 | 说明 |
| --- | --- | --- |
| Node.js | 20 或 22 | 22 是 CI 用的版本 |
| Rust | stable 起步，建议 nightly | 见下方「Windows 链接器」 |
| Tauri CLI | 随 `npm install` 装上 | 在 devDependencies 里 |
| WebView2 | Windows 10/11 自带 | 旧系统需单独装 Runtime |
| Visual Studio Build Tools | C++ 工作负载 | Windows 编译必需 |

Linux 还需 `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`。

### 跑起来

```bash
npm install
npx tauri dev          # 开发模式，热更新
```

### 打安装包

```bash
bash scripts/build.sh           # debug 版，快
bash scripts/build.sh release   # release 版，包更小、更慢
```

产物在仓库内的 `release/` 目录，文件名 `workbuddy-router-<版本>-setup.exe`。

`build.sh` 先做版本一致性校验（`src-tauri/Cargo.toml` 与 `src-tauri/tauri.conf.json` 必须相同），再调 `scripts/build-exe-local.sh` 完成：注入 Agent Companion sidecar → 构建前端 → 打 NSIS 包。

只跑前置检查、不构建：

```bash
SKIP_BUILD=1 bash scripts/build.sh
```

### 首次构建会自己准备的东西

- **updater 签名密钥**：`~/.wb-switch/wb-switch-updater.key` 不存在时自动生成一份自签名密钥，仅供本地构建自测。正式发布要换成自己的密钥对，并把公钥写进 `tauri.conf.json`。
- **`.cargo/config.toml`**：内容是本机 rustup 的绝对路径，**不入库**。脚本检测到 nightly 的 `lld-link.exe` 时按当前机器生成；装了 Visual Studio Build Tools 的机器不生成，直接走默认 `link.exe`。需要手动配置时参考 `.cargo/config.toml.example`。

### Windows 链接器

没装 MSVC 构建工具、又在 Git Bash 里构建的机器，默认链接器会被 Git Bash 自带的 `/usr/bin/link` 抢名字，报 `link: extra operand '*.rlib'`。装 nightly 并复制一份 `lld-link.exe` 即可，构建脚本会自动接管：

```bash
rustup toolchain install nightly-x86_64-pc-windows-msvc --profile minimal
cp "$HOME/.rustup/toolchains/nightly-x86_64-pc-windows-msvc/lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe" \
   "$HOME/.rustup/toolchains/nightly-x86_64-pc-windows-msvc/bin/lld-link.exe"
```

## 八、支持的工具矩阵

| 工具 | 账号切换 | 会话复制 | 自动关闭重开 | 自动轮换 | 悬浮窗监听 |
| --- | :---: | :---: | :---: | :---: | :---: |
| WorkBuddy | ✅ | ✅ | ✅ | — | ✅ |
| CodeBuddy IDE | ✅ | ✅ | ✅ | — | ✅ |
| CodeBuddy CLI | ✅ | — | — | ✅ | — |
| VS Code CodeBuddy 插件 | ✅ | ✅ | ✅ | — | ✅ |
| JetBrains IDE 插件 | ✅ | — | ✅ | — | — |

「设置 → 支持工具」可按客户端逐个开关入口。关掉后该端从账号页隐藏、不再轮询状态，不影响账号库。JetBrains 端默认关闭。

## 九、常用操作

1. **添加账号**：账号页 →「OAuth 扫码登录」；也可「导入备份」恢复历史账号
2. **切换账号**：账号卡片 →「切换」，可勾选把当前会话一并复制过去
3. **开放模型能力**：网关页 → 新建 API Key → 填进客户端
4. **看用量**：网关页看逐请求统计；积分统计 / Token 统计看整体趋势
5. **管关联会话**：「关联会话」页按客户端看副本状态，增量同步或处理分歧
6. **自动轮换**：设置 → CodeBuddy CLI 自动轮换，按积分紧迫度设默认账号

## 十、代码结构：哪块来自哪半

```
crates/wb-switch-core/      账号、切换、会话、统计      ← A 的骨架
  └── src/modules/proxy/    内置网关，Rust 重写        ← B 的替代品
crates/wb-switch-server/    无头服务，供浏览器版使用
src/                        React 前端（桌面版与浏览器版共用）
src-tauri/                  Tauri 外壳与命令注册
vendor/agent-companion/     会话悬浮窗，第三方组件快照
npm/                        npm 包与 CLI 入口
scripts/                    构建与发版脚本
```

`proxy/` 之外的模块基本都能在 workbuddy-switch 里找到对应物；`proxy/` 是把 workbuddy2api-hub 的行为用 axum + reqwest + rusqlite 重做了一遍。**仓库里没有任何 Python 代码。**

## 十一、与两个上游的关系

- 代码已**脱离实时跟随**，不再从上游三方合并，两个上游的后续演进不会自动进入本仓库。
- A 的桌面端逻辑与 B 的协议语义仍保持兼容：端口、接口路径、请求与响应的形状都按 hub 原样，换用本应用不需要改客户端配置。
- 有问题请提到本仓库，不要拿本项目的改动去上游开 issue。

## 十二、数据与隐私

- 账号凭据、积分快照、网关台账、统计缓存全部存在本机 `~/.wb-switch/`
- 网关只监听 `127.0.0.1`，API Key 只在本机校验
- 除登录、积分查询、模型请求这些必要的官方接口调用外，不上传任何数据
- 自动更新已禁用（`UPDATER_ENABLED = false`），应用不会主动向外检查新版本

## 十三、macOS 权限

切换账号需要写入认证文件，macOS 要求授权「App 管理」或「完全磁盘访问」：

1. 首次切换报「无权限」时，点「打开系统设置」
2. 优先在 **App 管理** 打开本应用开关；列表里没有，再到 **完全磁盘访问** 添加
3. 授权后重启应用生效；设置页「权限检测」可随时验证

## 十四、浏览器版

代码在仓库里（`crates/wb-switch-server` + `npm/`），但 **npm 包尚未发布到本项目名下**——现在执行 `npm i -g workbuddy-switch` 装到的是上游项目发布的包，不是这个仓库构建出来的东西，两者不要混用。

要自己跑，从源码起本地服务即可（二进制名是 `wb-switch`）：

```bash
cargo build -p wb-switch-server --release
./target/release/wb-switch                # serve：起服务并打开浏览器
./target/release/wb-switch serve --port 8790 --no-open
./target/release/wb-switch status         # 终端查看当前账号
```

界面与桌面版一致，覆盖上述模块，但不提供会话悬浮窗。macOS 下的权限由启动服务的终端进程决定。

## 十五、免责声明

本项目是第三方工具，与腾讯及 WorkBuddy / CodeBuddy 官方无关联。它通过读写本机客户端数据与调用官方接口工作，**可能因官方调整而失效**；使用非官方客户端也可能触发账号风控、限流或封禁。请勿用于商业转售，建议使用小号并自担风险。

## 十六、致谢与许可

- 桌面端骨架来自 [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch)
- 网关协议与设计来自 [ardeyouxipianyi/workbuddy2api-hub](https://github.com/ardeyouxipianyi/workbuddy2api-hub)
- 会话悬浮窗由 [Agent Companion](https://github.com/changexbc/agent-companion) 提供
- 感谢 [Linux.do](https://linux.do) 社区

本项目以 MIT 许可发布，详见 [LICENSE](./LICENSE)。
