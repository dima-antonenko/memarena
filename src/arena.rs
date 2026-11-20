//! Growing arena: a chain of bump chunks.
//!
//! Objects do not move. [`Arena::reset`] keeps the newest chunk and returns the
//! rest to the system. [`Arena::rewind`] restores the cursor and frees chunks
//! that appeared after the checkpoint.

use crate::chunk::{self, allocate};
use crate::layout::{align_up, dangling_slice, hist_index};
use crate::profile::{Profiler, thread_tag};
use crate::stats::{AllocStats, Meters, empty_histogram};
use crate::{Alloc, AllocError};
use std::alloc::Layout;
use std::cell::Cell;
use std::panic::Location;
use std::ptr::{self, NonNull};

const MIN_CHUNK: usize = 4 * 1024;
const MAX_CHUNK: usize = 1024 * 1024;
const CHUNK_ALIGN: usize = 16;

struct ChunkHeader {
    prev: *mut ChunkHeader,
    data: *mut u8,
    end: *mut u8,
    /// Cursor at the moment this chunk was left. Unused on the current chunk.
    frozen: *mut u8,
    alloc_ptr: *mut u8,
    alloc_size: usize,
    alloc_align: usize,
}

/// Arena checkpoint. Invalid after [`Arena::reset`].
#[derive(Clone, Copy)]
pub struct ArenaCheckpoint {
    generation: u32,
    chunk: *mut ChunkHeader,
    cursor: *mut u8,
    user: usize,
    live: usize,
    last_ptr: *mut u8,
    last_size: usize,
}

pub struct Arena {
    head: Cell<*mut ChunkHeader>,
    cursor: Cell<*mut u8>,
    end: Cell<*mut u8>,
    last_ptr: Cell<*mut u8>,
    last_size: Cell<usize>,
    generation: Cell<u32>,
    meters: Meters,
    profile: Profiler,
}

unsafe impl Send for Arena {}

impl Arena {
    pub const fn new() -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
            cursor: Cell::new(ptr::null_mut()),
            end: Cell::new(ptr::null_mut()),
            last_ptr: Cell::new(ptr::null_mut()),
            last_size: Cell::new(0),
            generation: Cell::new(0),
            meters: Meters::new(),
            profile: Profiler::new(true),
        }
    }

    pub fn with_capacity(capacity: usize) -> Result<Self, AllocError> {
        let arena = Self::new();
        arena.grow(capacity.max(1))?;
        Ok(arena)
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

    #[track_caller]
    pub fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_inner(layout)
    }

    /// # Safety
    ///
    /// Arena memory comes back through `reset`, `rewind`, and `Drop`. This call
    /// reads nothing and does not free an individual object.
    pub unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}

    pub fn stats(&self) -> AllocStats {
        let (used, free, largest, hist) = self.occupancy();
        AllocStats::from_parts(
            self.meters.reserved(),
            self.meters.user_bytes(),
            used,
            free,
            largest,
            self.meters.live(),
            self.meters.peak(),
            self.meters.chunks(),
            0,
            hist,
        )
    }

    pub fn remaining(&self) -> usize {
        let end = self.end.get();
        if end.is_null() {
            0
        } else {
            end as usize - self.cursor.get() as usize
        }
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

    #[track_caller]
    pub fn realloc_last(&self, new_size: usize) -> Result<NonNull<[u8]>, AllocError> {
        self.realloc_last_inner(new_size, Location::caller())
    }

    pub fn checkpoint(&self) -> ArenaCheckpoint {
        ArenaCheckpoint {
            generation: self.generation.get(),
            chunk: self.head.get(),
            cursor: self.cursor.get(),
            user: self.meters.user_bytes(),
            live: self.meters.live(),
            last_ptr: self.last_ptr.get(),
            last_size: self.last_size.get(),
        }
    }

    #[track_caller]
    pub fn rewind(&mut self, checkpoint: ArenaCheckpoint) {
        let caller = Location::caller();
        if checkpoint.generation != self.generation.get() {
            panic!("checkpoint относится к другому поколению арены");
        }
        if checkpoint.chunk.is_null() {
            self.free_all();
            self.head.set(ptr::null_mut());
            self.cursor.set(ptr::null_mut());
            self.end.set(ptr::null_mut());
        } else {
            let already_current = checkpoint.chunk == self.head.get();
            let current_cursor = self.cursor.get() as usize;
            let mut found = false;
            let mut cursor = self.head.get();
            while !cursor.is_null() {
                if cursor == checkpoint.chunk {
                    found = true;
                    break;
                }
                cursor = unsafe { (*cursor).prev };
            }
            if !found {
                panic!("checkpoint устарел или не принадлежит этой арене");
            }
            let data = unsafe { (*checkpoint.chunk).data as usize };
            let end = unsafe { (*checkpoint.chunk).end as usize };
            let want = checkpoint.cursor as usize;
            if want < data || want > end || (already_current && want > current_cursor) {
                panic!("checkpoint устарел или не принадлежит этой арене");
            }
            let mut current = self.head.get();
            while !current.is_null() && current != checkpoint.chunk {
                let prev = unsafe { (*current).prev };
                unsafe { free_chunk(current) };
                current = prev;
            }
            self.head.set(checkpoint.chunk);
            self.cursor.set(checkpoint.cursor);
            self.end.set(unsafe { (*checkpoint.chunk).end });
        }
        self.last_ptr.set(checkpoint.last_ptr);
        self.last_size.set(checkpoint.last_size);
        self.meters.set_user_live(checkpoint.user, checkpoint.live);
        self.recompute_reserved();
        self.profile.set_live(checkpoint.user);
        self.profile.record_reset(caller, thread_tag());
    }

    #[track_caller]
    pub fn reset(&mut self) {
        let caller = Location::caller();
        let head = self.head.get();
        if !head.is_null() {
            let mut older = unsafe { (*head).prev };
            unsafe { (*head).prev = ptr::null_mut() };
            while !older.is_null() {
                let prev = unsafe { (*older).prev };
                unsafe { free_chunk(older) };
                older = prev;
            }
            let data = unsafe { (*head).data };
            unsafe { (*head).frozen = data };
            self.cursor.set(data);
            self.end.set(unsafe { (*head).end });
        }
        self.last_ptr.set(ptr::null_mut());
        self.last_size.set(0);
        self.generation.set(self.generation.get().wrapping_add(1));
        self.meters.set_user_live(0, 0);
        self.recompute_reserved();
        self.profile.set_live(0);
        self.profile.record_reset(caller, thread_tag());
    }

    fn realloc_last_inner(
        &self,
        new_size: usize,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let last = self.last_ptr.get();
        let old = self.last_size.get();
        let cursor = self.cursor.get();
        if last.is_null() || old == 0 || new_size == 0 || cursor.is_null() {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        if last as usize + old != cursor as usize {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        let new_end = (last as usize).checked_add(new_size).ok_or(AllocError)?;
        if new_end > self.end.get() as usize {
            self.profile.record_oom(new_size, 1, caller, thread_tag());
            return Err(AllocError);
        }
        if new_size > old {
            self.meters.add_user(new_size - old);
        } else {
            self.meters.sub_user(old - new_size);
        }
        self.profile.retarget_live(old, new_size);
        let head = self.head.get();
        self.cursor
            .set(unsafe { (*head).alloc_ptr.with_addr(new_end) });
        self.last_size.set(new_size);
        Ok(NonNull::slice_from_raw_parts(
            unsafe { NonNull::new_unchecked(last) },
            new_size,
        ))
    }

    #[track_caller]
    fn allocate_inner(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let caller = Location::caller();
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
        if let Some(ptr) = self.try_bump(layout) {
            self.finish(ptr, layout, caller, thread);
            return Ok(NonNull::slice_from_raw_parts(
                unsafe { NonNull::new_unchecked(ptr) },
                layout.size(),
            ));
        }
        let need = layout.size().saturating_add(layout.align());
        if self.grow(need).is_err() {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
            return Err(AllocError);
        }
        match self.try_bump(layout) {
            Some(ptr) => {
                self.finish(ptr, layout, caller, thread);
                Ok(NonNull::slice_from_raw_parts(
                    unsafe { NonNull::new_unchecked(ptr) },
                    layout.size(),
                ))
            }
            None => {
                self.profile
                    .record_oom(layout.size(), layout.align(), caller, thread);
                Err(AllocError)
            }
        }
    }

    fn finish(
        &self,
        ptr: *mut u8,
        layout: Layout,
        caller: &'static Location<'static>,
        thread: u64,
    ) {
        self.last_ptr.set(ptr);
        self.last_size.set(layout.size());
        self.meters.note_alloc(layout.size(), 0);
        self.profile
            .record_alloc(layout.size(), layout.align(), ptr as usize, caller, thread);
    }

    fn try_bump(&self, layout: Layout) -> Option<*mut u8> {
        let head = self.head.get();
        if head.is_null() {
            return None;
        }
        let cursor = self.cursor.get() as usize;
        let aligned = align_up(cursor, layout.align())?;
        let next = aligned.checked_add(layout.size())?;
        if next > self.end.get() as usize {
            return None;
        }
        let base = unsafe { (*head).alloc_ptr };
        let ptr = base.with_addr(aligned);
        self.cursor.set(base.with_addr(next));
        Some(ptr)
    }

    fn grow(&self, need: usize) -> Result<(), AllocError> {
        let current_cap = self.current_capacity();
        let data_cap = next_chunk_data(current_cap, need);
        let header = size_of::<ChunkHeader>();
        let data_off = align_up(header, CHUNK_ALIGN).ok_or(AllocError)?;
        let total = data_off.checked_add(data_cap).ok_or(AllocError)?;
        let layout = Layout::from_size_align(total, CHUNK_ALIGN).map_err(|_| AllocError)?;
        let raw = allocate(layout, false)?;
        let header_ptr = raw.as_ptr() as *mut ChunkHeader;
        let data = unsafe { raw.as_ptr().add(data_off) };
        let end = unsafe { data.add(data_cap) };
        unsafe {
            ptr::write(
                header_ptr,
                ChunkHeader {
                    prev: self.head.get(),
                    data,
                    end,
                    frozen: data,
                    alloc_ptr: raw.as_ptr(),
                    alloc_size: total,
                    alloc_align: CHUNK_ALIGN,
                },
            );
        }
        if let Some(old) = NonNull::new(self.head.get()) {
            unsafe { (*old.as_ptr()).frozen = self.cursor.get() };
        }
        self.head.set(header_ptr);
        self.cursor.set(data);
        self.end.set(end);
        self.meters.add_reserved(total);
        self.meters.add_chunk();
        Ok(())
    }

    fn current_capacity(&self) -> usize {
        let head = self.head.get();
        if head.is_null() {
            0
        } else {
            unsafe { (*head).end as usize - (*head).data as usize }
        }
    }

    fn recompute_reserved(&self) {
        let mut count = 0;
        let mut bytes = 0;
        let mut cursor = self.head.get();
        while !cursor.is_null() {
            count += 1;
            bytes += unsafe { (*cursor).alloc_size };
            cursor = unsafe { (*cursor).prev };
        }
        self.meters.set_reserved(bytes);
        self.meters.set_chunks(count);
    }

    fn free_all(&mut self) {
        let mut cursor = self.head.get();
        while !cursor.is_null() {
            let prev = unsafe { (*cursor).prev };
            unsafe { free_chunk(cursor) };
            cursor = prev;
        }
    }

    fn occupancy(&self) -> (usize, usize, usize, [usize; crate::layout::HIST_BUCKETS]) {
        let mut used = 0;
        let mut free = 0;
        let mut largest = 0;
        let mut hist = empty_histogram();
        let head = self.head.get();
        let mut cursor = head;
        while !cursor.is_null() {
            let data = unsafe { (*cursor).data as usize };
            let end = unsafe { (*cursor).end as usize };
            let at = if cursor == head {
                self.cursor.get() as usize
            } else {
                unsafe { (*cursor).frozen as usize }
            };
            let consumed = at.saturating_sub(data);
            let tail = end.saturating_sub(at);
            used += consumed;
            free += tail;
            if tail > largest {
                largest = tail;
            }
            if let Some(index) = hist_index(tail) {
                hist[index] += 1;
            }
            cursor = unsafe { (*cursor).prev };
        }
        (used, free, largest, hist)
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        self.free_all();
    }
}

unsafe impl Alloc for Arena {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Arena::allocate(self, layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { Arena::deallocate(self, ptr, layout) }
    }

    fn stats(&self) -> AllocStats {
        Arena::stats(self)
    }
}

fn next_chunk_data(current: usize, need: usize) -> usize {
    let grown = if current == 0 {
        MIN_CHUNK
    } else {
        current.saturating_mul(2).clamp(current, MAX_CHUNK)
    };
    grown.max(need).max(1)
}

unsafe fn free_chunk(chunk: *mut ChunkHeader) {
    unsafe {
        let layout = Layout::from_size_align_unchecked((*chunk).alloc_size, (*chunk).alloc_align);
        chunk::deallocate(NonNull::new_unchecked((*chunk).alloc_ptr), layout, false);
    }
}
