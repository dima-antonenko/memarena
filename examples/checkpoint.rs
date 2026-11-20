//! The inner bump region rewinds; the outer allocation stays.

use memarena::Bump;
use std::alloc::Layout;

fn main() {
    let mut bump = Bump::with_capacity(1024).expect("регион");
    let layout = Layout::from_size_align(16, 8).unwrap();
    let outer = bump.allocate(layout).expect("outer");
    let outer_addr = outer.as_ptr() as *mut u8 as usize;
    let mark = bump.checkpoint();

    let inner = bump.allocate(layout).expect("inner");
    let inner_addr = inner.as_ptr() as *mut u8 as usize;
    println!("внешний {outer_addr:#x}, внутренний {inner_addr:#x}");

    bump.rewind(mark);
    assert_eq!(bump.stats().live_allocations, 1);
    let again = bump.allocate(layout).expect("снова");
    assert_eq!(again.as_ptr() as *mut u8 as usize, inner_addr);

    let grown = bump.realloc_last(48).expect("рост последнего блока");
    assert_eq!(grown.as_ptr() as *mut u8 as usize, inner_addr);
    assert_ne!(outer_addr, inner_addr);
    println!("{}", bump.stats());
}
