//! 领域类型与状态机

use serde::{Deserialize, Serialize};

/// 分类 id 常量（与 llm.yaml categories.id 对应；额外分类由配置注入）
pub const CATEGORY_TODO: &str = "todo";
pub const CATEGORY_NOTIFICATION: &str = "notification";
pub const CATEGORY_CONVERSATION: &str = "conversation";
pub const CATEGORY_MISC: &str = "misc";
/// 招聘推广类（宣讲会/双选会/网申推荐/投递邀请）：只做通知，永不建待办
pub const CATEGORY_CAREER_PROMO: &str = "career_promo";
/// 需要本人行动的核心分类：即使没有明确时间也建待办（标记待补截止时间）
pub const CORE_ACTION_CATEGORIES: [&str; 3] = ["interview", "written_test", "assessment"];

/// 提醒策略（决定 checker 生成哪些提醒时刻）
pub mod remind_policy {
    /// 常规：截止前 days_before 天的 lead_time（默认 09:00）
    pub const NORMAL: &str = "normal";
    /// 紧急（收到时距截止 ≤24h）：立即提醒 + 截止前 2 小时
    pub const URGENT: &str = "urgent";
    /// 链接/资格失效类：立即提醒 + 截止前 2 小时
    pub const LINK_EXPIRY: &str = "link_expiry";
}

/// 事务类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Todo,
    Notification,
}

impl ItemKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemKind::Todo => "todo",
            ItemKind::Notification => "notification",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(ItemKind::Todo),
            "notification" => Some(ItemKind::Notification),
            _ => None,
        }
    }
}

/// 事务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    Active,
    Completed,
    Silent,
    /// 截止时间已过（由检查器自动流转；不提醒、可手动恢复）
    Expired,
}

impl ItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemStatus::Active => "active",
            ItemStatus::Completed => "completed",
            ItemStatus::Silent => "silent",
            ItemStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(ItemStatus::Active),
            "completed" => Some(ItemStatus::Completed),
            "silent" => Some(ItemStatus::Silent),
            "expired" => Some(ItemStatus::Expired),
            _ => None,
        }
    }
}

/// 发送日志状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendStatus {
    Pending,
    Sent,
    Failed,
}

impl SendStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SendStatus::Pending => "pending",
            SendStatus::Sent => "sent",
            SendStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(SendStatus::Pending),
            "sent" => Some(SendStatus::Sent),
            "failed" => Some(SendStatus::Failed),
            _ => None,
        }
    }
}

/// 入库邮件记录
#[derive(Debug, Clone, Serialize)]
pub struct EmailRecord {
    pub id: i64,
    pub uid: Option<i64>,
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub from_name: String,
    pub body_text: String,
    pub category: String,
    /// 邮件发件时间（Date 头解析，RFC3339；解析失败为空）
    pub sent_at: String,
    /// 收件入库时间（RFC3339）
    pub received_at: String,
    pub item_id: Option<i64>,
    pub reply_to_item_id: Option<i64>,
    pub account_id: i64,
    pub handled: bool,
    /// 用户手动标注（空 = 未标注；预设值见 web 前端，也可自由填写）
    pub user_label: String,
    /// 用户标注备注
    pub user_note: String,
}

/// 邮件标注的预设值（与前端下拉一致；存储为自由文本便于扩展）
pub mod email_label {
    pub const NONE: &str = "";
    pub const CORRECT: &str = "correct";
    pub const WRONG_CATEGORY: &str = "wrong_category";
    pub const SHOULD_TODO: &str = "should_todo";
    pub const SHOULD_NOT_TODO: &str = "should_not_todo";
    pub const DEADLINE_MISSING: &str = "deadline_missing";
    pub const OTHER: &str = "other";
}

/// 待办/通知事务
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub kind: ItemKind,
    pub title: String,
    pub party: String,
    pub event: String,
    pub category: String,
    pub deadline: Option<String>,  // RFC3339
    pub remind_at: Option<String>, // RFC3339（最早一次提醒时刻）
    /// 提醒策略：normal | urgent | link_expiry（见 [remind_policy]）
    #[serde(default)]
    pub remind_policy: String,
    pub needs_review: bool,
    pub status: ItemStatus,
    pub source_email_id: Option<i64>,
    pub account_id: i64,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 发送日志
#[derive(Debug, Clone, Serialize)]
pub struct SendLog {
    pub id: i64,
    pub item_id: i64,
    pub attempt: i64,
    pub kind: String, // reminder | retry | manual
    pub to_addr: String,
    pub subject: String,
    pub body: String,
    pub status: SendStatus,
    pub error: Option<String>,
    pub scheduled_at: String, // 幂等键之一（首次计划发送时刻）
    pub sent_at: Option<String>,
    pub next_retry_at: Option<String>,
    pub message_id: Option<String>,
}

/// LLM 提取的待办结构化信息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoExtract {
    pub party: String,            // 公司/个人（简短）
    pub event: String,            // 事件（5-10 字）
    pub title: String,            // 事项标题
    pub deadline: Option<String>, // RFC3339
    pub category: Option<String>, // 可选：二级类型
    #[serde(default)]
    pub notes: String,            // 可选：备注/推算说明
}

/// 回复意图
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyIntent {
    Done,
    Silent,
    None,
}

/// 一次检查/处理产生的统计
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunReport {
    pub fetched: usize,
    pub new_emails: usize,
    pub classified: usize,
    pub items_created: usize,
    pub items_updated: usize,
    pub reminders_sent: usize,
    pub reminders_failed: usize,
    pub retries: usize,
    pub missed_compensated: usize,
    pub replies_handled: usize,
    pub llm_fallback_misc: usize,
    pub expired: usize,
    pub tool_rounds: usize,
    pub approvals_created: usize,
}

// ---------- 多邮箱账户 ----------

/// 邮箱账户（DB 存储；密码以 base64 混淆存储，序列化输出一律脱敏）
#[derive(Debug, Clone, Deserialize)]
pub struct MailAccount {
    pub id: i64,
    pub label: String,
    pub address: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_user: String,
    pub imap_password: String,
    pub imap_tls_insecure: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_password: String,
    pub smtp_tls_insecure: bool,
    pub enabled: bool,
    pub is_default: bool,
    pub reminder_to: String,
    pub created_at: String,
    pub updated_at: String,
}

impl MailAccount {
    /// 从静态配置构造种子账户（首次迁移）。
    pub fn from_config(cfg: &crate::config::MailConfig) -> Self {
        let now = crate::db::now_str();
        MailAccount {
            id: 0,
            label: "默认账户".into(),
            address: cfg.address.clone(),
            imap_host: cfg.imap.host.clone(),
            imap_port: cfg.imap.port,
            imap_user: cfg.imap.user.clone(),
            imap_password: cfg.imap.password.clone(),
            imap_tls_insecure: cfg.imap.tls_insecure,
            smtp_host: cfg.smtp.host.clone(),
            smtp_port: cfg.smtp.port,
            smtp_user: cfg.smtp.user.clone(),
            smtp_password: cfg.smtp.password.clone(),
            smtp_tls_insecure: cfg.smtp.tls_insecure,
            enabled: true,
            is_default: true,
            reminder_to: String::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// 密码混淆（可逆 base64；本地工具，目的仅防裸看）
    pub fn obfuscate(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    /// 解混淆
    pub fn deobfuscate(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(s.trim())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| s.to_string())
    }

    pub fn imap_endpoint(&self) -> crate::config::Endpoint {
        crate::config::Endpoint {
            host: self.imap_host.clone(),
            port: self.imap_port,
            user: if self.imap_user.trim().is_empty() {
                self.address.clone()
            } else {
                self.imap_user.clone()
            },
            // 不变式：MailAccount 结构体中密码恒为明文；
            // 混淆/解混淆只发生在 DB 写/读边界（db.rs），此处原样透传。
            // 若在此再解混淆，16 字符等合法 base64 明文密码会被二次解码成乱码。
            password: self.imap_password.clone(),
            tls_insecure: self.imap_tls_insecure,
        }
    }

    pub fn smtp_endpoint(&self) -> crate::config::Endpoint {
        crate::config::Endpoint {
            host: self.smtp_host.clone(),
            port: self.smtp_port,
            user: if self.smtp_user.trim().is_empty() {
                self.address.clone()
            } else {
                self.smtp_user.clone()
            },
            password: self.smtp_password.clone(),
            tls_insecure: self.smtp_tls_insecure,
        }
    }

    /// 对外 JSON（密码一律 `***`；前端编辑时用占位符回传表示"保持不变"）。
    pub fn public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "label": self.label,
            "address": self.address,
            "imap_host": self.imap_host,
            "imap_port": self.imap_port,
            "imap_user": self.imap_user,
            "imap_password": if self.imap_password.is_empty() { "" } else { "***" },
            "imap_tls_insecure": self.imap_tls_insecure,
            "smtp_host": self.smtp_host,
            "smtp_port": self.smtp_port,
            "smtp_user": self.smtp_user,
            "smtp_password": if self.smtp_password.is_empty() { "" } else { "***" },
            "smtp_tls_insecure": self.smtp_tls_insecure,
            "enabled": self.enabled,
            "is_default": self.is_default,
            "reminder_to": self.reminder_to,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

// ---------- 动态分类 ----------

#[derive(Debug, Clone, Serialize)]
pub struct CategoryInfo {
    pub id: String,
    pub label: String,
    /// 该类邮件是否自动创建事务
    pub create_item: bool,
    /// 创建事务时的 kind：todo | notification
    pub kind: String,
    /// 来源：builtin | llm | user
    pub source: String,
    pub created_at: String,
}

// ---------- 审批单（Agent 删/改类工具申请） ----------

#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    pub id: i64,
    pub tool_name: String,
    /// 工具参数（JSON，批准时重放执行）
    pub payload_json: String,
    /// 人类可读摘要（UI 展示）
    pub summary: String,
    /// pending | approved | rejected
    pub status: String,
    pub source_email_id: Option<i64>,
    pub decided_at: Option<String>,
    pub created_at: String,
}

// ---------- Agent 运行记录（审计） ----------

#[derive(Debug, Clone, Serialize)]
pub struct AgentRun {
    pub id: i64,
    pub email_id: Option<i64>,
    pub account_id: i64,
    pub rounds: usize,
    pub tool_calls: usize,
    pub final_json: String,
    pub trace: String,
    /// ok | error
    pub status: String,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_with(pass: &str) -> MailAccount {
        MailAccount {
            id: 1,
            label: "t".into(),
            address: "u@example.com".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_user: "u@example.com".into(),
            imap_password: pass.into(),
            imap_tls_insecure: false,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_user: "u@example.com".into(),
            smtp_password: pass.into(),
            smtp_tls_insecure: false,
            enabled: true,
            is_default: true,
            reminder_to: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 回归：结构体密码为明文时，endpoint 必须原样透传。
    /// 16 字符合法 base64 的明文密码（如 "MDEyMzQ1Njc4OWFi"）若被再次
    /// base64 解码会变成乱码，导致 IMAP/SMTP 登录失败。
    #[test]
    fn endpoint_passes_password_through_verbatim() {
        // 合法 base64 且长度为 4 的倍数（二次解码会"成功"产出垃圾字节）
        let a = account_with("MDEyMzQ1Njc4OWFi");
        assert_eq!(a.imap_endpoint().password, "MDEyMzQ1Njc4OWFi");
        assert_eq!(a.smtp_endpoint().password, "MDEyMzQ1Njc4OWFi");

        // 非 base64 密码同样必须原样透传
        let b = account_with("普通密码 with space");
        assert_eq!(b.imap_endpoint().password, "普通密码 with space");
        assert_eq!(b.smtp_endpoint().password, "普通密码 with space");
    }
}
