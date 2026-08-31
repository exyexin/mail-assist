//! mail2 —— 邮件管理工具核心库
//! 模块职责：
//! - config:  配置加载（config.yaml / 服务商文本 config + llm.yaml）
//! - model:   领域类型与状态机
//! - db:      SQLite 持久化（多账户/动态分类/审批/审计）
//! - mail:    IMAP 收信 / SMTP 发信 / 邮件解析（含发件时间）
//! - llm:     DeepSeek 分类与结构化提取 + tool call 消息协议
//! - agent:   LLM Agent 模式（工具注册表 / 审批流 / 主循环）
//! - deadline: 截止时间推算（人类可读时间归一化 + 相对时限 "X小时/日内/24H内完成"）
//! - pipeline: 新邮件处理流水线（多账户 / 手动范围拉取）
//! - scheduler: 每分调度（每日 4 检查时刻 + 提醒窗口）
//! - checker: 漏发/失败检测与 30 分钟重试 + 过期清扫
//! - notifier: 提醒邮件（主题格式【类型/公司/事件/月日】）+ webhook
//! - api:     REST /api/v1 + 静态 Web 前端
//! - clock:   可注入时钟（测试伪造时间）

pub mod agent;
pub mod api;
pub mod checker;
pub mod clock;
pub mod config;
pub mod db;
pub mod deadline;
pub mod llm;
pub mod mail;
pub mod model;
pub mod notifier;
pub mod pipeline;
pub mod scheduler;

/// 组装应用上下文（config + db + smtp + llm + clock），供 main / 测试共用。
pub struct App {
    pub cfg: std::sync::Arc<config::AppConfig>,
    pub db: std::sync::Arc<db::Db>,
    pub smtp: std::sync::Arc<mail::SmtpClient>,
    pub llm: std::sync::Arc<llm::LlmClient>,
    pub clock: std::sync::Arc<dyn clock::Clock>,
}

impl App {
    pub fn build(
        cfg: config::AppConfig,
        clock: std::sync::Arc<dyn clock::Clock>,
    ) -> anyhow::Result<App> {
        let db = db::Db::open(cfg.data_dir.clone())?;
        db.seed_defaults(&cfg)?;
        let smtp = mail::SmtpClient::from_config(&cfg.mail.smtp, &cfg.mail.address)?;
        let llm = llm::LlmClient::new(&cfg.llm)?;
        Ok(App {
            cfg: std::sync::Arc::new(cfg),
            db: std::sync::Arc::new(db),
            smtp: std::sync::Arc::new(smtp),
            llm: std::sync::Arc::new(llm),
            clock,
        })
    }
}
