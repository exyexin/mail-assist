//! e2e 测试公共设施：
//! - 本地 greenmail 邮件服务器（SMTP 127.0.0.1:3025 / IMAP 127.0.0.1:3143，auth disabled）
//! - 内嵌 mock LLM HTTP 桩（脚本化分类/提取/意图，不消耗真实 DeepSeek API）
//! - 临时配置目录 + FakeClock

use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use mailparse::MailHeaderMap;
use std::sync::Arc;

pub const GREENMAIL_SMTP: (&str, u16) = ("127.0.0.1", 3025);
pub const GREENMAIL_IMAP: (&str, u16) = ("127.0.0.1", 3143);

/// 确认 greenmail 可用；不可用给出启动指引。
pub fn ensure_greenmail() {
    use std::net::TcpStream;
    let ok =
        TcpStream::connect(GREENMAIL_SMTP).is_ok() && TcpStream::connect(GREENMAIL_IMAP).is_ok();
    assert!(
        ok,
        "本地测试邮件服务器不可用（需 greenmail: SMTP 3025 / IMAP 3143）。请运行 scripts/dev.sh greenmail-start 或:\n\
         podman run -d --name mail2-greenmail -p 3025:3025 -p 3143:3143 greenmail/standalone"
    );
}

/// 通过 greenmail SMTP 投递一封测试邮件（可指定 Date 头发件时间与 message-id/in-reply-to）。
pub fn send_mail(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
) {
    send_mail_at(from, to, subject, body, message_id, in_reply_to, None);
}

/// 投递测试邮件并指定 Date 头（用于范围拉取/相对时限推算测试）。
#[allow(clippy::too_many_arguments)]
pub fn send_mail_at(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    date: Option<chrono::DateTime<chrono::Utc>>,
) {
    let mailer = SmtpTransport::builder_dangerous(GREENMAIL_SMTP.0)
        .port(GREENMAIL_SMTP.1)
        .credentials(Credentials::new("x".into(), "x".into()))
        .build();
    let mut builder = Message::builder()
        .from(from.parse().unwrap())
        .to(to.parse().unwrap())
        .subject(subject.to_string());
    if let Some(mid) = message_id {
        builder = builder.message_id(Some(mid.to_string()));
    }
    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(irt.to_string());
    }
    if let Some(d) = date {
        builder = builder.date(d.into());
    }
    let email = builder
        .header(lettre::message::header::ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .unwrap();
    mailer.send(&email).unwrap();
}

/// 统计测试邮箱中“未读（\\Seen 未设置）”的邮件数量。
/// 只取 FLAGS（不取正文），用于验证自动拉取不会把用户的邮件标记成已读。
pub async fn unseen_count(user: &str) -> usize {
    use async_imap::Client;
    use futures_util::TryStreamExt;
    let tcp = tokio::net::TcpStream::connect(GREENMAIL_IMAP)
        .await
        .unwrap();
    let client = Client::new(tcp);
    let mut session = client.login(user, "x").await.map_err(|(e, _)| e).unwrap();
    session.select("INBOX").await.unwrap();
    let msgs = session.fetch("1:*", "(FLAGS)").await.unwrap();
    let mut unseen = 0usize;
    let mut stream = msgs;
    while let Some(msg) = stream.try_next().await.unwrap() {
        let seen = msg
            .flags()
            .any(|f| matches!(f, async_imap::types::Flag::Seen));
        if !seen {
            unseen += 1;
        }
    }
    drop(stream);
    session.logout().await.ok();
    unseen
}

/// 读取测试邮箱全部邮件的 (subject, body, message_id, in_reply_to)。
pub async fn read_inbox(user: &str) -> Vec<(String, String, String, String)> {
    use async_imap::Client;
    use futures_util::TryStreamExt;
    let tcp = tokio::net::TcpStream::connect(GREENMAIL_IMAP)
        .await
        .unwrap();
    let client = Client::new(tcp);
    let mut session = client.login(user, "x").await.map_err(|(e, _)| e).unwrap();
    session.select("INBOX").await.unwrap();
    let msgs = session.fetch("1:*", "(BODY.PEEK[])").await.unwrap();
    let mut out = Vec::new();
    let mut stream = msgs;
    while let Some(msg) = stream.try_next().await.unwrap() {
        if let Some(body) = msg.body() {
            let parsed = mailparse::parse_mail(body).unwrap();
            let get = |name: &str| -> String {
                parsed
                    .get_headers()
                    .get_first_value(name)
                    .unwrap_or_default()
            };
            let text = parsed.get_body().unwrap_or_default();
            out.push((
                get("Subject"),
                text,
                get("Message-ID")
                    .trim_matches('<')
                    .trim_matches('>')
                    .to_string(),
                get("In-Reply-To"),
            ));
        }
    }
    drop(stream);
    session.logout().await.ok();
    out
}

/// mock LLM：按邮件内容返回脚本化 JSON（OpenAI 兼容响应格式）。
pub struct MockLlmServer {
    pub base_url: String,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockLlmServer {
    pub async fn start() -> MockLlmServer {
        let app = axum::Router::new().route("/chat/completions", axum::routing::post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        MockLlmServer {
            base_url,
            shutdown: tx,
        }
    }

    pub fn stop(self) {
        let _ = self.shutdown.send(());
    }
}

async fn handler(axum::Json(body): axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let has_tools = body["tools"].is_array();
    let system = messages
        .iter()
        .find(|m| m["role"] == "system")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("");
    let user_text: String = messages
        .iter()
        .filter(|m| m["role"] == "user" || m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // ---- Agent 模式（请求带 tools）：脚本化 tool call 循环 ----
    if has_tools {
        let tool_msgs = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .count();
        let content = if tool_msgs == 0 {
            // 第一轮：要求读完整邮件
            let email_id = regex_find(&user_text, r"email_id:\s*(\d+)").unwrap_or(1);
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_get_email",
                            "type": "function",
                            "function": { "name": "get_email", "arguments": format!(r#"{{"id": {email_id}}}"#) }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })
        } else if tool_msgs == 1 && user_text.contains("REQ_UPDATE_ITEM") {
            // 修改请求测试：第二轮发起 update_item（应生成审批单）
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_update_item",
                            "type": "function",
                            "function": { "name": "update_item",
                                "arguments": r#"{"id": 1, "patch": {"title": "Agent 修改的标题"}}"# }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })
        } else if tool_msgs == 1 && user_text.contains("REQ_CREATE_ITEM") {
            // 模拟真实 Agent：直接调用 create_item 建项且 deadline 为 null
            // （用于验证 Agent 建项后的截止时间补推算）
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_create_item",
                            "type": "function",
                            "function": { "name": "create_item",
                                "arguments": r#"{"category": "written_test", "title": "在线笔试邀请", "party": "某公司", "event": "在线笔试", "deadline": null, "notes": ""}"# }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })
        } else {
            // 最终答案：分类 + 事项（与 legacy 分支一致的计算）
            let cat = mock_classify(&user_text);
            let item = if cat == "todo" { mock_extract(&user_text) } else { serde_json::json!(null) };
            let content = serde_json::json!({
                "category": cat,
                "item": item,
                "summary": "mock agent final"
            });
            serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": content.to_string() }, "finish_reason": "stop" }]
            })
        };
        return axum::Json(content);
    }

    // ---- legacy 模式（无 tools）：分类 / 提取 / 意图 ----
    let content = if system.contains("提取") {
        mock_extract(&user_text).to_string()
    } else if system.contains("提醒邮件回复") {
        if user_text.contains("已完成") {
            r#"{"intent":"done"}"#.to_string()
        } else if user_text.contains("不再提醒") {
            r#"{"intent":"silent"}"#.to_string()
        } else {
            r#"{"intent":"none"}"#.to_string()
        }
    } else {
        serde_json::json!({ "category": mock_classify(&user_text) }).to_string()
    };

    axum::Json(serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }]
    }))
}

fn regex_find(text: &str, re: &str) -> Option<i64> {
    let r = regex::Regex::new(re).ok()?;
    r.captures(text)?.get(1)?.as_str().parse().ok()
}

fn mock_classify(user_text: &str) -> &'static str {
    if user_text.contains("面试") && (user_text.contains("问卷") || user_text.contains("调研") || user_text.contains("反馈")) {
        "notification"
    } else if user_text.contains("笔试") {
        "written_test"
    } else if user_text.contains("测评") {
        "assessment"
    } else if user_text.contains("会议") {
        "todo"
    } else if user_text.contains("面试") {
        "interview"
    } else if user_text.contains("账单") || user_text.contains("公告") {
        "notification"
    } else if user_text.contains("讨论") {
        "conversation"
    } else {
        "misc"
    }
}

fn mock_extract(user_text: &str) -> serde_json::Value {
    // 测试用：正文里写 `DEADLINE:<RFC3339>` 可让 mock 返回指定截止时间
    if let Some(rest) = user_text.split("DEADLINE:").nth(1) {
        let d = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(['。', '，', ',', '.'])
            .to_string();
        if chrono::DateTime::parse_from_rfc3339(&d).is_ok() {
            return serde_json::json!({
                "party": "字节跳动", "event": "技术面试", "title": "技术面试邀请",
                "deadline": d, "category": null
            });
        }
    }
    if user_text.contains("没有时间") || user_text.contains("暂无时间") {
        serde_json::json!({"party":"某公司","event":"待定事项","title":"待定事项","deadline":null,"category":null})
    } else if user_text.contains("请在") {
        // 相对时限邮件：LLM 提取不到明确截止时间（由系统用发件时间推算）
        serde_json::json!({"party":"某公司","event":"线上笔试","title":"线上笔试邀请","deadline":null,"category":null})
    } else if user_text.contains("面试") {
        serde_json::json!({"party":"字节跳动","event":"技术面试","title":"技术面试邀请","deadline":"2027-09-10T14:00:00+08:00","category":null})
    } else if user_text.contains("会议") {
        serde_json::json!({"party":"Acme 公司","event":"项目评审会","title":"项目评审会议","deadline":"2027-09-20T10:00:00+08:00","category":null})
    } else {
        serde_json::json!({"party":"未知","event":"一般事项","title":"一般事项","deadline":null,"category":null})
    }
}

/// 构造指向 greenmail + mock LLM 的临时配置目录。
pub fn temp_config(llm_base_url: &str, mail_user: &str, smtp_port: u16) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let yaml = format!(
        r#"
listen: "127.0.0.1:0"
auth_token: ""
data_dir: "{data_dir}"
timezone: "Asia/Shanghai"
poll_interval_secs: 300
reminder:
  to: "{mail_user}"
  days_before: 1
  lead_time: "09:00"
  webhook_url: ""
check_times: ["06:00", "12:30", "18:30", "00:00"]
mail:
  address: "{mail_user}"
  imap:
    host: "127.0.0.1"
    port: 3143
    user: "{mail_user}"
    password: "x"
  smtp:
    host: "127.0.0.1"
    port: {smtp_port}
    user: "x"
    password: "x"
"#,
        data_dir = dir.path().join("data").to_string_lossy(),
        mail_user = mail_user,
        smtp_port = smtp_port,
    );
    std::fs::write(dir.path().join("config.yaml"), yaml).unwrap();
    std::fs::write(
        dir.path().join("llm.yaml"),
        format!(
            "api_key: \"test-key\"\nbase_url: \"{llm_base_url}\"\nmodel: \"mock\"\ntimeout_secs: 10\ncategories:\n  - {{ id: todo, label: 事务预约/待办, create_item: true }}\n  - {{ id: interview, label: 面试, create_item: true }}\n  - {{ id: written_test, label: 笔试, create_item: true }}\n  - {{ id: assessment, label: 测评, create_item: true }}\n  - {{ id: notification, label: 通知类, create_item: false }}\n  - {{ id: conversation, label: 对话交流型, create_item: false }}\n  - {{ id: misc, label: 其他, create_item: false }}\n"
        ),
    )
    .unwrap();
    dir
}

/// 从临时目录加载配置并组装 App（可覆盖 smtp 端口以注入故障）。
pub fn build_app(
    dir: &tempfile::TempDir,
    clock: Arc<dyn mail2::clock::Clock>,
    smtp_port_override: Option<u16>,
) -> mail2::App {
    let mut cfg = mail2::config::AppConfig::load(dir.path()).unwrap();
    if let Some(p) = smtp_port_override {
        cfg.mail.smtp.port = p;
    }
    mail2::App::build(cfg, clock).unwrap()
}

pub fn fake_clock(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Arc<mail2::clock::FakeClock> {
    use chrono::TimeZone;
    let t = chrono_tz::Asia::Shanghai
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap();
    Arc::new(mail2::clock::FakeClock::new(
        t.with_timezone(&chrono::Local),
    ))
}

pub fn unique_user() -> String {
    format!("mail2-e2e-{}@localhost", uuid::Uuid::new_v4())
}
