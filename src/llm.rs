//! DeepSeek LLM 客户端（OpenAI 兼容 chat/completions）：
//!
//! - chat_tools:   Agent 路径（function calling 循环；请求/响应消息类型）
//! - classify:      邮件分类（分类 id 限定于 llm.yaml categories）
//! - extract_todo:  事务/待办邮件结构化提取（party/event/deadline/title）
//! - reply_intent:  判断是否为对提醒邮件的"已完成/不再提醒"回复
//!
//! 失败语义：网络/超时/坏 JSON → Err；调用方降级（misc 分类 / 关键词回退）。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LlmConfig;
use crate::model::TodoExtract;

#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    #[allow(dead_code)] // 保留用于日志/未来重试策略
    timeout_secs: u64,
    pub categories: Vec<crate::config::CategoryDef>,
}

// ---------- 消息/请求/响应类型（Agent 与 legacy 共用） ----------

/// OpenAI 兼容的 function call 描述（请求与响应同构）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub function: FunctionCall,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: String) -> Self {
        ChatMessage {
            role: role.to_string(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    typ: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: ChoiceMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChoiceMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ClassifyOut {
    category: String,
}

#[derive(Debug, Deserialize)]
struct ReplyOut {
    intent: String, // done | silent | none
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .context("构建 HTTP 客户端失败")?;
        Ok(LlmClient {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            timeout_secs: cfg.timeout_secs,
            categories: cfg.categories.clone(),
        })
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    async fn post(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let started = std::time::Instant::now();
        let n_messages = req.messages.len();
        let n_tools = req.tools.as_ref().map(|t| t.len()).unwrap_or(0);
        // 完整请求内容（含 messages 全文）写入 debug 级日志，便于追溯 LLM 调用流程
        tracing::debug!(
            "LLM 请求: url={} model={} messages={} tools={} temperature={}\n{}",
            self.url(),
            req.model,
            n_messages,
            n_tools,
            req.temperature,
            serde_json::to_string_pretty(req).unwrap_or_else(|_| "<序列化失败>".to_string())
        );
        let resp = match self
            .http
            .post(self.url())
            .bearer_auth(&self.api_key)
            .json(req)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "LLM HTTP 请求异常: url={} ms={} err={e:#}",
                    self.url(),
                    started.elapsed().as_millis()
                );
                return Err(e).context("LLM 请求失败");
            }
        };
        let status = resp.status();
        let text = resp.text().await.context("读取 LLM 响应失败")?;
        let elapsed_ms = started.elapsed().as_millis();
        if !status.is_success() {
            tracing::warn!(
                "LLM 返回错误: status={status} ms={elapsed_ms} body={}",
                truncate(&text, 300)
            );
            bail!("LLM 返回 {status}: {}", truncate(&text, 300));
        }
        match serde_json::from_str::<ChatResponse>(&text) {
            Ok(parsed) => {
                let first = parsed.choices.first();
                let finish = first
                    .and_then(|c| c.finish_reason.as_deref())
                    .unwrap_or("");
                let n_calls = first
                    .and_then(|c| c.message.tool_calls.as_ref())
                    .map(|v| v.len())
                    .unwrap_or(0);
                let content_len = first
                    .and_then(|c| c.message.content.as_deref())
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                tracing::info!(
                    "LLM 调用完成: model={} ms={elapsed_ms} status={status} finish_reason={finish} content_len={content_len} tool_calls={n_calls}",
                    req.model,
                );
                // 完整响应原文（含工具调用参数与最终 JSON）写入 debug 级日志
                tracing::debug!("LLM 响应全文 (model={}):\n{}", req.model, text);
                Ok(parsed)
            }
            Err(e) => {
                tracing::warn!("LLM 响应解析失败: {e}（耗时 {elapsed_ms}ms）");
                Err(e).context("LLM 响应格式错误")
            }
        }
    }

    /// Agent 路径：带工具定义的对话（function calling；不强制 json_object）。
    pub async fn chat_tools(
        &self,
        messages: &[ChatMessage],
        tools: Vec<Value>,
    ) -> Result<ChatResponse> {
        let tool_names = tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect::<Vec<_>>()
            .join(", ");
        tracing::debug!(
            "Agent LLM 调用: model={} messages={} tools=[{tool_names}]（完整请求见下一条 debug）",
            self.model,
            messages.len()
        );
        let req = ChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            temperature: 0.1,
            response_format: None,
            tools: Some(tools),
        };
        self.post(&req).await
    }

    /// legacy 路径：强制 JSON 输出的单轮对话（分类/提取/意图）。
    async fn chat_json(&self, system: &str, user: &str) -> Result<String> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::text("system", system.to_string()),
                ChatMessage::text("user", user.to_string()),
            ],
            temperature: 0.1,
            response_format: Some(ResponseFormat {
                typ: "json_object".into(),
            }),
            tools: None,
        };
        let parsed = self.post(&req).await?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        if content.trim().is_empty() {
            bail!("LLM 返回空内容");
        }
        Ok(content)
    }

    /// 分类：返回 categories 中的 id；不可识别 → "misc"。
    pub async fn classify(&self, subject: &str, body: &str, from: &str) -> Result<String> {
        tracing::debug!("LLM 分类请求: from={from} subject={subject} body_len={}", body.chars().count());
        let cat_list = self
            .categories
            .iter()
            .map(|c| format!("{}:{}", c.id, c.label))
            .collect::<Vec<_>>()
            .join(", ");
        let system = format!(
            "你是邮件分类器。请将邮件归入以下类型之一（只能输出 JSON，格式 {{\"category\": \"类型id\"}}）：{cat_list}。\
             判断标准：事务预约/待办类 = 面试邀请、笔试安排、在线测评、会议预约、需要按时赴约或完成任务的事项\
             （通常包含时间/链接/需要确认参加等预约动作）；\
             通知类 = 投递成功/简历已收到、进度反馈、问卷调研、面试或笔试或测评结果通知、感谢信、录用通知、\
             系统通知、公告、账单、验证码、订阅推送等；\
             注意：反馈式邮件即使出现“面试”“笔试”“测评”字样（例如“面试体验问卷”“面试结果通知”“投递成功通知”），\
             只要不是在预约/安排一次面试/笔试/测评，就归为通知类；\
             对话交流型 = 人与人之间的对话、讨论、回复；\
             其他 = 广告、垃圾或无法归类的内容。\
             若邮件同时是事务又是对话，以事务为准。分类 id 必须严格来自列表。"
        );
        let user = format!(
            "发件人: {from}\n主题: {subject}\n正文:\n{}",
            truncate(body, 4000)
        );
        for attempt in 0..2 {
            match self.chat_json(&system, &user).await {
                Ok(content) => {
                    let parsed: ClassifyOut = serde_json::from_str(&content)
                        .or_else(|_| serde_json::from_str(&extract_json_object(&content)))
                        .unwrap_or(ClassifyOut {
                            category: String::new(),
                        });
                    let id = parsed.category.trim().to_string();
                    if self.categories.iter().any(|c| c.id == id) {
                        tracing::info!("LLM 分类结果: category={id}");
                        return Ok(id);
                    }
                    if attempt == 0 {
                        tracing::debug!("LLM 分类返回非法类型 {id:?}，重试一次");
                        continue;
                    }
                }
                Err(_) if attempt == 0 => {
                    tracing::debug!("LLM 分类调用失败，重试一次");
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        bail!("LLM 分类未返回合法类型")
    }

    /// 待办结构化提取。
    pub async fn extract_todo(&self, subject: &str, body: &str, from: &str) -> Result<TodoExtract> {
        tracing::debug!("LLM 待办提取请求: from={from} subject={subject} body_len={}", body.chars().count());
        let system = "你是待办事项提取器。从邮件中提取结构化信息，只能输出 JSON：\
             {\"party\": \"公司或联系人名称（简短，如 字节跳动 或 张三；无则空字符串）\",\
              \"event\": \"事项名称（5-10个汉字以内，如 技术面试）\",\
              \"title\": \"完整标题\",\
              \"deadline\": \"截止/开始时间，RFC3339 格式（如 2027-09-10T14:00:00+08:00）；\
                  若邮件没有明确时间则为 null\",\
              \"category\": \"可选二级类型，无则 null\"}\n\
              规则：deadline 必须来自邮件原文的明确时间（面试时间、截止日期等），\
              原文中的“2026-04-24 11:00(GMT+08:00)”“2026/4/24 14:00”“4月24日 下午2:00”等写法\
              也要转换成 RFC3339 输出；没有明确时间就输出 null，绝不编造；\
              party 取公司名或人名；event 提炼动作+对象。";
        let user = format!(
            "发件人: {from}\n主题: {subject}\n正文:\n{}",
            truncate(body, 4000)
        );
        for attempt in 0..2 {
            match self.chat_json(&system, &user).await {
                Ok(content) => {
                    let obj = extract_json_object(&content);
                    if let Ok(mut ex) = serde_json::from_str::<TodoExtract>(&obj) {
                        if ex.title.trim().is_empty() {
                            ex.title = subject.to_string();
                        }
                        if let Some(d) = &ex.deadline {
                            if chrono::DateTime::parse_from_rfc3339(d).is_err() {
                                ex.deadline = None; // 非法时间视为未提取到
                            }
                        }
                        ex.party = ex.party.trim().to_string();
                        ex.event = ex.event.trim().to_string();
                        tracing::info!(
                            "LLM 待办提取结果: party={} event={} deadline={:?} title={}",
                            ex.party,
                            ex.event,
                            ex.deadline,
                            ex.title
                        );
                        return Ok(ex);
                    }
                    if attempt == 0 {
                        continue;
                    }
                }
                Err(_) if attempt == 0 => continue,
                Err(e) => return Err(e),
            }
        }
        bail!("LLM 待办提取失败")
    }

    /// 回复意图：done（已完成）/ silent（不再提醒）/ none。
    pub async fn reply_intent(
        &self,
        subject: &str,
        body: &str,
    ) -> Result<crate::model::ReplyIntent> {
        tracing::debug!("LLM 回复意图请求: subject={subject} body_len={}", body.chars().count());
        let system = "你是提醒邮件回复分析器。用户在回复我们发送的提醒邮件。只能输出 JSON：\
             {\"intent\": \"done|silent|none\"}。\
             done = 用户表示事项已完成（如：已完成、已参加、done）；\
             silent = 用户表示不需要再提醒（如：不再提醒、取消提醒、别发了、stop）；\
             none = 无法判断或与提醒无关。若同时表达已完成，优先 done。";
        let user = format!("主题: {subject}\n正文:\n{}", truncate(body, 2000));
        let content = self.chat_json(system, &user).await?;
        let obj = extract_json_object(&content);
        let parsed: ReplyOut = serde_json::from_str(&obj).unwrap_or(ReplyOut {
            intent: "none".into(),
        });
        let intent = match parsed.intent.trim().to_lowercase().as_str() {
            "done" => crate::model::ReplyIntent::Done,
            "silent" => crate::model::ReplyIntent::Silent,
            _ => crate::model::ReplyIntent::None,
        };
        tracing::info!("LLM 回复意图结果: intent={intent:?}");
        Ok(intent)
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}\n…[截断]")
    }
}

/// 从 LLM 输出中提取第一个 JSON 对象（容忍前后缀说明文字）。
pub fn extract_json_object(s: &str) -> String {
    let s = s.trim();
    if let Some(a) = s.find('{') {
        if let Some(b) = s.rfind('}') {
            if b > a {
                return s[a..=b].to_string();
            }
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_works() {
        assert_eq!(extract_json_object("好的：{\"a\":1}"), "{\"a\":1}");
        assert_eq!(extract_json_object("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn truncate_works() {
        assert_eq!(truncate("你好世界", 2), "你好\n…[截断]");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn tool_call_deser() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"call_1","type":"function",
            "function":{"name":"list_categories","arguments":"{}"}}]}}]}"#;
        let resp: ChatResponse = serde_json::from_str(raw).unwrap();
        let calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].function.name, "list_categories");
        assert_eq!(calls[0].id, "call_1");
    }
}
