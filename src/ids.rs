//! 核心标识符类型（文档 §5.2：统一使用 UUIDv7，本地生成、按时间大致有序）。
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
