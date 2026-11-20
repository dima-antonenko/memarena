//! Four threads allocate from a shared free-list through local magazines.

use memarena::{ThreadCache, ThreadLocalArena, allocation_ptr};
use std::alloc::Layout;
use std::thread;

fn main() {
    let local = ThreadLocalArena::new();
    local.with(|arena| {
        let value = arena.alloc(7u32).unwrap();
        println!("локальная арена: {value}");
    });

    let cache = ThreadCache::new();
    let mut handles = Vec::new();
    for id in 0..4 {
        let cache = cache.clone();
        handles.push(thread::spawn(move || {
            let layout = Layout::from_size_align(64, 8).unwrap();
            let ptr = cache.allocate(layout).unwrap();
            unsafe {
                std::ptr::write_bytes(ptr.as_ptr() as *mut u8, id, 64);
                cache.deallocate(allocation_ptr(ptr), layout);
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let stats = cache.stats();
    println!("{stats}");
    assert_eq!(stats.live_allocations, 0);
}
