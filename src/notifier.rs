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
        &cfg.category_label(&item.category),
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
