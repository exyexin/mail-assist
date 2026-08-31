//! RFC822/MIME 邮件解析：主题、收发件人、正文（text/plain 优先，HTML 降级纯文本）、
//! In-Reply-To / References（用于提醒回复识别）。

use anyhow::{Context, Result};
use mailparse::{parse_mail as mp_parse, MailHeaderMap};

#[derive(Debug, Clone, Default)]
pub struct ParsedMail {
    pub message_id: String,
    pub subject: String,
    pub from_addr: String,
    pub from_name: String,
    pub body_text: String,
    pub in_reply_to: String,
    pub references: String,
    pub date: String,
    /// 发件时间（Date 头解析，RFC3339；解析失败为空字符串）
    pub sent_at: String,
}

pub fn parse_mail(raw: &[u8]) -> Result<ParsedMail> {
    let parsed = mp_parse(raw).context("MIME 解析失败")?;
    let mut out = ParsedMail::default();

    let headers = parsed.get_headers();
    out.message_id = headers
        .get_first_value("Message-ID")
        .unwrap_or_default()
        .trim_matches('<')
        .trim_matches('>')
        .to_string();
    out.subject = decode_rfc2047(&headers.get_first_value("Subject").unwrap_or_default());
    let from = headers.get_first_value("From").unwrap_or_default();
    (out.from_name, out.from_addr) = parse_from(&from);
    out.in_reply_to = headers.get_first_value("In-Reply-To").unwrap_or_default();
    out.references = headers.get_first_value("References").unwrap_or_default();
    out.date = headers.get_first_value("Date").unwrap_or_default();
    // 发件时间：RFC2822 Date 头 → RFC3339；解析失败留空（由入库方回退收件时间）
    out.sent_at = parse_sent_at(&out.date);

    // 正文：优先 text/plain；否则 text/html 去标签；否则全拼接
    out.body_text = extract_body(&parsed);

    if out.message_id.is_empty() {
        // 无 Message-ID（某些服务器/客户端）——用 subject+date+from 的稳定指纹
        out.message_id = format!(
            "fingerprint-{}",
            format!("{}|{}|{}", out.subject, out.date, out.from_addr)
                .chars()
                .map(|c| c as u32)
                .sum::<u32>()
        );
    }
    Ok(out)
}

/// RFC2822 Date 头 → RFC3339；无法解析返回空串。
/// mailparse::dateparse 返回 epoch 秒（i64），转为 UTC RFC3339。
pub fn parse_sent_at(date: &str) -> String {
    if date.trim().is_empty() {
        return String::new();
    }
    match mailparse::dateparse(date) {
        Ok(ts) => chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// "Name <addr>" / "addr" → (name, addr)
fn parse_from(from: &str) -> (String, String) {
    let from = from.trim();
    if let Some(lt) = from.rfind('<') {
        let addr = from[lt + 1..].trim_end_matches('>').trim().to_string();
        let name = from[..lt].trim().trim_matches('"').to_string();
        (name, addr)
    } else {
        (String::new(), from.to_string())
    }
}

/// 解码 RFC2047 编码的主题（=?UTF-8?B?...?= / =?UTF-8?Q?...?=）
pub fn decode_rfc2047(s: &str) -> String {
    // B/Q 编码文本本身不含 '?'（Q 编码中 '?' 写作 =3F，B 用 base64 字母表），
    // 因此 [^?]* 可以安全匹配到真正的终止符 ?=。
    let re = regex::Regex::new(r"=\?([^?]+)\?([bBqQ])\?([^?]*)\?=").unwrap();
    re.replace_all(s, |caps: &regex::Captures| {
        let charset = caps[1].to_string();
        let enc = caps[2].to_ascii_uppercase();
        let text = caps[3].to_string();
        let decoded = if enc == "B" {
            decode_b(&text)
        } else {
            decode_q(&text)
        };
        if charset.eq_ignore_ascii_case("utf-8")
            || charset.eq_ignore_ascii_case("gbk")
            || charset.eq_ignore_ascii_case("gb2312")
        {
            decoded
        } else {
            text
        }
    })
    .into_owned()
}

fn decode_b(s: &str) -> String {
    use base64::Engine;
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    match base64::engine::general_purpose::STANDARD.decode(cleaned) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => s.to_string(),
    }
}

fn decode_q(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() - 1 + 1 => {
                if i + 2 < bytes.len() {
                    if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                        out.push(v);
                        i += 3;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_body(parsed: &mailparse::ParsedMail) -> String {
    fn walk(part: &mailparse::ParsedMail, plain: &mut Option<String>, html: &mut Option<String>) {
        if part.subparts.is_empty() {
            let ctype = part.ctype.mimetype.to_lowercase();
            let body = part.get_body().unwrap_or_default();
            match ctype.as_str() {
                "text/plain" if plain.is_none() => {
                    let t = body.trim().to_string();
                    if !t.is_empty() {
                        *plain = Some(t);
                    }
                }
                "text/html" if html.is_none() => {
                    let t = strip_html(&body);
                    if !t.is_empty() {
                        *html = Some(t);
                    }
                }
                _ => {}
            }
        } else {
            for p in &part.subparts {
                walk(p, plain, html);
            }
        }
    }
    let mut plain = None;
    let mut html = None;
    walk(parsed, &mut plain, &mut html);
    plain.or(html).unwrap_or_default()
}

/// HTML → 纯文本（简单去标签；保留换行）
pub fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut text = String::new();
    for line in out.lines() {
        let line = line.trim();
        if !line.is_empty() {
            text.push_str(line);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2047_base64_subject() {
        assert_eq!(decode_rfc2047("=?UTF-8?B?6Z2i6K+V6YKA6K+3?="), "面试邀请");
        assert_eq!(
            decode_rfc2047("Re: =?UTF-8?B?6Z2i6K+V6YKA6K+3?= 已确认"),
            "Re: 面试邀请 已确认"
        );
    }

    #[test]
    fn rfc2047_q_subject() {
        assert_eq!(decode_rfc2047("=?UTF-8?Q?=E9=9D=A2=E8=AF=95?="), "面试");
    }

    #[test]
    fn parse_typical_mail() {
        let raw = "From: HR <hr@acme.com>\r\nTo: me@local\r\nSubject: =?UTF-8?B?6Z2i6K+V6YKA6K+3?=\r\nMessage-ID: <abc@acme.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n你好，请参加面试。".as_bytes();
        let m = parse_mail(raw).unwrap();
        assert_eq!(m.message_id, "abc@acme.com");
        assert_eq!(m.subject, "面试邀请");
        assert_eq!(m.from_addr, "hr@acme.com");
        assert!(m.body_text.contains("参加面试"));
    }

    #[test]
    fn html_body_fallback() {
        let raw = b"From: a@b.c\r\nSubject: x\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>line1</p><p>line2</p></body></html>";
        let m = parse_mail(raw).unwrap();
        assert!(m.body_text.contains("line1"));
        assert!(m.body_text.contains("line2"));
    }
}
