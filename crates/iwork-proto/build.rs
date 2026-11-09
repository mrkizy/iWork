//! 编译 `proto/iwork/` 下的全部 `.proto`：
//! - protox 编译（不需要系统 protoc，三平台 CI 一致；ADR-0004），prost-build 生成 Rust 类型；
//! - 把 `FileDescriptorSet` 写进 `OUT_DIR`，供兼容测试与运行时反射使用；
//! - 从包名 `iwork.v<N>` 解析协议主版本，生成 `PROTOCOL_MAJOR`——主版本的真值只有包名一处。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use prost::Message;
use prost_types::FileDescriptorSet;

/// 协议包名的前缀；包名必须是 `iwork.v<N>`。
const PACKAGE_PREFIX: &str = "iwork.v";

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo 总会设置 CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo 总会设置 OUT_DIR"));
    let proto_root = manifest_dir.join("../../proto");
    let iwork_dir = proto_root.join("iwork");
    println!("cargo:rerun-if-changed={}", iwork_dir.display());

    let files = proto_files(&iwork_dir);
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
    }

    let mut compiler =
        protox::Compiler::new([&proto_root]).unwrap_or_else(|e| panic!("protox 初始化失败：{e}"));
    compiler.include_imports(true).include_source_info(true);
    compiler
        .open_files(&files)
        .unwrap_or_else(|e| panic!("编译 proto 失败：{e:?}"));
    let fds = compiler.file_descriptor_set();

    let package = protocol_package(&fds);
    let major = parse_major(&package);

    // 运行时用的描述符不需要注释位置信息，去掉以减小产物体积；生成代码时保留，让文档注释进 Rust。
    let mut runtime_fds = fds.clone();
    for file in &mut runtime_fds.file {
        file.source_code_info = None;
    }
    fs::write(
        out_dir.join("file_descriptor_set.binpb"),
        runtime_fds.encode_to_vec(),
    )
    .expect("写 file_descriptor_set.binpb 失败");

    prost_build::Config::new()
        // 事件推送是热路径，bytes 字段零拷贝。
        .bytes(["."])
        // map 用 BTreeMap：HashMap 的编码顺序每次运行都不同，同一条消息编出的字节就不确定，
        // 金样本「只增不改」的逐字节比对与 dump-compat 都会随机失败。
        .btree_map(["."])
        // 生成 prost::Name，金样本按类型名对应描述符。
        .enable_type_names()
        .compile_fds(fds)
        .unwrap_or_else(|e| panic!("prost-build 生成失败：{e}"));

    let generated = out_dir.join(format!("{package}.rs"));
    let protocol_rs = format!(
        "/// 协议主版本，从 proto 包名 `{package}` 解析得出（构建时生成，不手抄）。\n\
         pub const PROTOCOL_MAJOR: u32 = {major};\n\
         /// 当前协议的 proto 包名。\n\
         pub const PROTO_PACKAGE: &str = \"{package}\";\n\
         /// 生成的 Protobuf 类型。路径不带版本号：升主版本只改 `.proto` 的包名。\n\
         #[allow(clippy::all, clippy::pedantic, missing_docs, reason = \"prost 生成代码，不手改\")]\n\
         pub mod pb {{\n    include!({generated:?});\n}}\n",
    );
    fs::write(out_dir.join("protocol.rs"), protocol_rs).expect("写 protocol.rs 失败");
}

/// `proto/iwork/` 下全部 `.proto`，排序后返回，保证生成结果与目录遍历顺序无关。
fn proto_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort();
    assert!(!out.is_empty(), "{} 下没有 .proto 文件", dir.display());
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("读目录 {} 失败：{e}", dir.display()));
    for entry in entries {
        let path = entry.expect("读目录项失败").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            out.push(path);
        }
    }
}

/// 本仓库 `.proto`（文件名以 `iwork/` 开头）的唯一包名；出现第二个包名即报错。
fn protocol_package(fds: &FileDescriptorSet) -> String {
    let mut packages: Vec<&str> = fds
        .file
        .iter()
        .filter(|f| f.name().starts_with("iwork/"))
        .map(|f| f.package())
        .collect();
    packages.sort_unstable();
    packages.dedup();
    match packages.as_slice() {
        [one] => (*one).to_owned(),
        other => panic!("proto/iwork/ 下必须只有一个包名，实际：{other:?}"),
    }
}

fn parse_major(package: &str) -> u32 {
    package
        .strip_prefix(PACKAGE_PREFIX)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("包名 {package:?} 不是 {PACKAGE_PREFIX}<N> 的形式"))
}
