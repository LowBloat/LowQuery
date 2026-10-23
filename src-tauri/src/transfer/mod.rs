//! Moving data in and out: exports to SQL, CSV and JSON, imports from CSV
//! and JSON. Jobs run on a session of their own, report progress as
//! `transfer-progress` events and stop when cancelled.

pub mod catalog;
pub mod export;
pub mod import;
#[cfg(test)]
pub mod round_trip;
pub mod sql;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// The table or object being written or read.
    pub object: String,
    /// 1-based position of the object, and how many there are.
    pub step: usize,
    pub steps: usize,
    /// Rows of the current object so far.
    pub rows: u64,
}

impl Progress {
    pub fn new(object: &str, step: usize, steps: usize, rows: u64) -> Self {
        Progress {
            object: object.to_string(),
            step,
            steps,
            rows,
        }
    }
}

/// Running jobs: their cancel flag and the session they run on.
#[derive(Default)]
pub struct Jobs(Mutex<HashMap<String, (Arc<AtomicBool>, String)>>);

impl Jobs {
    pub fn start(&self, id: &str, session_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.0
            .lock()
            .insert(id.to_string(), (flag.clone(), session_id.to_string()));
        flag
    }

    pub fn finish(&self, id: &str) {
        self.0.lock().remove(id);
    }

    /// Flags the job; returns its session so the running query can be
    /// cancelled on the server too.
    pub fn cancel(&self, id: &str) -> Option<String> {
        let jobs = self.0.lock();
        let (flag, session) = jobs.get(id)?;
        flag.store(true, Ordering::SeqCst);
        Some(session.clone())
    }
}
