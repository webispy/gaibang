//! Wall time and sleeping, behind a trait so tests never wait.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock {
    fn now_millis(&self) -> i64;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Time moves only when slept or advanced.
pub struct FakeClock {
    now: Mutex<i64>,
    slept: Mutex<Vec<Duration>>,
}

impl FakeClock {
    pub fn new(now_millis: i64) -> FakeClock {
        FakeClock { now: Mutex::new(now_millis), slept: Mutex::new(vec![]) }
    }

    pub fn advance(&self, duration: Duration) {
        *self.now.lock().unwrap() += duration.as_millis() as i64;
    }

    pub fn slept(&self) -> Vec<Duration> {
        self.slept.lock().unwrap().clone()
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> i64 {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap().push(duration);
        self.advance(duration);
    }
}
