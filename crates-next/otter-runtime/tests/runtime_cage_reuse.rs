//! Runtime-handle shutdown must release isolate GC pages before the next
//! cold-start iteration.

use otter_gc::cage_stats;
use otter_runtime::Otter;

#[test]
fn repeated_otter_build_drop_returns_gc_pages_to_cage() -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..32 {
        let before = cage_stats().map_or(0, |stats| stats.allocated_pages);
        let otter = Otter::builder().build()?;
        let during = cage_stats().expect("cage is initialized by Otter::build");
        assert!(
            during.allocated_pages >= before,
            "live isolate should own GC pages while the handle is alive"
        );
        drop(otter);
        let after = cage_stats().expect("cage remains initialized after runtime drop");
        assert_eq!(
            after.allocated_pages, before,
            "dropping the final runtime handle must join the isolate and return its GC pages"
        );
    }
    Ok(())
}
