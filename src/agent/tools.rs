//! 首批内置工具（需求 9.1）：
//! - 只读：get_email / search_emails / list_items / list_categories
//! - 新增（自主）：create_category / create_item
//! - 改/删（审批）：update_item / set_item_status / delete_item / update_email_category / delete_email
//!
//! 新增工具请按 `docs/toolcall-skill.md` 的五步法实现并在此注册。

use serde_json::{json, Value};
use std::sync::Arc;

use crate::db::{Db, EmailFilter, ItemFilter};
use crate::model::{Item, ItemKind, ItemStatus};
use crate::notifier;

use super::registry::{SafetyLevel, ToolContext, ToolDef, ToolRegistry, ToolResult};

// ---------- 小工具 ----------

fn jstr<'a>(args: &'a Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn jbool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn jint(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn obj_schema(props: Value, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

/// 生成审批单（改/删类工具的执行方式）。
fn request_approval(ctx: &ToolContext<'_>, tool: &str, args: &Value, summary: &str) -> ToolResult {
    let payload = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
    match ctx
        .db
        .insert_approval(tool, &payload, summary, ctx.source_email_id)
    {
        Ok(id) => ToolResult {
            ok: true,
            content: serde_json::to_string(&json!({
                "ok": true,
                "needs_approval": true,
                "approval_id": id,
                "message": format!("修改/删除申请 #{id} 已提交，等待用户在控制台批准后生效。批准前数据不会被改动。"),
            }))
            .unwrap_or_else(|_| "{}".into()),
            error_code: Some("needs_approval".into()),
            created_approval: true,
        },
        Err(e) => ToolResult::err("internal", format!("审批单创建失败: {e:#}")),
    }
}

fn check_item_exists(db: &Db, id: i64) -> Result<(), ToolResult> {
    match db.get_item(id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(ToolResult::err("not_found", format!("事项 #{id} 不存在"))),
        Err(e) => Err(ToolResult::err("internal", format!("{e:#}"))),
    }
}

fn check_email_exists(db: &Db, id: i64) -> Result<(), ToolResult> {
    match db.get_email(id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(ToolResult::err("not_found", format!("邮件 #{id} 不存在"))),
        Err(e) => Err(ToolResult::err("internal", format!("{e:#}"))),
    }
}

fn valid_deadline(s: &str) -> bool {
    !s.trim().is_empty() && chrono::DateTime::parse_from_rfc3339(s.trim()).is_ok()
}

/// 宽容归一化截止时间：RFC3339 原样返回；人类可读时间（如 "2026-04-24 11:00"、
/// "2026-04-24 11:00(GMT+08:00)"）转成 RFC3339（bad case 修复）。
fn normalize_deadline(s: &str, tz: chrono_tz::Tz) -> Option<String> {
    let t = s.trim();
    if valid_deadline(t) {
        return Some(t.to_string());
    }
    crate::deadline::parse_human_datetime(t, tz).map(|(d, _)| d)
}

// ---------- 只读 ----------

struct GetEmailTool;
impl ToolDef for GetEmailTool {
    fn name(&self) -> &'static str {
        "get_email"
    }
    fn description(&self) -> &'static str {
        "读取一封已入库邮件的完整信息（含完整正文、发件时间、当前分类与关联事项）。参数 id 为邮件 ID。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({ "id": { "type": "integer", "description": "邮件 ID" } }),
            vec!["id"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::ReadOnly
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少整数参数 id");
        };
        match ctx.db.get_email(id) {
            Ok(Some(e)) => ToolResult::ok_content(serde_json::to_value(&e).unwrap_or(json!({}))),
            Ok(None) => ToolResult::err("not_found", format!("邮件 #{id} 不存在")),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct SearchEmailsTool;
impl ToolDef for SearchEmailsTool {
    fn name(&self) -> &'static str {
        "search_emails"
    }
    fn description(&self) -> &'static str {
        "按分类/关键词/发件时间搜索已入库邮件（列表不含正文，阅读正文请用 get_email）。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "category": { "type": "string", "description": "分类 id（可选）" },
                "q": { "type": "string", "description": "关键词（主题/发件人/正文，可选）" },
                "since": { "type": "string", "description": "发件时间下界 RFC3339（可选）" },
                "limit": { "type": "integer", "description": "最多返回条数，默认 20，最大 50" },
            }),
            vec![],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::ReadOnly
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let limit = jint(&args, "limit").unwrap_or(20).clamp(1, 50) as usize;
        let f = EmailFilter {
            category: jstr(&args, "category"),
            q: jstr(&args, "q"),
            since: jstr(&args, "since"),
            before: None,
            account_id: None,
            label: None,
            limit,
        };
        match ctx.db.list_emails(&f) {
            Ok(emails) => {
                let list: Vec<Value> = emails
                    .iter()
                    .map(|e| {
                        json!({
                            "id": e.id,
                            "subject": e.subject,
                            "from": format!("{} <{}>", e.from_name, e.from_addr),
                            "category": e.category,
                            "sent_at": e.sent_at,
                            "item_id": e.item_id,
                        })
                    })
                    .collect();
                ToolResult::ok_content(json!({ "ok": true, "count": list.len(), "emails": list }))
            }
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct ListItemsTool;
impl ToolDef for ListItemsTool {
    fn name(&self) -> &'static str {
        "list_items"
    }
    fn description(&self) -> &'static str {
        "查询事项（待办/通知）列表，支持状态/分类/联系人/关键词过滤；用于查重与了解现状。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "status": { "type": "string", "description": "active/completed/silent/expired（可选）" },
                "category": { "type": "string", "description": "分类 id（可选）" },
                "party": { "type": "string", "description": "公司/联系人（可选）" },
                "q": { "type": "string", "description": "关键词（可选）" },
                "limit": { "type": "integer", "description": "最多返回条数，默认 50，最大 200" },
            }),
            vec![],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::ReadOnly
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let status = match jstr(&args, "status").as_deref() {
            Some(s) => match ItemStatus::parse(s) {
                Some(st) => Some(st),
                None => return ToolResult::err("invalid_args", format!("非法 status: {s}")),
            },
            None => None,
        };
        let limit = jint(&args, "limit").unwrap_or(50).clamp(1, 200) as usize;
        let f = ItemFilter {
            status,
            category: jstr(&args, "category"),
            party: jstr(&args, "party"),
            q: jstr(&args, "q"),
            limit,
            ..Default::default()
        };
        match ctx.db.list_items(&f) {
            Ok(items) => {
                let list: Vec<Value> = items.iter().map(|i| serde_json::to_value(i).unwrap_or(json!({}))).collect();
                ToolResult::ok_content(json!({ "ok": true, "count": list.len(), "items": list }))
            }
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct ListCategoriesTool;
impl ToolDef for ListCategoriesTool {
    fn name(&self) -> &'static str {
        "list_categories"
    }
    fn description(&self) -> &'static str {
        "查询当前全部分类（id/名称/是否自动建事项/事务类型/来源）。分类不存在时可用 create_category 新增。"
    }
    fn parameters(&self) -> Value {
        obj_schema(json!({}), vec![])
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::ReadOnly
    }
    fn execute(&self, ctx: &ToolContext<'_>, _args: Value) -> ToolResult {
        match ctx.db.list_categories() {
            Ok(cats) => {
                let list: Vec<Value> = cats
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id, "label": c.label, "create_item": c.create_item,
                            "kind": c.kind, "source": c.source,
                        })
                    })
                    .collect();
                ToolResult::ok_content(json!({ "ok": true, "count": list.len(), "categories": list }))
            }
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

// ---------- 新增（自主） ----------

struct CreateCategoryTool;
impl ToolDef for CreateCategoryTool {
    fn name(&self) -> &'static str {
        "create_category"
    }
    fn description(&self) -> &'static str {
        "新增邮件分类（如 线上测评、材料提交）。id 用小写字母/数字/下划线（1-32 字符）。新建分类默认不自动建项（create_item=false），需要建待办时请显式调用 create_item。已存在则返回已有分类，不会报错。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "id": { "type": "string", "description": "分类 id，如 online_assessment" },
                "label": { "type": "string", "description": "显示名称，如 线上测评" },
                "create_item": { "type": "boolean", "description": "该分类邮件是否自动创建事项，默认 false（新建分类不建项）" },
                "kind": { "type": "string", "description": "事务类型 todo（有截止时间的待办）或 notification（普通通知），默认 notification" },
            }),
            vec!["id", "label"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Create
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jstr(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        let Some(label) = jstr(&args, "label") else {
            return ToolResult::err("invalid_args", "缺少 label");
        };
        if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
            || id.len() > 32
            || id.is_empty()
        {
            return ToolResult::err("invalid_args", "id 需为 1-32 位小写字母/数字/下划线/连字符");
        }
        let kind = jstr(&args, "kind").unwrap_or_else(|| "notification".into());
        if kind != "todo" && kind != "notification" {
            return ToolResult::err("invalid_args", "kind 只能为 todo 或 notification");
        }
        // 需求 2：Agent 新建的分类默认不自动建项（create_item 默认 false）；
        // 即使显式传 true，也必须是 kind=todo 的分类，避免推广/通知类分类自动产生待办。
        let create_item = jbool(&args, "create_item", false) && kind == "todo";
        match ctx
            .db
            .upsert_category(&id, &label, create_item, &kind, "llm")
        {
            Ok(created) => {
                let c = ctx.db.get_category(&id).ok().flatten();
                ToolResult::ok_content(json!({
                    "ok": true,
                    "created": created,
                    "category": c.map(|c| json!({"id": c.id, "label": c.label, "create_item": c.create_item, "kind": c.kind})).unwrap_or(json!(null)),
                }))
            }
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct CreateItemTool;
impl ToolDef for CreateItemTool {
    fn name(&self) -> &'static str {
        "create_item"
    }
    fn description(&self) -> &'static str {
        "创建待办/通知事项。category 必须是已有分类 id（可用 list_categories 查询）。deadline 为 RFC3339（如 2027-09-10T14:00:00+08:00），没有明确时间则传 null。同一来源邮件只会关联一次（幂等）。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "category": { "type": "string", "description": "分类 id（必填）" },
                "title": { "type": "string", "description": "事项完整标题（必填）" },
                "party": { "type": "string", "description": "公司/联系人简称" },
                "event": { "type": "string", "description": "5-10 字事件名（用于提醒主题）" },
                "deadline": { "type": ["string", "null"], "description": "截止时间 RFC3339 或 null" },
                "notes": { "type": "string", "description": "备注" },
            }),
            vec!["category", "title"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Create
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(category) = jstr(&args, "category") else {
            return ToolResult::err("invalid_args", "缺少 category");
        };
        let Some(title) = jstr(&args, "title") else {
            return ToolResult::err("invalid_args", "缺少 title");
        };
        let cat = match ctx.db.get_category(&category) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return ToolResult::err(
                    "invalid_args",
                    format!("分类 {category} 不存在，请先用 create_category 创建"),
                )
            }
            Err(e) => return ToolResult::err("internal", format!("{e:#}")),
        };
        let deadline = jstr(&args, "deadline")
            .filter(|s| !s.trim().is_empty() && s != "null")
            .and_then(|d| normalize_deadline(&d, ctx.cfg.timezone));
        if let Some(d) = &deadline {
            if !valid_deadline(d) {
                return ToolResult::err("invalid_args", format!("deadline 需为 RFC3339 或 null: {d}"));
            }
        }
        // 幂等：同一来源邮件已有关联事项则返回已有
        if let Some(sid) = ctx.source_email_id {
            if let Ok(Some(item_id)) = ctx.db.item_for_email(sid) {
                if let Ok(Some(existing)) = ctx.db.get_item(item_id) {
                    return ToolResult::ok_content(json!({
                        "ok": true,
                        "created": false,
                        "message": "该邮件已关联事项，返回已有事项",
                        "item": serde_json::to_value(&existing).unwrap_or(json!({})),
                    }));
                }
            }
        }
        let now = ctx.clock.now().to_rfc3339();
        let mut item = Item {
            id: 0,
            kind: if cat.kind == "notification" {
                ItemKind::Notification
            } else {
                ItemKind::Todo
            },
            title,
            party: jstr(&args, "party").unwrap_or_default(),
            event: notifier::clamp_event(&jstr(&args, "event").unwrap_or_default()),
            category: category.clone(),
            remind_at: None,
            remind_policy: String::new(),
            deadline,
            needs_review: false,
            status: ItemStatus::Active,
            source_email_id: ctx.source_email_id,
            account_id: ctx.account_id,
            notes: jstr(&args, "notes").unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        // 分级提醒策略：链接失效类/24h 内紧急 → 立即提醒 + 截止前 2h
        let text = ctx
            .source_email_id
            .and_then(|eid| ctx.db.get_email(eid).ok().flatten())
            .map(|e| format!("{} {}", e.subject, e.body_text))
            .unwrap_or_default();
        let link_expiry = crate::deadline::match_link_expiry_hint(&text).is_some()
            && crate::deadline::match_action_hint(&text).is_none();
        item.remind_policy = notifier::decide_remind_policy(
            item.deadline.as_deref(),
            link_expiry,
            &now,
        );
        item.remind_at = notifier::reminder_times(&item, ctx.cfg).into_iter().next();
        match ctx.db.insert_item(&item) {
            Ok(id) => {
                if let Some(sid) = ctx.source_email_id {
                    let _ = ctx.db.set_email_item(sid, id);
                }
                let item = ctx.db.get_item(id).ok().flatten();
                ToolResult::ok_content(json!({
                    "ok": true,
                    "created": true,
                    "item_id": id,
                    "item": item.map(|i| serde_json::to_value(i).unwrap_or(json!({}))).unwrap_or(json!(null)),
                }))
            }
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

// ---------- 改/删（审批） ----------

struct UpdateItemTool;
impl ToolDef for UpdateItemTool {
    fn name(&self) -> &'static str {
        "update_item"
    }
    fn description(&self) -> &'static str {
        "修改事项（标题/联系人/事件/分类/截止时间/备注/状态）。该操作会生成审批申请，须用户批准后才生效。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "id": { "type": "integer", "description": "事项 ID" },
                "patch": {
                    "type": "object",
                    "description": "要修改的字段（仅传需要修改的字段）",
                    "properties": {
                        "title": { "type": "string" },
                        "party": { "type": "string" },
                        "event": { "type": "string" },
                        "category": { "type": "string" },
                        "deadline": { "type": ["string", "null"] },
                        "notes": { "type": "string" },
                        "status": { "type": "string", "description": "active/completed/silent/expired" },
                    },
                },
            }),
            vec!["id", "patch"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Mutate
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_item_exists(ctx.db, id) { return e; }
        let patch = args.get("patch").cloned().unwrap_or(json!({}));
        if !patch.is_object() || patch.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            return ToolResult::err("invalid_args", "patch 至少包含一个字段");
        }
        if let Some(d) = patch.get("deadline") {
            if d.is_string() && normalize_deadline(d.as_str().unwrap_or_default(), ctx.cfg.timezone).is_none() {
                return ToolResult::err("invalid_args", "patch.deadline 需为 RFC3339 或 null");
            }
        }
        let fields: Vec<&str> = patch.as_object().map(|o| o.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
        request_approval(
            ctx,
            "update_item",
            &json!({ "id": id, "patch": patch }),
            &format!("修改事项 #{id}：{}", fields.join("、")),
        )
    }

    /// 审批通过后的真正执行：应用 patch 字段。
    fn apply(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        let Ok(Some(mut item)) = ctx.db.get_item(id) else {
            return ToolResult::err("not_found", format!("事项 #{id} 不存在"));
        };
        let patch = args.get("patch").cloned().unwrap_or(json!({}));
        if let Some(v) = patch.get("title").and_then(|x| x.as_str()) {
            item.title = v.trim().to_string();
        }
        if let Some(v) = patch.get("party").and_then(|x| x.as_str()) {
            item.party = v.trim().to_string();
        }
        if let Some(v) = patch.get("event").and_then(|x| x.as_str()) {
            item.event = notifier::clamp_event(v);
        }
        if let Some(v) = patch.get("category").and_then(|x| x.as_str()) {
            item.category = v.trim().to_string();
        }
        if let Some(v) = patch.get("notes").and_then(|x| x.as_str()) {
            item.notes = v.to_string();
        }
        if let Some(d) = patch.get("deadline") {
            if d.is_null() {
                item.deadline = None;
            } else if let Some(s) = d.as_str() {
                let norm = match normalize_deadline(s, ctx.cfg.timezone) {
                    Some(n) => n,
                    None => {
                        return ToolResult::err(
                            "invalid_args",
                            "patch.deadline 需为 RFC3339 或 null",
                        )
                    }
                };
                item.deadline = Some(norm);
            }
        }
        if let Some(s) = patch.get("status").and_then(|x| x.as_str()) {
            match ItemStatus::parse(s) {
                Some(st) => item.status = st,
                None => return ToolResult::err("invalid_args", format!("非法 status: {s}")),
            }
        }
        item.remind_policy = notifier::decide_remind_policy(
            item.deadline.as_deref(),
            item.remind_policy == crate::model::remind_policy::LINK_EXPIRY,
            &ctx.clock.now().to_rfc3339(),
        );
        item.remind_at = notifier::reminder_times(&item, ctx.cfg).into_iter().next();
        item.needs_review = item.kind == ItemKind::Todo && item.deadline.is_none() && item.needs_review;
        item.updated_at = crate::db::now_str();
        match ctx.db.update_item(&item) {
            Ok(()) => ToolResult::ok_content(json!({
                "ok": true,
                "message": format!("事项 #{id} 已修改"),
                "item": serde_json::to_value(&item).unwrap_or(json!({})),
            })),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct SetItemStatusTool;
impl ToolDef for SetItemStatusTool {
    fn name(&self) -> &'static str {
        "set_item_status"
    }
    fn description(&self) -> &'static str {
        "修改事项状态（如标记完成/静默/恢复进行中）。该操作会生成审批申请，须用户批准后才生效。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "id": { "type": "integer", "description": "事项 ID" },
                "status": { "type": "string", "description": "active/completed/silent/expired" },
            }),
            vec!["id", "status"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Mutate
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_item_exists(ctx.db, id) { return e; }
        let Some(status) = jstr(&args, "status") else {
            return ToolResult::err("invalid_args", "缺少 status");
        };
        if ItemStatus::parse(&status).is_none() {
            return ToolResult::err("invalid_args", format!("非法 status: {status}"));
        }
        request_approval(
            ctx,
            "set_item_status",
            &json!({ "id": id, "status": status }),
            &format!("将事项 #{id} 状态改为 {status}"),
        )
    }

    /// 审批通过后执行状态变更。
    fn apply(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        let Some(status) = jstr(&args, "status") else {
            return ToolResult::err("invalid_args", "缺少 status");
        };
        let Some(st) = ItemStatus::parse(&status) else {
            return ToolResult::err("invalid_args", format!("非法 status: {status}"));
        };
        if let Err(e) = check_item_exists(ctx.db, id) { return e; }
        match ctx.db.set_item_status(id, st) {
            Ok(()) => ToolResult::ok_content(json!({
                "ok": true,
                "message": format!("事项 #{id} 状态已改为 {status}"),
            })),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct DeleteItemTool;
impl ToolDef for DeleteItemTool {
    fn name(&self) -> &'static str {
        "delete_item"
    }
    fn description(&self) -> &'static str {
        "删除事项（连带其发送日志）。该操作会生成审批申请，须用户批准后才生效。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({ "id": { "type": "integer", "description": "事项 ID" } }),
            vec!["id"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Mutate
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_item_exists(ctx.db, id) { return e; }
        request_approval(ctx, "delete_item", &json!({ "id": id }), &format!("删除事项 #{id}（含发送日志）"))
    }

    /// 审批通过后执行删除。
    fn apply(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_item_exists(ctx.db, id) { return e; }
        match ctx.db.delete_item(id) {
            Ok(()) => ToolResult::ok_content(json!({ "ok": true, "message": format!("事项 #{id} 已删除") })),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct UpdateEmailCategoryTool;
impl ToolDef for UpdateEmailCategoryTool {
    fn name(&self) -> &'static str {
        "update_email_category"
    }
    fn description(&self) -> &'static str {
        "修改已有邮件的分类（不用于给本次新邮件定类；新邮件分类直接在最终 JSON 的 category 字段给出）。该操作会生成审批申请，须用户批准后才生效。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({
                "id": { "type": "integer", "description": "邮件 ID" },
                "category": { "type": "string", "description": "分类 id" },
            }),
            vec!["id", "category"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Mutate
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_email_exists(ctx.db, id) { return e; }
        let Some(category) = jstr(&args, "category") else {
            return ToolResult::err("invalid_args", "缺少 category");
        };
        request_approval(
            ctx,
            "update_email_category",
            &json!({ "id": id, "category": category }),
            &format!("将邮件 #{id} 重新分类为 {category}"),
        )
    }

    /// 审批通过后执行重分类。
    fn apply(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        let Some(category) = jstr(&args, "category") else {
            return ToolResult::err("invalid_args", "缺少 category");
        };
        if let Err(e) = check_email_exists(ctx.db, id) { return e; }
        match ctx.db.update_email_category(id, &category) {
            Ok(()) => ToolResult::ok_content(json!({
                "ok": true,
                "message": format!("邮件 #{id} 已重新分类为 {category}"),
            })),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

struct DeleteEmailTool;
impl ToolDef for DeleteEmailTool {
    fn name(&self) -> &'static str {
        "delete_email"
    }
    fn description(&self) -> &'static str {
        "删除一封已入库邮件。该操作会生成审批申请，须用户批准后才生效。"
    }
    fn parameters(&self) -> Value {
        obj_schema(
            json!({ "id": { "type": "integer", "description": "邮件 ID" } }),
            vec!["id"],
        )
    }
    fn safety(&self) -> SafetyLevel {
        SafetyLevel::Mutate
    }
    fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_email_exists(ctx.db, id) { return e; }
        request_approval(ctx, "delete_email", &json!({ "id": id }), &format!("删除邮件 #{id}"))
    }

    /// 审批通过后执行删除。
    fn apply(&self, ctx: &ToolContext<'_>, args: Value) -> ToolResult {
        let Some(id) = jint(&args, "id") else {
            return ToolResult::err("invalid_args", "缺少 id");
        };
        if let Err(e) = check_email_exists(ctx.db, id) { return e; }
        match ctx.db.delete_email(id) {
            Ok(()) => ToolResult::ok_content(json!({ "ok": true, "message": format!("邮件 #{id} 已删除") })),
            Err(e) => ToolResult::err("internal", format!("{e:#}")),
        }
    }
}

// ---------- 默认注册表 ----------

/// 构建默认工具集（新增工具请在此追加注册）。
pub fn default_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(GetEmailTool));
    r.register(Arc::new(SearchEmailsTool));
    r.register(Arc::new(ListItemsTool));
    r.register(Arc::new(ListCategoriesTool));
    r.register(Arc::new(CreateCategoryTool));
    r.register(Arc::new(CreateItemTool));
    r.register(Arc::new(UpdateItemTool));
    r.register(Arc::new(SetItemStatusTool));
    r.register(Arc::new(DeleteItemTool));
    r.register(Arc::new(UpdateEmailCategoryTool));
    r.register(Arc::new(DeleteEmailTool));
    r
}
