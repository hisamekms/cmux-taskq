//! The system's clock and UUIDs behind the application's [`Clock`] and
//! [`IdGenerator`] ports.

use std::{sync::Arc, time::SystemTime};

use uuid::Uuid;

use crate::application::{Clock, Generators, IdGenerator};

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn system_time(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Random (version 4) UUIDs.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidGenerator;

impl IdGenerator for UuidGenerator {
    fn uuid(&self) -> String {
        Uuid::new_v4().to_string()
    }
}

/// The wall clock and random UUIDs, what the binary runs with.
pub fn system() -> Generators {
    Generators {
        clock: Arc::new(SystemClock),
        ids: Arc::new(UuidGenerator),
    }
}

unsafe extern "C" {
    fn tzset();
}

/// The host's time zone at the unix second `at`, in seconds east of UTC:
/// `TZ` when set, the system's zone otherwise (ADR-0051 decision 8). 0
/// when the zone cannot be read.
pub fn local_utc_offset(at: i64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let time = at as libc::time_t;
    // SAFETY: `tzset` reads `TZ` into the C library's zone; `localtime_r`
    // writes only into `tm`, which lives on this stack frame.
    unsafe {
        tzset();
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        // `c_long` is `i64` here, but not on every target.
        #[allow(clippy::useless_conversion)]
        i64::from(tm.tm_gmtoff)
    }
}

/// The host's logical cores, for the KPIs' `load` axis.
pub fn logical_cores() -> Option<usize> {
    std::thread::available_parallelism().ok().map(usize::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset is whole minutes within a day either way, and the cores
    /// are some.
    #[test]
    fn reads_the_zone_and_the_cores() {
        let offset = local_utc_offset(1_800_000_000);
        assert!(offset.abs() < 86_400 && offset % 60 == 0, "{offset}");
        assert!(logical_cores().is_some_and(|cores| cores > 0));
    }
}
