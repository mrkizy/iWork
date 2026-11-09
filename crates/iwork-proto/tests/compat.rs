//! 协议兼容的 CI 检查（T01 方案 3.7；规则全文见 `proto/compat/README.md`）：
//! ① 当前描述符与契约双向比对；② 契约目录的主版本号 == `PROTOCOL_MAJOR`；③ 金样本用生成的类型解码。
//! 契约比对函数本身的「两步测试」在 `src/compat.rs` 的单元测试里（合成描述符，不依赖仓库文件）。

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use iwork_proto::{FILE_DESCRIPTOR_SET, PROTOCOL_MAJOR, compat};
use prost::Message;
use prost_reflect::{DynamicMessage, ReflectMessage};
use prost_types::FileDescriptorSet;

fn contract() -> FileDescriptorSet {
    let path = support::major_dir().join("contract.binpb");
    let bytes = fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "读契约 {} 失败：{e}。新主版本第一次建契约运行 `{}`",
            path.display(),
            compat::UPDATE_COMMAND
        )
    });
    FileDescriptorSet::decode(bytes.as_slice()).expect("契约必须能解码")
}

fn current() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("FILE_DESCRIPTOR_SET 必须能解码")
}

#[test]
fn contract_matches_current_descriptor_both_ways() {
    let contract = contract();
    // 对照：契约不是空的，否则「双向一致」可能只是两个空集相等。
    assert!(
        contract.file.iter().any(|f| !f.message_type.is_empty()),
        "契约里没有任何消息"
    );
    if let Err(diffs) = compat::check(&current(), &contract) {
        let lines: Vec<String> = diffs.iter().map(|d| format!("  {d}")).collect();
        panic!(
            "协议与兼容契约不一致（{} 处）：\n{}",
            diffs.len(),
            lines.join("\n")
        );
    }
}

#[test]
fn contract_directory_major_equals_protocol_major() {
    let majors: BTreeSet<u32> = fs::read_dir(support::compat_root())
        .expect("proto/compat/ 必须存在")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('v')?.parse().ok())
        .collect();
    // 改了包名却没建新契约目录，或建了超前的目录，都在这里红。
    assert_eq!(
        majors.last().copied(),
        Some(PROTOCOL_MAJOR),
        "proto/compat/ 下最高的 v<N> 必须等于包名里的主版本，实际目录：{majors:?}"
    );
    assert!(support::major_dir().join("contract.binpb").is_file());
}

/// `samples/` 下全部 `.binpb`，按文件名排序。
fn sample_files() -> Vec<PathBuf> {
    let dir = support::major_dir().join("samples");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读样本目录 {} 失败：{e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "binpb"))
        .collect();
    files.sort();
    files
}

/// `Approval.1.binpb` → ("Approval", 1)。
fn parse_sample_name(path: &Path) -> (String, usize) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("样本文件名必须是 UTF-8");
    let (name, index) = stem
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("样本文件名 {stem} 不是 <消息>.<n> 的形式"));
    let index = index
        .parse()
        .unwrap_or_else(|_| panic!("样本文件名 {stem} 的编号不是数字"));
    (name.to_owned(), index)
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("读 {} 失败：{e}", path.display()))
}

#[test]
fn samples_decode_with_generated_types() {
    let files = sample_files();
    assert!(
        !files.is_empty(),
        "没有任何金样本：运行 dump-compat 生成，空集不能算通过"
    );

    let pool = support::pool();
    for bin_path in &files {
        let (name, _) = parse_sample_name(bin_path);
        let sample_type = support::SAMPLE_TYPES
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "{} 的类型 {name} 不在 tests/support 的登记表里",
                    bin_path.display()
                )
            });
        let desc = support::descriptor(&pool, &name);

        // 二进制 → 生成的 Rust 类型 → 再编码：生成类型不认识的字段在这一步丢失。
        let original = fs::read(bin_path).expect("读样本失败");
        let reencoded = (sample_type.reencode)(&original)
            .unwrap_or_else(|e| panic!("{} 用生成的类型解码失败：{e}", bin_path.display()));
        let decoded =
            DynamicMessage::decode(desc.clone(), reencoded.as_slice()).expect("重编码结果必能解码");

        // 与人读的 .txtpb 比（按当前描述符解析后比较消息本身，与排版无关）。
        let txt_path = bin_path.with_extension("txtpb");
        let expected = DynamicMessage::parse_text_format(desc.clone(), &read_text(&txt_path))
            .unwrap_or_else(|e| panic!("{} 解析失败：{e}", txt_path.display()));
        // 不用 assert_eq：DynamicMessage 的 Debug 会带上整个描述符池，失败输出没法读。
        assert!(
            decoded == expected,
            "{} 解码结果与 .txtpb 不一致\n解码得到：\n{}\n期望（.txtpb）：\n{}",
            bin_path.display(),
            decoded.to_text_format(),
            expected.to_text_format()
        );

        // 与 .json 比：TS 侧拿同一份 .json 对 protobuf-es 的 toJson，所以 Rust 侧的 JSON 映射也要逐值一致。
        let json_path = bin_path.with_extension("json");
        let expected_json: serde_json::Value = serde_json::from_str(&read_text(&json_path))
            .unwrap_or_else(|e| panic!("{} 不是合法 JSON：{e}", json_path.display()));
        let actual_json = serde_json::to_value(&decoded).expect("写 JSON 失败");
        assert_eq!(
            actual_json,
            expected_json,
            "{} 的 JSON 与 .json 不一致",
            bin_path.display()
        );
    }
}

#[test]
fn registered_samples_exist_and_never_change() {
    let dir = support::major_dir().join("samples");
    let mut count = 0usize;
    for sample_type in support::SAMPLE_TYPES {
        for (i, build) in sample_type.builders.iter().enumerate() {
            let [bin_path, _, _] = support::sample_paths(&dir, sample_type.name, i + 1);
            let on_disk = fs::read(&bin_path).unwrap_or_else(|e| {
                panic!(
                    "登记了样本却没有文件 {}（{e}）：运行 cargo run -p iwork-proto --example dump-compat -- proto/compat/v{PROTOCOL_MAJOR}/samples",
                    bin_path.display()
                )
            });
            // 不用 assert_eq：两段字节的 Debug 输出很长，又看不出哪个字段变了。
            assert!(
                build() == on_disk,
                "{} 的构造函数输出与磁盘样本不同：样本只增不改，构造函数改回去并另加一个新样本；\
                 若是字段改了号，那是破坏性变更",
                bin_path.display()
            );
            count += 1;
        }
    }
    assert!(count > 0, "登记表是空的");
}

#[test]
fn every_domain_event_variant_has_a_sample() {
    let pool = support::pool();
    let desc = support::descriptor(&pool, "DomainEvent");
    let oneof = desc.oneofs().next().expect("DomainEvent 必须有 oneof");
    let all: BTreeSet<String> = oneof.fields().map(|f| f.name().to_owned()).collect();

    let sampled: BTreeSet<String> = sample_files()
        .iter()
        .filter(|p| parse_sample_name(p).0 == "DomainEvent")
        .flat_map(|p| {
            let message =
                DynamicMessage::decode(desc.clone(), fs::read(p).expect("读样本失败").as_slice())
                    .expect("样本必能解码");
            message
                .descriptor()
                .fields()
                .filter(|f| message.has_field(f))
                .map(|f| f.name().to_owned())
                .collect::<Vec<_>>()
        })
        .collect();

    let missing: Vec<&String> = all.difference(&sampled).collect();
    assert!(
        missing.is_empty(),
        "这些事件还没有金样本（在 tests/support 里加构造函数）：{missing:?}"
    );
}
