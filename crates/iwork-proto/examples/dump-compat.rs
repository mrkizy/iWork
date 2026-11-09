//! 生成金样本：`cargo run -p iwork-proto --example dump-compat -- proto/compat/v1/samples`
//!
//! 按 `tests/support` 的登记表，为每个构造函数写出 `<短消息名>.<n>.{binpb,txtpb,json}`。
//! 样本只增不改：目标文件已存在且内容不同就拒绝（退出码 1），一个文件也不写；内容相同的跳过
//! （`.txtpb`、`.json` 按含义比，见 `support::same_content`）。
//! 想保护新字段的取值，就加一个新构造函数生成新编号的样本，旧样本不动。

#[path = "../tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [dir] = args.as_slice() else {
        eprintln!(
            "用法：cargo run -p iwork-proto --example dump-compat -- <样本目录，如 proto/compat/v1/samples>"
        );
        return ExitCode::from(2);
    };
    let dir = Path::new(dir);
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("建目录 {} 失败：{e}", dir.display());
        return ExitCode::from(2);
    }

    let pool = support::pool();
    let mut to_write: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut refused: Vec<PathBuf> = Vec::new();
    let mut unchanged = 0usize;
    for sample_type in support::SAMPLE_TYPES {
        let desc = support::descriptor(&pool, sample_type.name);
        for (i, build) in sample_type.builders.iter().enumerate() {
            let files = support::render(desc.clone(), build());
            let paths = support::sample_paths(dir, sample_type.name, i + 1);
            let contents = [
                files.binpb,
                files.txtpb.into_bytes(),
                files.json.into_bytes(),
            ];
            for (path, content) in paths.into_iter().zip(contents) {
                match fs::read(&path) {
                    Ok(existing) if support::same_content(&desc, &path, &existing, &content) => {
                        unchanged += 1
                    }
                    Ok(_) => refused.push(path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        to_write.push((path, content))
                    }
                    Err(e) => {
                        eprintln!("读 {} 失败：{e}", path.display());
                        return ExitCode::from(2);
                    }
                }
            }
        }
    }

    if !refused.is_empty() {
        eprintln!(
            "拒绝写入：以下样本已存在且内容不同。样本只增不改——别改旧构造函数，加一个新的生成新编号的样本："
        );
        for path in &refused {
            eprintln!("  {}", path.display());
        }
        return ExitCode::from(1);
    }
    for (path, content) in &to_write {
        if let Err(e) = fs::write(path, content) {
            eprintln!("写 {} 失败：{e}", path.display());
            return ExitCode::from(2);
        }
        println!("新增 {}", path.display());
    }
    println!(
        "新增 {} 个文件，{} 个已存在且相同",
        to_write.len(),
        unchanged
    );
    ExitCode::SUCCESS
}
