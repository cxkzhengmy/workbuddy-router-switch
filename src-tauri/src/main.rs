// 隐藏控制台窗口。原来只对 release 生效（Tauri 模板的默认写法），
// 但 debug 构建的 PE 子系统是 WINDOWS_CUI——双击 target/debug 下的 exe
// 会弹一个黑色控制台窗口（其 conhost 与该 exe 同生共死，关掉窗口会把
// 整个应用一起带走）。debug 与 release 都应无窗口，所以不加条件。
//
// 代价：控制台被剥离后 `eprintln!` 失去出口（更新、签到、单实例等处会用到）。
// 要看这些输出请用 `cargo run`：那样进程借当前终端的 stdio，
// 与 PE 子系统无关，输出照样能看见。
#![windows_subsystem = "windows"]

fn main() {
    wb_switch_rust_lib::run()
}
