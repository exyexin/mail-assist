//! 邮件子系统：解析 / IMAP 收信 / SMTP 发信

mod imap;
mod parse;
mod smtp;

pub use imap::ImapClient;
pub use parse::{parse_mail, ParsedMail};
pub use smtp::SmtpClient;
