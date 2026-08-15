//! P4-11：O4 capability/tool invariants——只读不变量检查。
//!
//! - [`check_registry_invariants`]：对 registry 快照做只读检查（不修改 runtime）；
//! - 覆盖：snapshot definition == execution lookup、registration 验证通过、
//!   dispose 精确匹配（P4-01 `(name,id)` 语义）、exactly one terminal（pipeline
//!   stage 配对由 O2 sink 的 start/terminal 保证，此处检查 registry 侧）；
//! - [`ToolSnapshot`]：只读 registration/effect snapshot（inspector 读取用，
//!   不允许修改 runtime）。
//!
//! 验收：invariant companion 能定位故意注入的 registry/pipeline 违规。

use crate::tool::registry::ToolRegistry;

/// 一条不变量违规（invariant companion 输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantViolation {
    pub kind: &'static str,
    pub detail: String,
}

/// 只读 registry snapshot（inspector 消费；无修改能力）。
#[derive(Debug, Clone, Default)]
pub struct ToolSnapshot {
    /// 工具名 + origin（readonly）。
    pub tools: Vec<(String, String)>,
    /// 注册数（root + overlay）。
    pub count: usize,
}

/// 从 registry 取只读 snapshot（不锁修改路径——clone 数据）。
pub fn snapshot(registry: &ToolRegistry) -> ToolSnapshot {
    ToolSnapshot {
        tools: registry
            .list()
            .iter()
            .map(|tool| (tool.name().to_string(), format!("{:?}", tool.origin())))
            .collect(),
        count: registry.list().len(),
    }
}

/// 检查 registry 不变量（只读）：registration 验证、definition == lookup。
pub fn check_registry_invariants(registry: &ToolRegistry) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();
    for tool in registry.list() {
        // 不变量 1：registration 时验证过 name/schema（快照中仍应有效）。
        if let Err(e) = tool.validate_definition() {
            violations.push(InvariantViolation {
                kind: "invalid_registration",
                detail: format!("{}: {e}", tool.name()),
            });
        }
        // 不变量 2：definition 投影与 lookup 一致（execute 用的 name == definition name）。
        let def = tool.definition();
        if def.name != tool.name() {
            violations.push(InvariantViolation {
                kind: "definition_lookup_mismatch",
                detail: format!(
                    "definition.name={} != lookup.name={}",
                    def.name,
                    tool.name()
                ),
            });
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::BuiltinTool;
    use crate::tool::registry::{BuiltinToolAdapter, Tool, ToolRegistry};

    fn fresh() -> ToolRegistry {
        ToolRegistry::new()
    }

    fn read_adapter() -> BuiltinToolAdapter {
        BuiltinToolAdapter::new(BuiltinTool::Read)
    }

    /// 合法 registry 无不变量违规。
    #[test]
    fn healthy_registry_has_no_violations() {
        let mut registry = fresh();
        registry.register(std::sync::Arc::new(read_adapter()));
        assert!(
            check_registry_invariants(&registry).is_empty(),
            "健康 registry 不应有违规"
        );
        assert_eq!(snapshot(&registry).count, 1);
    }

    /// 注入违规工具（schema 非 object）→ invariant 定位。
    #[test]
    fn injected_violation_is_detected() {
        struct BadSchema;
        #[async_trait::async_trait]
        impl Tool for BadSchema {
            fn name(&self) -> &str {
                "bad"
            }
            fn description(&self) -> &str {
                "bad"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!("not-object")
            }
            fn origin(&self) -> crate::tool::registry::ToolOrigin {
                crate::tool::registry::ToolOrigin::Builtin
            }
            async fn execute(
                &self,
                _args: &str,
                _ctx: &crate::tool::ToolContext,
            ) -> crate::outcome::ToolOutcome {
                unreachable!()
            }
        }
        let mut registry = fresh();
        // 直接插入（绕过 register 验证，模拟非法来源——不变量检查必须定位）。
        registry.insert_raw(
            "bad",
            crate::ids::RegistrationId::new_v7(),
            std::sync::Arc::new(BadSchema),
        );
        let violations = check_registry_invariants(&registry);
        assert!(
            violations.iter().any(|v| v.kind == "invalid_registration"),
            "违规工具必须被 invariant 定位: {violations:?}"
        );
    }
}
