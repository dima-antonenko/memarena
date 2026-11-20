//! Holes between live blocks are external fragmentation.

use memarena::{FreeList, allocation_ptr};
use std::alloc::Layout;

fn main() {
    let list = FreeList::new();
    let layout = Layout::from_size_align(16, 8).unwrap();
    let first = list.allocate(layout).unwrap();
    let middle = list.allocate(layout).unwrap();
    let last = list.allocate(layout).unwrap();

    unsafe {
        list.deallocate(allocation_ptr(first), layout);
        list.deallocate(allocation_ptr(last), layout);
    }
    println!("после освобождения краёв:\n{}", list.stats());
    assert!(list.stats().external_fragmentation > 0);

    unsafe { list.deallocate(allocation_ptr(middle), layout) };
    println!("после слияния:\n{}", list.stats());
    assert_eq!(list.stats().external_fragmentation, 0);
    assert_eq!(list.stats().live_allocations, 0);
}
