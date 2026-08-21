use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use everyfile::projection::SearchProjection;
use everyfile::query::{CancellationToken, SortDirection, SortField, SortOrder};

fn percentile(samples: &mut [Duration], percentile: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percentile / 100]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: qualify_projection <search.projection> [query]")?;
    let query = std::env::args().nth(2).unwrap_or_else(|| "lib".into());
    let projection = SearchProjection::open(Path::new(&path), None)?;
    let sorts = [
        SortField::Relevance,
        SortField::ModificationTime,
        SortField::CreationTime,
        SortField::FileName,
        SortField::FullPath,
        SortField::FileSize,
    ];
    for field in sorts {
        let mut samples = Vec::with_capacity(25);
        for _ in 0..25 {
            let started = Instant::now();
            let ranked = projection.search_ranked(
                &query,
                &HashMap::new(),
                100,
                SortOrder {
                    field,
                    direction: SortDirection::Ascending,
                },
                &CancellationToken::default(),
            )?;
            std::hint::black_box(ranked.rows);
            samples.push(started.elapsed());
        }
        let p95 = percentile(&mut samples, 95);
        let p99 = percentile(&mut samples, 99);
        println!(
            "sort={field:?} p95_us={} p99_us={}",
            p95.as_micros(),
            p99.as_micros()
        );
    }
    Ok(())
}
