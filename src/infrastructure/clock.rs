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
