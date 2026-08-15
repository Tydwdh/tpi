//! P5-07：policy profile——actor/effect/resource 的决策。
//!
//! - [`PolicyDecision`]：Allow / Deny / RequireApproval；
//! - [`PolicyProfile`]：按作用域（workspace 外访问 / 网络 / 写工具）的决策；
//! - [`DEFAULT_PROFILE`]：保持 current default（`allow_outside_workspace=true`
//!   等现有行为）；[`STRICT_PROFILE`]：显式严格（拒绝 workspace 外、要求确认）。
//!
//! 先保持 current default；新增显式 strict profile（不改变默认行为）。

/// 策略决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny,
    RequireApproval,
}

/// 作用域（策略评估的最小单元）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyScope {
    /// workspace 外路径访问（read/write/bash/file）。
    OutsideWorkspace,
    /// 网络（web_fetch/web_search）。
    Network,
    /// 写工具（edit/write——副作用）。
    WriteEffect,
    /// 进程执行（bash/process）。
    Process,
}

/// 策略 profile（按作用域的决策表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyProfile {
    pub outside_workspace: PolicyDecision,
    pub network: PolicyDecision,
    pub write_effect: PolicyDecision,
    pub process: PolicyDecision,
}

/// current default：保持现有行为（allow_outside_workspace=true、web 允许、写允许、进程允许）。
pub const DEFAULT_PROFILE: PolicyProfile = PolicyProfile {
    outside_workspace: PolicyDecision::Allow,
    network: PolicyDecision::Allow,
    write_effect: PolicyDecision::Allow,
    process: PolicyDecision::Allow,
};

/// 显式 strict profile：拒绝 workspace 外、写/进程要求确认。
pub const STRICT_PROFILE: PolicyProfile = PolicyProfile {
    outside_workspace: PolicyDecision::Deny,
    network: PolicyDecision::Allow,
    write_effect: PolicyDecision::RequireApproval,
    process: PolicyDecision::RequireApproval,
};

impl PolicyProfile {
    pub fn decide(&self, scope: PolicyScope) -> PolicyDecision {
        match scope {
            PolicyScope::OutsideWorkspace => self.outside_workspace,
            PolicyScope::Network => self.network,
            PolicyScope::WriteEffect => self.write_effect,
            PolicyScope::Process => self.process,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// current default：所有作用域 Allow（保持现有行为）。
    #[test]
    fn default_profile_allows_all() {
        for scope in [
            PolicyScope::OutsideWorkspace,
            PolicyScope::Network,
            PolicyScope::WriteEffect,
            PolicyScope::Process,
        ] {
            assert_eq!(DEFAULT_PROFILE.decide(scope), PolicyDecision::Allow);
        }
    }

    /// strict profile：workspace 外 Deny；写/进程 RequireApproval。
    #[test]
    fn strict_profile_denies_and_requires_approval() {
        assert_eq!(
            STRICT_PROFILE.decide(PolicyScope::OutsideWorkspace),
            PolicyDecision::Deny
        );
        assert_eq!(
            STRICT_PROFILE.decide(PolicyScope::WriteEffect),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            STRICT_PROFILE.decide(PolicyScope::Process),
            PolicyDecision::RequireApproval
        );
        // 网络保持 Allow。
        assert_eq!(
            STRICT_PROFILE.decide(PolicyScope::Network),
            PolicyDecision::Allow
        );
    }
}
