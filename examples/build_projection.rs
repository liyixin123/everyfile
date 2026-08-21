use std::path::Path;

use everyfile::index::IndexStore;
use everyfile::projection::SearchProjection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database = std::env::args()
        .nth(1)
        .ok_or("usage: build_projection <index.sqlite3> <search.projection>")?;
    let projection = std::env::args()
        .nth(2)
        .ok_or("usage: build_projection <index.sqlite3> <search.projection>")?;
    let store = IndexStore::open(Path::new(&database))?;
    SearchProjection::build_from_store(Path::new(&projection), &store)?;
    Ok(())
}
