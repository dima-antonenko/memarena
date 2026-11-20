//! Chunks obtained from the system.
//!
//! The ordinary path goes through [`std::alloc::alloc`]. The global allocator
//! uses [`std::alloc::System`] so `alloc` does not re-enter itself.

use crate::AllocError;
use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr::NonNull;

/// Allocate a block. `system == true` bypasses `#[global_allocator]`.
pub fn allocate(layout: Layout, system: bool) -> Result<NonNull<u8>, AllocError> {
    if layout.size() == 0 {
        return Err(AllocError);
    }
    let ptr = unsafe {
        if system {
            System.alloc(layout)
        } else {
            std::alloc::alloc(layout)
        }
    };
    NonNull::new(ptr).ok_or(AllocError)
}

/// Free a block obtained from [`allocate`] with the same layout and flag.
///
/// # Safety
/// `ptr` points at a live allocation made the same way.
pub unsafe fn deallocate(ptr: NonNull<u8>, layout: Layout, system: bool) {
    unsafe {
        if system {
            System.dealloc(ptr.as_ptr(), layout);
        } else {
            std::alloc::dealloc(ptr.as_ptr(), layout);
        }
    }
}

/// The single region of a bump allocator.
pub struct RawAlloc {
    ptr: NonNull<u8>,
    layout: Layout,
    system: bool,
}

impl RawAlloc {
    pub fn new(capacity: usize, system: bool) -> Result<Self, AllocError> {
        let size =
            crate::layout::align_up(capacity.max(1), crate::layout::MIN_ALIGN).ok_or(AllocError)?;
        let layout =
            Layout::from_size_align(size, crate::layout::MIN_ALIGN).map_err(|_| AllocError)?;
        let ptr = allocate(layout, system)?;
        Ok(Self {
            ptr,
            layout,
            system,
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for RawAlloc {
    fn drop(&mut self) {
        unsafe { deallocate(self.ptr, self.layout, self.system) }
    }
}
