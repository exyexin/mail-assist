//! 可扩展 Tool call 注册表（需求 9.2 的代码标准）。
//!
//! 设计目标：新增一个 LLM 可调用工具 = 实现 [ToolDef] + 注册到 registry，
//! 无需改动 Agent 主循环与 LLM 客户端。完整标准见 `docs/toolcall-skill.md`。
//!
//! 安全分级：
//! - `ReadOnly`：只读查询，Agent 可随时自主调用；
//! - `Create`：新增数据（分类/事项），Agent 可自主调用，但必须写审计 trace；
//! - `Mutate`：修改/删除既有数据，**不直接执行**，只生成审批单（approvals），
//!   用户批准后由系统重放执行。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::clock::Clock;
use crate::config::AppConfig;
use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyLevel {
    ReadOnly,
    Create,
    Mutate,
}

/// 工具执行上下文（每次调用注入；工具实现不得自行持有状态）。
pub struct ToolContext<'a> {
    pub db: &'a Db,
    pub cfg: &'a AppConfig,
    pub clock: &'a dyn Clock,
    /// 触发本次 Agent 运行的来源邮件（create_item 等工具用于幂等关联）
    pub source_email_id: Option<i64>,
    pub account_id: i64,
}

/// 工具执行结果（content 为返回给 LLM 的 JSON 字符串）。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    pub content: String,
    /// invalid_args | not_found | needs_approval | unknown_tool | internal
    pub error_code: Option<String>,
    /// 本次调用是否生成了审批单（用于统计）
    pub created_approval: bool,
}

impl ToolResult {
    pub fn ok_content(content: Value) -> Self {
        ToolResult {
            ok: true,
            content: serde_json::to_string(&content).unwrap_or_else(|_| "{}".into()),
            error_code: None,
            created_approval: false,
        }
    }

    pub fn err(code: &str, message: impl Into<String>) -> Self {
        ToolResult {
            ok: false,
            content: serde_json::to_string(&json!({
                "ok": false,
                "error_code": code,
                "message": message.into(),
            }))
            .unwrap_or_else(|_| "{}".into()),
            error_code: Some(code.to_string()),
            created_approval: false,
        }
    }
}

/// 工具定义：同步执行（所有现有工具仅做毫秒级 DB 操作；如需 IO 可改为 spawn）。
pub trait ToolDef: Send + Sync {
    /// 工具名（LLM function name；全局唯一，snake_case）
    fn name(&self) -> &'static str;
    /// 中文描述（写入 system prompt 与 OpenAI tools 描述）
    fn description(&self) -> &'static str;
    /// 参数 JSON Schema（OpenAI function.parameters 格式）
    fn parameters(&self) -> Value;
    /// 安全分级
    fn safety(&self) -> SafetyLevel;
    /// Agent 运行期的执行入口：
    /// - ReadOnly/Create 工具在此直接执行；
    /// - Mutate 工具在此只生成审批单（不修改数据）。
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult;
    /// 审批通过后的真正执行（仅 Mutate 工具需要实现；默认拒绝）。
    fn apply(&self, _ctx: &ToolContext<'_>, _args: Value) -> ToolResult {
        ToolResult::err("not_allowed", "该工具不支持审批重放")
    }
}

/// 工具注册表。
pub struct ToolRegistry {
    tools: Vec<Arc<dyn ToolDef>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry { tools: Vec::new() }
    }

    /// 注册工具（重复 name 会覆盖；建议启动时一次性注册）。
    pub fn register(&mut self, t: Arc<dyn ToolDef>) {
        self.tools.retain(|x| x.name() != t.name());
        self.tools.push(t);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolDef>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 生成 OpenAI function-calling 的 `tools` 数组。
    pub fn to_openai(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
