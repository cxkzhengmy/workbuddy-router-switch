//! 量化 Token 统计扫描耗时，定位瓶颈在哪一段。
//!
//! 跑法：cargo run -p wb-switch-core --example token_stats_bench --release

use std::time::Instant;

fn main() {
    // 1) 纯 IO 上限：顺序读一遍所有 jsonl 要多久
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let root = std::path::Path::new(&home).join(".workbuddy").join("projects");
    let mut files = Vec::new();
    walk(&root, &mut files);

    let t = Instant::now();
    let mut bytes = 0u64;
    for f in &files {
        if let Ok(data) = std::fs::read(f) {
            bytes += data.len() as u64;
        }
    }
    let io_secs = t.elapsed().as_secs_f64();
    println!(
        "  纯读：{} 个文件，{:.0} MB，{:.2}s（{:.0} MB/s）",
        files.len(),
        bytes as f64 / 1_048_576.0,
        io_secs,
        bytes as f64 / 1_048_576.0 / io_secs.max(0.001)
    );

    // 1.5) 指纹计算各段耗时
    let t = Instant::now();
    let roots = wb_switch_core::modules::token_stats::scan_roots();
    println!("  scan_roots: {} 个根，{:.1}ms", roots.len(), t.elapsed().as_secs_f64() * 1000.0);
    for r in &roots {
        let t = Instant::now();
        let mut found = Vec::new();
        collect_jsonl(r, &mut found);
        println!(
            "    {:<58} jsonl={:4} {:.0}ms",
            r.display().to_string().chars().take(58).collect::<String>(),
            found.len(),
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    let t = Instant::now();
    let fp = wb_switch_core::modules::token_stats::data_fingerprint();
    println!("  data_fingerprint 总计: {:.0}ms (fp={fp})", t.elapsed().as_secs_f64() * 1000.0);

    // 2) 首次（冷）与第二次（命中缓存）
    let t = Instant::now();
    let v = wb_switch_core::modules::token_stats::get_statistics(None);
    let cold = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let v2 = wb_switch_core::modules::token_stats::get_statistics(None);
    let warm = t.elapsed().as_secs_f64();
    println!("  首次扫描：{cold:.2}s");
    println!("  再次加载：{warm:.3}s（缓存命中）");
    println!("  加速：{:.0}x", cold / warm.max(0.001));
    println!("  两次结果一致：{}", *v == *v2);
    let total = cold;
    if let Some(sources) = v["sources"].as_array() {
        for s in sources {
            let name = s["source"].as_str().unwrap_or("?");
            println!(
                "    {name:16} files={:4} sessions={:4} tokens={:12} errors={}",
                s["filesScanned"].as_u64().unwrap_or(0),
                s["sessions"].as_array().map(Vec::len).unwrap_or(0),
                s["summary"]["totalTokens"].as_u64().unwrap_or(0),
                s["parseErrors"].as_u64().unwrap_or(0),
            );
        }
    }
    println!(
        "\n  IO 占统计总耗时的比例：{:.0}%",
        io_secs / total.max(0.001) * 100.0
    );

    // 3) 缓存失效验证：改一个数据文件的时间戳，指纹必须变化
    let fp_before = wb_switch_core::modules::token_stats::data_fingerprint();
    if let Some(target) = files.first() {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(target) {
            let now = std::time::SystemTime::now();
            let _ = f.set_modified(now);
            drop(f);
            let fp_after = wb_switch_core::modules::token_stats::data_fingerprint();
            println!(
                "\n  改动一个文件后指纹变化：{}（{} → {}）",
                fp_before != fp_after,
                fp_before,
                fp_after
            );
            let t = Instant::now();
            let _ = wb_switch_core::modules::token_stats::get_statistics(None);
            println!("  改动后重新扫描：{:.2}s（应重新计算，不走缓存）", t.elapsed().as_secs_f64());
        }
    }
}

/// 与生产代码 `files()` 保持一致的实现（含 file_type 优化）。
/// 早先这里用 `p.is_dir()`，实测会把 IDE 根算成 874ms —— 那是旧写法的成本，
/// 不是生产代码的成本，容易误读。
fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            if p.file_name().and_then(|n| n.to_str()) != Some("subagents") {
                collect_jsonl(&p, out);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(p);
        }
    }
}
