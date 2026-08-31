//! 可注入时钟：生产用系统时间，测试用 FakeClock 伪造。

use chrono::{DateTime, Duration, Local};
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Local>;
}

/// 系统时钟
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// 测试用假时钟
pub struct FakeClock {
    inner: Mutex<DateTime<Local>>,
}

impl FakeClock {
    pub fn new(t: DateTime<Local>) -> Self {
        FakeClock {
            inner: Mutex::new(t),
        }
    }

    pub fn set(&self, t: DateTime<Local>) {
        *self.inner.lock().unwrap() = t;
    }

    pub fn advance(&self, d: Duration) {
        let mut g = self.inner.lock().unwrap();
        *g += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Local> {
        *self.inner.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn fake_clock_works() {
        let t = Local.with_ymd_and_hms(2027, 9, 10, 12, 0, 0).unwrap();
        let c = FakeClock::new(t);
        assert_eq!(c.now(), t);
        c.advance(Duration::minutes(30));
        assert_eq!(c.now(), t + Duration::minutes(30));
    }
}
