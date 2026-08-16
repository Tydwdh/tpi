//! 核心标识符类型：统一使用本地生成、按时间大致有序的 UUIDv7。
//!
//! M1 起从 u64 包装升级为 UUIDv7；对外接口不变。

use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new_v7() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn from_u128(value: u128) -> Self {
                Self(Uuid::from_u128(value))
            }

            /// 从 UUID 字符串解析（失败返回 [`uuid::Error`]）。
            pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0.to_string())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                let uuid = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
                Ok(Self(uuid))
            }
        }
    };
}

id_type!(RequestId);
id_type!(ToolCallId);
id_type!(EventId);
id_type!(SessionId);
id_type!(RunId);
id_type!(RegistrationId);
// O1（P1-07）：trace 因果链身份——只在真实边界注入（一次 public Agent Run =
// 一个 TraceId；span 用 SpanId；跨 run 因果用显式 link，不嵌套到永不关闭的树）。
id_type!(TraceId);
id_type!(SpanId);
