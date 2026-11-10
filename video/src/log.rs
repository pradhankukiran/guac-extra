// SPDX-License-Identifier: Apache-2.0
//! Logging through a callback the host (guacd) provides.

use std::sync::Arc;

/// Log levels, matching the order of guacd's levels.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GxLogLevel {
    Error = 0,
    Warning = 1,
    Info = 2,
    Debug = 3,
}

type Sink = Arc<dyn Fn(GxLogLevel, &str) + Send + Sync>;

/// Cheap to clone; does nothing without a sink.
#[derive(Clone, Default)]
pub struct Logger(Option<Sink>);

impl Logger {
    pub fn new(f: impl Fn(GxLogLevel, &str) + Send + Sync + 'static) -> Self {
        Logger(Some(Arc::new(f)))
    }

    pub fn log(&self, level: GxLogLevel, msg: impl FnOnce() -> String) {
        if let Some(f) = &self.0 {
            f(level, &msg());
        }
    }

    pub fn error(&self, msg: impl FnOnce() -> String) {
        self.log(GxLogLevel::Error, msg)
    }
    pub fn warn(&self, msg: impl FnOnce() -> String) {
        self.log(GxLogLevel::Warning, msg)
    }
    pub fn info(&self, msg: impl FnOnce() -> String) {
        self.log(GxLogLevel::Info, msg)
    }
    pub fn debug(&self, msg: impl FnOnce() -> String) {
        self.log(GxLogLevel::Debug, msg)
    }
}
