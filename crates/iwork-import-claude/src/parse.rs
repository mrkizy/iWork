//! 选择性解析（T10f 方案 A3.1「被跳过的字段怎么跳」）。
//!
//! 只认得要取的那几个键：MCP 条目的名字、`type`、`env` / `headers` 的键名，插件登记文件里的插件键与第一条安装记录的
//! `installPath`。别的键（`command`、`args`、`url`、各种值、别的顶层键）都交给 `IgnoredAny`，不变成任何 Rust 值。
//! 原文只在这里的私有类型里短暂出现：名字一读到就经 [`Name::from_source`] 变成出口类型，`type` 一读到就和固定的表比成
//! [`Transport`]。这里的类型都不实现 `Debug`。解析失败只交出行、列：`serde_json` 的错误文字会把看到的值写进去，不往外传。

use std::fmt;
use std::path::PathBuf;

use iwork_ports::local_import::{Name, Transport};
use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// 解析失败的位置。
pub(crate) struct ParseError {
    pub line: usize,
    pub column: usize,
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ParseError> {
    serde_json::from_slice(bytes).map_err(|e| ParseError {
        line: e.line(),
        column: e.column(),
    })
}

/// 一台 MCP：名字、连接方式、要填值的键名。
pub(crate) struct Server {
    pub name: Name,
    pub transport: Transport,
    pub value_keys: Vec<Name>,
}

/// 一个插件：登记文件里的键，与它第一条 user 范围安装记录的安装目录（只拿来定位文件，不输出；记录里没有就是 `None`）。
pub(crate) struct Plugin {
    pub key: Name,
    pub install_path: Option<PathBuf>,
}

/// 用户配置（`.claude.json`）：只取顶层 `mcpServers`。
pub(crate) fn user_config(bytes: &[u8]) -> Result<Vec<Server>, ParseError> {
    parse::<UserConfig>(bytes).map(|c| c.0)
}

/// 插件自带的 MCP 配置（`.mcp.json`）：顶层恰好只有 `mcpServers` 一个键、且它的值是对象 → 取里面那层（包了一层的写法）；
/// 否则顶层就是服务器表（本机看到的写法）。先过一遍只看顶层的形状（值都不留），再照形状解析（方案 A3.1，e877c60）。
pub(crate) fn plugin_mcp(bytes: &[u8]) -> Result<Vec<Server>, ParseError> {
    let top = parse::<TopShape>(bytes)?;
    if top.keys == 1 && top.wrapper_is_object {
        user_config(bytes)
    } else {
        parse::<ServerMap>(bytes).map(|m| m.0)
    }
}

/// 插件登记文件（`installed_plugins.json`）：只取 `plugins` 的键，与每个键下第一条 `scope` 为 `user` 的安装记录的 `installPath`。
/// 没有 user 记录的键（只装在 local 等别的范围）不列（方案 A3.1，e877c60）。
pub(crate) fn plugin_registry(bytes: &[u8]) -> Result<Vec<Plugin>, ParseError> {
    parse::<Registry>(bytes).map(|c| c.0)
}

// ---------- 用户配置 ----------

struct UserConfig(Vec<Server>);

impl<'de> Deserialize<'de> for UserConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = UserConfig;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UserConfig, A::Error> {
                let mut servers = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "mcpServers" {
                        servers.extend(map.next_value::<ServerMap>()?.0);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(UserConfig(servers))
            }
        }
        d.deserialize_map(V)
    }
}

// ---------- 插件自带的 MCP 配置：顶层的形状 ----------

/// 顶层有几个键，`mcpServers` 的值是不是对象。值一律吃掉不留。
struct TopShape {
    keys: usize,
    wrapper_is_object: bool,
}

impl<'de> Deserialize<'de> for TopShape {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = TopShape;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TopShape, A::Error> {
                let mut top = TopShape {
                    keys: 0,
                    wrapper_is_object: false,
                };
                while let Some(key) = map.next_key::<String>()? {
                    top.keys += 1;
                    if key == "mcpServers" {
                        top.wrapper_is_object = map.next_value::<IsObject>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(top)
            }
        }
        d.deserialize_map(V)
    }
}

/// 一个值是不是对象；内容吃掉不留。
struct IsObject(bool);

impl<'de> Deserialize<'de> for IsObject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = IsObject;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<IsObject, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(IsObject(true))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<IsObject, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(IsObject(false))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
            fn visit_unit<E: de::Error>(self) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<IsObject, E> {
                Ok(IsObject(false))
            }
        }
        d.deserialize_any(V)
    }
}

// ---------- 服务器表与一台服务器 ----------

struct ServerMap(Vec<Server>);

impl<'de> Deserialize<'de> for ServerMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ServerMap;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of servers")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ServerMap, A::Error> {
                let mut servers = Vec::new();
                while let Some(raw) = map.next_key::<String>()? {
                    let shape = map.next_value::<ServerShape>()?;
                    servers.push(shape.into_server(Name::from_source(&raw))); // 出口一：MCP 的条目名
                }
                Ok(ServerMap(servers))
            }
        }
        d.deserialize_map(V)
    }
}

/// 一台服务器里要取的：`type`（只出标签）、`env` 与 `headers` 的键名。
struct ServerShape {
    transport: Transport,
    value_keys: Vec<Name>,
}

impl ServerShape {
    fn into_server(self, name: Name) -> Server {
        Server {
            name,
            transport: self.transport,
            value_keys: self.value_keys,
        }
    }
}

impl<'de> Deserialize<'de> for ServerShape {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ServerShape;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a server object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ServerShape, A::Error> {
                let mut shape = ServerShape {
                    transport: Transport::Stdio,
                    value_keys: Vec::new(),
                };
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => shape.transport = map.next_value::<TransportLabel>()?.0,
                        "env" | "headers" => {
                            shape.value_keys.extend(map.next_value::<KeyNames>()?.0)
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(shape)
            }
        }
        d.deserialize_map(V)
    }
}

/// 连接方式：来源里的写法和固定的表逐字比，原文不留。没写（null）或 `stdio` → Stdio；`http`、`streamable-http` → Http；
/// `sse` → Sse；别的任何值（别的字符串、数字、对象）→ Other。
pub(crate) fn transport_of(raw: Option<&str>) -> Transport {
    match raw {
        None | Some("stdio") => Transport::Stdio,
        Some("http" | "streamable-http") => Transport::Http,
        Some("sse") => Transport::Sse,
        Some(_) => Transport::Other,
    }
}

struct TransportLabel(Transport);

impl<'de> Deserialize<'de> for TransportLabel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = TransportLabel;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a transport")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<TransportLabel, E> {
                Ok(TransportLabel(transport_of(Some(v))))
            }
            fn visit_unit<E: de::Error>(self) -> Result<TransportLabel, E> {
                Ok(TransportLabel(transport_of(None)))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<TransportLabel, E> {
                Ok(TransportLabel(Transport::Other))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<TransportLabel, E> {
                Ok(TransportLabel(Transport::Other))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<TransportLabel, E> {
                Ok(TransportLabel(Transport::Other))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<TransportLabel, E> {
                Ok(TransportLabel(Transport::Other))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<TransportLabel, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(TransportLabel(Transport::Other))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TransportLabel, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(TransportLabel(Transport::Other))
            }
        }
        d.deserialize_any(V)
    }
}

/// `env` / `headers`：只收键名，值交给 `IgnoredAny`。null 当作没有键。
struct KeyNames(Vec<Name>);

impl<'de> Deserialize<'de> for KeyNames {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = KeyNames;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of names")
            }
            fn visit_unit<E: de::Error>(self) -> Result<KeyNames, E> {
                Ok(KeyNames(Vec::new()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyNames, A::Error> {
                let mut keys = Vec::new();
                while let Some(raw) = map.next_key::<String>()? {
                    map.next_value::<IgnoredAny>()?;
                    keys.push(Name::from_source(&raw)); // 出口二：env / headers 的键名
                }
                Ok(KeyNames(keys))
            }
        }
        d.deserialize_any(V)
    }
}

// ---------- 插件登记文件 ----------

struct Registry(Vec<Plugin>);

impl<'de> Deserialize<'de> for Registry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Registry;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Registry, A::Error> {
                // 文件在就该有 plugins：没有这个键是形状不对（换了结构、拼错了），不当成「没装插件」（审查 1/F-1）
                let mut plugins = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "plugins" {
                        plugins
                            .get_or_insert_with(Vec::new)
                            .extend(map.next_value::<PluginMap>()?.0);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                plugins
                    .map(Registry)
                    .ok_or_else(|| de::Error::missing_field("plugins"))
            }
        }
        d.deserialize_map(V)
    }
}

struct PluginMap(Vec<Plugin>);

impl<'de> Deserialize<'de> for PluginMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = PluginMap;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of plugins")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PluginMap, A::Error> {
                let mut plugins = Vec::new();
                while let Some(raw) = map.next_key::<String>()? {
                    if let Some(install_path) = map.next_value::<FirstUserInstall>()?.0 {
                        plugins.push(Plugin {
                            key: Name::from_source(&raw),
                            install_path,
                        }); // 出口四：插件键
                    }
                }
                Ok(PluginMap(plugins))
            }
        }
        d.deserialize_map(V)
    }
}

/// 一个插件键下的安装记录数组：第一条 `scope` 为 `user` 的记录的 `installPath`。外层 `None` = 没有 user 记录；
/// 内层 `None` = 有 user 记录而它没写 `installPath`。不合并各条记录。
struct FirstUserInstall(Option<Option<PathBuf>>);

impl<'de> Deserialize<'de> for FirstUserInstall {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FirstUserInstall;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array of installations")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<FirstUserInstall, A::Error> {
                let mut first = None;
                while let Some(record) = seq.next_element::<InstallRecord>()? {
                    if first.is_none() && record.user_scope {
                        first = Some(record.install_path);
                    }
                }
                Ok(FirstUserInstall(first))
            }
        }
        d.deserialize_seq(V)
    }
}

/// 一条安装记录里要的两样：范围是不是 `user`，`installPath`。范围的原文只拿来比，不留。
struct InstallRecord {
    user_scope: bool,
    install_path: Option<PathBuf>,
}

impl<'de> Deserialize<'de> for InstallRecord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = InstallRecord;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an installation object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<InstallRecord, A::Error> {
                // scope 必须有（审查 2/F-1）：缺了或拼错了是形状不对，不当成「不是 user」静默跳过。installPath 可选
                let mut user_scope = None;
                let mut install_path = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "scope" => user_scope = Some(map.next_value::<String>()? == "user"),
                        "installPath" => {
                            install_path = Some(PathBuf::from(map.next_value::<String>()?))
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                let user_scope = user_scope.ok_or_else(|| de::Error::missing_field("scope"))?;
                Ok(InstallRecord {
                    user_scope,
                    install_path,
                })
            }
        }
        d.deserialize_map(V)
    }
}
