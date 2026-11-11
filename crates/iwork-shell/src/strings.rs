//! 通用遍历：收集一个可序列化值里的全部字符串叶子（方案 3.3「AST 里的字符串叶子全部走到」）。
//!
//! 第三方解析器的 AST 与词片段在 `serde` 特性下都派生了 `Serialize`。这里写一个只收字符串的
//! `Serializer` 走一遍，不手写按节点的 `match`：手写就要列节点和字段，漏一个就是 9/F-7；
//! 解析器升级加了节点，这里也自动走到。枚举变体名与字段名不是字符串叶子，不收。

use serde::Serialize;
use serde::ser;

/// `value` 里的全部字符串叶子，按序列化顺序。
pub(crate) fn leaves<T: Serialize + ?Sized>(value: &T) -> Vec<String> {
    let mut out = Vec::new();
    // 收集器本身不会失败；第三方类型的 Serialize 实现报错时已收到的叶子照用
    let _ = value.serialize(Collector { out: &mut out });
    out
}

#[derive(Debug)]
pub(crate) struct Never(String);

impl std::fmt::Display for Never {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Never {}

impl ser::Error for Never {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

struct Collector<'a> {
    out: &'a mut Vec<String>,
}

impl<'a> Collector<'a> {
    fn again(&mut self) -> Collector<'_> {
        Collector { out: self.out }
    }
}

type R = Result<(), Never>;

impl<'a> ser::Serializer for Collector<'a> {
    type Ok = ();
    type Error = Never;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_str(self, v: &str) -> R {
        self.out.push(v.to_owned());
        Ok(())
    }
    fn serialize_char(self, v: char) -> R {
        self.out.push(v.to_string());
        Ok(())
    }
    fn serialize_bool(self, _: bool) -> R {
        Ok(())
    }
    fn serialize_i8(self, _: i8) -> R {
        Ok(())
    }
    fn serialize_i16(self, _: i16) -> R {
        Ok(())
    }
    fn serialize_i32(self, _: i32) -> R {
        Ok(())
    }
    fn serialize_i64(self, _: i64) -> R {
        Ok(())
    }
    fn serialize_u8(self, _: u8) -> R {
        Ok(())
    }
    fn serialize_u16(self, _: u16) -> R {
        Ok(())
    }
    fn serialize_u32(self, _: u32) -> R {
        Ok(())
    }
    fn serialize_u64(self, _: u64) -> R {
        Ok(())
    }
    fn serialize_f32(self, _: f32) -> R {
        Ok(())
    }
    fn serialize_f64(self, _: f64) -> R {
        Ok(())
    }
    fn serialize_bytes(self, _: &[u8]) -> R {
        Ok(())
    }
    fn serialize_none(self) -> R {
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> R {
        v.serialize(self)
    }
    fn serialize_unit(self) -> R {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> R {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> R {
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> R {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        v: &T,
    ) -> R {
        v.serialize(self)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Never> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, Never> {
        Ok(self)
    }
}

impl<'a> ser::SerializeSeq for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeTuple for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeTupleStruct for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeTupleVariant for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeMap for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> R {
        k.serialize(self.again())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeStruct for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl<'a> ser::SerializeStructVariant for Collector<'a> {
    type Ok = ();
    type Error = Never;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> R {
        v.serialize(self.again())
    }
    fn end(self) -> R {
        Ok(())
    }
}
