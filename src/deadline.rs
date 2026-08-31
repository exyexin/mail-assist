//! 截止时间推算：
//!
//! 1. **相对时限**（需求 7）：邮件含 `请在{x日, 24小时, 48小时, 72小时, 24H, within 48 hours}内完成/…`
//!    之类的文本时，按 `deadline = 发件时间(或收件时间) + 相对时限` 推算。
//!    策略：确定性正则优先（零 LLM 成本、结果可复现）。
//! 2. **人类可读明确时间**（bad case 修复）：LLM 常把 `2026-04-24 11:00(GMT+08:00)`、
//!    `2026/4/24 14:00`、`4月24日 下午2:00` 等写坏或不愿转成 RFC3339，
//!    这里做宽容解析，把正文/主题中的明确时间归一化为 RFC3339。
//!
//! 综合入口 [infer_deadline]：已提取 deadline（RFC3339）→ 明确时间 → 相对时限。

use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, TimeZone, Utc};
use chrono_tz::Tz;
use regex::Regex;
use std::sync::OnceLock;

/// 命中相对时限表达（x 日 / 24/48/72 小时 / 24H / within N hours|days + 动作词），返回该时限。
pub fn match_relative_hint(text: &str) -> Option<Duration> {
    // "请在3日内完成" / "请于7个工作日内回复" / "24H内完成" / "within 48 hours" 等
    static RE_DAYS: OnceLock<Regex> = OnceLock::new();
    static RE_HOURS: OnceLock<Regex> = OnceLock::new();
    static RE_EN: OnceLock<Regex> = OnceLock::new();
    let re_days = RE_DAYS.get_or_init(|| {
        Regex::new(
            r"\s*(\d{1,2})\s*(?:个)?\s*(?:工作|自然)?\s*[日天]\s*之?\s*内\s*(?:完成|回复|确认|提交|作答|办理|处理|操作|参加|参与|联系|反馈|登记|填写|激活|操作)",
        )
        .unwrap()
    });
    let re_hours = RE_HOURS.get_or_init(|| {
        Regex::new(
            r"\s*(\d{1,3})\s*(?:个)?\s*(?:小时|h|H|hr|HR|hours?|Hours?)\s*之?\s*内\s*(?:完成|回复|确认|提交|作答|办理|处理|操作|参加|参与|联系|反馈|登记|填写|激活|操作)",
        )
        .unwrap()
    });
    let re_en = RE_EN.get_or_init(|| {
        Regex::new(
            r"(?:(?:within|in)\s+(?P<a>\d{1,3})\s*(?P<au>hours?|days?)\s*(?:please\s+)?(?:complete|respond|reply|confirm|submit|finish|take)|(?:please\s+)?(?:complete|respond|reply|confirm|submit|finish|take)\s+[^.\n]{0,40}?\bwithin\s+(?P<b>\d{1,3})\s*(?P<bu>hours?|days?))",
        )
        .unwrap()
    });

    if let Some(caps) = re_hours.captures(text) {
        if let Ok(h) = caps[1].parse::<i64>() {
            if h > 0 && h <= 24 * 30 {
                return Some(Duration::hours(h));
            }
        }
    }
    if let Some(caps) = re_days.captures(text) {
        if let Ok(d) = caps[1].parse::<i64>() {
            if d > 0 && d <= 90 {
                return Some(Duration::days(d));
            }
        }
    }
    if let Some(caps) = re_en.captures(text) {
        let (n, unit) = match (caps.name("a"), caps.name("au")) {
            (Some(n), Some(u)) => (n.as_str(), u.as_str()),
            _ => match (caps.name("b"), caps.name("bu")) {
                (Some(n), Some(u)) => (n.as_str(), u.as_str()),
                _ => return None,
            },
        };
        let n = n.parse::<i64>().ok()?;
        if n <= 0 {
            return None;
        }
        if unit.to_lowercase().starts_with("hour") {
            if n <= 24 * 30 {
                return Some(Duration::hours(n));
            }
        } else if n <= 90 {
            return Some(Duration::days(n));
        }
    }
    None
}

/// 基于发件/收件时间推算截止时间；基准都为空/非法 → None。
/// 返回 RFC3339（UTC 归一，便于排序与提醒计算）。
pub fn infer_relative_deadline(
    text: &str,
    sent_at: Option<&str>,
    received_at: Option<&str>,
    _tz: Tz,
) -> Option<String> {
    let dur = match_relative_hint(text)?;
    let base = sent_at
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .or_else(|| received_at.and_then(|s| DateTime::parse_from_rfc3339(s).ok()))?;
    Some((base + dur).to_rfc3339())
}

/// 生成备注说明（写入 item.notes）。
pub fn inference_note(text: &str, sent_at: Option<&str>) -> Option<String> {
    let dur = match_relative_hint(text)?;
    let base = sent_at.and_then(|s| DateTime::parse_from_rfc3339(s).ok())?;
    let unit = if dur.num_hours() % 24 == 0 {
        format!("{}日", dur.num_hours() / 24)
    } else {
        format!("{}小时", dur.num_hours())
    };
    Some(format!(
        "【推算】截止时间依据邮件原文“{}”＋发件时间 {} 推算（+{unit}）",
        truncate_hint(text),
        base.with_timezone(&Utc).format("%Y-%m-%d %H:%M").to_string()
    ))
}

/// 截取命中片段用于备注展示（“内”前最多 24 个字符，含“内”本身）。
fn truncate_hint(text: &str) -> String {
    // 找到第一个 '内' 的字符序号（find 返回字节序号，需转换后再按字符截取）
    let byte_idx = text.find('内');
    let char_pos = match byte_idx {
        Some(b) => text[..b].chars().count(),
        None => return "请在X小时内完成".to_string(),
    };
    let start = char_pos.saturating_sub(24);
    let seg: String = text.chars().skip(start).take(char_pos - start + 1).collect();
    let seg = seg.trim_start().to_string();
    if !seg.is_empty() {
        seg
    } else {
        "请在X小时内完成".to_string()
    }
}

// ---------- 人类可读明确时间宽容解析 ----------

/// 尝试从文本中解析人类可读的明确时间（bad case 修复）：
/// 支持 `2026-04-24 11:00(GMT+08:00)`、`2026/4/24 14:00`、`2026年4月24日 09:30`、
/// `4月24日 14:00`（年份按当前年推断）、`下午2:00`、`截止 2026-04-24`（按当日 23:59:59）。
///
/// 返回 (RFC3339, 命中的原文片段)。解析不出 → None（绝不编造）。
pub fn parse_human_datetime(text: &str, tz: Tz) -> Option<(String, String)> {
    static RE_FULL: OnceLock<Regex> = OnceLock::new();
    static RE_MD: OnceLock<Regex> = OnceLock::new();
    static RE_DEADLINE_DATE: OnceLock<Regex> = OnceLock::new();
    static RE_AMPM: OnceLock<Regex> = OnceLock::new();

    let re_full = RE_FULL.get_or_init(|| {
        Regex::new(
            r"(?P<y>\d{4})\s*[-/年.]\s*(?P<mo>\d{1,2})\s*[-/月.]\s*(?P<d>\d{1,2})\s*[日号]?\s*(?:[T\s]\s*)?(?P<h>\d{1,2})\s*[:：-]\s*(?P<mi>\d{2})(?:\s*[:：]\s*(?P<s>\d{2}))?\s*(?:\(?(?:GMT|UTC)\s*(?P<off>[+-]\d{1,2}):?(?P<offmi>\d{2})?\)?)?",
        )
        .unwrap()
    });
    let re_md = RE_MD.get_or_init(|| {
        Regex::new(
            r"(?P<mo>\d{1,2})\s*月\s*(?P<d>\d{1,2})\s*[日号]?\s*(?P<ampm>[上下午晚上]+)?\s*(?P<h>\d{1,2})\s*[:：]\s*(?P<mi>\d{2})",
        )
        .unwrap()
    });
    let re_deadline_date = RE_DEADLINE_DATE.get_or_init(|| {
        Regex::new(
            r"(?:(?P<y>\d{4})\s*[-/年.]\s*)?(?:(?P<mo>\d{1,2})\s*月\s*(?P<d>\d{1,2})|(?P<mo2>\d{1,2})\s*[-/]\s*(?P<d2>\d{1,2}))\s*[日号]?",
        )
        .unwrap()
    });
    let re_ampm = RE_AMPM.get_or_init(|| {
        Regex::new(r"(?P<ampm>[上下午晚上]+)\s*(?P<h>\d{1,2})\s*[:：]\s*(?P<mi>\d{2})").unwrap()
    });

    // 1) 完整日期时间（含年份；可带 GMT/UTC 偏移）
    if let Some(caps) = re_full.captures(text) {
        let (y, mo, d) = (num(&caps, "y")?, num(&caps, "mo")?, num(&caps, "d")?);
        let (h, mi) = (num(&caps, "h")?, num(&caps, "mi")?);
        let s = caps.name("s").and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        if let Some((dt, snip)) = build_datetime(
            y,
            mo,
            d,
            h,
            mi,
            s,
            caps.name("off").as_ref(),
            caps.name("offmi").as_ref(),
            tz,
            &caps[0],
        ) {
            return Some((dt.to_rfc3339(), snip));
        }
    }
    // 2) M月D日 HH:MM（年份按当前年推断）
    if let Some(caps) = re_md.captures(text) {
        let mo = num(&caps, "mo")?;
        let d = num(&caps, "d")?;
        let mut h = num(&caps, "h")?;
        let mi = num(&caps, "mi")?;
        if let Some(a) = caps.name("ampm").map(|m| m.as_str()) {
            if (a.contains('下') || a.contains('晚')) && h < 12 {
                h += 12;
            }
        }
        let now = Local::now().with_timezone(&tz);
        let mut y = now.year();
        let dt_local = tz
            .with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, 0)
            .single()
            .or_else(|| tz.with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, 0).latest());
        let mut dt = dt_local?;
        // 已经过去的日期（今天之前）→ 视为下一年
        if dt < now {
            y += 1;
            dt = tz
                .with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, 0)
                .single()
                .or_else(|| tz.with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, 0).latest())?;
        }
        return Some((dt.to_rfc3339(), caps[0].to_string()));
    }
    // 3) 仅日期 + 截止语义（截止/截至/前/之前/before/deadline）→ 当日 23:59:59
    if let Some(caps) = re_deadline_date.captures(text) {
        let pre: String = text[..caps.get(0).unwrap().start()]
            .chars()
            .rev()
            .take(8)
            .collect();
        let ctx_start: String = pre.chars().rev().collect();
        let ctx_end = text[caps.get(0).unwrap().end()..]
            .chars()
            .take(8)
            .collect::<String>();
        let is_deadline = ["截止", "截至", "前", "以前", "之前", "deadline", "before"]
            .iter()
            .any(|k| ctx_start.contains(k) || ctx_end.contains(k));
        if is_deadline {
            let (mo, d) = match (caps.name("mo"), caps.name("d")) {
                (Some(m), Some(dd)) => (
                    m.as_str().parse::<i32>().ok()?,
                    dd.as_str().parse::<i32>().ok()?,
                ),
                _ => (
                    caps.name("mo2")?.as_str().parse::<i32>().ok()?,
                    caps.name("d2")?.as_str().parse::<i32>().ok()?,
                ),
            };
            let now = Local::now().with_timezone(&tz);
            let y = match caps.name("y") {
                Some(m) => m.as_str().parse::<i32>().ok()?,
                None => now.year(),
            };
            let mk = |y: i32| {
                tz.with_ymd_and_hms(y, mo as u32, d as u32, 23, 59, 59)
                    .single()
                    .or_else(|| tz.with_ymd_and_hms(y, mo as u32, d as u32, 23, 59, 59).latest())
            };
            let mut dt = mk(y)?;
            // 无年份且日期已过 → 视为下一年
            if caps.name("y").is_none() && dt < now {
                dt = mk(y + 1)?;
            }
            return Some((dt.to_rfc3339(), caps[0].to_string()));
        }
    }
    // 4) 上午/下午 HH:MM（无日期，仅当上下文含 截止/前/完成 等任务词）
    if let Some(caps) = re_ampm.captures(text) {
        let mut h = num(&caps, "h")?;
        let mi = num(&caps, "mi")?;
        let a = caps.name("ampm").map(|m| m.as_str()).unwrap_or("");
        if (a.contains('下') || a.contains('晚')) && h < 12 {
            h += 12;
        }
        let now = Local::now().with_timezone(&tz);
        let today = tz
            .with_ymd_and_hms(now.year(), now.month(), now.day(), h as u32, mi as u32, 0)
            .single()?;
        let dt = if today >= now { today } else { today + Duration::days(1) };
        return Some((dt.to_rfc3339(), caps[0].to_string()));
    }
    None
}

/// 综合截止时间推断（建项与 Agent 建项后补填共用）：
/// 1) 已提取的 deadline（RFC3339 合法）原样返回；
/// 2) 正文明确时间（[parse_human_datetime]）；
/// 3) 相对时限（[match_relative_hint]，基准 = sent_at 优先、received_at 兜底）。
/// 返回 (deadline, 备注说明)。
pub fn infer_deadline(
    body: &str,
    existing: Option<String>,
    sent_at: Option<&str>,
    received_at: Option<&str>,
    tz: Tz,
) -> (Option<String>, Option<String>) {
    if let Some(d) = existing {
        let d = d.trim().to_string();
        if !d.is_empty() && DateTime::parse_from_rfc3339(&d).is_ok() {
            return (Some(d), None);
        }
        // LLM 给出的非 RFC3339 明确时间（如 "2026-04-24 11:00"）尝试归一化
        if let Some((norm, snip)) = parse_human_datetime(&d, tz) {
            return (
                Some(norm),
                Some(format!("【推算】将提取到的时间“{snip}”归一化为 RFC3339")),
            );
        }
    }
    if let Some((dt, snip)) = parse_human_datetime(body, tz) {
        return (
            Some(dt),
            Some(format!("【推算】按邮件原文明确时间解析：“{snip}”")),
        );
    }
    if let Some(d) = infer_relative_deadline(body, sent_at, received_at, tz) {
        let note = inference_note(body, sent_at.or(received_at));
        return (Some(d), note);
    }
    (None, None)
}

/// 解析 RFC3339（宽容：接受带偏移或缺省视为本地），供其它模块复用。
#[allow(dead_code)]
pub fn parse_rfc(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).ok()
}

// ---------- 内部工具 ----------

fn num(caps: &regex::Captures<'_>, name: &str) -> Option<i32> {
    caps.name(name).and_then(|m| m.as_str().parse().ok())
}

/// 组装完整日期时间；带 GMT/UTC 偏移时用偏移构造，否则用配置时区。
#[allow(clippy::too_many_arguments)]
fn build_datetime(
    y: i32,
    mo: i32,
    d: i32,
    h: i32,
    mi: i32,
    s: i32,
    off: Option<&regex::Match<'_>>,
    offmi: Option<&regex::Match<'_>>,
    tz: Tz,
    snip: &str,
) -> Option<(DateTime<FixedOffset>, String)> {
    if let Some(o) = off {
        let sign = if o.as_str().starts_with('-') { -1 } else { 1 };
        let oh = o.as_str()[1..].parse::<i32>().ok()?;
        let om = offmi
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(0);
        let offset = FixedOffset::east_opt(sign * (oh * 3600 + om * 60))?;
        let naive = chrono::NaiveDate::from_ymd_opt(y, mo as u32, d as u32)?.and_hms_opt(h as u32, mi as u32, s as u32)?;
        return offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| (dt, snip.to_string()));
    }
    tz.with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, s as u32)
        .single()
        .or_else(|| tz.with_ymd_and_hms(y, mo as u32, d as u32, h as u32, mi as u32, s as u32).latest())
        .map(|dt| (dt.fixed_offset(), snip.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_day_hints() {
        assert_eq!(match_relative_hint("请在3日内完成笔试。").map(|d| d.num_days()), Some(3));
        assert_eq!(match_relative_hint("请于7个工作日内回复确认。").map(|d| d.num_days()), Some(7));
        assert_eq!(match_relative_hint("请在15天内提交材料").map(|d| d.num_days()), Some(15));
        assert_eq!(match_relative_hint("请在48小时内完成测评").map(|d| d.num_hours()), Some(48));
        assert_eq!(match_relative_hint("请在24小时内确认参加").map(|d| d.num_hours()), Some(24));
        assert_eq!(match_relative_hint("请在72小时内回复邮件确认").map(|d| d.num_hours()), Some(72));
    }

    #[test]
    fn matches_english_and_h_units() {
        // bad case：用户报告 “24H内完成xxx” 未记录截止时间
        assert_eq!(match_relative_hint("请在24H内完成在线测评").map(|d| d.num_hours()), Some(24));
        assert_eq!(match_relative_hint("请在72h内完成笔试作答").map(|d| d.num_hours()), Some(72));
        assert_eq!(match_relative_hint("请于48HR内确认参加").map(|d| d.num_hours()), Some(48));
        assert_eq!(match_relative_hint("24小时内完成问卷").map(|d| d.num_hours()), Some(24));
        assert_eq!(
            match_relative_hint("please complete the assessment within 48 hours").map(|d| d.num_hours()),
            Some(48)
        );
        assert_eq!(
            match_relative_hint("Please finish the test within 3 days").map(|d| d.num_days()),
            Some(3)
        );
    }

    #[test]
    fn matches_real_world_variants() {
        // bad case：真实邮件 “建议您在48小时内完成测评” （无“请”前缀）
        assert_eq!(
            match_relative_hint("建议您在48小时内完成测评").map(|d| d.num_hours()),
            Some(48)
        );
        // bad case：真实邮件 “请在7个工作日之内完成在线测评”（“之”在“内”前）
        assert_eq!(
            match_relative_hint("请在7个工作日之内完成在线测评").map(|d| d.num_days()),
            Some(7)
        );
        assert_eq!(
            match_relative_hint("建议您在24小时之内完成作答").map(|d| d.num_hours()),
            Some(24)
        );
        // 动作词缺失仍不算
        assert_eq!(match_relative_hint("物流将在48小时内送达"), None);
    }

    #[test]
    fn no_action_word_no_match() {
        // 只有时限没有动作词 → 不算
        assert_eq!(match_relative_hint("会议时长约2小时"), None);
        assert_eq!(match_relative_hint("会议时长约2小时，请准时参加"), None);
        // 无关内容
        assert_eq!(match_relative_hint("祝您生活愉快"), None);
        // 小时数超界（如 1000 小时）不匹配
        assert_eq!(match_relative_hint("请在999小时内完成"), None);
    }

    #[test]
    fn infers_deadline_from_sent_at() {
        let sent = "2027-09-01T10:00:00+08:00";
        let d = infer_relative_deadline("请在48小时内完成", Some(sent), None, chrono_tz::Asia::Shanghai)
            .unwrap();
        assert_eq!(d, "2027-09-03T10:00:00+08:00");
        let d = infer_relative_deadline("请在3日内回复", Some(sent), None, chrono_tz::Asia::Shanghai)
            .unwrap();
        assert_eq!(d, "2027-09-04T10:00:00+08:00");
    }

    #[test]
    fn falls_back_to_received_at() {
        // bad case D：无发件时间 → 用收件时间兜底
        let recv = "2027-09-01T10:00:00+08:00";
        let d = infer_relative_deadline("请在24H内完成作答", None, Some(recv), chrono_tz::Asia::Shanghai)
            .unwrap();
        assert_eq!(d, "2027-09-02T10:00:00+08:00");
    }

    #[test]
    fn no_sent_at_no_inference() {
        assert_eq!(
            infer_relative_deadline("请在48小时内完成", None, None, chrono_tz::Asia::Shanghai),
            None
        );
        assert_eq!(
            infer_relative_deadline("请在48小时内完成", Some("not-a-date"), None, chrono_tz::Asia::Shanghai),
            None
        );
    }

    #[test]
    fn note_generated() {
        let n = inference_note("请在48小时内完成", Some("2027-09-01T10:00:00+08:00")).unwrap();
        assert!(n.contains("推算"), "{n}");
        assert!(n.contains("48小时"), "{n}");
    }

    #[test]
    fn note_snippet_windows_correctly() {
        // 长正文：片段应为“内”前最多 24 字符，含“内”，且不含模板残留
        let a = "同学您好，恭喜通过简历筛选，请在72小时内完成在线测评。";
        let n = inference_note(a, Some("2027-09-01T10:00:00+08:00")).unwrap();
        assert!(n.contains("请在72小时内"), "{n}");
        assert!(!n.contains("完成内…"), "不得混入模板残留字符: {n}");
        let b = "您好，请于48小时内完成在线笔试作答，逾期视为放弃。";
        let n = inference_note(b, Some("2027-09-01T10:00:00+08:00")).unwrap();
        assert!(n.contains("请于48小时内"), "{n}");
    }

    #[test]
    fn parses_explicit_human_datetimes() {
        let tz = chrono_tz::Asia::Shanghai;
        // bad case B：LLM 常见输出形态
        let (d, snip) = parse_human_datetime("面试时间：2026-04-24 11:00(GMT+08:00)", tz).unwrap();
        assert_eq!(d, "2026-04-24T11:00:00+08:00");
        assert!(snip.contains("2026-04-24"), "{snip}");
        let (d, _) = parse_human_datetime("笔试时间 2026/4/24 14:00", tz).unwrap();
        assert_eq!(d, "2026-04-24T14:00:00+08:00");
        let (d, _) = parse_human_datetime("请于2026年4月24日 09:30前完成", tz).unwrap();
        assert_eq!(d, "2026-04-24T09:30:00+08:00");
        let (d, _) = parse_human_datetime("面试时间：2026-04-24 11-00", tz).unwrap();
        assert_eq!(d, "2026-04-24T11:00:00+08:00");
        // 截止语义的纯日期 → 当日 23:59:59
        let (d, _) = parse_human_datetime("请在2026-09-30前完成问卷", tz).unwrap();
        assert_eq!(d, "2026-09-30T23:59:59+08:00");
        let (d, _) = parse_human_datetime("截止2026-09-30", tz).unwrap();
        assert_eq!(d, "2026-09-30T23:59:59+08:00");
        // 无年份的 M月D日 + 截止语义（年份按当前年/次年推断）
        let (d, _) = parse_human_datetime("请于9月30日前完成问卷", tz).unwrap();
        assert!(d.ends_with("-09-30T23:59:59+08:00"), "{d}");
    }

    #[test]
    fn parses_month_day_and_ampm() {
        let tz = chrono_tz::Asia::Shanghai;
        let (d, _) = parse_human_datetime("4月24日 14:00 面试", tz).unwrap();
        assert!(d.contains("-04-24T14:00:00+08:00"), "{d}");
        let (d, _) = parse_human_datetime("面试时间：4月24日 下午2:00", tz).unwrap();
        assert!(d.contains("-04-24T14:00:00+08:00"), "{d}");
        let (d, _) = parse_human_datetime("请于今天 晚上 8:00 前提交", tz).unwrap();
        assert!(d.contains("T20:00:00+08:00"), "{d}");
    }

    #[test]
    fn human_datetime_no_false_positive() {
        let tz = chrono_tz::Asia::Shanghai;
        assert!(parse_human_datetime("祝您生活愉快", tz).is_none());
        assert!(parse_human_datetime("版本号 2024.10.09", tz).is_none());
        assert!(parse_human_datetime("请及时关注邮箱", tz).is_none());
    }

    #[test]
    fn infer_deadline_prefers_explicit_over_relative() {
        let body = "请在24H内完成作答，截止2026-09-30。";
        let (d, note) = infer_deadline(
            body,
            None,
            Some("2026-09-01T10:00:00+08:00"),
            None,
            chrono_tz::Asia::Shanghai,
        );
        let d = d.unwrap();
        assert!(d.starts_with("2026-09-30"), "显式日期优先于相对时限: {d}");
        assert!(note.unwrap().contains("明确时间"));
    }

    #[test]
    fn infer_deadline_normalizes_llm_deadline() {
        // LLM 提取出非 RFC3339 的明确时间 → 归一化（bad case B）
        let (d, note) = infer_deadline(
            "正文无关",
            Some("2026-04-24 11:00(GMT+08:00)".to_string()),
            None,
            None,
            chrono_tz::Asia::Shanghai,
        );
        assert_eq!(d.as_deref(), Some("2026-04-24T11:00:00+08:00"));
        assert!(note.unwrap().contains("归一化"));
    }
}
