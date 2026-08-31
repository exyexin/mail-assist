//! 调度器：每 30 秒 tick：
//!
//! - 提醒窗口扫描（调用 checker，内部幂等）
//! - 每日检查时刻（默认 06:00/12:30/18:30/00:00，可用 config 覆盖）触发完整检查并记录日志
//!
//! 检查时刻按 kv 中的 "check_fired:<YYYY-MM-DD HH:MM>" 去重（同一天同一时刻只跑一次）。

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::checker;
use crate::clock::Clock;
use crate::config::AppConfig;
use crate::db::Db;
use crate::mail::SmtpClient;
use crate::model::RunReport;

pub const TICK_INTERVAL_SECS: u64 = 30;

/// 一次调度 tick（serve 循环调用；测试可直接调用）。
pub async fn tick(
    db: &Db,
    smtp: &SmtpClient,
    cfg: &AppConfig,
    clock: &dyn Clock,
) -> Result<RunReport> {
    let now = clock.now();
    let hm = now.format("%H:%M").to_string();
    let day_key = now.format("%Y-%m-%d").to_string();

    let mut report = RunReport::default();

    // 检查时刻触发（去重）
    if cfg.check_times.iter().any(|t| t == &hm) {
        let fired_key = format!("check_fired:{day_key} {hm}");
        if db.kv_get(&fired_key)?.is_none() {
            info!("到达每日检查时刻 {hm}，执行完整检查");
            let r = checker::run_check(db, smtp, cfg, clock).await?;
            report.missed_compensated += r.missed_compensated;
            report.reminders_sent += r.reminders_sent;
            report.reminders_failed += r.reminders_failed;
            db.kv_set(&fired_key, &now.to_rfc3339())?;
        } else {
            debug!("检查时刻 {hm} 今日已触发过，跳过");
        }
    }

    // 提醒窗口扫描（检查器内部自带幂等与时间过滤，每 tick 跑一遍保证不漏）
    let r = checker::run_check(db, smtp, cfg, clock).await?;
    report.missed_compensated += r.missed_compensated;
    report.reminders_sent += r.reminders_sent;
    report.reminders_failed += r.reminders_failed;

    debug!("调度 tick 完成: now={} 漏发补偿={} 发送成功={} 发送失败={}", now, report.missed_compensated, report.reminders_sent, report.reminders_failed);
    Ok(report)
}

/// serve 模式的后台调度循环。
pub async fn run_loop(
    db: std::sync::Arc<Db>,
    smtp: std::sync::Arc<SmtpClient>,
    cfg: std::sync::Arc<AppConfig>,
    clock: std::sync::Arc<dyn Clock>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(TICK_INTERVAL_SECS));
    loop {
        interval.tick().await;
        let started = std::time::Instant::now();
        if let Err(e) = tick(&db, &smtp, &cfg, clock.as_ref()).await {
            warn!("调度 tick 失败: {e:#}");
        }
        debug!("调度循环 tick 耗时: {}ms", started.elapsed().as_millis());
    }
}
