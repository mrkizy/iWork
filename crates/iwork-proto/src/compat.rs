//! 协议兼容契约：比对与单调扩充（T01 方案 3.7；规则全文见 `proto/compat/README.md`）。
//!
//! 契约是一个只增不减的 `FileDescriptorSet`（`proto/compat/v<N>/contract.binpb`）。比对时把描述符摊平成
//! 「字段 → 形状」「枚举值 → 数值」两张表再逐项比，所以文件怎么拆分、注释怎么写都不影响结果，
//! 只看会影响线格式与语义的东西：编号、类型、label、oneof 归属、枚举值。
//!
//! 测试（`tests/compat.rs`）与 `examples/update-contract.rs` 共用这里的函数，规则只写一处。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet};

/// 新增字段后要跑的命令；比对报「不在契约里」时原样给出。
pub const UPDATE_COMMAND: &str = "cargo run -p iwork-proto --example update-contract -- --additive";

/// 不进契约的文件（按文件名的最后一段匹配）与理由。
pub const EXCLUDED_FILES: &[(&str, &str)] = &[(
    "view.proto",
    "ViewPatch 只在客户端进程内由 Rust 侧传给网页层，不落盘、不跨进程，改结构没有数据风险（T01 方案 S8）",
)];

/// 依赖进来的标准类型（Timestamp 等）由 protobuf 官方保证兼容，不进契约。
const WELL_KNOWN_PACKAGE_PREFIX: &str = "google.protobuf";

/// 一条契约差异。`Display` 给出能直接读懂的一行说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    /// 契约里有、当前描述符没有（字段、枚举值、消息或枚举本身）。
    Missing { path: String },
    /// 同名条目的某一方面变了。
    Changed {
        path: String,
        aspect: Aspect,
        contract: String,
        current: String,
    },
    /// 当前描述符有、契约里没有：新增后忘了登记。
    NotInContract { path: String },
}

/// 字段或枚举值被比对的方面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aspect {
    Number,
    Type,
    Label,
    Oneof,
    Value,
}

impl fmt::Display for Aspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Aspect::Number => "编号",
            Aspect::Type => "类型",
            Aspect::Label => "label",
            Aspect::Oneof => "oneof 归属",
            Aspect::Value => "值",
        })
    }
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Difference::Missing { path } => write!(
                f,
                "{path} 在契约里，当前描述符没有：同一主版本内不许删，只能标 deprecated 留着；删除要升主版本"
            ),
            Difference::Changed {
                path,
                aspect,
                contract,
                current,
            } => write!(
                f,
                "{path} {aspect} {contract} → {current}（已进契约的条目不许改；破坏性变更要升主版本）"
            ),
            Difference::NotInContract { path } => {
                write!(
                    f,
                    "{path} 不在契约里：新增的条目要登记进契约，运行 `{UPDATE_COMMAND}`"
                )
            }
        }
    }
}

/// 双向比对：契约里的每个条目在当前描述符里都一致地存在，当前的每个条目也都已登记进契约。
///
/// 返回全部差异（不在第一条就停），方便一次改完。
pub fn check(
    current: &FileDescriptorSet,
    contract: &FileDescriptorSet,
) -> Result<(), Vec<Difference>> {
    let current = Catalog::of(current);
    let contract = Catalog::of(contract);
    let mut diffs = contract.differences_against(&current);
    diffs.extend(current.unregistered_in(&contract));
    if diffs.is_empty() { Ok(()) } else { Err(diffs) }
}

/// 把当前描述符里新增的条目扩充进契约，返回新契约。
///
/// 契约里已有的条目只要在当前描述符里缺失或不一致，就拒绝并逐条列出——扩充只能加，不能改。
/// 已有条目全部一致时，新契约就是当前描述符本身（去掉不进契约的文件与注释位置），
/// 因为它恰好是「旧契约 ∪ 新增条目」。
pub fn additive_update(
    contract: &FileDescriptorSet,
    current: &FileDescriptorSet,
) -> Result<FileDescriptorSet, Vec<Difference>> {
    let diffs = Catalog::of(contract).differences_against(&Catalog::of(current));
    if diffs.is_empty() {
        Ok(contract_view(current))
    } else {
        Err(diffs)
    }
}

/// 描述符里会写进契约的部分：去掉排除的文件、标准类型与注释位置信息。
pub fn contract_view(fds: &FileDescriptorSet) -> FileDescriptorSet {
    let file = fds
        .file
        .iter()
        .filter(|f| is_contract_file(f.name(), f.package()))
        .map(|f| {
            let mut f = f.clone();
            f.source_code_info = None;
            f
        })
        .collect();
    FileDescriptorSet { file }
}

fn is_contract_file(name: &str, package: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    !package.starts_with(WELL_KNOWN_PACKAGE_PREFIX)
        && !EXCLUDED_FILES.iter().any(|(excluded, _)| *excluded == base)
}

/// 字段在线格式与语义上要保持不变的全部方面。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldShape {
    number: i32,
    /// 标量写类型名，消息与枚举写全名，换了引用的类型也算变。
    type_desc: String,
    /// proto3 的 `optional` 改变有无存在性语义，算 label 的一部分。
    label: String,
    /// 真 oneof 的名字；proto3 `optional` 生成的合成 oneof 不算（已体现在 label 里）。
    oneof: Option<String>,
}

/// 摊平后的描述符：消息与枚举的全名、`消息.字段` → 形状、`枚举.值` → 数值。
#[derive(Default)]
struct Catalog {
    messages: BTreeSet<String>,
    enums: BTreeSet<String>,
    fields: BTreeMap<String, FieldShape>,
    enum_values: BTreeMap<String, i32>,
}

impl Catalog {
    fn of(fds: &FileDescriptorSet) -> Self {
        let mut catalog = Catalog::default();
        for file in fds
            .file
            .iter()
            .filter(|f| is_contract_file(f.name(), f.package()))
        {
            let package = file.package();
            for message in &file.message_type {
                catalog.add_message(package, message);
            }
            for enumeration in &file.enum_type {
                catalog.add_enum(package, enumeration);
            }
        }
        catalog
    }

    fn add_message(&mut self, scope: &str, message: &DescriptorProto) {
        let name = qualify(scope, message.name());
        for field in &message.field {
            let shape = field_shape(message, field);
            self.fields
                .insert(format!("{name}.{}", field.name()), shape);
        }
        for nested in &message.nested_type {
            self.add_message(&name, nested);
        }
        for enumeration in &message.enum_type {
            self.add_enum(&name, enumeration);
        }
        self.messages.insert(name);
    }

    fn add_enum(&mut self, scope: &str, enumeration: &EnumDescriptorProto) {
        let name = qualify(scope, enumeration.name());
        for value in &enumeration.value {
            self.enum_values
                .insert(format!("{name}.{}", value.name()), value.number());
        }
        self.enums.insert(name);
    }

    /// 以 `self` 为契约，找出 `current` 里缺失或不一致的条目。
    fn differences_against(&self, current: &Catalog) -> Vec<Difference> {
        let mut diffs = Vec::new();
        for name in self.messages.difference(&current.messages) {
            diffs.push(Difference::Missing { path: name.clone() });
        }
        for name in self.enums.difference(&current.enums) {
            diffs.push(Difference::Missing { path: name.clone() });
        }
        for (path, want) in &self.fields {
            match current.fields.get(path) {
                None => diffs.push(Difference::Missing { path: path.clone() }),
                Some(have) => field_changes(path, want, have, &mut diffs),
            }
        }
        for (path, want) in &self.enum_values {
            match current.enum_values.get(path) {
                None => diffs.push(Difference::Missing { path: path.clone() }),
                Some(have) if have != want => diffs.push(Difference::Changed {
                    path: path.clone(),
                    aspect: Aspect::Value,
                    contract: want.to_string(),
                    current: have.to_string(),
                }),
                Some(_) => {}
            }
        }
        diffs
    }

    /// 以 `self` 为当前描述符，找出还没登记进 `contract` 的条目。
    fn unregistered_in(&self, contract: &Catalog) -> Vec<Difference> {
        let names = self
            .messages
            .difference(&contract.messages)
            .chain(self.enums.difference(&contract.enums))
            .chain(
                self.fields
                    .keys()
                    .filter(|k| !contract.fields.contains_key(*k)),
            )
            .chain(
                self.enum_values
                    .keys()
                    .filter(|k| !contract.enum_values.contains_key(*k)),
            );
        names
            .map(|path| Difference::NotInContract { path: path.clone() })
            .collect()
    }
}

fn field_changes(path: &str, want: &FieldShape, have: &FieldShape, diffs: &mut Vec<Difference>) {
    let mut push = |aspect, contract: String, current: String| {
        if contract != current {
            diffs.push(Difference::Changed {
                path: path.to_owned(),
                aspect,
                contract,
                current,
            });
        }
    };
    push(
        Aspect::Number,
        want.number.to_string(),
        have.number.to_string(),
    );
    push(Aspect::Type, want.type_desc.clone(), have.type_desc.clone());
    push(Aspect::Label, want.label.clone(), have.label.clone());
    push(
        Aspect::Oneof,
        describe_oneof(&want.oneof),
        describe_oneof(&have.oneof),
    );
}

fn describe_oneof(oneof: &Option<String>) -> String {
    oneof
        .as_deref()
        .map_or_else(|| "（无）".to_owned(), str::to_owned)
}

fn field_shape(message: &DescriptorProto, field: &FieldDescriptorProto) -> FieldShape {
    let type_desc = match field.r#type() {
        Type::Message | Type::Enum | Type::Group => {
            field.type_name().trim_start_matches('.').to_owned()
        }
        scalar => scalar
            .as_str_name()
            .trim_start_matches("TYPE_")
            .to_lowercase(),
    };
    let label = match (field.label(), field.proto3_optional()) {
        (Label::Repeated, _) => "repeated",
        (Label::Required, _) => "required",
        (Label::Optional, true) => "optional",
        (Label::Optional, false) => "singular",
    }
    .to_owned();
    let oneof = if field.proto3_optional() {
        None
    } else {
        field
            .oneof_index
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| message.oneof_decl.get(i))
            .map(|o| o.name().to_owned())
    };
    FieldShape {
        number: field.number(),
        type_desc,
        label,
        oneof,
    }
}

fn qualify(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}.{name}")
    }
}

#[cfg(test)]
mod tests {
    //! 契约比对的「两步测试」（T01 方案 3.7）：合成描述符，不依赖仓库文件。

    use prost_types::{FieldDescriptorProto, FileDescriptorProto, OneofDescriptorProto};

    use super::*;

    fn int_field(name: &str, number: i32) -> FieldDescriptorProto {
        FieldDescriptorProto {
            name: Some(name.to_owned()),
            number: Some(number),
            label: Some(Label::Optional.into()),
            r#type: Some(Type::Int32.into()),
            ..Default::default()
        }
    }

    /// 一个文件、包 `test`、一个消息 `M`，字段按 (名字, 编号) 给出。
    fn descriptor(fields: &[(&str, i32)]) -> FileDescriptorSet {
        descriptor_with(
            fields.iter().map(|(n, k)| int_field(n, *k)).collect(),
            vec![],
        )
    }

    fn descriptor_with(
        field: Vec<FieldDescriptorProto>,
        oneof_decl: Vec<OneofDescriptorProto>,
    ) -> FileDescriptorSet {
        FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some("test/m.proto".to_owned()),
                package: Some("test".to_owned()),
                syntax: Some("proto3".to_owned()),
                message_type: vec![DescriptorProto {
                    name: Some("M".to_owned()),
                    field,
                    oneof_decl,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn rendered(diffs: &[Difference]) -> Vec<String> {
        diffs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn two_step_additive_then_renumber_is_caught() {
        let c0 = descriptor(&[("a", 1)]);

        // 第 1 步：新增 b=2，扩充进契约后通过。
        let d1 = descriptor(&[("a", 1), ("b", 2)]);
        let c1 = additive_update(&c0, &d1).expect("只新增字段，扩充必须成功");
        assert!(
            Catalog::of(&c1).fields.contains_key("test.M.b"),
            "扩充后契约要含 b"
        );
        assert_eq!(check(&d1, &c1), Ok(()));

        // 第 2 步：把刚进契约的 b 改号，比对与扩充都必须拒绝，并点名。
        let d2 = descriptor(&[("a", 1), ("b", 3)]);
        let diffs = check(&d2, &c1).expect_err("改号必须被发现");
        assert_eq!(
            diffs,
            vec![Difference::Changed {
                path: "test.M.b".to_owned(),
                aspect: Aspect::Number,
                contract: "2".to_owned(),
                current: "3".to_owned(),
            }]
        );
        assert!(
            rendered(&diffs)[0].contains("test.M.b 编号 2 → 3"),
            "{:?}",
            rendered(&diffs)
        );
        assert_eq!(
            additive_update(&c1, &d2).expect_err("改号不能被扩充吸收"),
            diffs
        );
    }

    #[test]
    fn new_field_must_be_registered_then_passes() {
        let c1 = descriptor(&[("a", 1), ("b", 2)]);
        let d3 = descriptor(&[("a", 1), ("b", 2), ("c", 3)]);

        let diffs = check(&d3, &c1).expect_err("新字段没登记必须报错");
        assert_eq!(
            diffs,
            vec![Difference::NotInContract {
                path: "test.M.c".to_owned()
            }]
        );
        assert!(
            rendered(&diffs)[0].contains(UPDATE_COMMAND),
            "错误信息要给出命令"
        );

        let c2 = additive_update(&c1, &d3).expect("只新增字段，扩充必须成功");
        assert_eq!(check(&d3, &c2), Ok(()));
    }

    #[test]
    fn removed_field_is_missing_and_cannot_be_absorbed() {
        let contract = descriptor(&[("a", 1), ("b", 2)]);
        let current = descriptor(&[("a", 1)]);
        let expected = vec![Difference::Missing {
            path: "test.M.b".to_owned(),
        }];
        assert_eq!(check(&current, &contract), Err(expected.clone()));
        assert_eq!(additive_update(&contract, &current), Err(expected));
    }

    #[test]
    fn type_label_and_oneof_changes_are_each_reported() {
        let contract = descriptor(&[("a", 1)]);

        let mut retyped = int_field("a", 1);
        retyped.r#type = Some(Type::String.into());
        let diffs = check(&descriptor_with(vec![retyped], vec![]), &contract).unwrap_err();
        assert_eq!(
            rendered(&diffs),
            vec![format!("test.M.a 类型 int32 → string{}", tail())]
        );

        let mut repeated = int_field("a", 1);
        repeated.label = Some(Label::Repeated.into());
        let diffs = check(&descriptor_with(vec![repeated], vec![]), &contract).unwrap_err();
        assert_eq!(
            rendered(&diffs),
            vec![format!("test.M.a label singular → repeated{}", tail())]
        );

        let mut in_oneof = int_field("a", 1);
        in_oneof.oneof_index = Some(0);
        let oneof = OneofDescriptorProto {
            name: Some("kind".to_owned()),
            ..Default::default()
        };
        let diffs = check(&descriptor_with(vec![in_oneof], vec![oneof]), &contract).unwrap_err();
        assert_eq!(
            rendered(&diffs),
            vec![format!("test.M.a oneof 归属 （无） → kind{}", tail())]
        );
    }

    #[test]
    fn enum_value_renumber_and_addition() {
        let with_enum = |values: &[(&str, i32)]| {
            let mut fds = descriptor(&[]);
            fds.file[0].enum_type.push(EnumDescriptorProto {
                name: Some("E".to_owned()),
                value: values
                    .iter()
                    .map(|(n, v)| prost_types::EnumValueDescriptorProto {
                        name: Some((*n).to_owned()),
                        number: Some(*v),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
            fds
        };
        let contract = with_enum(&[("E_ZERO", 0), ("E_ONE", 1)]);
        assert_eq!(
            check(&with_enum(&[("E_ZERO", 0), ("E_ONE", 2)]), &contract),
            Err(vec![Difference::Changed {
                path: "test.E.E_ONE".to_owned(),
                aspect: Aspect::Value,
                contract: "1".to_owned(),
                current: "2".to_owned(),
            }])
        );
        assert_eq!(
            check(
                &with_enum(&[("E_ZERO", 0), ("E_ONE", 1), ("E_TWO", 2)]),
                &contract
            ),
            Err(vec![Difference::NotInContract {
                path: "test.E.E_TWO".to_owned()
            }])
        );
    }

    #[test]
    fn excluded_file_never_enters_contract() {
        let mut fds = descriptor(&[("a", 1)]);
        fds.file[0].name = Some("test/view.proto".to_owned());
        assert!(contract_view(&fds).file.is_empty());
        // 契约是空的，排除的文件也不会被报「不在契约里」。
        assert_eq!(check(&fds, &FileDescriptorSet::default()), Ok(()));
        // 对照：同样内容换个文件名就要登记。
        assert!(check(&descriptor(&[("a", 1)]), &FileDescriptorSet::default()).is_err());
    }

    fn tail() -> &'static str {
        "（已进契约的条目不许改；破坏性变更要升主版本）"
    }
}
