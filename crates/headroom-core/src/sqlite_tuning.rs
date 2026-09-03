//! Process-wide SQLite settings, applied before the first connection.
//!
//! SQLite tracks allocation statistics by default, and the counters live
//! behind one global mutex that every `malloc` and `free` takes. FTS5 makes
//! many small allocations per query, so concurrent searches serialise on that
//! mutex no matter how many connections or cores are available: measured
//! against the memory index, twenty concurrent searches took 20x one search
//! even with a private connection each. Turning the statistics off removes
//! the mutex; nothing in this codebase reads them.

use std::sync::Once;

static TUNED: Once = Once::new();

/// Turn off SQLite's global allocation statistics.
///
/// Must run before the first connection is opened — SQLite refuses the change
/// once its library has initialised, and the failure is silent by design here:
/// an older or differently-built SQLite that declines simply keeps the
/// counters, which costs speed and nothing else.
pub fn apply() {
    TUNED.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0);
    });
}
