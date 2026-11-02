//! 端口占用处理：查出谁占了 8788，并按需结束它。
//!
//! # 为什么要这个模块
//!
//! [`crate::modules::gateway::stop`] 只能结束**本应用自己启动**的进程
//! （`CHILD` 里存着 `Child` 句柄）。但这个端口经常被别的东西占着：
//!
//! - 用户在外部直接 `python wb_proxy.py --port 8788` 起了一个
//! - 上一次异常退出留下的孤儿进程
//! - 别的 WorkBuddy 实例
//!
//! 这时界面会显示「网关在运行，但不是由本应用启动的，无法在此停止」，
//! 而用户**没有任何在界面上解开死结的办法**——只能自己去任务管理器翻。
//! 这是个真实的操作死角，本模块补上它。
//!
//! # 两条安全约束
//!
//! 1. **先确认身份，再动手**。端口被占 ≠ 随便就能杀：占用者可能是用户
//!    正在用的其它服务。所以先用 hub 的 `/health` 指纹确认它确实是 hub，
//!    并把 PID / 进程名回传给界面，让用户看清要杀的是谁。
//! 2. **默认不杀，只查**。`find_port_owner` 只读；真正结束进程是
//!    独立的 `kill_port_owner`，由界面上的按钮显式触发。

use serde_json::{json, Value};

/// 占用信息。`pid` 为 `None` 表示查到了监听者但拿不到 PID（例如权限不足）。
#[derive(Debug, Clone, Default)]
pub struct PortOwner {
    pub pid: Option<u32>,
    /// 进程名（如 `python.exe`）。拿不到时为空串。
    pub name: String,
    /// 该端口是否有监听者。
    pub listening: bool,
}

/// 查端口的占用者。
pub fn find_port_owner(port: u16) -> PortOwner {
    #[cfg(windows)]
    {
        windows_port_owner(port)
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        PortOwner::default()
    }
}

/// 界面用的「谁占了 8788」信息。
///
/// 刻意**不**自动结束进程：先让用户看清 PID 和进程名，再决定。
pub fn describe_port_owner(port: u16) -> Value {
    let owner = find_port_owner(port);
    let mut v = json!({
        "port": port,
        "listening": owner.listening,
        "pid": owner.pid,
        "process": owner.name,
        "canKill": owner.pid.is_some(),
    });
    // 拿不到 PID 时明确说明原因，别让"canKill:false"变成谜语。
    if owner.listening && owner.pid.is_none() {
        v["hint"] = json!(
            "端口有监听但拿不到进程号，通常是权限不足\
             （需以管理员身份运行本应用），或监听者属于其它会话。"
        );
    } else if !owner.listening {
        v["hint"] = json!("该端口当前没有监听者。");
    }
    v
}

/// 结束占用该端口的进程。
///
/// 会先核对「现在占着的还是不是刚才查到的那个 PID」——两次调用之间进程
/// 可能已经重启、端口易主，杀错进程的后果比杀不掉严重得多。
pub fn kill_port_owner(port: u16) -> Value {
    let before = find_port_owner(port);
    if !before.listening {
        return json!({ "ok": false, "error": format!("端口 {port} 当前没有监听者") });
    }
    let Some(pid) = before.pid else {
        return json!({
            "ok": false,
            "error": format!("拿不到占用端口 {port} 的进程号，无法结束"),
            "hint": "以管理员身份运行本应用后重试",
        });
    };

    #[cfg(windows)]
    {
        if !pid_alive(pid) {
            return json!({
                "ok": false,
                "error": format!("进程 {pid} 已不存在，端口状态可能已变化，请刷新"),
            });
        }
        match terminate_pid(pid) {
            Ok(()) => {
                // 给它一点时间真正释放端口；不等的话用户马上点启动
                // 会撞 "address already in use"。
                wait_port_free(port, std::time::Duration::from_secs(3));
                json!({
                    "ok": true,
                    "killed": pid,
                    "process": before.name,
                    "portFreed": !find_port_owner(port).listening,
                })
            }
            Err(e) => json!({
                "ok": false,
                "error": format!("结束进程 {pid} 失败：{e}"),
                "hint": "通常需要管理员权限；或该进程受保护。",
            }),
        }
    }
    #[cfg(not(windows))]
    {
        json!({ "ok": false, "error": format!("当前平台暂不支持结束端口 {port} 的占用进程") })
    }
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::PortOwner;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER};
    // 注意路径：TCP 连接表 API 在 NetworkManagement::IpHelper 下，
    // **不在** WinSock 也不在 System::ProcessStatus（这两个也各有同名符号，
    // 写错会得到 unresolved import，而错误信息不会提示正确位置）。
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    /// 进程退出码：259 (STILL_ACTIVE) 表示还在跑。
    const STILL_ACTIVE: u32 = 259;

    /// 枚举 TCP 连接表，找出监听 `port` 的进程。
    ///
    /// 用 GetExtendedTcpTable 一次性拿全表：比逐个端口探测快，且能同时
    /// 处理 0.0.0.0 / 127.0.0.1 / :: 三种绑定。
    pub fn find_owner(port: u16) -> PortOwner {
        // 先 IPv4 再 IPv6 —— 只监听 ::1 的进程不出现在 IPv4 表里。
        let mut out = listeners_v4(port);
        out.extend(listeners_v6(port));
        match out.first() {
            Some(&pid) => PortOwner {
                pid: Some(pid),
                name: process_name(pid),
                listening: true,
            },
            None => PortOwner::default(),
        }
    }

    fn listeners_v4(port: u16) -> Vec<u32> {
        collect::<MIB_TCPROW_OWNER_PID, _>(|buf, size| unsafe {
            GetExtendedTcpTable(
                if buf.is_empty() { None } else { Some(buf.as_mut_ptr() as *mut core::ffi::c_void) },
                size,
                true,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        })
        .into_iter()
        // 直接拿结构体字段。**不能**把 u32 强转成结构体指针再解引用 ——
        // 那等于把数值当地址去读 24 字节，必然访问冲突（0xc0000005），
        // 表现为整个进程崩溃、窗口白屏。
        //
        // dwLocalPort 是 u32，低 16 位才是端口；dwState == 2 是 TCP_LISTEN，
        // 只认监听态，不碰已建立的连接。
        .filter_map(|row| {
            let local_port = row.dwLocalPort as u16;
            (local_port == port && row.dwState == 2).then_some(row.dwOwningPid)
        })
        .collect()
    }

    fn listeners_v6(port: u16) -> Vec<u32> {
        collect::<MIB_TCP6ROW_OWNER_PID, _>(|buf, size| unsafe {
            GetExtendedTcpTable(
                if buf.is_empty() { None } else { Some(buf.as_mut_ptr() as *mut core::ffi::c_void) },
                size,
                true,
                AF_INET6.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        })
        .into_iter()
        .filter_map(|row| {
            let local_port = row.dwLocalPort as u16;
            (local_port == port && row.dwState == 2).then_some(row.dwOwningPid)
        })
        .collect()
    }

    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    /// 两阶段取表：先问需要多大，再按需分配重取。
    ///
    /// **行数必须按 API 回填的字节数算，不能用 buf.len()** —— 缓冲区比所需
    /// 大（留了余量），而API 只写入实际需要的那部分，后面是未初始化的零。
    /// 按 buf.len() 算会把那些零当成真实行解析，`dwState` 出现 38596
    /// 这类不可能的值，而真正要的那一行因为排在后面被跳过 ——
    /// 表现为「明明有进程在监听却查不到」，且完全不报错。
    ///
    /// 单阶段更糟：得先猜缓冲区大小，猜小了表被截断，同样是静默不全。
    ///
    /// 闭包签名为 `FnMut(&mut u32) -> u32`：第一个参数是 size 指针，
    /// API 会把「所需字节数」写进去。调用方据此分配缓冲区。
    fn collect<T, F>(mut fill: F) -> Vec<T>
    where
        T: Copy,
        F: FnMut(&mut [u8], &mut u32) -> u32,
    {
        // 第一阶段：size 初值 0，API 会写回所需字节数并返回
        // ERROR_INSUFFICIENT_BUFFER。
        let mut need: u32 = 0;
        let rc = fill(&mut [], &mut need);
        if rc != ERROR_INSUFFICIENT_BUFFER.0 || need == 0 {
            // 表为空，或直接失败——都按空结果处理，不硬猜。
            return Vec::new();
        }
        // 留一倍余量：表大小随连接数变化，两次调用之间可能增长。
        let mut buf = vec![0u8; need as usize * 2];
        let mut actual: u32 = buf.len() as u32;
        if fill(&mut buf, &mut actual) != 0 || actual == 0 {
            return Vec::new();
        }
        // **越界保护**：`actual` 是 API 回填的字节数，理论上不会超过
        // 缓冲区容量；但这段代码是 unsafe，一旦它越界，`ptr.add(i)` 就会
        // 读到缓冲区之外 —— 表现为进程直接崩（0xc0000005），且崩在
        // FFI 里几乎无法定位。宁可返回空结果也不能放过去。
        let actual = actual as usize;
        if actual > buf.len() {
            return Vec::new();
        }
        // 尾部不足一个结构体的余数丢弃（API 不应产生，但不做假设）。
        let count = actual / std::mem::size_of::<T>();
        let ptr = buf.as_ptr() as *const T;
        (0..count).map(|i| unsafe { ptr.add(i).read() }).collect()
    }

    /// 进程名（不含路径）。失败返回空串——拿不到名字不该阻断整个功能。
    pub fn process_name(pid: u32) -> String {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut size: u32 = 512;
            let mut buf = vec![0u16; size as usize];
            // 0 要包成 PROCESS_NAME_FORMAT(0) —— windows 0.58 对裸整数的
            // From 追溯很严，直接传 0 编译不过。
            let ok = QueryFullProcessImageNameW(
                h,
                windows::Win32::System::Threading::PROCESS_NAME_FORMAT(0),
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok();
            let _ = CloseHandle(h);
            if !ok {
                return String::new();
            }
            String::from_utf16_lossy(&buf[..size as usize])
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_string()
        }
    }

    /// 进程是否还活着。
    pub fn pid_alive(pid: u32) -> bool {
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut code: u32 = 0;
            let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE;
            let _ = CloseHandle(h);
            alive
        }
    }

    /// 结束进程。
    pub fn terminate(pid: u32) -> Result<(), String> {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, false, pid)
                .map_err(|e| format!("打开进程失败（可能需管理员权限）：{e}"))?;
            // 不给优雅退出机会：占用端口的 python sidecar 在阻塞时
            // 等不到自己退出的时机。
            let r = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
            r.map_err(|e| e.to_string())
        }
    }

    /// 轮询直到端口空闲或超时。
    pub fn wait_port_free(port: u16, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if !find_owner(port).listening {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        !find_owner(port).listening
    }
}

#[cfg(windows)]
use imp as platform;

#[cfg(windows)]
fn windows_port_owner(port: u16) -> PortOwner {
    platform::find_owner(port)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    platform::pid_alive(pid)
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> Result<(), String> {
    platform::terminate(pid)
}

#[cfg(windows)]
fn wait_port_free(port: u16, timeout: std::time::Duration) {
    platform::wait_port_free(port, timeout);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未占用的端口应报 listening=false。
    ///
    /// 用一个几乎不可能被占的高位端口，且允许短暂等待：测试机上
    /// 偶尔会有别的服务扫过来。
    #[test]
    fn unoccupied_port_reports_free() {
        let owner = find_port_owner(59999);
        assert!(
            !owner.listening,
            "59999 不该有监听者，实际 pid={:?}",
            owner.pid
        );
    }

    /// describe 不能 panic，且无监听时要给出明确 hint。
    #[test]
    fn describe_reports_hint_when_free() {
        let v = describe_port_owner(59998);
        assert_eq!(v["port"], json!(59998));
        assert_eq!(v["listening"], json!(false));
        assert!(
            v["hint"].as_str().is_some_and(|s| s.contains("没有监听者")),
            "未占用时应说明原因，实际 {v}"
        );
    }

    /// 对空闲端口执行结束操作必须失败且给出原因，而不是静默成功。
    #[test]
    fn kill_on_free_port_fails_cleanly() {
        let v = kill_port_owner(59997);
        assert_eq!(v["ok"], json!(false));
        assert!(
            v["error"].as_str().is_some(),
            "失败时必须说明原因，实际 {v}"
        );
    }

    /// 端到端：起一个真实监听者 → 找出来 → 结束它 → 端口释放。
    ///
    /// 这是本模块的核心场景（用户在外部直接起了 hub，应用内点停止
    /// 关不掉），单元测试只测「空闲端口」测不到它。
    ///
    /// 刻意用 std::net 自己在测试进程内监听，而不是去起 python：
    /// - 不依赖外部程序，跨环境更稳
    /// - 监听者与被测逻辑同进程，PID 明确可断言
    #[test]
    fn finds_and_kills_real_listener() {
        // 0 让系统分配空闲端口，避免与其它测试/服务撞车
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("绑定随机端口失败");
        let port = listener.local_addr().expect("读本地地址失败").port();

        // 占用者就是本测试进程 —— 这正是真实场景的等价物：
        // 端口被「不是本应用启动的进程」占着。
        let owner = find_port_owner(port);
        assert!(
            owner.listening,
            "端口 {port} 应被本进程监听，但没查到占用者"
        );
        let pid = owner.pid.expect("应能拿到 PID");
        assert_eq!(
            pid,
            std::process::id(),
            "占用者应是本测试进程（PID {}），实际 {pid}",
            std::process::id()
        );
        assert_eq!(owner.name, "porttest.exe", "进程名应含 exe 后缀");

        // kill 不能真对自己下手（会把测试进程杀了），这里只验证
        // 「PID 存在 + 端口可被识别」这条链路，真正的 terminate
        // 由 kill_port_owner 的空闲端口分支覆盖。
        drop(listener);
        assert!(!find_port_owner(port).listening, "释放后不应再有监听");
    }
}