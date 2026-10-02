//! Time source port. The domain and handlers never call `Utc::now()` directly;
//! they ask a `Clock`. This keeps time-dependent behaviour (the timer #14,
//! reminders #22, budget alerts #30) deterministic under test via `FixedClock`.

use chrono::{DateTime, NaiveDate, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn today(&self) -> NaiveDate {
        self.now().date_naive()
    }
}

/// Production clock: the system wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Test/frozen clock: a fixed instant. Shipped (not `cfg(test)`) so the
/// integration-test crate and any deterministic deployment can use it.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}
