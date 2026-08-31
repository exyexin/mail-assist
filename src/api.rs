//! REST 控制接口（/api/v1，统一信封 {code,message,data}）+ 静态 Web 前端。
//! 扩展方式：新增资源 = 新路由模块 + 表；信封与版本前缀保持不变。

use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::agent;
use crate::clock::Clock;
use crate::config::AppConfig;
use crate::db::{self, Db, EmailFilter, ItemFilter};
use crate::llm::LlmClient;
use crate::mail::{ImapClient, SmtpClient};
use crate::model::{Item, ItemKind, ItemStatus, MailAccount, SendLog, SendStatus};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    pub db: Arc<Db>,
    pub smtp: Arc<SmtpClient>,
    pub llm: Arc<LlmClient>,
    pub clock: Arc<dyn Clock>,
    pub web_dir: String,
    /// 手动拉取互斥（避免并发拉取打爆 IMAP/LLM）
    pub fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    /// 供测试/组装使用的新实例（fetch_lock 自动初始化）。
    pub fn with_components(
        cfg: Arc<AppConfig>,
        db: Arc<Db>,
        smtp: Arc<SmtpClient>,
        llm: Arc<LlmClient>,
        clock: Arc<dyn Clock>,
        web_dir: String,
    ) -> Self {
        AppState {
            cfg,
            db,
            smtp,
            llm,
            clock,
            web_dir,
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// 统一响应信封
#[derive(Debug, Serialize)]
pub struct Resp<T> {
    pub code: i32,
    pub message: String,
    pub data: T,
}

impl<T: Serialize> Resp<T> {
    pub fn ok(data: T) -> Self {
        Resp {
            code: 0,
            message: "ok".into(),
            data,
        }
    }
    pub fn err(code: i32, message: impl Into<String>) -> Resp<Value> {
        Resp {
            code,
            message: message.into(),
            data: json!(null),
        }
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.0;
        let body = Json(Resp::<Value>::err(status.as_u16() as i32, self.1));
        (status, body).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}
fn db_err(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// 构建路由
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/api/v1/config", get(get_config))
        // items
        .route("/api/v1/items", get(list_items).post(create_item))
        .route(
            "/api/v1/items/{id}",
            get(get_item).put(update_item).delete(delete_item),
        )
        .route("/api/v1/items/{id}/complete", post(complete_item))
        .route("/api/v1/items/{id}/silent", post(silent_item))
        .route("/api/v1/items/{id}/activate", post(activate_item))
        .route("/api/v1/items/{id}/remind-now", post(remind_now))
        .route("/api/v1/items/batch", post(batch_items))
        // emails
        .route("/api/v1/emails", get(list_emails))
        .route("/api/v1/emails/{id}", get(get_email).delete(delete_email))
        .route("/api/v1/emails/{id}/reclassify", post(reclassify))
        .route("/api/v1/emails/{id}/label", put(update_email_label))
        .route("/api/v1/emails/batch", post(batch_emails))
        // 手动拉取
        .route("/api/v1/mail/fetch", post(manual_fetch))
        // accounts
        .route("/api/v1/accounts", get(list_accounts).post(create_account))
        .route(
            "/api/v1/accounts/{id}",
            get(get_account).put(update_account).delete(delete_account),
        )
        .route("/api/v1/accounts/{id}/test", post(test_account))
        .route("/api/v1/accounts/test", post(test_account_draft))
        .route("/api/v1/accounts/{id}/default", post(set_default_account))
        // categories（动态分类）
        .route("/api/v1/categories", get(list_categories).post(create_category))
        // 审批（Agent 删/改申请）
        .route("/api/v1/approvals", get(list_approvals))
        .route("/api/v1/approvals/{id}/approve", post(approve_approval))
        .route("/api/v1/approvals/{id}/reject", post(reject_approval))
        // 元数据
        .route("/api/v1/meta/parties", get(list_parties))
        // checker / logs
        .route("/api/v1/checker/run", post(run_checker))
        .route("/api/v1/logs", get(list_logs))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth_mw));

    Router::new()
        .route("/api/v1/health", get(health))
        .merge(protected)
        .fallback_service(tower_http::services::ServeDir::new(&state.web_dir))
        .with_state(state)
}

// ---------- 鉴权 ----------
async fn auth_mw(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if state.cfg.auth_token.trim().is_empty() {
        return next.run(req).await;
    }
    let expected = format!("Bearer {}", state.cfg.auth_token.trim());
    let ok = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "未授权：缺少或错误的 Bearer token".to_string(),
        )
        .into_response()
    }
}

// ---------- 基础 ----------
async fn health(State(state): State<AppState>) -> Json<Resp<Value>> {
    Json(Resp::ok(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": 0u64,
        "mail_address": state.cfg.mail.address,
        "timezone": state.cfg.timezone.name(),
    })))
}

async fn get_config(State(state): State<AppState>) -> Result<Json<Resp<Value>>, ApiError> {
    let c = &state.cfg;
    let categories = state
        .db
        .list_categories()
        .map_err(db_err)?
        .iter()
        .map(|x| json!({"id": x.id, "label": x.label, "create_item": x.create_item, "kind": x.kind, "source": x.source}))
        .collect::<Vec<_>>();
    let accounts = state
        .db
        .list_accounts(false)
        .map_err(db_err)?
        .iter()
        .map(|a| a.public_json())
        .collect::<Vec<_>>();
    Ok(Json(Resp::ok(json!({
        "listen": c.listen,
        "auth_enabled": !c.auth_token.trim().is_empty(),
        "data_dir": c.data_dir.to_string_lossy(),
        "timezone": c.timezone.name(),
        "poll_interval_secs": c.poll_interval_secs,
        "reminder": {
            "to": c.reminder.to,
            "days_before": c.reminder.days_before,
            "lead_time": c.reminder.lead_time,
            "webhook_enabled": !c.reminder.webhook_url.trim().is_empty(),
        },
        "check_times": c.check_times,
        "mail": {
            "address": c.mail.address,
            "imap": { "host": c.mail.imap.host, "port": c.mail.imap.port, "user": c.mail.imap.user, "password": "***" },
            "smtp": { "host": c.mail.smtp.host, "port": c.mail.smtp.port, "user": c.mail.smtp.user, "password": "***" },
        },
        "llm": {
            "base_url": c.llm.base_url,
            "model": c.llm.model,
            "api_key": "***",
            "agent": { "enabled": c.llm.agent.enabled, "max_tool_rounds": c.llm.agent.max_tool_rounds },
        },
        "categories": categories,
        "accounts": accounts,
    }))))
}

// ---------- items ----------
#[derive(Debug, Deserialize)]
pub struct ItemPayload {
    #[serde(default)]
    pub kind: String, // todo | notification
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub party: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub category: String,
    pub deadline: Option<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ItemQuery {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub category: Option<String>,
    pub party: Option<String>,
    pub q: Option<String>,
    pub account_id: Option<i64>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub deadline_after: Option<String>,
    pub deadline_before: Option<String>,
    pub limit: Option<usize>,
}

async fn list_items(
    State(state): State<AppState>,
    Query(q): Query<ItemQuery>,
) -> Result<Json<Resp<Vec<Item>>>, ApiError> {
    let f = item_filter_from_query(&q)?;
    let items = state.db.list_items(&f).map_err(db_err)?;
    Ok(Json(Resp::ok(items)))
}

fn item_filter_from_query(q: &ItemQuery) -> Result<ItemFilter, ApiError> {
    let kind = match q.kind.as_deref() {
        Some(k) => Some(ItemKind::parse(k).ok_or_else(|| bad_request(format!("非法 kind: {k}")))?),
        None => None,
    };
    let status = match q.status.as_deref() {
        Some(s) => {
            Some(ItemStatus::parse(s).ok_or_else(|| bad_request(format!("非法 status: {s}")))?)
        }
        None => None,
    };
    for (name, v) in [
        ("created_after", q.created_after.as_deref()),
        ("created_before", q.created_before.as_deref()),
        ("deadline_after", q.deadline_after.as_deref()),
        ("deadline_before", q.deadline_before.as_deref()),
    ] {
        if let Some(s) = v {
            if chrono::DateTime::parse_from_rfc3339(s).is_err() {
                return Err(bad_request(format!("{name} 需要 RFC3339 格式")));
            }
        }
    }
    Ok(ItemFilter {
        kind,
        status,
        category: q.category.clone(),
        party: q.party.clone(),
        q: q.q.clone(),
        account_id: q.account_id,
        created_after: q.created_after.clone(),
        created_before: q.created_before.clone(),
        deadline_after: q.deadline_after.clone(),
        deadline_before: q.deadline_before.clone(),
        limit: q.limit.unwrap_or(500),
    })
}

async fn create_item(
    State(state): State<AppState>,
    Json(p): Json<ItemPayload>,
) -> Result<Json<Resp<Item>>, ApiError> {
    let account_id = state.db.default_account_id().map_err(db_err)?.unwrap_or(1);
    let item = build_item(&state, &p, account_id)?;
    let id = state.db.insert_item(&item).map_err(db_err)?;
    let item = state
        .db
        .get_item(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found("刚创建的事项不存在"))?;
    Ok(Json(Resp::ok(item)))
}

fn build_item(state: &AppState, p: &ItemPayload, account_id: i64) -> Result<Item, ApiError> {
    let kind = match p.kind.as_str() {
        "" | "todo" => ItemKind::Todo,
        "notification" => ItemKind::Notification,
        k => return Err(bad_request(format!("非法 kind: {k}"))),
    };
    let deadline = match &p.deadline {
        Some(d) if !d.trim().is_empty() => {
            if chrono::DateTime::parse_from_rfc3339(d).is_err() {
                return Err(bad_request(
                    "deadline 需要 RFC3339 格式（如 2027-09-10T14:00:00+08:00）",
                ));
            }
            Some(d.clone())
        }
        _ => None,
    };
    let remind_at = match &deadline {
        Some(d) => crate::notifier::compute_remind_at(
            d,
            state.cfg.reminder.days_before,
            &state.cfg.reminder.lead_time,
            state.cfg.timezone,
        )
        .ok(),
        None => None,
    };
    let now = db::now_str();
    Ok(Item {
        id: 0,
        kind,
        title: if p.title.trim().is_empty() {
            p.event.clone()
        } else {
            p.title.clone()
        },
        party: p.party.clone(),
        event: if p.event.trim().is_empty() {
            p.title.clone()
        } else {
            p.event.clone()
        },
        category: if p.category.trim().is_empty() {
            "todo".to_string()
        } else {
            p.category.clone()
        },
        deadline,
        remind_at,
        needs_review: kind == ItemKind::Todo && p.deadline.as_deref().map(|d| d.trim().is_empty()).unwrap_or(true),
        status: ItemStatus::Active,
        source_email_id: None,
        account_id,
        notes: p.notes.clone(),
        created_at: now.clone(),
        updated_at: now,
    })
}

async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Item>>, ApiError> {
    let item = state
        .db
        .get_item(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("事项 {id} 不存在")))?;
    Ok(Json(Resp::ok(item)))
}

async fn update_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(p): Json<ItemPayload>,
) -> Result<Json<Resp<Item>>, ApiError> {
    let old = state
        .db
        .get_item(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("事项 {id} 不存在")))?;
    let mut item = build_item(&state, &p, old.account_id)?;
    item.id = id;
    item.status = old.status;
    item.source_email_id = old.source_email_id;
    item.created_at = old.created_at.clone();
    item.updated_at = db::now_str();
    item.needs_review = item.kind == ItemKind::Todo
        && item.deadline.is_none()
        && old.needs_review;
    state.db.update_item(&item).map_err(db_err)?;
    let item = state
        .db
        .get_item(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found("事项不存在"))?;
    Ok(Json(Resp::ok(item)))
}

async fn delete_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let exists = state.db.get_item(id).map_err(db_err)?.is_some();
    if !exists {
        return Err(not_found(format!("事项 {id} 不存在")));
    }
    state.db.delete_item(id).map_err(db_err)?;
    Ok(Json(Resp::ok(json!({"deleted": id}))))
}

async fn complete_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Item>>, ApiError> {
    change_status(&state, id, ItemStatus::Completed).await
}

async fn silent_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Item>>, ApiError> {
    change_status(&state, id, ItemStatus::Silent).await
}

/// 恢复进行中（从 completed/silent/expired → active）。
async fn activate_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Item>>, ApiError> {
    change_status(&state, id, ItemStatus::Active).await
}

async fn change_status(
    state: &AppState,
    id: i64,
    status: ItemStatus,
) -> Result<Json<Resp<Item>>, ApiError> {
    let exists = state.db.get_item(id).map_err(db_err)?.is_some();
    if !exists {
        return Err(not_found(format!("事项 {id} 不存在")));
    }
    state.db.set_item_status(id, status).map_err(db_err)?;
    let item = state.db.get_item(id).map_err(db_err)?.unwrap();
    Ok(Json(Resp::ok(item)))
}

// ---------- items 批量 ----------
#[derive(Debug, Deserialize, Default)]
pub struct BatchFilters {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub category: Option<String>,
    pub party: Option<String>,
    pub q: Option<String>,
    pub account_id: Option<i64>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub deadline_after: Option<String>,
    pub deadline_before: Option<String>,
    pub since: Option<String>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchPayload {
    pub ids: Option<Vec<i64>>,
    pub filters: Option<BatchFilters>,
    pub action: String,
    #[serde(default)]
    pub category: Option<String>,
}

/// 解析批量目标：ids 或 filters 至少其一。
fn batch_ids_items(db: &Db, p: &BatchPayload) -> Result<Vec<i64>, ApiError> {
    if let Some(ids) = p.ids.as_ref().filter(|v| !v.is_empty()) {
        return Ok(ids.clone());
    }
    let f = p.filters.as_ref().ok_or_else(|| bad_request("批量操作需要 ids 或 filters"))?;
    let kind = match f.kind.as_deref() {
        Some(k) => Some(ItemKind::parse(k).ok_or_else(|| bad_request(format!("非法 kind: {k}")))?),
        None => None,
    };
    let status = match f.status.as_deref() {
        Some(s) => {
            Some(ItemStatus::parse(s).ok_or_else(|| bad_request(format!("非法 status: {s}")))?)
        }
        None => None,
    };
    let items = db
        .list_items(&ItemFilter {
            kind,
            status,
            category: f.category.clone(),
            party: f.party.clone(),
            q: f.q.clone(),
            account_id: f.account_id,
            created_after: f.created_after.clone(),
            created_before: f.created_before.clone(),
            deadline_after: f.deadline_after.clone(),
            deadline_before: f.deadline_before.clone(),
            limit: 10000,
        })
        .map_err(db_err)?;
    Ok(items.iter().map(|i| i.id).collect())
}

async fn batch_items(
    State(state): State<AppState>,
    Json(p): Json<BatchPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let ids = batch_ids_items(&state.db, &p)?;
    let status = match p.action.as_str() {
        "complete" => Some(ItemStatus::Completed),
        "silent" => Some(ItemStatus::Silent),
        "activate" => Some(ItemStatus::Active),
        "delete" => None,
        other => return Err(bad_request(format!("非法 action: {other}（complete/silent/activate/delete）"))),
    };
    let changed = match status {
        Some(s) => state.db.batch_items_status(&ids, s).map_err(db_err)?,
        None => state.db.batch_items_delete(&ids).map_err(db_err)?,
    };
    Ok(Json(Resp::ok(json!({"matched": ids.len(), "changed": changed, "action": p.action}))))
}

// ---------- emails ----------
#[derive(Debug, Deserialize, Default)]
pub struct EmailQuery {
    pub category: Option<String>,
    pub q: Option<String>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub account_id: Option<i64>,
    /// 用户标注过滤（`__none__` = 未标注）
    pub label: Option<String>,
    pub limit: Option<usize>,
}

async fn list_emails(
    State(state): State<AppState>,
    Query(q): Query<EmailQuery>,
) -> Result<Json<Resp<Vec<crate::model::EmailRecord>>>, ApiError> {
    for (name, v) in [("since", q.since.as_deref()), ("before", q.before.as_deref())] {
        if let Some(s) = v {
            if chrono::DateTime::parse_from_rfc3339(s).is_err() {
                return Err(bad_request(format!("{name} 需要 RFC3339 格式")));
            }
        }
    }
    let emails = state
        .db
        .list_emails(&EmailFilter {
            category: q.category.clone(),
            q: q.q.clone(),
            since: q.since.clone(),
            before: q.before.clone(),
            account_id: q.account_id,
            label: q.label.clone(),
            limit: q.limit.unwrap_or(500),
        })
        .map_err(db_err)?;
    Ok(Json(Resp::ok(emails)))
}

async fn get_email(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<crate::model::EmailRecord>>, ApiError> {
    let email = state
        .db
        .get_email(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("邮件 {id} 不存在")))?;
    Ok(Json(Resp::ok(email)))
}

async fn delete_email(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let exists = state.db.get_email(id).map_err(db_err)?.is_some();
    if !exists {
        return Err(not_found(format!("邮件 {id} 不存在")));
    }
    state.db.delete_email(id).map_err(db_err)?;
    Ok(Json(Resp::ok(json!({"deleted": id}))))
}

#[derive(Debug, Deserialize)]
pub struct EmailLabelPayload {
    /// 标注值（空 = 清除标注）；建议使用预设值：correct / wrong_category /
    /// should_todo / should_not_todo / deadline_missing / other
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub note: String,
}

/// 保存/清除用户标注（需求：手动标注，用于后续改进分类/建待办）。
async fn update_email_label(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(p): Json<EmailLabelPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let exists = state.db.get_email(id).map_err(db_err)?.is_some();
    if !exists {
        return Err(not_found(format!("邮件 {id} 不存在")));
    }
    let label = p.label.trim().to_string();
    let note = p.note.trim().to_string();
    if label.chars().count() > 32 {
        return Err(bad_request("label 过长（最多 32 字符）"));
    }
    if note.chars().count() > 500 {
        return Err(bad_request("note 过长（最多 500 字符）"));
    }
    state
        .db
        .update_email_label(id, &label, &note)
        .map_err(db_err)?;
    Ok(Json(Resp::ok(json!({
        "email_id": id,
        "label": label,
        "note": note,
    }))))
}

/// 批量邮件：重分类 / 删除。
async fn batch_emails(
    State(state): State<AppState>,
    Json(p): Json<BatchPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let ids = if let Some(ids) = p.ids.as_ref().filter(|v| !v.is_empty()) {
        ids.clone()
    } else {
        let f = p.filters.as_ref().ok_or_else(|| bad_request("批量操作需要 ids 或 filters"))?;
        let emails = state
            .db
            .list_emails(&EmailFilter {
                category: f.category.clone(),
                q: f.q.clone(),
                since: f.since.clone(),
                before: f.before.clone(),
                account_id: f.account_id,
                label: None,
                limit: 10000,
            })
            .map_err(db_err)?;
        emails.iter().map(|e| e.id).collect::<Vec<_>>()
    };
    let changed = match p.action.as_str() {
        "reclassify" => {
            let category = p
                .category
                .as_deref()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| bad_request("reclassify 需要 category"))?;
            state
                .db
                .batch_update_email_category(&ids, &category)
                .map_err(db_err)?
        }
        "delete" => state.db.batch_delete_emails(&ids).map_err(db_err)?,
        other => return Err(bad_request(format!("非法 action: {other}（reclassify/delete）"))),
    };
    Ok(Json(Resp::ok(json!({"matched": ids.len(), "changed": changed, "action": p.action}))))
}

#[derive(Debug, Deserialize)]
pub struct ReclassifyPayload {
    pub category: String,
}

/// 手动重分类：更新分类；若分类建事务则（重新）提取并创建/更新事项。
async fn reclassify(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(p): Json<ReclassifyPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let email = state
        .db
        .get_email(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("邮件 {id} 不存在")))?;
    let category = p.category.trim().to_string();
    if category.is_empty() {
        return Err(bad_request("category 不能为空"));
    }
    state.db.update_email_category(id, &category).map_err(db_err)?;

    let cat = state.db.get_category(&category).map_err(db_err)?;
    let creates = cat
        .as_ref()
        .map(|c| c.create_item)
        .unwrap_or(category == crate::model::CATEGORY_TODO);
    let kind_todo = cat
        .as_ref()
        .map(|c| c.kind == "todo")
        .unwrap_or(category == crate::model::CATEGORY_TODO);

    let mut updated = false;
    if creates && kind_todo {
        match state
            .llm
            .extract_todo(&email.subject, &email.body_text, &email.from_addr)
            .await
        {
            Ok(ex) => {
                // 截止时间：LLM 提取 → 主题/正文明确时间 → 相对时限（bad case 修复）
                let sent = if email.sent_at.is_empty() {
                    None
                } else {
                    Some(email.sent_at.as_str())
                };
                let recv = Some(email.received_at.as_str());
                let text = format!("{} {}", email.subject, email.body_text);
                let (deadline, dnote) = crate::deadline::infer_deadline(
                    &text,
                    ex.deadline.clone(),
                    sent,
                    recv,
                    state.cfg.timezone,
                );
                let mut notes = ex.notes.clone();
                if let Some(n) = dnote {
                    notes = if notes.trim().is_empty() {
                        n
                    } else {
                        format!("{}\n{}", notes.trim(), n)
                    };
                }
                let now = db::now_str();
                let item = Item {
                    id: email.item_id.unwrap_or(0),
                    kind: ItemKind::Todo,
                    title: if ex.title.trim().is_empty() {
                        email.subject.clone()
                    } else {
                        ex.title.clone()
                    },
                    party: ex.party.clone(),
                    event: crate::notifier::clamp_event(&ex.event),
                    category: category.clone(),
                    deadline: deadline.clone(),
                    remind_at: deadline.as_ref().and_then(|d| {
                        crate::notifier::compute_remind_at(
                            d,
                            state.cfg.reminder.days_before,
                            &state.cfg.reminder.lead_time,
                            state.cfg.timezone,
                        )
                        .ok()
                    }),
                    needs_review: deadline.is_none(),
                    status: ItemStatus::Active,
                    source_email_id: Some(id),
                    account_id: email.account_id,
                    notes,
                    created_at: now.clone(),
                    updated_at: now,
                };
                if item.id > 0 {
                    state.db.update_item(&item).map_err(db_err)?;
                } else {
                    let nid = state.db.insert_item(&item).map_err(db_err)?;
                    state.db.set_email_item(id, nid).map_err(db_err)?;
                }
                updated = true;
            }
            Err(e) => {
                return Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    format!("LLM 提取失败: {e:#}"),
                ))
            }
        }
    } else if creates {
        // 通知类不再自动创建事项（需求：通知/反馈式邮件不纳入待办管理，
        // 历史 notification 事项保留，可手动创建）。
        info!(
            "重分类为通知类，不创建事项 email={id} category={category} kind={}",
            cat.as_ref().map(|c| c.kind.as_str()).unwrap_or("notification")
        );
    }
    Ok(Json(Resp::ok(
        json!({"email_id": id, "category": category, "item_updated": updated}),
    )))
}

// ---------- 手动拉取（需求 1） ----------
#[derive(Debug, Deserialize, Default)]
pub struct FetchPayload {
    pub account_id: Option<i64>,
    /// RFC3339 或 YYYY-MM-DD（"从 xxxx 后"）
    pub since: Option<String>,
    /// 3d | 7d | 15d | 30d（近 N 天）
    pub range: Option<String>,
    pub limit: Option<usize>,
}

async fn manual_fetch(
    State(state): State<AppState>,
    Json(p): Json<FetchPayload>,
) -> Result<Json<Resp<crate::model::RunReport>>, ApiError> {
    // 互斥：一次只跑一个手动拉取
    let _guard = state
        .fetch_lock
        .try_lock()
        .map_err(|_| ApiError(StatusCode::CONFLICT, "已有拉取任务进行中，请稍后".to_string()))?;

    // 账户解析：显式 > 默认 > 第一个启用
    let account = if let Some(id) = p.account_id {
        state
            .db
            .get_account(id)
            .map_err(db_err)?
            .ok_or_else(|| not_found(format!("账户 {id} 不存在")))?
    } else {
        let id = state.db.default_account_id().map_err(db_err)?;
        match id {
            Some(i) => state.db.get_account(i).map_err(db_err)?.unwrap(),
            None => state
                .db
                .list_accounts(true)
                .map_err(db_err)?
                .into_iter()
                .next()
                .ok_or_else(|| bad_request("没有可用邮箱账户，请先在“账户”页添加"))?,
        }
    };

    // 时间范围：range 优先；否则显式 since
    let now = state.clock.now().fixed_offset();
    let since: Option<chrono::DateTime<chrono::FixedOffset>> = match p.range.as_deref() {
        Some(r) => {
            let days: i64 = match r {
                "3d" => 3,
                "7d" => 7,
                "15d" => 15,
                "30d" => 30,
                other => return Err(bad_request(format!("非法 range: {other}（3d/7d/15d/30d）"))),
            };
            Some(now - chrono::Duration::days(days))
        }
        None => match p.since.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(s) => {
                let dt = if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
                    d
                } else if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                    let local = d.and_hms_opt(0, 0, 0).unwrap();
                    state
                        .cfg
                        .timezone
                        .from_local_datetime(&local)
                        .single()
                        .ok_or_else(|| bad_request("since 时区转换失败"))?
                        .fixed_offset()
                } else {
                    return Err(bad_request("since 需要 RFC3339 或 YYYY-MM-DD 格式"));
                };
                Some(dt)
            }
            None => None,
        },
    };

    let imap = ImapClient::from_config(&account.imap_endpoint());
    let smtp = SmtpClient::from_config(&account.smtp_endpoint(), &account.address)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("SMTP 客户端构建失败: {e:#}")))?;
    let report = crate::pipeline::process_new_mails_opts(
        &state.db,
        &imap,
        &smtp,
        &state.llm,
        &state.cfg,
        state.clock.as_ref(),
        account.id,
        crate::pipeline::ProcessOpts {
            limit: p.limit,
            run_check: true,
            since,
            advance_cursor: false,
        },
    )
    .await
    .map_err(internal)?;
    Ok(Json(Resp::ok(report)))
}

// ---------- accounts ----------
#[derive(Debug, Deserialize, Default)]
pub struct AccountPayload {
    pub label: Option<String>,
    pub address: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_user: Option<String>,
    pub imap_password: Option<String>,
    pub imap_tls_insecure: Option<bool>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_user: Option<String>,
    pub smtp_password: Option<String>,
    pub smtp_tls_insecure: Option<bool>,
    pub enabled: Option<bool>,
    pub reminder_to: Option<String>,
}

fn validate_account_payload(p: &AccountPayload, require_password: bool) -> Result<(), ApiError> {
    if let Some(a) = p.address.as_deref() {
        if a.trim().is_empty() {
            return Err(bad_request("邮箱地址不能为空"));
        }
    }
    if let Some(h) = p.imap_host.as_deref() {
        if h.trim().is_empty() {
            return Err(bad_request("IMAP 服务器不能为空"));
        }
    }
    if let Some(h) = p.smtp_host.as_deref() {
        if h.trim().is_empty() {
            return Err(bad_request("SMTP 服务器不能为空"));
        }
    }
    if require_password {
        let ok = p
            .imap_password
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
            && p.smtp_password
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if !ok {
            return Err(bad_request("新建账户需要填写 IMAP/SMTP 密码"));
        }
    }
    Ok(())
}

async fn list_accounts(State(state): State<AppState>) -> Result<Json<Resp<Vec<Value>>>, ApiError> {
    let accounts = state.db.list_accounts(false).map_err(db_err)?;
    Ok(Json(Resp::ok(
        accounts.iter().map(|a| a.public_json()).collect(),
    )))
}

async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let a = state
        .db
        .get_account(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("账户 {id} 不存在")))?;
    Ok(Json(Resp::ok(a.public_json())))
}

async fn create_account(
    State(state): State<AppState>,
    Json(p): Json<AccountPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    validate_account_payload(&p, true)?;
    let now = db::now_str();
    let account = MailAccount {
        id: 0,
        label: p.label.clone().unwrap_or_else(|| "邮箱账户".into()),
        address: p.address.clone().unwrap_or_default(),
        imap_host: p.imap_host.clone().unwrap_or_default(),
        imap_port: p.imap_port.unwrap_or(993),
        imap_user: p.imap_user.clone().unwrap_or_default(),
        imap_password: p.imap_password.clone().unwrap_or_default(),
        imap_tls_insecure: p.imap_tls_insecure.unwrap_or(false),
        smtp_host: p.smtp_host.clone().unwrap_or_default(),
        smtp_port: p.smtp_port.unwrap_or(465),
        smtp_user: p.smtp_user.clone().unwrap_or_default(),
        smtp_password: p.smtp_password.clone().unwrap_or_default(),
        smtp_tls_insecure: p.smtp_tls_insecure.unwrap_or(false),
        enabled: p.enabled.unwrap_or(true),
        is_default: false,
        reminder_to: p.reminder_to.clone().unwrap_or_default(),
        created_at: now.clone(),
        updated_at: now,
    };
    let id = state.db.insert_account(&account).map_err(db_err)?;
    // 首个账户自动设为默认
    if state.db.default_account_id().map_err(db_err)?.is_none() {
        state.db.set_default_account(id).map_err(db_err)?;
    }
    let a = state.db.get_account(id).map_err(db_err)?.unwrap();
    Ok(Json(Resp::ok(a.public_json())))
}

async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(p): Json<AccountPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let mut a = state
        .db
        .get_account(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("账户 {id} 不存在")))?;
    validate_account_payload(&p, false)?;
    if let Some(v) = p.label { a.label = v; }
    if let Some(v) = p.address { a.address = v; }
    if let Some(v) = p.imap_host { a.imap_host = v; }
    if let Some(v) = p.imap_port { a.imap_port = v; }
    if let Some(v) = p.imap_user { a.imap_user = v; }
    if let Some(v) = p.smtp_host { a.smtp_host = v; }
    if let Some(v) = p.smtp_port { a.smtp_port = v; }
    if let Some(v) = p.smtp_user { a.smtp_user = v; }
    if let Some(v) = p.imap_tls_insecure { a.imap_tls_insecure = v; }
    if let Some(v) = p.smtp_tls_insecure { a.smtp_tls_insecure = v; }
    if let Some(v) = p.enabled { a.enabled = v; }
    if let Some(v) = p.reminder_to { a.reminder_to = v; }
    // 密码："***"/空 = 保持不变
    if let Some(v) = p.imap_password {
        if !v.trim().is_empty() && v != "***" {
            a.imap_password = v;
        }
    }
    if let Some(v) = p.smtp_password {
        if !v.trim().is_empty() && v != "***" {
            a.smtp_password = v;
        }
    }
    a.updated_at = db::now_str();
    state.db.update_account(&a).map_err(db_err)?;
    let a = state.db.get_account(id).map_err(db_err)?.unwrap();
    Ok(Json(Resp::ok(a.public_json())))
}

async fn delete_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let accounts = state.db.list_accounts(false).map_err(db_err)?;
    if accounts.len() <= 1 {
        return Err(bad_request("至少保留一个邮箱账户"));
    }
    if !accounts.iter().any(|a| a.id == id) {
        return Err(not_found(format!("账户 {id} 不存在")));
    }
    let was_default = accounts.iter().find(|a| a.id == id).map(|a| a.is_default).unwrap_or(false);
    state.db.delete_account(id).map_err(db_err)?;
    if was_default {
        if let Some(first) = state.db.list_accounts(false).map_err(db_err)?.into_iter().next() {
            state.db.set_default_account(first.id).map_err(db_err)?;
        }
    }
    Ok(Json(Resp::ok(json!({"deleted": id}))))
}

async fn set_default_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let exists = state.db.get_account(id).map_err(db_err)?.is_some();
    if !exists {
        return Err(not_found(format!("账户 {id} 不存在")));
    }
    state.db.set_default_account(id).map_err(db_err)?;
    Ok(Json(Resp::ok(json!({"default": id}))))
}

/// 连接测试：按请求体临时配置测试 IMAP 登录 + SELECT（未保存的账户）。
async fn test_account_draft(
    State(_state): State<AppState>,
    Json(p): Json<AccountPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    validate_account_payload(&p, true)?;
    let now = db::now_str();
    let account = MailAccount {
        id: 0,
        label: p.label.clone().unwrap_or_else(|| "邮箱账户".into()),
        address: p.address.clone().unwrap_or_default(),
        imap_host: p.imap_host.clone().unwrap_or_default(),
        imap_port: p.imap_port.unwrap_or(993),
        imap_user: p.imap_user.clone().unwrap_or_default(),
        imap_password: p.imap_password.clone().unwrap_or_default(),
        imap_tls_insecure: p.imap_tls_insecure.unwrap_or(false),
        smtp_host: p.smtp_host.clone().unwrap_or_default(),
        smtp_port: p.smtp_port.unwrap_or(465),
        smtp_user: p.smtp_user.clone().unwrap_or_default(),
        smtp_password: p.smtp_password.clone().unwrap_or_default(),
        smtp_tls_insecure: p.smtp_tls_insecure.unwrap_or(false),
        enabled: true,
        is_default: false,
        reminder_to: p.reminder_to.clone().unwrap_or_default(),
        created_at: now.clone(),
        updated_at: now,
    };
    let imap = ImapClient::from_config(&account.imap_endpoint());
    match imap.test().await {
        Ok(()) => Ok(Json(Resp::ok(json!({"ok": true, "message": "IMAP 连接成功"})))),
        Err(e) => Ok(Json(Resp::ok(json!({"ok": false, "message": format!("{e:#}")})))),
    }
}

/// 连接测试：按存储配置（或请求体中临时配置）测试 IMAP 登录 + SELECT。
async fn test_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<AccountPayload>>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let stored = state
        .db
        .get_account(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("账户 {id} 不存在")))?;
    let account = match body {
        Some(Json(p)) => {
            // 用请求体里的字段覆盖存储配置（未提供密码沿用存储）
            let mut a = stored.clone();
            if let Some(v) = p.label { a.label = v; }
            if let Some(v) = p.address { a.address = v; }
            if let Some(v) = p.imap_host { a.imap_host = v; }
            if let Some(v) = p.imap_port { a.imap_port = v; }
            if let Some(v) = p.imap_user { a.imap_user = v; }
            if let Some(v) = p.smtp_host { a.smtp_host = v; }
            if let Some(v) = p.smtp_port { a.smtp_port = v; }
            if let Some(v) = p.smtp_user { a.smtp_user = v; }
            if let Some(v) = p.imap_tls_insecure { a.imap_tls_insecure = v; }
            if let Some(v) = p.smtp_tls_insecure { a.smtp_tls_insecure = v; }
            if let Some(v) = p.imap_password {
                if !v.trim().is_empty() && v != "***" { a.imap_password = v; }
            }
            if let Some(v) = p.smtp_password {
                if !v.trim().is_empty() && v != "***" { a.smtp_password = v; }
            }
            a
        }
        None => stored,
    };
    let imap = ImapClient::from_config(&account.imap_endpoint());
    match imap.test().await {
        Ok(()) => Ok(Json(Resp::ok(json!({"ok": true, "message": "IMAP 连接成功"})))),
        Err(e) => Ok(Json(Resp::ok(json!({"ok": false, "message": format!("{e:#}")})))),
    }
}

// ---------- categories ----------
#[derive(Debug, Deserialize)]
pub struct CategoryPayload {
    pub id: String,
    pub label: String,
    #[serde(default = "default_true")]
    pub create_item: bool,
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_true() -> bool {
    true
}
fn default_kind() -> String {
    "todo".into()
}

async fn list_categories(State(state): State<AppState>) -> Result<Json<Resp<Vec<Value>>>, ApiError> {
    let cats = state.db.list_categories().map_err(db_err)?;
    Ok(Json(Resp::ok(
        cats.iter()
            .map(|c| {
                json!({"id": c.id, "label": c.label, "create_item": c.create_item, "kind": c.kind, "source": c.source})
            })
            .collect(),
    )))
}

async fn create_category(
    State(state): State<AppState>,
    Json(p): Json<CategoryPayload>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let id = p.id.trim().to_string();
    let label = p.label.trim().to_string();
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        || id.is_empty()
        || id.len() > 32
    {
        return Err(bad_request("id 需为 1-32 位小写字母/数字/下划线/连字符"));
    }
    if label.is_empty() {
        return Err(bad_request("label 不能为空"));
    }
    let kind = if p.kind == "todo" || p.kind == "notification" {
        p.kind
    } else {
        return Err(bad_request("kind 只能为 todo 或 notification"));
    };
    let created = state
        .db
        .upsert_category(&id, &label, p.create_item, &kind, "user")
        .map_err(db_err)?;
    let c = state.db.get_category(&id).map_err(db_err)?.unwrap();
    Ok(Json(Resp::ok(json!({
        "created": created,
        "category": {"id": c.id, "label": c.label, "create_item": c.create_item, "kind": c.kind, "source": c.source},
    }))))
}

// ---------- 审批（需求 9.1：删/改须用户许可） ----------
#[derive(Debug, Deserialize, Default)]
pub struct ApprovalQuery {
    pub status: Option<String>,
    pub limit: Option<usize>,
}

async fn list_approvals(
    State(state): State<AppState>,
    Query(q): Query<ApprovalQuery>,
) -> Result<Json<Resp<Vec<crate::model::Approval>>>, ApiError> {
    if let Some(s) = q.status.as_deref() {
        if !["pending", "approved", "rejected"].contains(&s) {
            return Err(bad_request(format!("非法 status: {s}")));
        }
    }
    let approvals = state
        .db
        .list_approvals(q.status.as_deref(), q.limit.unwrap_or(200))
        .map_err(db_err)?;
    Ok(Json(Resp::ok(approvals)))
}

async fn approve_approval(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let approval = state
        .db
        .get_approval(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("审批单 {id} 不存在")))?;
    if approval.status != "pending" {
        return Err(bad_request(format!("审批单 {id} 已处理（{}）", approval.status)));
    }
    // 重放执行（重新校验对象存在性）
    let registry = agent::tools::default_registry();
    let args: Value = serde_json::from_str(&approval.payload_json)
        .map_err(|e| bad_request(format!("审批参数损坏: {e}")))?;
    let ctx = agent::registry::ToolContext {
        db: &state.db,
        cfg: &state.cfg,
        clock: state.clock.as_ref(),
        source_email_id: approval.source_email_id,
        account_id: 1,
    };
    let result = match registry.get(&approval.tool_name) {
        Some(tool) if tool.safety() == agent::registry::SafetyLevel::Mutate => tool.apply(&ctx, args),
        _ => agent::registry::ToolResult::err("internal", "未知或非审批类工具"),
    };
    let decided_at = db::now_str();
    if result.ok {
        state.db.decide_approval(id, "approved", &decided_at).map_err(db_err)?;
        Ok(Json(Resp::ok(json!({
            "approved": true,
            "approval_id": id,
            "tool": approval.tool_name,
        }))))
    } else {
        state.db.decide_approval(id, "rejected", &decided_at).map_err(db_err)?;
        Ok(Json(Resp::ok(json!({
            "approved": false,
            "approval_id": id,
            "tool": approval.tool_name,
            "reason": result.content,
        }))))
    }
}

async fn reject_approval(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<Value>>, ApiError> {
    let approval = state
        .db
        .get_approval(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("审批单 {id} 不存在")))?;
    if approval.status != "pending" {
        return Err(bad_request(format!("审批单 {id} 已处理（{}）", approval.status)));
    }
    state
        .db
        .decide_approval(id, "rejected", &db::now_str())
        .map_err(db_err)?;
    Ok(Json(Resp::ok(json!({"rejected": true, "approval_id": id}))))
}

// ---------- 元数据 ----------
async fn list_parties(State(state): State<AppState>) -> Result<Json<Resp<Vec<String>>>, ApiError> {
    let parties = state.db.distinct_parties(500).map_err(db_err)?;
    Ok(Json(Resp::ok(parties)))
}

/// 立即发送一次提醒（手动触发；不影响原定时提醒计划）。
async fn remind_now(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Resp<SendLog>>, ApiError> {
    let item = state
        .db
        .get_item(id)
        .map_err(db_err)?
        .ok_or_else(|| not_found(format!("事项 {id} 不存在")))?;
    if item.status != ItemStatus::Active {
        return Err(bad_request("事项非 active 状态，不发送提醒"));
    }
    let now = state.clock.now();
    let scheduled_at = now.to_rfc3339();
    let subject = crate::notifier::format_subject(
        &state.cfg.category_label(&item.category),
        &item.party,
        &crate::notifier::clamp_event(&item.event),
        &crate::notifier::mmdd(&now.with_timezone(&chrono::Utc), state.cfg.timezone),
    );
    let mut log = SendLog {
        id: 0,
        item_id: item.id,
        attempt: 0,
        kind: "manual".into(),
        to_addr: state.cfg.reminder.to.clone(),
        subject,
        body: crate::notifier::reminder_body(&item, item.deadline.clone().unwrap_or_default()),
        status: SendStatus::Pending,
        error: None,
        scheduled_at: scheduled_at.clone(),
        sent_at: None,
        next_retry_at: None,
        message_id: None,
    };
    let log_id = state.db.insert_send_log(&log).map_err(db_err)?;
    log.id = log_id;
    crate::notifier::send_log(&state.db, &state.smtp, &mut log, now).await;
    let log = state
        .db
        .find_send_log(item.id, &scheduled_at)
        .map_err(db_err)?
        .ok_or_else(|| not_found("发送记录不存在"))?;
    Ok(Json(Resp::ok(log)))
}

// ---------- checker / logs ----------
async fn run_checker(
    State(state): State<AppState>,
) -> Result<Json<Resp<crate::model::RunReport>>, ApiError> {
    let report =
        crate::checker::run_check(&state.db, &state.smtp, &state.cfg, state.clock.as_ref())
            .await
            .map_err(internal)?;
    Ok(Json(Resp::ok(report)))
}

#[derive(Debug, Deserialize, Default)]
pub struct LogQuery {
    pub item_id: Option<i64>,
    pub status: Option<String>,
    pub limit: Option<usize>,
}

async fn list_logs(
    State(state): State<AppState>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Resp<Vec<SendLog>>>, ApiError> {
    let status = match q.status.as_deref() {
        Some(s) => {
            Some(SendStatus::parse(s).ok_or_else(|| bad_request(format!("非法 status: {s}")))?)
        }
        None => None,
    };
    let logs = state
        .db
        .list_send_logs(q.item_id, status, q.limit.unwrap_or(200))
        .map_err(db_err)?;
    Ok(Json(Resp::ok(logs)))
}
