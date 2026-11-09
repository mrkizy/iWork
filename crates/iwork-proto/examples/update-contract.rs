//! 把当前描述符里新增的消息、字段、枚举值扩充进契约：
//! `cargo run -p iwork-proto --example update-contract -- --additive`
//!
//! 只有 `--additive` 一种操作，没有重置（T01 方案 3.7）：契约里已有的条目只要被改了或删了，就拒绝写入、
//! 逐条列出差异、退出码 1。破坏性变更只能升主版本（包名改 iwork.v<N+1>，新建 proto/compat/v<N+1>/）。

use std::path::Path;
use std::process::ExitCode;
use std::{env, fs};

use iwork_proto::{FILE_DESCRIPTOR_SET, PROTOCOL_MAJOR, compat};
use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args != ["--additive"] {
        eprintln!("用法：{}", compat::UPDATE_COMMAND);
        return ExitCode::from(2);
    }
    let major_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../proto/compat/v{PROTOCOL_MAJOR}"));
    let path = major_dir.join("contract.binpb");
    let current = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET)
        .expect("FILE_DESCRIPTOR_SET 是构建脚本写的，必能解码");

    // 只有整个主版本目录还不存在（新主版本的第一次）才从空契约开始；目录在而契约文件不在，
    // 等于在同一主版本里重置契约，拒绝——从版本控制里恢复它。
    let contract = match fs::read(&path) {
        Ok(bytes) => match FileDescriptorSet::decode(bytes.as_slice()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("契约 {} 解码失败：{e}", path.display());
                return ExitCode::from(2);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !major_dir.exists() => {
            println!("主版本 v{PROTOCOL_MAJOR} 还没有契约目录，从空契约开始");
            FileDescriptorSet::default()
        }
        Err(e) => {
            eprintln!(
                "读契约 {} 失败：{e}。同一主版本内不能重建契约，请从版本控制恢复该文件",
                path.display()
            );
            return ExitCode::from(2);
        }
    };

    let updated = match compat::additive_update(&contract, &current) {
        Ok(updated) => updated,
        Err(diffs) => {
            eprintln!(
                "拒绝写入：契约里已有的条目与当前描述符不一致（{} 处）：",
                diffs.len()
            );
            for d in &diffs {
                eprintln!("  {d}");
            }
            return ExitCode::from(1);
        }
    };

    let added = match compat::check(&updated, &contract) {
        Ok(()) => Vec::new(),
        Err(diffs) => diffs,
    };
    if added.is_empty() {
        println!("契约已是最新，没有新增条目");
        return ExitCode::SUCCESS;
    }
    if let Err(e) =
        fs::create_dir_all(&major_dir).and_then(|()| fs::write(&path, updated.encode_to_vec()))
    {
        eprintln!("写契约 {} 失败：{e}", path.display());
        return ExitCode::from(2);
    }
    println!("契约 {} 新增 {} 个条目：", path.display(), added.len());
    for d in &added {
        if let compat::Difference::NotInContract { path } = d {
            println!("  + {path}");
        }
    }
    ExitCode::SUCCESS
}
