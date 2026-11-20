//! Behavior of the bump, the arena, the free-list, threads, and the profiler.

use memarena::{
    Alloc, AllocStats, Arena, Bump, FreeList, ThreadCache, ThreadLocalArena, allocation_ptr,
};
use std::alloc::Layout;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

fn aligned_alloc(alloc: &impl Alloc, size: usize, align: usize) -> NonNull<[u8]> {
    let layout = Layout::from_size_align(size, align).unwrap();
    let ptr = alloc.allocate(layout).expect("allocate");
    let addr = ptr.as_ptr() as *mut u8 as usize;
    assert_eq!(addr % align, 0, "align {align}");
    ptr
}

fn check_alignment(alloc: &impl Alloc) {
    let mut align = 1usize;
    while align <= 4096 {
        let ptr = aligned_alloc(alloc, 8, align);
        let layout = Layout::from_size_align(8, align).unwrap();
        unsafe { alloc.deallocate(allocation_ptr(ptr), layout) };
        align *= 2;
    }
}

fn zst(alloc: &impl Alloc) {
    let layout = Layout::from_size_align(0, 8).unwrap();
    let before_user = alloc.stats().user_bytes;
    let ptr = alloc.allocate(layout).unwrap();
    assert_eq!(ptr.as_ptr() as *mut u8 as usize % 8, 0);
    assert_eq!(alloc.stats().user_bytes, before_user);
    unsafe { alloc.deallocate(allocation_ptr(ptr), layout) };
}

#[test]
fn bump_alignment_zst_padding_and_rewind() {
    let bump = Bump::with_capacity(64 * 1024).unwrap();
    check_alignment(&bump);
    zst(&bump);

    let bump = Bump::with_capacity(256).unwrap();
    aligned_alloc(&bump, 1, 1);
    aligned_alloc(&bump, 8, 64);
    let stats = bump.stats();
    assert_eq!(stats.user_bytes, 9);
    assert!(stats.internal_fragmentation > 0);
    assert_eq!(stats.external_fragmentation, 0);
    assert_eq!(stats.used_bytes + stats.free_bytes, stats.reserved);

    let mut bump = Bump::with_capacity(256).unwrap();
    let layout = Layout::from_size_align(16, 8).unwrap();
    bump.allocate(layout).unwrap();
    let mark = bump.checkpoint();
    let second = bump.allocate(layout).unwrap();
    let second_addr = second.as_ptr() as *mut u8 as usize;
    assert_eq!(bump.stats().live_allocations, 2);
    bump.rewind(mark);
    assert_eq!(bump.stats().live_allocations, 1);
    let third = bump.allocate(layout).unwrap();
    assert_eq!(third.as_ptr() as *mut u8 as usize, second_addr);

    let bump = Bump::with_capacity(64).unwrap();
    let first = bump.allocate(layout).unwrap();
    let addr = first.as_ptr() as *mut u8 as usize;
    let grown = bump.realloc_last(32).unwrap();
    assert_eq!(grown.as_ptr() as *mut u8 as usize, addr);
    assert_eq!(bump.stats().user_bytes, 32);
    let err = bump.realloc_last(10_000);
    assert!(err.is_err());
    assert_eq!(bump.stats().user_bytes, 32);
}

#[test]
fn arena_grows_resets_and_rewinds() {
    let arena = Arena::new();
    check_alignment(&arena);
    zst(&arena);

    let mut arena = Arena::new();
    let mark = arena.checkpoint();
    arena.alloc(1u64).unwrap();
    let chunks = arena.stats().chunk_count;
    let user = arena.stats().user_bytes;
    while arena.stats().chunk_count == chunks {
        arena.alloc([0u8; 1024]).unwrap();
    }
    assert!(arena.stats().chunk_count > chunks);
    assert!(arena.stats().external_fragmentation > 0 || arena.stats().chunk_count > 1);
    arena.rewind(mark);
    assert_eq!(arena.stats().chunk_count, 0);
    assert_eq!(arena.stats().live_allocations, 0);

    arena.alloc(7u32).unwrap();
    let kept = arena.stats().user_bytes;
    let cp = arena.checkpoint();
    while arena.stats().chunk_count < 2 {
        arena.alloc([0u8; 2048]).unwrap();
    }
    arena.rewind(cp);
    assert_eq!(arena.stats().chunk_count, 1);
    assert_eq!(arena.stats().user_bytes, kept);

    arena.reset();
    assert_eq!(arena.stats().live_allocations, 0);
    assert_eq!(arena.stats().user_bytes, 0);
    assert_eq!(arena.stats().chunk_count, 1);
    let _ = user;
}

#[test]
#[should_panic(expected = "поколению")]
fn arena_reset_invalidates_checkpoint() {
    let mut arena = Arena::new();
    let mark = arena.checkpoint();
    arena.alloc(1u8).unwrap();
    arena.reset();
    arena.rewind(mark);
}

#[test]
fn freelist_alignment_reuse_and_box() {
    let list = FreeList::new();
    check_alignment(&list);
    zst(&list);

    let layout = Layout::from_size_align(24, 8).unwrap();
    let first = list.allocate(layout).unwrap();
    let addr = first.as_ptr() as *mut u8 as usize;
    unsafe { list.deallocate(allocation_ptr(first), layout) };
    let second = list.allocate(layout).unwrap();
    assert_eq!(second.as_ptr() as *mut u8 as usize, addr);
    unsafe { list.deallocate(allocation_ptr(second), layout) };

    let wide = Layout::from_size_align(8, 256).unwrap();
    let ptr = list.allocate(wide).unwrap();
    assert_eq!(ptr.as_ptr() as *mut u8 as usize % 256, 0);
    unsafe { ptr::write_bytes(ptr.as_ptr() as *mut u8, 0x5A, 8) };
    unsafe { list.deallocate(allocation_ptr(ptr), wide) };
    assert_eq!(list.stats().live_allocations, 0);

    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct Bomb(u8);
    impl Drop for Bomb {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    {
        let value = list.alloc_box(Bomb(1)).unwrap();
        assert_eq!(value.0, 1);
        assert_eq!(list.stats().live_allocations, 1);
    }
    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    assert_eq!(list.stats().live_allocations, 0);
}

#[test]
#[should_panic(expected = "не принадлежит")]
fn freelist_rejects_foreign_pointer() {
    let list = FreeList::new();
    let local = 0u8;
    let layout = Layout::from_size_align(1, 1).unwrap();
    unsafe { list.deallocate(NonNull::from(&local), layout) };
}

#[test]
fn profile_reports_callsite() {
    let bump = Bump::with_capacity(1024).unwrap();
    bump.enable_profiling();
    let _ = tagged_alloc(&bump);
    let report = bump.profile_report();
    assert!(report.enabled);
    assert!(report.events_recorded >= 1);
    assert!(report.peak_live_bytes >= 32);
    assert!(report.size_histogram.iter().any(|count| *count > 0));
    assert!(
        report
            .top_by_count
            .iter()
            .flatten()
            .any(|site| site.file.ends_with("allocators.rs")
                && site.count >= 1
                && site.bytes >= 32)
    );
    assert!(report.recent.iter().flatten().any(|event| event.size == 32));
}

fn tagged_alloc(bump: &Bump) -> NonNull<[u8]> {
    bump.allocate(Layout::from_size_align(32, 8).unwrap())
        .unwrap()
}

#[test]
fn thread_local_arenas_are_isolated() {
    let local = ThreadLocalArena::new();
    local.reset();
    local.with(|arena| {
        arena.alloc(11u64).unwrap();
        assert!(arena.stats().live_allocations >= 1);
    });
    thread::spawn(|| {
        let local = ThreadLocalArena::new();
        local.with(|arena| {
            assert_eq!(arena.stats().live_allocations, 0);
            arena.alloc(9u32).unwrap();
        });
    })
    .join()
    .unwrap();
}

#[test]
fn thread_cache_reuses_and_survives_cross_thread_free() {
    let cache = ThreadCache::new();
    let layout = Layout::from_size_align(32, 8).unwrap();
    let first = cache.allocate(layout).unwrap();
    let addr = first.as_ptr() as *mut u8 as usize;
    unsafe { cache.deallocate(allocation_ptr(first), layout) };
    let second = cache.allocate(layout).unwrap();
    assert_eq!(
        second.as_ptr() as *mut u8 as usize,
        addr,
        "журнал должен вернуть тот же блок"
    );
    unsafe { cache.deallocate(allocation_ptr(second), layout) };

    let sent = cache.allocate(layout).unwrap();
    let addr = sent.as_ptr() as *mut u8 as usize;
    let other = cache.clone();
    thread::spawn(move || {
        let thin = unsafe { NonNull::new_unchecked(addr as *mut u8) };
        unsafe { other.deallocate(thin, layout) };
    })
    .join()
    .unwrap();
    assert_eq!(cache.stats().live_allocations, 0);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let cache = cache.clone();
        handles.push(thread::spawn(move || {
            let mut batch = Vec::new();
            for index in 0..40 {
                let size = 16 + (index % 5) * 8;
                let layout = Layout::from_size_align(size, 8).unwrap();
                let ptr = cache.allocate(layout).unwrap();
                unsafe { ptr::write_bytes(ptr.as_ptr() as *mut u8, 0xAB, size) };
                batch.push((allocation_ptr(ptr), layout));
            }
            for (ptr, layout) in batch {
                unsafe { cache.deallocate(ptr, layout) };
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let stats: AllocStats = cache.stats();
    assert_eq!(stats.live_allocations, 0);
    assert_eq!(stats.user_bytes, 0);
    assert_eq!(stats.cached_bytes, 0);
}

#[test]
fn invariants_hold_for_a_churn() {
    let list = FreeList::new();
    let mut live = Vec::new();
    for index in 0..30 {
        let size = 8 + (index % 9) * 24;
        let layout = Layout::from_size_align(size, 8).unwrap();
        live.push((list.allocate(layout).unwrap(), layout));
        if index % 2 == 0
            && let Some((ptr, layout)) = live.pop()
        {
            unsafe { list.deallocate(allocation_ptr(ptr), layout) };
        }
        let stats = list.stats();
        assert_eq!(
            stats.internal_fragmentation,
            stats.used_bytes - stats.user_bytes
        );
        assert_eq!(
            stats.external_fragmentation,
            stats.free_bytes - stats.largest_free_block
        );
    }
}

#[test]
fn catch_unwind_leaves_bump_usable() {
    let bump = Bump::with_capacity(32).unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        bump.allocate(Layout::from_size_align(10_000, 8).unwrap())
            .unwrap();
    }));
    assert!(result.is_err());
    assert!(
        bump.allocate(Layout::from_size_align(8, 8).unwrap())
            .is_ok()
    );
}
