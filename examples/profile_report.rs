//! The profiler groups allocations by callsite.

use memarena::Bump;
use std::alloc::Layout;

fn load(bump: &Bump, size: usize) {
    bump.allocate(Layout::from_size_align(size, 8).unwrap())
        .unwrap();
}

fn main() {
    let bump = Bump::with_capacity(4096).unwrap();
    bump.enable_profiling();
    load(&bump, 32);
    load(&bump, 128);
    bump.allocate(Layout::from_size_align(8, 8).unwrap())
        .unwrap();
    let report = bump.profile_report();
    println!("{report}");
    assert!(report.events_recorded >= 3);
    assert!(
        report
            .top_by_bytes
            .iter()
            .flatten()
            .any(|site| site.bytes >= 128)
    );
}
