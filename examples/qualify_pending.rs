use std::fs;
use std::time::Instant;

use everyfile::coordinator::build_first_index;
use everyfile::fsevents;
use everyfile::reconciliation::{EventBatch, reconcile_committed_root};
use tempfile::tempdir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempdir()?;
    let data = tempdir()?;
    build_first_index(root.path(), data.path())?;
    for index in 0..10_000 {
        fs::write(
            root.path().join(format!("ordinary-change-{index:05}.txt")),
            b"change",
        )?;
    }
    let started = Instant::now();
    let reconciled = reconcile_committed_root(
        root.path(),
        data.path(),
        &EventBatch {
            stream_identity: fsevents::stream_identity(root.path())?,
            highest_event_id: fsevents::current_event_id(),
            paths: vec![root.path().to_path_buf()],
            history_lost: false,
            ids_wrapped: false,
            root_changed: false,
        },
    )?;
    let elapsed = started.elapsed();
    let rows = reconciled.projection.search("ordinary-change", 100)?;
    println!(
        "changes=10000 elapsed_ms={} published_rows={} coverage={:?}",
        elapsed.as_millis(),
        rows.len(),
        reconciled.coverage
    );
    if elapsed.as_secs_f64() >= 2.0 || rows.len() != 100 {
        return Err("10,000-change gate failed".into());
    }
    Ok(())
}
