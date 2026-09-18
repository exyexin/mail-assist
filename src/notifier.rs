//! 提醒通知：主题格式【类型/公司或个人/事件(5-10字)/截止时间(月日)】+ 邮件/webhook 发送。

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::db::Db;
use crate::mail::SmtpClient;
use crate::model::{Item, SendLog, SendStatus};

/// 主题格式：【类型/{公司,个人}(简短)/事件(5-10字以内)/截止时间(月日格式，例如0818)】
pub fn format_subject(category_label: &str, party: &str, event: &str, mmdd: &str) -> String {
    let party = sanitize_segment(party);
    let event = sanitize_segment(event);
    format!("【{category_label}/{party}/{event}/{mmdd}】")
}

/// 段内分隔符清理（/ 会破坏格式，替换为空格）
fn sanitize_segment(s: &str) -> String {
    let t = s.replace(['/', '\\', '【', '】'], " ").replace('\n', " ");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim().to_string();
    if t.is_empty() {
        "未命名".to_string()
    } else {
        t
    }
}

/// 事件控制在 5-10 字（超出截断，不足补全语义由 LLM 保证）
pub fn clamp_event(s: &str) -> String {
    let mut s = sanitize_segment(s);
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > 10 {
        s = chars[..10].iter().collect();
    }
    s
}

/// 月日格式（0818）
pub fn mmdd(dt: &DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    let local = dt.with_timezone(&tz);
    format!("{:02}{:02}", local.month(), local.day())
}

/// 分类显示名：优先数据库分类（含 Agent 运行期新建的分类），回退配置，最后回退 id。
/// （修复：运行期新建的分类此前不在 llm.yaml 中 → 提醒主题泄漏英文 id。）
pub fn category_label(db: &Db, cfg: &AppConfig, id: &str) -> String {
    if let Ok(Some(c)) = db.get_category(id) {
        if !c.label.trim().is_empty() {
            return c.label;
        }
    }
    cfg.category_label(id)
}

/// 截止时间的月日（主题用）；无法解析时回退到提醒时刻的月日。
fn deadline_mmdd(item: &Item, cfg: &AppConfig) -> String {
    if let Some(d) = &item.deadline {
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(d) {
            return mmdd(&t.with_timezone(&Utc), cfg.timezone);
        }
    }
    String::new()
}

/// 提醒邮件正文
pub fn reminder_body(item: &Item, deadline_local: String) -> String {
    let kind = match item.kind {
        crate::model::ItemKind::Todo => "待办",
        crate::model::ItemKind::Notification => "通知",
    };
    format!(
        "【mail2 提醒】{kind}事项即将到期。\n\n\
         事项：{event}\n\
         单位/联系人：{party}\n\
         截止时间：{deadline}\n\n\
         说明：{notes}\n\n\
         ——\n\
         若该事项已完成，请直接回复本邮件并包含【已完成】；\n\
         若不再需要提醒，请回复【不再提醒】。回复后系统将自动停止提醒。",
        kind = kind,
        event = if item.event.is_empty() {
            &item.title
        } else {
            &item.event
        },
        party = item.party,
        deadline = deadline_local,
        notes = if item.notes.is_empty() {
            "（无）"
        } else {
            &item.notes
        },
    )
}

/// 计算提醒计划时刻：deadline - days_before 天、当天的 lead_time。
pub fn compute_remind_at(
    deadline: &str,
    days_before: i64,
    lead_time: &str,
    tz: chrono_tz::Tz,
) -> Result<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(deadline).context("deadline 解析失败")?;
    let (h, m) = crate::config::parse_hhmm(lead_time)?;
    let local = dt.with_timezone(&tz);
    let day = local.date_naive() - chrono::Duration::days(days_before);
    let remind_local = day
        .and_hms_opt(h, m, 0)
        .ok_or_else(|| anyhow::anyhow!("提醒时刻构造失败"))?;
    let remind = tz
        .from_local_datetime(&remind_local)
        .single()
        .ok_or_else(|| anyhow::anyhow!("提醒时刻时区转换失败"))?;
    Ok(remind.with_timezone(&Utc).to_rfc3339())
}

/// 截止前 N 小时的提醒时刻（分级提醒的“临期二次提醒”）。
pub fn compute_hours_before(deadline: &str, hours: i64, tz: chrono_tz::Tz) -> Result<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(deadline).context("deadline 解析失败")?;
    let _ = tz;
    Ok((dt.with_timezone(&Utc) - chrono::Duration::hours(hours)).to_rfc3339())
}

/// 分级提醒计划（需求 3：紧急/失效类收到即提醒 + 截止前 2h；中长期 T-days_before @ lead_time）。
///
/// 返回按时间升序、去重后的提醒时刻列表（RFC3339）：
/// - `normal`：`deadline - days_before 天 @ lead_time`
/// - `urgent` / `link_expiry`：`created_at`（收到即提醒，计划时刻稳定幂等） + `deadline - 2h`
pub fn reminder_times(item: &Item, cfg: &AppConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(deadline) = item.deadline.as_deref().filter(|d| !d.is_empty()) else {
        return out;
    };
    let policy = item.remind_policy.as_str();
    let immediate_first = policy == crate::model::remind_policy::URGENT
        || policy == crate::model::remind_policy::LINK_EXPIRY;
    if immediate_first {
        let mut base = if item.created_at.trim().is_empty() {
            crate::db::now_str()
        } else {
            item.created_at.clone()
        };
        if chrono::DateTime::parse_from_rfc3339(&base).is_err() {
            base = crate::db::now_str();
        }
        out.push(base);
        if let Ok(second) = compute_hours_before(deadline, 2, cfg.timezone) {
            out.push(second);
        }
    } else if let Ok(first) = compute_remind_at(
        deadline,
        cfg.reminder.days_before,
        &cfg.reminder.lead_time,
        cfg.timezone,
    ) {
        out.push(first);
    }
    // 丢弃非法项、去重、按时间升序
    let mut parsed: Vec<(DateTime<chrono::FixedOffset>, String)> = out
        .into_iter()
        .filter_map(|s| DateTime::parse_from_rfc3339(&s).ok().map(|d| (d, s)))
        .collect();
    parsed.sort_by_key(|(d, _)| *d);
    parsed.dedup_by(|a, b| a.0 == b.0);
    parsed.into_iter().map(|(_, s)| s).collect()
}

/// 提醒策略选择：链接失效类 > 截止在 24h 内（紧急）> 常规。
pub fn decide_remind_policy(deadline: Option<&str>, link_expiry: bool, now: &str) -> String {
    use crate::model::remind_policy as rp;
    let Some(d) = deadline.filter(|d| !d.is_empty()) else {
        return rp::NORMAL.to_string();
    };
    if link_expiry {
        return rp::LINK_EXPIRY.to_string();
    }
    let (Ok(dl), Ok(now)) = (
        DateTime::parse_from_rfc3339(d),
        DateTime::parse_from_rfc3339(now),
    ) else {
        return rp::NORMAL.to_string();
    };
    let lead = dl - now;
    // 仅“尚未过期且 24 小时内到期”才算紧急；历史邮件补跑时截止时间已过去，
    // 不应标成紧急（checker 会按过期处理，避免噪音）。
    if lead > chrono::Duration::zero() && lead <= chrono::Duration::hours(24) {
        rp::URGENT.to_string()
    } else {
        rp::NORMAL.to_string()
    }
}

/// 为事项准备提醒发送记录（幂等：同一 (item, scheduled_at) 只建一次）。
/// 返回 (log, 是否新记录)。
pub fn ensure_reminder_log(
    db: &Db,
    item: &Item,
    cfg: &AppConfig,
    scheduled_at: &str,
) -> Result<(SendLog, bool)> {
    if let Some(existing) = db.find_send_log(item.id, scheduled_at)? {
        return Ok((existing, false));
    }
    let subject = format_subject(
        &category_label(db, cfg, &item.category),
        &item.party,
        &clamp_event(&item.event),
        &deadline_mmdd(item, cfg),
    );
    let body = reminder_body(item, item.deadline.clone().unwrap_or_default());
    let log = SendLog {
        id: 0,
        item_id: item.id,
        attempt: 0,
        kind: "reminder".to_string(),
        to_addr: cfg.reminder.to.clone(),
        subject,
        body,
        status: SendStatus::Pending,
        error: None,
        scheduled_at: scheduled_at.to_string(),
        sent_at: None,
        next_retry_at: None,
        message_id: None,
    };
    let id = db.insert_send_log(&log)?;
    let mut log = log;
    log.id = id;
    Ok((log, true))
}

/// 发送一条 send_log（阻塞 SMTP 调用放 spawn_blocking）。
/// 成功 → sent + message_id；失败 → failed + next_retry_at = now + 30min。
pub async fn send_log(
    db: &Db,
    smtp: &SmtpClient,
    log: &mut SendLog,
    now: chrono::DateTime<chrono::Local>,
) {
    let message_id = format!("<mail2-{}@mail2.local>", uuid::Uuid::new_v4());
    let to = log.to_addr.clone();
    let subject = log.subject.clone();
    let body = log.body.clone();
    let mid = message_id.clone();
    let smtp2 = smtp.clone();
    let result = tokio::task::spawn_blocking(move || smtp2.send(&to, &subject, &body, &mid)).await;
    match result {
        Ok(Ok(())) => {
            log.status = SendStatus::Sent;
            log.sent_at = Some(now.to_rfc3339());
            log.error = None;
            log.next_retry_at = None;
            log.message_id = Some(message_id);
            info!(
                "提醒发送成功 item={} to={} subject={}",
                log.item_id, log.to_addr, log.subject
            );
        }
        Ok(Err(e)) => {
            log.status = SendStatus::Failed;
            log.error = Some(format!("{e:#}"));
            log.next_retry_at = Some((now + chrono::Duration::minutes(30)).to_rfc3339());
            warn!("提醒发送失败 item={} err={e:#}", log.item_id);
        }
        Err(e) => {
            log.status = SendStatus::Failed;
            log.error = Some(format!("发送任务异常: {e}"));
            log.next_retry_at = Some((now + chrono::Duration::minutes(30)).to_rfc3339());
            warn!("提醒发送任务异常 item={} err={e}", log.item_id);
        }
    }
    if let Err(e) = db.update_send_log(log) {
        warn!("更新 send_log 失败: {e:#}");
    }
}

/// 可选 App 推送（webhook POST JSON {title, content}），失败仅告警不影响主流程。
pub async fn send_webhook(url: &str, title: &str, content: &str) {
    if url.trim().is_empty() {
        return;
    }
    let client = reqwest::Client::new();
    let payload = serde_json::json!({ "title": title, "content": content });
    match client
        .post(url)
        .timeout(std::time::Duration::from_secs(10))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => info!("webhook 推送成功: {title}"),
        Ok(r) => warn!("webhook 推送失败 HTTP {}: {title}", r.status()),
        Err(e) => warn!("webhook 推送异常: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;

    #[test]
    fn subject_format() {
        let s = format_subject("事务预约/待办", "字节跳动", "技术面试", "0910");
        assert_eq!(s, "【事务预约/待办/字节跳动/技术面试/0910】");
    }

    #[test]
    fn subject_format_sanitizes() {
        let s = format_subject("事务预约/待办", "A/B 公司", "面/试", "0818");
        assert_eq!(s, "【事务预约/待办/A B 公司/面 试/0818】");
    }

    #[test]
    fn mmdd_format() {
        let dt = Utc.with_ymd_and_hms(2027, 8, 18, 2, 0, 0).unwrap();
        assert_eq!(mmdd(&dt, chrono_tz::Asia::Shanghai), "0818");
    }

    fn test_cfg() -> crate::config::AppConfig {
        crate::config::AppConfig {
            listen: "127.0.0.1:0".into(),
            auth_token: String::new(),
            data_dir: std::path::PathBuf::from("/tmp"),
            mail_db: std::path::PathBuf::from("/tmp/mail.db"),
            todo_db: std::path::PathBuf::from("/tmp/todo.db"),
            timezone: chrono_tz::Asia::Shanghai,
            poll_interval_secs: 60,
            reminder: crate::config::ReminderConfig {
                to: "me@example.com".into(),
                days_before: 1,
                lead_time: "09:00".into(),
                webhook_url: String::new(),
            },
            check_times: vec!["06:00".into()],
            mail: Default::default(),
            llm: crate::config::LlmConfig {
                api_key: "k".into(),
                base_url: "http://127.0.0.1:1".into(),
                model: "mock".into(),
                timeout_secs: 5,
                categories: vec![],
                agent: Default::default(),
            },
        }
    }

    fn mk_item(policy: &str, deadline: Option<&str>) -> Item {
        Item {
            id: 1,
            kind: ItemKind::Todo,
            title: "t".into(),
            party: "某公司".into(),
            event: "在线笔试".into(),
            category: "written_test".into(),
            deadline: deadline.map(|d| d.to_string()),
            remind_at: None,
            remind_policy: policy.to_string(),
            needs_review: false,
            status: crate::model::ItemStatus::Active,
            source_email_id: None,
            account_id: 1,
            notes: String::new(),
            created_at: "2027-09-01T00:00:00+00:00".into(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn reminder_times_by_policy() {
        let cfg = test_cfg();
        // 常规：截止前 1 天 09:00（+08）
        let t = reminder_times(&mk_item("normal", Some("2027-09-10T14:00:00+08:00")), &cfg);
        assert_eq!(t, vec!["2027-09-09T01:00:00+00:00".to_string()]);
        // 紧急/失效：立即（created_at）+ 截止前 2 小时
        for policy in ["urgent", "link_expiry"] {
            let t = reminder_times(&mk_item(policy, Some("2027-09-10T14:00:00+08:00")), &cfg);
            assert_eq!(t.len(), 2, "{policy} 应有两次提醒");
            assert_eq!(t[0], "2027-09-01T00:00:00+00:00");
            assert_eq!(t[1], "2027-09-10T04:00:00+00:00");
        }
        // 无截止时间 → 无提醒计划
        assert!(reminder_times(&mk_item("normal", None), &cfg).is_empty());
    }

    #[test]
    fn decide_remind_policy_levels() {
        let now = "2027-09-01T00:00:00+00:00";
        // 24h 内 → 紧急
        assert_eq!(
            decide_remind_policy(Some("2027-09-01T10:00:00+00:00"), false, now),
            crate::model::remind_policy::URGENT
        );
        // 中长期 → 常规
        assert_eq!(
            decide_remind_policy(Some("2027-09-10T10:00:00+00:00"), false, now),
            crate::model::remind_policy::NORMAL
        );
        // 链接失效类优先
        assert_eq!(
            decide_remind_policy(Some("2027-09-10T10:00:00+00:00"), true, now),
            crate::model::remind_policy::LINK_EXPIRY
        );
        // 无截止时间 → 常规
        assert_eq!(
            decide_remind_policy(None, false, now),
            crate::model::remind_policy::NORMAL
        );
    }

    #[test]
    fn compute_remind_at_one_day_before() {
        let r = compute_remind_at(
            "2027-09-10T14:00:00+08:00",
            1,
            "09:00",
            chrono_tz::Asia::Shanghai,
        )
        .unwrap();
        // 2027-09-09 09:00 +08:00 → 2027-09-09T01:00:00Z
        assert_eq!(r, "2027-09-09T01:00:00+00:00");
    }

    #[test]
    fn clamp_event_len() {
        assert_eq!(
            clamp_event("一二三四五六七八九十十一"),
            "一二三四五六七八九十"
        );
    }

    #[test]
    fn reminder_body_contains_reply_hints() {
        let item = Item {
            id: 1,
            kind: ItemKind::Todo,
            title: "t".into(),
            party: "字节跳动".into(),
            event: "技术面试".into(),
            category: "todo".into(),
            deadline: Some("2027-09-10T14:00:00+08:00".into()),
            remind_at: None,
            remind_policy: String::new(),
            needs_review: false,
            status: crate::model::ItemStatus::Active,
            source_email_id: None,
            account_id: 1,
            notes: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let b = reminder_body(&item, "2027-09-10 14:00 (+08:00)".into());
        assert!(b.contains("【已完成】"));
        assert!(b.contains("【不再提醒】"));
    }
}
