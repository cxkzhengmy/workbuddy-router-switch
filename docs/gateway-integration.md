# 网关集成（workbuddy2api-hub sidecar）

把 [workbuddy2api-hub](https://github.com/ardeyouxipianyi/workbuddy2api-hub) 作为**受管子进程**挂在
workbuddy-switch 里，对外提供 OpenAI / Anthropic 兼容的反向代理。

## 设计取向：进程外挂，不重写

hub 的反代内核（`wb_proxy.py` 17724 行 + `wb_accounts.py` 3590 行）承担三件事：

1. 三协议转换（Chat Completions / Responses / 原生 Anthropic Messages）
2. 设备指纹派生与 WAF 指纹脱敏（`wb_fingerprint.py` 的 `derive_id`）
3. 多账号池调度、限额护栏、熔断降级

这些逻辑的价值在于**持续对抗上游风控**——上游规则一变就要跟着改。在 Rust 侧重写等于
放弃全部对抗成果、从此长期追赶，因此本集成一行协议逻辑都没有复制。

hub 侧**零改动**：`wb_proxy.py` 不需要知道宿主是谁，它只是一个监听 8788 的本地 HTTP 服务。

## 为什么能共用账号

两边登录打的是**同一套官方端点**：

| 环节 | 端点 |
| --- | --- |
| 申请 state | `/v2/plugin/auth/state?platform=…` |
| 换取 token | `/v2/plugin/auth/token?state=…` |
| 拉取资料 | `/v2/plugin/login/account?state=…` |

域名同为 `www.codebuddy.cn` / `www.workbuddy.ai`。所以同一账号在两边登录产出的是同一种
token，账号库可以互转。

## 使用

网关**随应用一起分发**，装完即用，无需另外下载或指定目录。

1. 侧栏进入「网关」
2. 点「启动」，状态转为运行中
3. 「从账号库同步」把本应用账号写入网关账号池
4. 用 `Base URL`（`http://127.0.0.1:8788/v1`）配置 Claude Code / Codex 等客户端

唯一的外部依赖是 **Python 3.9+**（hub 是纯标准库 Python，不需要 pip install 任何东西）。

## 内嵌机制

hub 的 21 个 `wb_*.py` + `dashboard.html`（约 2.0MB）在构建时复制进
`src-tauri/resources/gateway-hub/`，由 `tauri.conf.json` 的 `bundle.resources` 打进安装包。

**为什么不能直接在资源目录里运行**：

- 安装目录在 Windows 上是 `Program Files`，Python 无法在其中生成 `__pycache__`；
- `wb_proxy.py` 用 `os.path.dirname(os.path.abspath(__file__))` 定位 `accounts/`、
  `usage/` 的相对位置，写死在只读目录会直接报错。

所以资源只是**载体**，运行时由 `gateway::ensure_runtime()` 释放到可写目录：

```
安装包 resources/gateway-hub/   （只读）
        ↓ 首次启动释放
~/.wb-switch/gateway/runtime/    （可写，Python 在此运行）
```

释放是**幂等**的：按「文件数 + 总字节 + 清单哈希」算指纹存到 `.release-stamp`，
指纹一致就跳过。应用升级后 hub 版本变化，指纹改变会自动重新释放——
只判断「文件是否存在」是不够的，旧文件会留在原地。

若构建时未内嵌 hub（源码直接运行），自动退回使用配置里的 `hubDir`，开发期照常可用。

## 改变数据落位（C 盘紧张时）

**在界面上改**：「网关 → 配置」，两个目录各有一个输入框和一个「浏览」按钮
（浏览走系统原生目录选择器，不用手敲路径）。

| 目录 | 内容 | 大小 |
| --- | --- | --- |
| 运行时目录 | hub 运行时代码 | 约 2MB，只写一次 |
| 账号库目录 | 账号凭证 + 用量日志 | 随使用增长 |

真正会长大的是**账号库目录**，省空间优先挪它。

保存前会先探测目录可用性：不存在会尝试创建，已存在则试写探针确认可写。
不可用时禁用保存并给出原因——否则要等到点「启动」才失败，而那时报错来自
Python、不指向目录权限，很难联想到真因。

**必须填绝对路径** —— 相对路径会被忽略。原因是 sidecar 的工作目录被设成
运行时目录自身，相对路径会解析出自指的怪路径。

改完账号库目录后点一次「从账号库同步」：原目录的凭证不会再被读，但源数据
始终在本应用的账号库里，重新同步一次即可。

界面上每个目录都标注了来源（默认位置 / 已在下方自定义 / 由环境变量指定）。

### 无头部署用环境变量

没有界面的场景（CI、容器里的 webui）可用环境变量：

| 环境变量 | 管什么 |
| --- | --- |
| `WB_GATEWAY_RUNTIME_DIR` | 运行时目录 |
| `WB_GATEWAY_ACCOUNTS_DIR` | 账号库目录 |

优先级：**界面设置 > 环境变量 > 默认**。桌面用户在界面上改一定生效——
若某处设过环境变量，界面上会显示「由环境变量指定」，提示了真实原因。

### 为什么不默认放 exe 同级

安装版 exe 位于 `C://Program Files\...`，普通用户**无写权限**，直接写会失败。
绿色版（解压即用）想放在 exe 旁边，在配置里指向那个位置即可。

## 数据落位

| 内容 | 路径（默认） |
| --- | --- |
| 网关配置 | `~/.wb-switch/gateway.json` |
| 内嵌 hub 的运行时 | `~/.wb-switch/gateway/runtime/` |
| 网关账号库 | `~/.wb-switch/gateway/accounts/<uid>.json` |
| sidecar 日志 | `~/.wb-switch/gateway/gateway.log` |

## 两个必须知道的边界

### 时间戳单位不同

| | 单位 | 换算 |
| --- | --- | --- |
| switch | **毫秒** | `norm_ts()`：小于 1e10 则 ×1000 |
| hub | **秒** | `normalize_epoch()`：大于 1e11 则 ÷1000 |

`expiresAt` 直接照搬会让过期判断差 1000 倍，表现为「刚登录就过期」或「过期了还能用很久」。
`gateway_sync::ms_to_secs` 负责显式换算，不指望对方兜底。

### 加密信封账号会被跳过

WorkBuddy 2026-09-24 之后把 token 存成 `{$wbEncrypted, envelope}`。switch 侧按原样保留
（交给 WorkBuddy 自己解密），但 hub 的 `Account.__init__` 是 `str(data.get("accessToken"))——
把信封对象 `str()` 一下会得到整个字典的字面量，认证必然失败。

所以同步**只搬运明文 token**；遇到信封账号如实报告为跳过，不做猜测性的反向加密——
那是 hub 自己 `wb_atrest` 的职责，宿主不该重复实现它的解密链路。

需要这类账号时，在网关面板用它的「导入桌面端」功能添加。

网关账号库独立于 `~/.wb-switch/accounts.json`（单文件数组），两者格式不同，由 `sync`
显式转换，不做目录级共享，避免一边写坏另一边。

## 排查「弹出命令行窗口」

Windows 上 GUI 应用用 `std::process::Command::new` 启子进程会**分配控制台并弹窗**。
本项目为此提供了 `process::cmd_builder()`，它在 Windows 上加
`CREATE_NO_WINDOW (0x0800_0000)`。

网关相关的 **7 处**全部走它（不是只有启动那一处）：

| 位置 | 为什么会弹 |
| --- | --- |
| `start()` 启 sidecar | 直接原因 |
| `detect_python()` | 依次试 `python`/`python3`/`py`，每个都弹一次 |
| `terminate()` 的 SIGTERM / `kill -0` / taskkill | 一次停止最多十几条命令，逐个弹 |
| 锁中毒时清理孤儿进程 | 边缘路径 |

**若仍看到窗口**，先确认看到的是什么：

- **文件资源管理器**（标题是路径、右侧有搜索框、内容是 `.exe/.pdb`）
  —— 那通常是构建产物目录，不是控制台窗口。
- 若确有黑色控制台窗口，检查是否来自**修复前构建的 exe**
  （对比 exe 修改时间与 `gateway.rs` 修改时间），或来自网关之外的功能
  （CodeBuddy CLI 切换、OAuth 登录等，各模块自行管理子进程）。

sidecar 的 stdout/stderr 已全部重定向到 `~/.wb-switch/gateway/gateway.log`，
不需要任何控制台窗口来观察它——用「网关 → 网关日志」面板即可。

## 生命周期

- **启动**：先探 `/health`，已在跑就直接返回（幂等）——两个进程抢同一端口会让第二个
  启动即崩，前端看到的是「启动失败」而非「已在运行」
- **停止**：只回收**本应用拉起**的进程。手工启动的网关（终端里跑 `start-wb-proxy.bat`、
  或 Docker 容器）只探活不接管，停止操作明确拒绝而不是去杀不归自己管的进程
- **退出**：Tauri `RunEvent::Exit` 时调 `gateway::shutdown()`，否则会留下孤儿进程占着 8788

## 面板鉴权

hub 的面板是两步鉴权：

1. `POST /panel/login` 提交密码 → 换 session token
2. 之后每个面板请求带 `X-Panel-Token` header

token 由 hub 侧维护，宿主只缓存在内存里，**不回显到前端**（状态接口只给
`panelLoggedIn` 布尔值）。token 失效时 `panel_call` 自动用保存的密码重登一次——
hub 重启或改密码都会让旧 token 失效，让用户手动重登没有意义。

面板密码与 API Key 相互独立，默认 `admin`。

## SSRF 边界

`gateway::normalize_panel_path` 只放行 hub 的面板路由前缀
（`accounts` / `usage` / `activity` / `tasks` / `scheduler` / `settings` / `updates` /
`logs` / `pricing` / `agents` / `proxy` / `health` / `realm` / `panel`），
其余一律降级到 `/health`。

**桌面端与 webui 必须共用这一个函数**——否则 webui 会成为绕过口。

## 双通道一致性

| 能力 | Tauri 命令 | HTTP 路由 |
| --- | --- | --- |
| 状态 | `get_gateway_status` | `GET /api/gateway/status` |
| 配置 | `save_gateway_config` | `POST /api/gateway/config` |
| 启动 | `start_gateway` | `POST /api/gateway/start` |
| 停止 | `stop_gateway` | `POST /api/gateway/stop` |
| 登录 | `gateway_panel_login` | `POST /api/gateway/login` |
| 转发 | `gateway_panel_request` | `POST /api/gateway/request` |
| 账号列表 | `list_gateway_accounts` | `GET /api/gateway/accounts` |
| 同步 | `sync_gateway_accounts` | `POST /api/gateway/sync` |

## 环境要求

- **Python 3.9+**：hub 是纯标准库 Python，**不需要 pip install 任何依赖**
  （Dockerfile 只 `COPY wb_*.py dashboard.html`），这也是 sidecar 成本低的原因

## 测试

```bash
cargo +nightly test -p wb-switch-core gateway
```

覆盖：时间戳双向换算、驼峰字段映射、信封拒绝搬运、文件名净化与 hub 一致、
区域映射 cn↔intl、面板路径白名单、配置读取不递归。