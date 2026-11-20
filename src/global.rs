//! Global allocator on top of the free-list.
//!
//! Chunks are requested from [`std::alloc::System`] so `alloc` does not re-enter
//! itself. The event log is off here: only the counters remain on this path.

use crate::AllocError;
use crate::freelist::FreeList;
use crate::stats::AllocStats;
use crate::sync::SpinLock;
use std::alloc::{GlobalAlloc, Layout};
use std::cmp;
use std::panic::Location;
use std::ptr::{self, NonNull};

pub struct GlobalFreeList {
    inner: SpinLock<FreeList>,
}

impl Default for GlobalFreeList {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalFreeList {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(FreeList::for_global()),
        }
    }

    pub fn stats(&self) -> AllocStats {
        self.inner.lock().stats()
    }
}

unsafe impl GlobalAlloc for GlobalFreeList {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.inner.lock().allocate_at(layout, Location::caller()) {
            Ok(ptr) => ptr.as_ptr() as *mut u8,
            Err(AllocError) => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else {
            return;
        };
        self.inner
            .lock()
            .deallocate_at(ptr, layout, Location::caller());
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() && layout.size() > 0 {
            unsafe { ptr::write_bytes(ptr, 0, layout.size()) };
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return ptr::null_mut();
        }
        let count = cmp::min(layout.size(), new_size);
        if count > 0 && !ptr.is_null() {
            unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, count) };
        }
        if !ptr.is_null() {
            unsafe { self.dealloc(ptr, layout) };
        }
        new_ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_without_installing_global() {
        let alloc = GlobalFreeList::new();
        let layout = Layout::from_size_align(24, 8).unwrap();
        unsafe {
            let ptr = GlobalAlloc::alloc(&alloc, layout);
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % 8, 0);
            ptr::write_bytes(ptr, 0x11, 24);
            let grown = GlobalAlloc::realloc(&alloc, ptr, layout, 40);
            assert!(!grown.is_null());
            GlobalAlloc::dealloc(&alloc, grown, Layout::from_size_align(40, 8).unwrap());
        }
        assert_eq!(alloc.stats().live_allocations, 0);
        assert_eq!(alloc.stats().user_bytes, 0);
    }
}
