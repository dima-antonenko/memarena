//! Bump allocator: one region, cursor only moves forward.
//!
//! There is no individual free. Memory comes back through [`Bump::reset`] or
//! [`Bump::rewind`]. References from [`Bump::alloc`] cannot outlive the region:
//! `reset` and `rewind` take `&mut self`, so they do not compile while those
//! references are live.

use crate::chunk::RawAlloc;
use crate::layout::{align_up, dangling_slice, hist_index};
use crate::profile::{Profiler, thread_tag};
use crate::stats::{AllocStats, Meters, empty_histogram};
use crate::{Alloc, AllocError};
use std::alloc::Layout;
use std::cell::Cell;
use std::panic::Location;
use std::ptr::{self, NonNull};

/// A cursor rewind point. Valid until [`Bump::reset`], and only if the cursor
/// has not moved left of this point since.
#[derive(Clone, Copy)]
pub struct BumpCheckpoint {
    generation: u32,
    cursor: *mut u8,
    user: usize,
    live: usize,
    last_ptr: *mut u8,
    last_size: usize,
}

pub struct Bump {
    region: RawAlloc,
    start: *mut u8,
    end: *mut u8,
    cursor: Cell<*mut u8>,
    last_ptr: Cell<*mut u8>,
    last_size: Cell<usize>,
    generation: Cell<u32>,
    meters: Meters,
    profile: Profiler,
}

unsafe impl Send for Bump {}

impl Bump {
    pub fn with_capacity(capacity: usize) -> Result<Self, AllocError> {
        let region = RawAlloc::new(capacity, false)?;
        let start = region.as_ptr();
        let end = unsafe { start.add(region.len()) };
        let reserved = region.len();
        let bump = Self {
            region,
            start,
            end,
            cursor: Cell::new(start),
            last_ptr: Cell::new(ptr::null_mut()),
            last_size: Cell::new(0),
            generation: Cell::new(0),
            meters: Meters::new(),
            profile: Profiler::new(true),
        };
        bump.meters.set_reserved(reserved);
        bump.meters.set_chunks(1);
        Ok(bump)
    }

    pub fn remaining(&self) -> usize {
        self.end as usize - self.cursor.get() as usize
    }

    #[track_caller]
    pub fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.bump(layout, Location::caller())
    }

    /// # Safety
    ///
    /// Bump memory comes back only through `reset` and `rewind`. This call reads
    /// nothing and frees nothing: this strategy has no individual `free`.
    pub unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}

    pub fn stats(&self) -> AllocStats {
        self.stats_now()
    }

    pub fn enable_profiling(&self) {
        self.profile.enable();
    }

    pub fn disable_profiling(&self) {
        self.profile.disable();
    }

    pub fn profile_report(&self) -> crate::profile::ProfileReport {
        self.profile.report()
    }

    /// Each allocation is a distinct object, so `&mut T` from `&self` does not alias.
    #[allow(clippy::mut_from_ref)]
    #[track_caller]
    pub fn alloc<T>(&self, value: T) -> Result<&mut T, AllocError> {
        let layout = Layout::new::<T>();
        let ptr = self.allocate(layout)?;
        unsafe {
            let typed = ptr.as_ptr() as *mut T;
            if layout.size() != 0 {
                typed.write(value);
            }
            Ok(&mut *typed)
        }
    }

    /// Each allocation is a distinct object, so a slice from `&self` does not alias another.
    #[allow(clippy::mut_from_ref)]
    #[track_caller]
    pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> Result<&mut [T], AllocError> {
        if src.is_empty() {
            return Ok(&mut []);
        }
        let layout = Layout::array::<T>(src.len()).map_err(|_| AllocError)?;
        let ptr = self.allocate(layout)?;
        unsafe {
            let typed = ptr.as_ptr() as *mut T;
            ptr::copy_nonoverlapping(src.as_ptr(), typed, src.len());
            Ok(&mut *std::slice::from_raw_parts_mut(typed, src.len()))
        }
    }

    #[track_caller]
    pub fn alloc_str(&self, value: &str) -> Result<&str, AllocError> {
        let bytes = self.alloc_slice_copy(value.as_bytes())?;
        Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
    }

    /// Grow or shrink the last allocation while it still ends at the cursor.
    #[track_caller]
    pub fn realloc_last(&self, new_size: usize) -> Result<NonNull<[u8]>, AllocError> {
        let caller = Location::caller();
        self.realloc_last_inner(new_size, caller)
    }

    pub fn checkpoint(&self) -> BumpCheckpoint {
        BumpCheckpoint {
            generation: self.generation.get(),
            cursor: self.cursor.get(),
            user: self.meters.user_bytes(),
            live: self.meters.live(),
            last_ptr: self.last_ptr.get(),
            last_size: self.last_size.get(),
        }
    }

    /// Rewind the cursor. Every allocation after `checkpoint` becomes invalid.
    #[track_caller]
    pub fn rewind(&mut self, checkpoint: BumpCheckpoint) {
        let caller = Location::caller();
        if checkpoint.generation != self.generation.get() {
            panic!("checkpoint относится к другому поколению bump");
        }
        let start = self.start as usize;
        let end = self.end as usize;
        let current = self.cursor.get() as usize;
        let want = checkpoint.cursor as usize;
        if want < start || want > end || want > current {
            panic!("checkpoint устарел или не принадлежит этому bump");
        }
        self.cursor.set(checkpoint.cursor);
        self.last_ptr.set(checkpoint.last_ptr);
        self.last_size.set(checkpoint.last_size);
        self.meters.set_user_live(checkpoint.user, checkpoint.live);
        self.profile.set_live(checkpoint.user);
        self.profile.record_reset(caller, thread_tag());
    }

    /// Put the cursor back at the start. The region's capacity is kept.
    #[track_caller]
    pub fn reset(&mut self) {
        let caller = Location::caller();
        self.cursor.set(self.start);
        self.last_ptr.set(ptr::null_mut());
        self.last_size.set(0);
        self.generation.set(self.generation.get().wrapping_add(1));
        self.meters.set_user_live(0, 0);
        self.meters.set_used(0);
        self.profile.set_live(0);
        self.profile.record_reset(caller, thread_tag());
    }

    #[track_caller]
    fn realloc_last_inner(
        &self,
        new_size: usize,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let last = self.last_ptr.get();
        let old = self.last_size.get();
        if last.is_null() || old == 0 || new_size == 0 {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        let cursor = self.cursor.get() as usize;
        if last as usize + old != cursor {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        let new_end = (last as usize).checked_add(new_size).ok_or(AllocError)?;
        if new_end > self.end as usize {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        if new_size > old {
            self.meters.add_user(new_size - old);
        } else {
            self.meters.sub_user(old - new_size);
        }
        self.profile.retarget_live(old, new_size);
        self.cursor.set(self.start.with_addr(new_end));
        self.last_size.set(new_size);
        Ok(NonNull::slice_from_raw_parts(
            unsafe { NonNull::new_unchecked(last) },
            new_size,
        ))
    }

    fn bump(
        &self,
        layout: Layout,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let thread = thread_tag();
        if layout.size() == 0 {
            self.meters.note_alloc(0, 0);
            let ptr = dangling_slice(layout);
            self.profile.record_alloc(
                0,
                layout.align(),
                ptr.as_ptr() as *mut u8 as usize,
                caller,
                thread,
            );
            return Ok(ptr);
        }
        let cursor = self.cursor.get() as usize;
        let Some(aligned) = align_up(cursor, layout.align()) else {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
            return Err(AllocError);
        };
        let Some(next) = aligned.checked_add(layout.size()) else {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
            return Err(AllocError);
        };
        if next > self.end as usize {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
            return Err(AllocError);
        }
        let ptr = self.start.with_addr(aligned);
        self.cursor.set(self.start.with_addr(next));
        self.last_ptr.set(ptr);
        self.last_size.set(layout.size());
        self.meters.note_alloc(layout.size(), 0);
        self.profile
            .record_alloc(layout.size(), layout.align(), aligned, caller, thread);
        Ok(NonNull::slice_from_raw_parts(
            unsafe { NonNull::new_unchecked(ptr) },
            layout.size(),
        ))
    }

    fn stats_now(&self) -> AllocStats {
        let used = self.cursor.get() as usize - self.start as usize;
        let free = self.end as usize - self.cursor.get() as usize;
        let mut hist = empty_histogram();
        if let Some(index) = hist_index(free) {
            hist[index] = 1;
        }
        AllocStats::from_parts(
            self.region.len(),
            self.meters.user_bytes(),
            used,
            free,
            free,
            self.meters.live(),
            self.meters.peak(),
            1,
            0,
            hist,
        )
    }
}

unsafe impl Alloc for Bump {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Bump::allocate(self, layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { Bump::deallocate(self, ptr, layout) }
    }

    fn stats(&self) -> AllocStats {
        Bump::stats(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zst_does_not_move_cursor() {
        let bump = Bump::with_capacity(64).unwrap();
        let before = bump.remaining();
        bump.alloc(()).unwrap();
        assert_eq!(bump.remaining(), before);
        assert_eq!(bump.stats().user_bytes, 0);
        assert_eq!(bump.stats().live_allocations, 1);
    }

    #[test]
    fn oom_keeps_stats() {
        let bump = Bump::with_capacity(32).unwrap();
        let layout = Layout::from_size_align(24, 1).unwrap();
        bump.allocate(layout).unwrap();
        let stats = bump.stats();
        assert!(bump.allocate(layout).is_err());
        assert_eq!(bump.stats().live_allocations, stats.live_allocations);
        assert_eq!(bump.stats().user_bytes, stats.user_bytes);
    }
}
