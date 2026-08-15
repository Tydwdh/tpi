//! P8-02：typed answer/approval lifecycle。
//!
//! - [`AnswerRequest`]：request_id + expiry + route（普通 composer 不能误投——
//!   答案只能经显式 route 送达）；
//! - [`AnswerRoute`]：请求来源（哪个 run/session 在等答案）；投递时校验
//!   request_id + 未过期 + route 匹配。
//!
//! 先 fake state tests；集成到 request_input 在 agent 接线时完成。

use crate::ids::RequestId;

/// 答案路由（标识等待答案的请求归属）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRoute {
    pub session_id: String,
    pub request_id: RequestId,
}

/// 一个待回答的请求（typed lifecycle）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRequest {
    pub route: AnswerRoute,
    /// 问题文本（模型输出；展示用）。
    pub question: String,
    /// 过期时间（unix 秒；0 = 不限制）。
    pub expires_at: u64,
}

/// 投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerDelivery {
    /// 已送达（匹配 request + 未过期 + route 一致）。
    Delivered,
    /// 过期（expiry 已过）。
    Expired,
    /// route 不匹配（普通 composer 误投被拒）。
    RouteMismatch,
    /// 未知 request（无等待请求）。
    UnknownRequest,
}

impl AnswerRequest {
    pub fn new(route: AnswerRoute, question: String, expires_at: u64) -> Self {
        Self {
            route,
            question,
            expires_at,
        }
    }

    /// 投递答案（now = 当前 unix 秒；now==0 表示无时间基准 = 不检查过期）。
    pub fn deliver(&self, route: &AnswerRoute, now: u64) -> AnswerDelivery {
        if route.request_id != self.route.request_id || route.session_id != self.route.session_id {
            return AnswerDelivery::RouteMismatch;
        }
        if self.expires_at != 0 && now != 0 && now > self.expires_at {
            return AnswerDelivery::Expired;
        }
        AnswerDelivery::Delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常投递：request + route 匹配 → Delivered。
    #[test]
    fn deliver_matching_route() {
        let route = AnswerRoute {
            session_id: "s1".into(),
            request_id: RequestId::from_u128(1),
        };
        let req = AnswerRequest::new(route.clone(), "问题?".into(), 0);
        assert_eq!(req.deliver(&route, 0), AnswerDelivery::Delivered);
    }

    /// 普通 composer 误投：route 不匹配被拒（不投到别的 request）。
    #[test]
    fn wrong_route_is_rejected() {
        let route = AnswerRoute {
            session_id: "s1".into(),
            request_id: RequestId::from_u128(1),
        };
        let wrong = AnswerRoute {
            session_id: "s2".into(),
            request_id: RequestId::from_u128(2),
        };
        let req = AnswerRequest::new(route, "问题?".into(), 0);
        assert_eq!(req.deliver(&wrong, 0), AnswerDelivery::RouteMismatch);
    }

    /// 过期：expiry 后拒绝。
    #[test]
    fn expired_request_rejected() {
        let route = AnswerRoute {
            session_id: "s1".into(),
            request_id: RequestId::from_u128(1),
        };
        let req = AnswerRequest::new(route.clone(), "问题?".into(), 1_000);
        assert_eq!(req.deliver(&route, 2_000), AnswerDelivery::Expired);
    }
}
