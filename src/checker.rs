//! 检查器：每次收到新邮件时 / 每天 {06:00,12:30,18:30,00:00} 运行。
//!
//! 1. 过期清扫（需求 6）：active 且截止时间已过 → expired（不再提醒、可手动恢复）
//! 2. 扫描 active 待办：提醒时刻已到但无成功发送记录 → 补建发送（漏发补偿）
//! 3. 发送 pending / failed 且 next_retry_at 已到 → 重试（attempt+1，再失败延迟 30 分钟）
//!
//! 已完成/静默/已过期事项不产生任何提醒。

use anyhow::Result;
use chrono::{DateTime, Local};
use tracing::{debug, info, warn};

use crate::clock::Clock;
use crate::config::AppConfig;
use crate::db::{Db, ItemFilter};
use crate::mail::SmtpClient;
use crate::model::{ItemStatus, RunReport, SendStatus};
use crate::notifier;

/// 执行一轮检查。clock 注入便于测试伪造时间。
pub async fn run_check(
    db: &Db,
    smtp: &SmtpClient,
    cfg: &AppConfig,
    clock: &dyn Clock,
) -> Result<RunReport> {
    let mut report = RunReport::default();
    let now = clock.now();
    let started = std::time::Instant::now();

    // ---- 0. 过期清扫：截止时间已过的 active 事项 → expired ----
    let expired = db.mark_expired(&now.to_rfc3339())?;
    if expired > 0 {
        report.expired = expired;
        info!("过期清扫: {expired} 个事项标记为 expired");
    }

    // ---- 1. 提醒窗口：active 事项到期该提醒而尚无成功记录 ----
    let items = db.list_items(&ItemFilter {
        status: Some(ItemStatus::Active),
        limit: 2000,
        ..Default::default()
    })?;
    for item in items {
        // 只处理有截止时间的事项
        let Some(deadline) = item.deadline.clone() else {
            debug!("检查跳过: item={} 无截止时间", item.id);
            continue;
        };
        // 截止时间已过的事项不补发提醒（避免历史事项提醒轰炸；UI 可手动标记完成）
        if let Ok(dd) = DateTime::parse_from_rfc3339(&deadline) {
            if dd.with_timezone(&Local) < now {
                debug!("检查跳过: item={} 截止时间已过 deadline={deadline}", item.id);
                continue;
            }
        }
        let remind_at = match notifier::compute_remind_at(
            &deadline,
            cfg.reminder.days_before,
            &cfg.reminder.lead_time,
            cfg.timezone,
        ) {
            Ok(r) => r,
            Err(e) => {
                debug!("检查跳过: item={} 提醒时刻计算失败: {e:#}", item.id);
                continue; // deadline 非法已在提取阶段过滤，这里再兜底
            }
        };
        let remind_dt = match DateTime::parse_from_rfc3339(&remind_at) {
            Ok(d) => d.with_timezone(&Local),
            Err(_) => continue,
        };
        if remind_dt > now {
            debug!("检查跳过: item={} 提醒时刻未到 remind_at={remind_at}", item.id);
            continue; // 提醒时刻未到
        }
        let (mut log, is_new) = notifier::ensure_reminder_log(db, &item, cfg, &remind_at)?;
        if is_new {
            report.missed_compensated += 1;
            debug!("检查补建提醒记录: item={} remind_at={remind_at}（漏发补偿）", item.id);
        }
        if log.status == SendStatus::Sent {
            debug!("检查跳过: item={} 提醒已成功发送", item.id);
            continue; // 已成功发送，无需再发
        }
        if log.status == SendStatus::Failed {
            if let Some(next) = &log.next_retry_at {
                if let Ok(next_dt) = DateTime::parse_from_rfc3339(next) {
                    if next_dt.with_timezone(&Local) > now {
                        debug!("检查跳过: item={} 未到重试时间 next_retry_at={next}", item.id);
                        continue; // 未到 30 分钟重试时间
                    }
                }
            }
        }
        // 发送（pending 或到期 failed）
        log.attempt += 1;
        info!(
            "发送提醒: item={} attempt={} remind_at={remind_at} to={}",
            log.item_id, log.attempt, log.to_addr
        );
        notifier::send_log(db, smtp, &mut log, now).await;
        match log.status {
            SendStatus::Sent => report.reminders_sent += 1,
            SendStatus::Failed => report.reminders_failed += 1,
            SendStatus::Pending => {}
        }
        // 可选 webhook（成功发送提醒时同步推送一次）
        if log.status == SendStatus::Sent && !cfg.reminder.webhook_url.trim().is_empty() {
            notifier::send_webhook(&cfg.reminder.webhook_url, &log.subject, &log.body).await;
        }
    }

    // ---- 2. 待发/到期重试的 send_log（含 API 手动触发、历史 pending 等） ----
    let due = db.logs_due()?;
    for mut log in due {
        // 事项已非 active → 取消发送（已完成/静默不再提醒）
        if let Some(item) = db.get_item(log.item_id)? {
            if item.status != ItemStatus::Active {
                log.status = SendStatus::Failed;
                log.error = Some(format!("事项状态为 {}，取消发送", item.status.as_str()));
                db.update_send_log(&log)?;
                info!("取消非 active 事项的提醒发送 item={}", log.item_id);
                continue;
            }
            // 截止时间已过 → 取消补发（历史事项不轰炸）
            if let Some(deadline) = &item.deadline {
                if let Ok(dd) = DateTime::parse_from_rfc3339(deadline) {
                    if dd.with_timezone(&Local) < now {
                        log.status = SendStatus::Failed;
                        log.error = Some("截止时间已过，取消补发".into());
                        db.update_send_log(&log)?;
                        info!("取消过期事项的提醒发送 item={}", log.item_id);
                        continue;
                    }
                }
            }
        }
        log.attempt += 1;
        notifier::send_log(db, smtp, &mut log, now).await;
        match log.status {
            SendStatus::Sent => {
                report.reminders_sent += 1;
                if !cfg.reminder.webhook_url.trim().is_empty() {
                    notifier::send_webhook(&cfg.reminder.webhook_url, &log.subject, &log.body)
                        .await;
                }
            }
            SendStatus::Failed => {
                report.reminders_failed += 1;
                warn!("重试发送失败 item={} attempt={}", log.item_id, log.attempt);
            }
            SendStatus::Pending => {}
        }
    }

    info!(
        "检查完成: 漏发补偿={} 发送成功={} 发送失败={} 耗时={}ms",
        report.missed_compensated,
        report.reminders_sent,
        report.reminders_failed,
        started.elapsed().as_millis()
    );
    Ok(report)
}
