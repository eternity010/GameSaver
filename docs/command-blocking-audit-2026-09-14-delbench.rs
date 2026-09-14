// F2 量级实测：`discard_pending_game` 原先留在主线程上的两步各要多久。
//
//   1. fs::rename 到隔离目录 —— 保留在同步段的依据（必须是元数据操作，不能按文件数走）；
//   2. fs::remove_dir_all 隔离目录 —— 挪进后台线程的依据。
//
// 用 std 的同名函数，与命令里调用的完全是同一个；文件数取实测的最大游戏体量
// （傻乎乎主播蔚来 20,717 个文件 / 7,289.7 MB）。
//
// 注意：默认路径在**游戏所在卷**上（`E:\GameSaverGames\games\`）。这不是可选的洁癖 ——
// 同一段代码在 %TEMP%（C: 盘、刚写完的树）上 rename 要 933 ms，在游戏卷上只要 8 ms，
// 放错卷会得出"rename 也很慢"的反向结论。脚本结束时会清掉自己的测试目录。
//
// 编译运行：
//   rustc -O docs/command-blocking-audit-2026-09-14-delbench.rs -o delbench.exe
//   delbench.exe [文件数] [目标目录]
//
// 本机实测（20,000 文件 / 625 MB）：remove_dir_all 8,762 ms（438 µs/文件），
// 按 20,717 文件折算 ≈ 9 s；同一棵树 rename 8~19 ms。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const FILE_BYTES: usize = 32 * 1024;
const SUBDIRS: usize = 40;
const RENAME_SAMPLES: usize = 4;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let file_count = arguments
        .first()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20_000);
    let base = arguments
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("E:\\GameSaverGames\\games\\.delbench"));

    let root = base.join("body");
    let quarantine = base.join("body.discarding");
    let _ = fs::remove_dir_all(&base);
    let payload = vec![0x5au8; FILE_BYTES];

    print!(
        "构造测试树 {file_count} × {} KB @ {} … ",
        FILE_BYTES / 1024,
        base.display()
    );
    let build = Instant::now();
    for index in 0..file_count {
        let directory = root.join(format!("sub{:02}", index % SUBDIRS));
        if index < SUBDIRS {
            fs::create_dir_all(&directory).expect("create dir");
        }
        fs::write(directory.join(format!("file{index:05}.bin")), &payload).expect("write file");
    }
    println!("{:.2} s", build.elapsed().as_secs_f64());

    let mut renames = Vec::new();
    let mut from = root.clone();
    let mut to = quarantine.clone();
    for _ in 0..RENAME_SAMPLES {
        let start = Instant::now();
        fs::rename(&from, &to).expect("rename");
        renames.push(start.elapsed().as_secs_f64() * 1000.0);
        std::mem::swap(&mut from, &mut to);
    }

    let start = Instant::now();
    fs::remove_dir_all(&from).expect("remove");
    let remove = start.elapsed().as_secs_f64() * 1000.0;

    println!(
        "\n=== 结果（{file_count} 个文件，约 {} MB）===",
        file_count * FILE_BYTES / 1024 / 1024
    );
    for (index, value) in renames.iter().enumerate() {
        println!("fs::rename  #{:<2}      {value:>9.1} ms", index + 1);
    }
    let rename_best = renames.iter().cloned().fold(f64::MAX, f64::min);
    let per_file = remove / file_count as f64 * 1000.0;
    println!("fs::remove_dir_all   {remove:>9.1} ms  ({per_file:.1} µs/文件)");
    println!(
        "\n按最大游戏 20,717 个文件折算：remove ≈ {:.0} ms，rename（最好一次）≈ {:.0} ms",
        per_file * 20_717.0 / 1000.0,
        rename_best
    );

    cleanup(Path::new(&base));
}

fn cleanup(base: &Path) {
    let _ = fs::remove_dir_all(base);
}
