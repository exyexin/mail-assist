//! 配置加载：
//! - `llm.yaml`（YAML，必需）
//! - 邮箱配置：优先 `config.yaml`（结构化 YAML）；否则读取 `config`
//!   （服务商导出的文本格式，自动正则提取；也兼容把 YAML 直接放进 `config`）。

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    /// 跳过 TLS 证书域名校验（加密保留）。
    /// 部分邮箱服务商证书与域名不匹配（如通配 *.icoremail.net 服务 imap.stu.xmu.edu.cn），
    /// 需显式设为 true 才能连接；默认 false（严格校验）。
    #[serde(default)]
    pub tls_insecure: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MailConfig {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub imap: Endpoint,
    #[serde(default)]
    pub smtp: Endpoint,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReminderConfig {
    #[serde(default)]
    pub to: String,
    #[serde(default = "default_days_before")]
    pub days_before: i64,
    #[serde(default = "default_lead_time")]
    pub lead_time: String,
    #[serde(default)]
    pub webhook_url: String,
}

fn default_days_before() -> i64 {
    1
}
fn default_lead_time() -> String {
    "09:00".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryDef {
    pub id: String,
    pub label: String,
    #[serde(default = "default_true")]
    pub create_item: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// 新邮件处理是否走 Agent（tool call）路径；false = 旧固定 prompt 路径
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Agent 循环最大轮数（每轮可能含多次工具调用）
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            enabled: true,
            max_tool_rounds: default_max_tool_rounds(),
        }
    }
}

fn default_max_tool_rounds() -> usize {
    8
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryDef>,
    #[serde(default)]
    pub agent: AgentConfig,
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_categories() -> Vec<CategoryDef> {
    vec![
        CategoryDef {
            id: "todo".into(),
            label: "事务预约/待办".into(),
            create_item: true,
        },
        CategoryDef {
            id: "interview".into(),
            label: "面试".into(),
            create_item: true,
        },
        CategoryDef {
            id: "assessment".into(),
            label: "测评".into(),
            create_item: true,
        },
        CategoryDef {
            id: "written_test".into(),
            label: "笔试".into(),
            create_item: true,
        },
        CategoryDef {
            id: "notification".into(),
            label: "通知类".into(),
            create_item: false,
        },
        CategoryDef {
            id: "conversation".into(),
            label: "对话交流型".into(),
            create_item: false,
        },
        CategoryDef {
            id: "misc".into(),
            label: "其他".into(),
            create_item: false,
        },
    ]
}

/// config.yaml 的结构化形态（全部带默认值，便于最小配置）
#[derive(Debug, Clone, Deserialize)]
pub struct YamlConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub reminder: ReminderConfig,
    #[serde(default = "default_check_times")]
    pub check_times: Vec<String>,
    #[serde(default)]
    pub mail: MailConfig,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_data_dir() -> String {
    "./data".to_string()
}
fn default_poll_interval() -> u64 {
    60
}
fn default_check_times() -> Vec<String> {
    vec![
        "06:00".into(),
        "12:30".into(),
        "18:30".into(),
        "00:00".into(),
    ]
}

impl Default for ReminderConfig {
    fn default() -> Self {
        ReminderConfig {
            to: String::new(),
            days_before: default_days_before(),
            lead_time: default_lead_time(),
            webhook_url: String::new(),
        }
    }
}

impl Default for YamlConfig {
    fn default() -> Self {
        YamlConfig {
            listen: default_listen(),
            auth_token: String::new(),
            data_dir: default_data_dir(),
            timezone: String::new(),
            poll_interval_secs: default_poll_interval(),
            reminder: ReminderConfig::default(),
            check_times: default_check_times(),
            mail: MailConfig::default(),
        }
    }
}

/// 运行时配置（解析完成后的最终形态）
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub listen: String,
    pub auth_token: String,
    pub data_dir: PathBuf,
    pub timezone: chrono_tz::Tz,
    pub poll_interval_secs: u64,
    pub reminder: ReminderConfig,
    pub check_times: Vec<String>,
    pub mail: MailConfig,
    pub llm: LlmConfig,
}

impl AppConfig {
    /// 从目录加载配置（dir 包含 llm.yaml 与 config.yaml 或 config）。
    pub fn load(dir: &Path) -> Result<Self> {
        let llm_path = dir.join("llm.yaml");
        let llm_raw = std::fs::read_to_string(&llm_path)
            .with_context(|| format!("无法读取 LLM 配置 {}", llm_path.display()))?;
        let llm: LlmConfig = serde_yaml::from_str(&llm_raw)
            .with_context(|| format!("解析 {} 失败（应为 YAML）", llm_path.display()))?;

        let yaml_path = dir.join("config.yaml");
        let text_path = dir.join("config");

        let mut yc: YamlConfig = if yaml_path.exists() {
            let raw = std::fs::read_to_string(&yaml_path)
                .with_context(|| format!("无法读取 {}", yaml_path.display()))?;
            serde_yaml::from_str(&raw)
                .with_context(|| format!("解析 {} 失败（应为 YAML）", yaml_path.display()))?
        } else if text_path.exists() {
            let raw = std::fs::read_to_string(&text_path)
                .with_context(|| format!("无法读取 {}", text_path.display()))?;
            // 先尝试 YAML（用户可能把结构化配置直接命名为 config），
            // 失败则按服务商文本导出格式解析。
            match serde_yaml::from_str::<YamlConfig>(&raw) {
                Ok(c) => c,
                Err(_) => parse_provider_text(&raw)
                    .with_context(|| format!("解析邮箱配置 {} 失败", text_path.display()))?,
            }
        } else {
            bail!(
                "未找到邮箱配置：需要 {} 或 {}（可参考 config.example）",
                yaml_path.display(),
                text_path.display()
            );
        };

        // 校验
        if yc.mail.address.trim().is_empty() {
            bail!("邮箱配置缺少 邮件地址");
        }
        if yc.mail.imap.host.trim().is_empty() {
            bail!("邮箱配置缺少 IMAP 服务器");
        }
        if yc.mail.smtp.host.trim().is_empty() {
            bail!("邮箱配置缺少 SMTP 服务器");
        }
        if yc.mail.imap.user.trim().is_empty() {
            yc.mail.imap.user = yc.mail.address.clone();
        }
        if yc.mail.smtp.user.trim().is_empty() {
            yc.mail.smtp.user = yc.mail.address.clone();
        }
        if yc.reminder.to.trim().is_empty() {
            yc.reminder.to = yc.mail.address.clone();
        }
        if llm.api_key.trim().is_empty() || llm.model.trim().is_empty() {
            bail!("llm.yaml 需要 api_key 与 model");
        }
        for t in &yc.check_times {
            parse_hhmm(t).with_context(|| format!("check_times 非法时刻: {t}"))?;
        }
        parse_hhmm(&yc.reminder.lead_time)
            .with_context(|| format!("reminder.lead_time 非法时刻: {}", yc.reminder.lead_time))?;

        let timezone: chrono_tz::Tz = if yc.timezone.trim().is_empty() {
            // 系统时区：IANA 名（/etc/localtime / TZ），失败回退上海
            iana_time_zone::get_timezone()
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(chrono_tz::Asia::Shanghai)
        } else {
            yc.timezone
                .parse()
                .with_context(|| format!("非法时区: {}", yc.timezone))?
        };

        let data_dir = PathBuf::from(&yc.data_dir);
        Ok(AppConfig {
            listen: yc.listen,
            auth_token: yc.auth_token,
            data_dir,
            timezone,
            poll_interval_secs: yc.poll_interval_secs.max(5),
            reminder: yc.reminder,
            check_times: yc.check_times,
            mail: yc.mail,
            llm,
        })
    }

    /// 分类定义查询（含默认兜底）
    pub fn category(&self, id: &str) -> Option<&CategoryDef> {
        self.llm.categories.iter().find(|c| c.id == id)
    }

    /// 分类 label（用于提醒主题【类型/…】）
    pub fn category_label(&self, id: &str) -> String {
        self.category(id)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// 该类邮件是否自动创建事务
    pub fn category_creates_item(&self, id: &str) -> bool {
        self.category(id).map(|c| c.create_item).unwrap_or(false)
    }
}

/// 解析 "HH:MM" → (h, m)
pub fn parse_hhmm(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() != 2 {
        bail!("需要 HH:MM 格式");
    }
    let h: u32 = parts[0].parse().context("小时非法")?;
    let m: u32 = parts[1].parse().context("分钟非法")?;
    if h > 23 || m > 59 {
        bail!("时刻越界");
    }
    Ok((h, m))
}

/// 解析邮箱服务商导出的文本配置（如 XMU 客户端专用密码导出页）。
fn parse_provider_text(raw: &str) -> Result<YamlConfig> {
    let addr: String =
        find_re(raw, r"邮件地址\s*([^\s@]+@[^\s@]+)").context("文本配置中未找到 邮件地址")?;
    let imap_host: String = find_re(raw, r"收信服务器\s*\(IMAP\)\s*([^\s:]+)")
        .context("文本配置中未找到 收信服务器(IMAP)")?;
    let imap_port: u16 = find_re(raw, r"收信服务器\s*\(IMAP\)[^\n]*?SSL\s*端口:\s*(\d+)")
        .context("文本配置中未找到 IMAP SSL 端口")?;
    let smtp_host: String = find_re(raw, r"发信服务器\s*\(SMTP\)\s*([^\s:]+)")
        .context("文本配置中未找到 发信服务器(SMTP)")?;
    let smtp_port: u16 = find_re(raw, r"发信服务器\s*\(SMTP\)[^\n]*?SSL\s*端口:\s*(\d+)")
        .context("文本配置中未找到 SMTP SSL 端口")?;

    // 密码：紧跟"专用密码"标记行之后的纯 token 行；兜底扫描全文字符串行。
    let mut password: Option<String> = None;
    let lines: Vec<&str> = raw.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if l.contains("专用密码") {
            for next in lines.iter().skip(i + 1).take(4) {
                let t = next.trim();
                if !t.is_empty() && !t.contains(' ') && !t.contains('\t') && t.is_ascii() {
                    password = Some(t.to_string());
                    break;
                }
                if !t.is_empty() && t.chars().any(|c| c.is_ascii_alphabetic()) {
                    break; // 遇到说明文字，不再往下找
                }
            }
            break;
        }
    }
    if password.is_none() {
        password = lines.iter().find_map(|l| {
            let t = l.trim();
            if t.len() >= 8
                && t.len() <= 64
                && t.chars().all(|c| c.is_ascii() && !c.is_whitespace())
            {
                Some(t.to_string())
            } else {
                None
            }
        });
    }
    let password = password.context("文本配置中未找到邮箱密码（专用密码）")?;

    Ok(YamlConfig {
        mail: MailConfig {
            address: addr.clone(),
            imap: Endpoint {
                host: imap_host,
                port: imap_port,
                user: addr.clone(),
                password: password.clone(),
                tls_insecure: false,
            },
            smtp: Endpoint {
                host: smtp_host,
                port: smtp_port,
                user: addr.clone(),
                password,
                tls_insecure: false,
            },
        },
        ..YamlConfig::default()
    })
}

/// 正则捕获第一组；返回 String（或 u16 等 FromStr 类型）。
fn find_re<T: std::str::FromStr>(raw: &str, re: &str) -> Option<T> {
    let regex = regex::Regex::new(re).ok()?;
    let caps = regex.captures(raw)?;
    caps.get(1)?.as_str().trim().parse::<T>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XMU_TEXT: &str = r#" 专用密码 my-app-password

your-app-password
客户端专用密码仅在生成时可见，支持设置多个，切勿使用其它方式保存，以防泄露

相关配置参数

邮件地址	user@example.com	
收信服务器 (IMAP)	imap.stu.xmu.edu.cn	SSL 端口: 993, 非 SSL 端口: 143
收信服务器 (POP3)	pop.stu.xmu.edu.cn	SSL 端口: 995, 非 SSL 端口: 110
发信服务器 (SMTP)	smtp.stu.xmu.edu.cn	SSL 端口: 465, 非 SSL 端口: 25
"#;

    #[test]
    fn parse_provider_text_basic() {
        let c = parse_provider_text(XMU_TEXT).unwrap();
        assert_eq!(c.mail.address, "user@example.com");
        assert_eq!(c.mail.imap.host, "imap.stu.xmu.edu.cn");
        assert_eq!(c.mail.imap.port, 993);
        assert_eq!(c.mail.smtp.host, "smtp.stu.xmu.edu.cn");
        assert_eq!(c.mail.smtp.port, 465);
        assert_eq!(c.mail.imap.password, "your-app-password");
        assert_eq!(c.mail.smtp.password, "your-app-password");
    }

    #[test]
    fn parse_hhmm_ok() {
        assert_eq!(parse_hhmm("06:00").unwrap(), (6, 0));
        assert_eq!(parse_hhmm("00:00").unwrap(), (0, 0));
        assert_eq!(parse_hhmm("6:0").unwrap(), (6, 0)); // 宽容解析
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("abc").is_err());
    }
}
