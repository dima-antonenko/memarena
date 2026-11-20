//! Free-list with size classes, splitting, and coalescing of adjacent blocks.
//!
//! A block header stores its own size and the previous block's size (boundary tags).
//! Free blocks hang in a doubly linked list of their class; a payload larger than
//! 4096 bytes goes on a separate list. An allocation larger than 4096 bytes gets
//! its own chunk and is returned to the system in full.
//!
//! For alignment greater than 16 bytes, the offset back to the header is written
//! in front of the user pointer. In debug the header carries a magic value, the
//! free body is filled with `0xDE`, and a double free or a foreign pointer panics.

use crate::chunk::{self, allocate};
use crate::layout::{
    HIST_BUCKETS, MIN_ALIGN, SIZE_CLASSES, align_up, bin_for_payload, class_for_request,
    dangling_slice, hist_index, payload_needed, round_up_min,
};
use crate::profile::{Profiler, thread_tag};
use crate::stats::{AllocStats, Meters, empty_histogram};
use crate::{Alloc, AllocError};
use std::alloc::Layout;
use std::cell::Cell;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

const HEADER: usize = size_of::<BlockHeader>();
const SMALL_REGION: usize = 64 * 1024;
const MAGIC: usize = 0xA11C_5AFE_A11C_5AFE;
const FOOTER_MAGIC: usize = 0xF007_E2F0_07E2_F007;
const F_FREE: usize = 1;
const F_LARGE: usize = 2;
const F_CACHED: usize = 4;
const FLAG_MASK: usize = 0xF;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
struct BlockHeader {
    size_and_flags: usize,
    prev_size: usize,
    user_size: usize,
    magic: usize,
    chunk: *mut FlChunk,
    _pad: usize,
}

#[repr(C)]
struct FreeNode {
    next: *mut BlockHeader,
    prev: *mut BlockHeader,
}

#[repr(C)]
struct FlChunk {
    next: *mut FlChunk,
    prev: *mut FlChunk,
    data: *mut u8,
    footer: *mut BlockHeader,
    alloc_ptr: *mut u8,
    alloc_size: usize,
    alloc_align: usize,
    owner_thread: u64,
    list_id: u64,
    large: bool,
    system: bool,
}

const _: () = assert!(size_of::<BlockHeader>() == 48);
const _: () = assert!(size_of::<BlockHeader>().is_multiple_of(16));
const _: () = assert!(size_of::<FreeNode>() <= 16);

/// Allocator with individual deallocation. Not `Sync`: across threads it is
/// wrapped by [`crate::SharedFreeList`].
pub struct FreeList {
    list_id: Cell<u64>,
    chunks: Cell<*mut FlChunk>,
    bins: [Cell<*mut BlockHeader>; SIZE_CLASSES.len()],
    large_free: Cell<*mut BlockHeader>,
    meters: Meters,
    profile: Profiler,
    system_backend: bool,
    checks: bool,
}

unsafe impl Send for FreeList {}

impl FreeList {
    pub fn new() -> Self {
        let list = Self::empty(false, cfg!(debug_assertions), true);
        list.ensure_id();
        list
    }

    /// Empty allocator for `#[global_allocator]`: chunks come from `System`.
    pub const fn for_global() -> Self {
        Self::empty(true, false, false)
    }

    const fn empty(system_backend: bool, checks: bool, allow_log: bool) -> Self {
        Self {
            list_id: Cell::new(0),
            chunks: Cell::new(ptr::null_mut()),
            bins: [const { Cell::new(ptr::null_mut()) }; SIZE_CLASSES.len()],
            large_free: Cell::new(ptr::null_mut()),
            meters: Meters::new(),
            profile: Profiler::new(allow_log),
            system_backend,
            checks,
        }
    }

    pub fn id(&self) -> u64 {
        self.ensure_id()
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

    pub fn stats(&self) -> AllocStats {
        self.snapshot()
    }

    #[track_caller]
    pub fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_at(layout, Location::caller())
    }

    /// # Safety
    /// `ptr` came from this allocator with the same `layout` and has not been freed yet.
    #[track_caller]
    pub unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.deallocate_at(ptr, layout, Location::caller());
    }

    #[track_caller]
    pub fn alloc_box<T>(&self, value: T) -> Result<FreeBox<'_, T>, AllocError> {
        let layout = Layout::new::<T>();
        let bytes = self.allocate(layout)?;
        let ptr = if layout.size() == 0 {
            // A ZST has no room in a block. Its destructor runs from `FreeBox`'s `Drop`
            // on a fresh empty value: every ZST instance is indistinguishable.
            std::mem::forget(value);
            NonNull::dangling()
        } else {
            unsafe {
                let typed = bytes.as_ptr() as *mut T;
                typed.write(value);
                NonNull::new_unchecked(typed)
            }
        };
        Ok(FreeBox {
            ptr,
            alloc: self,
            layout,
        })
    }

    pub(crate) fn profiler(&self) -> &Profiler {
        &self.profile
    }

    pub(crate) fn allocate_at(
        &self,
        layout: Layout,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let thread = self.thread_id();
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
        let Some(need) = payload_needed(layout) else {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
            return Err(AllocError);
        };
        let result = if need > SIZE_CLASSES[SIZE_CLASSES.len() - 1] {
            self.alloc_large(layout, need, thread, caller)
        } else {
            self.alloc_small(layout, need, thread, caller)
        };
        if result.is_err() {
            self.profile
                .record_oom(layout.size(), layout.align(), caller, thread);
        }
        if self.checks && result.is_ok() {
            self.debug_check();
        }
        result
    }

    pub(crate) fn deallocate_at(
        &self,
        ptr: NonNull<u8>,
        layout: Layout,
        caller: &'static Location<'static>,
    ) {
        let thread = self.thread_id();
        if layout.size() == 0 {
            self.meters.note_free(0, 0);
            self.profile
                .record_dealloc(0, layout.align(), 0, caller, thread);
            return;
        }
        let header = self
            .resolve(ptr.as_ptr(), layout)
            .unwrap_or_else(|_| panic!("указатель не принадлежит этому аллокатору"));
        self.reject_double_free(header);
        self.profile.record_dealloc(
            layout.size(),
            layout.align(),
            ptr.as_ptr() as usize,
            caller,
            thread,
        );
        if block_flags(header) & F_LARGE != 0 {
            let user = unsafe { (*header).user_size };
            let payload = payload_len(header);
            let chunk = unsafe { (*header).chunk };
            self.meters.note_free(user, payload);
            self.destroy_chunk(chunk);
            return;
        }
        self.release_small(header, true);
        if self.checks {
            self.debug_check();
        }
    }

    /// Class a block can be placed in when this thread is allowed to cache it.
    pub(crate) fn cache_class(
        &self,
        ptr: NonNull<u8>,
        layout: Layout,
        thread: u64,
    ) -> Option<usize> {
        if layout.size() == 0 || layout.align() > MIN_ALIGN || thread == 0 {
            return None;
        }
        let header = self
            .resolve(ptr.as_ptr(), layout)
            .unwrap_or_else(|_| panic!("указатель не принадлежит этому аллокатору"));
        self.reject_double_free(header);
        if block_flags(header) & F_LARGE != 0 {
            return None;
        }
        let chunk = unsafe { (*header).chunk };
        unsafe {
            if (*chunk).large || (*chunk).owner_thread != thread {
                return None;
            }
        }
        bin_for_payload(payload_len(header))
    }

    pub(crate) fn mark_cached(&self, ptr: NonNull<u8>, layout: Layout) {
        let header = self
            .resolve(ptr.as_ptr(), layout)
            .unwrap_or_else(|_| panic!("указатель не принадлежит этому аллокатору"));
        let user = unsafe { (*header).user_size };
        let payload = payload_len(header);
        set_flags(header, F_CACHED);
        self.meters.note_cache(user, payload);
    }

    pub(crate) fn reuse_cached(&self, user: NonNull<u8>, layout: Layout) {
        let header = self
            .resolve(user.as_ptr(), layout)
            .unwrap_or_else(|_| panic!("указатель не принадлежит этому аллокатору"));
        if block_flags(header) & F_CACHED == 0 {
            panic!("повторное использование блока не из журнала");
        }
        let payload = payload_len(header);
        if layout.size() > payload {
            panic!("класс журнала меньше запроса");
        }
        set_flags(header, 0);
        unsafe { (*header).user_size = layout.size() };
        self.meters.note_reuse(layout.size(), payload);
    }

    pub(crate) fn free_cached(&self, user: NonNull<u8>) {
        let layout = Layout::from_size_align(8, 8).expect("layout");
        let header = self
            .resolve(user.as_ptr(), layout)
            .unwrap_or_else(|_| panic!("битый указатель журнала"));
        if block_flags(header) & F_CACHED == 0 {
            panic!("журнал освобождает блок не из кэша");
        }
        let payload = payload_len(header);
        set_flags(header, 0);
        self.meters.note_uncache(payload);
        self.release_small(header, false);
        if self.checks {
            self.debug_check();
        }
    }

    fn alloc_small(
        &self,
        layout: Layout,
        need: usize,
        owner: u64,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let class = class_for_request(need).ok_or(AllocError)?;
        let search = SIZE_CLASSES[class];
        if let Some(block) = self.take_block(search) {
            return self.finish_block(block, layout, search, caller);
        }
        self.create_chunk(search, false, owner)?;
        let block = self.take_block(search).ok_or(AllocError)?;
        self.finish_block(block, layout, search, caller)
    }

    fn alloc_large(
        &self,
        layout: Layout,
        need: usize,
        owner: u64,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let chunk = self.create_chunk(need, true, owner)?;
        let block = unsafe { (*chunk).data as *mut BlockHeader };
        unsafe { (*block).user_size = layout.size() };
        let user = match self.place_user(block, layout) {
            Ok(user) => user,
            Err(err) => {
                self.destroy_chunk(chunk);
                return Err(err);
            }
        };
        self.meters.note_alloc(layout.size(), payload_len(block));
        self.profile.record_alloc(
            layout.size(),
            layout.align(),
            user as usize,
            caller,
            self.thread_id(),
        );
        Ok(slice_ptr(user, layout.size()))
    }

    fn finish_block(
        &self,
        block: *mut BlockHeader,
        layout: Layout,
        keep: usize,
        caller: &'static Location<'static>,
    ) -> Result<NonNull<[u8]>, AllocError> {
        self.carve(block, keep);
        unsafe { (*block).user_size = layout.size() };
        let user = match self.place_user(block, layout) {
            Ok(user) => user,
            Err(err) => {
                self.release_small(block, false);
                return Err(err);
            }
        };
        self.meters.note_alloc(layout.size(), payload_len(block));
        self.profile.record_alloc(
            layout.size(),
            layout.align(),
            user as usize,
            caller,
            self.thread_id(),
        );
        Ok(slice_ptr(user, layout.size()))
    }

    fn carve(&self, block: *mut BlockHeader, keep_payload: usize) {
        let total = block_size(block);
        let payload = total - HEADER;
        let keep_payload = keep_payload.min(payload);
        let keep_size = HEADER + keep_payload;
        if keep_size >= total || total - keep_size < HEADER + 16 {
            set_flags(block, 0);
            return;
        }
        let rest = total - keep_size;
        let chunk = unsafe { (*block).chunk };
        let base = unsafe { (*chunk).alloc_ptr };
        set_size(block, keep_size, 0);
        let rest_block = base.with_addr(block as usize + keep_size) as *mut BlockHeader;
        unsafe {
            write_header(rest_block, rest, 0, keep_size, 0, MAGIC, chunk);
            let after = base.with_addr(block as usize + total) as *mut BlockHeader;
            (*after).prev_size = rest;
        }
        self.insert(rest_block);
    }

    fn take_block(&self, min_payload: usize) -> Option<*mut BlockHeader> {
        if let Some(start) = class_for_request(min_payload) {
            for index in start..SIZE_CLASSES.len() {
                let head = self.bins[index].get();
                if !head.is_null() {
                    self.unlink(head);
                    return Some(head);
                }
            }
        }
        let mut cursor = self.large_free.get();
        let mut steps = 0;
        while !cursor.is_null() {
            steps += 1;
            if steps > 1_000_000 {
                panic!("цикл в free-list");
            }
            if payload_len(cursor) >= min_payload {
                self.unlink(cursor);
                return Some(cursor);
            }
            cursor = unsafe { (*free_node(cursor)).next };
        }
        None
    }

    fn release_small(&self, block: *mut BlockHeader, adjust: bool) {
        if adjust {
            let user = unsafe { (*block).user_size };
            self.meters.note_free(user, payload_len(block));
        }
        let block = self.absorb_neighbors(block);
        self.insert(block);
    }

    fn absorb_neighbors(&self, mut block: *mut BlockHeader) -> *mut BlockHeader {
        let next = next_block(block);
        if !is_footer(next) && is_free(next) {
            self.unlink(next);
            let new_size = block_size(block) + block_size(next);
            set_size(block, new_size, 0);
            let after = next_block(block);
            unsafe { (*after).prev_size = new_size };
        }
        if let Some(prev) = prev_block(block)
            && is_free(prev)
        {
            self.unlink(prev);
            let new_size = block_size(prev) + block_size(block);
            set_size(prev, new_size, 0);
            let after = next_block(prev);
            unsafe { (*after).prev_size = new_size };
            block = prev;
        }
        block
    }

    fn insert(&self, block: *mut BlockHeader) {
        let payload = payload_len(block);
        debug_assert!(payload >= 16);
        let head_cell = self.bin_cell(payload);
        let head = head_cell.get();
        set_flags(block, F_FREE);
        poison(payload_ptr(block), payload);
        unsafe {
            ptr::write(
                free_node(block),
                FreeNode {
                    next: head,
                    prev: ptr::null_mut(),
                },
            );
            if !head.is_null() {
                (*free_node(head)).prev = block;
            }
        }
        head_cell.set(block);
    }

    fn unlink(&self, block: *mut BlockHeader) {
        let node = free_node(block);
        let next = unsafe { (*node).next };
        let prev = unsafe { (*node).prev };
        if prev.is_null() {
            let cell = self.bin_cell(payload_len(block));
            debug_assert!(cell.get() == block);
            cell.set(next);
        } else {
            unsafe { (*free_node(prev)).next = next };
        }
        if !next.is_null() {
            unsafe { (*free_node(next)).prev = prev };
        }
    }

    fn bin_cell(&self, payload: usize) -> &Cell<*mut BlockHeader> {
        match bin_for_payload(payload) {
            Some(index) => &self.bins[index],
            None => &self.large_free,
        }
    }

    fn create_chunk(
        &self,
        payload_need: usize,
        large: bool,
        owner: u64,
    ) -> Result<*mut FlChunk, AllocError> {
        let payload = round_up_min(payload_need).ok_or(AllocError)?;
        let min_region = HEADER
            .checked_add(payload)
            .and_then(|value| value.checked_add(HEADER))
            .ok_or(AllocError)?;
        let region = if large {
            min_region
        } else {
            min_region.max(SMALL_REGION)
        };
        let data_off = align_up(size_of::<FlChunk>(), MIN_ALIGN).ok_or(AllocError)?;
        let total = data_off.checked_add(region).ok_or(AllocError)?;
        let layout = Layout::from_size_align(total, MIN_ALIGN).map_err(|_| AllocError)?;
        let raw = allocate(layout, self.system_backend)?;
        let usable = (region - HEADER) & !0xF;
        if usable < HEADER + 16 {
            unsafe { chunk::deallocate(raw, layout, self.system_backend) };
            return Err(AllocError);
        }
        let chunk = raw.as_ptr() as *mut FlChunk;
        let data = unsafe { raw.as_ptr().add(data_off) };
        let footer = unsafe { data.add(usable) as *mut BlockHeader };
        let id = self.ensure_id();
        unsafe {
            ptr::write(
                chunk,
                FlChunk {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                    data,
                    footer,
                    alloc_ptr: raw.as_ptr(),
                    alloc_size: total,
                    alloc_align: MIN_ALIGN,
                    owner_thread: owner,
                    list_id: id,
                    large,
                    system: self.system_backend,
                },
            );
            write_header(
                data as *mut BlockHeader,
                usable,
                if large { F_LARGE } else { 0 },
                0,
                0,
                MAGIC,
                chunk,
            );
            write_header(footer, HEADER, 0, usable, 0, FOOTER_MAGIC, ptr::null_mut());
        }
        self.link_chunk(chunk);
        self.meters.add_reserved(total);
        self.meters.add_chunk();
        if !large {
            self.insert(data as *mut BlockHeader);
        }
        Ok(chunk)
    }

    fn destroy_chunk(&self, chunk: *mut FlChunk) {
        self.unlink_chunk(chunk);
        let (size, align, system, ptr) = unsafe {
            (
                (*chunk).alloc_size,
                (*chunk).alloc_align,
                (*chunk).system,
                (*chunk).alloc_ptr,
            )
        };
        self.meters.sub_reserved(size);
        self.meters.sub_chunk();
        let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
        unsafe { chunk::deallocate(NonNull::new_unchecked(ptr), layout, system) };
    }

    fn link_chunk(&self, chunk: *mut FlChunk) {
        let head = self.chunks.get();
        unsafe {
            (*chunk).next = head;
            (*chunk).prev = ptr::null_mut();
            if !head.is_null() {
                (*head).prev = chunk;
            }
        }
        self.chunks.set(chunk);
    }

    fn unlink_chunk(&self, chunk: *mut FlChunk) {
        unsafe {
            let next = (*chunk).next;
            let prev = (*chunk).prev;
            if prev.is_null() {
                debug_assert!(self.chunks.get() == chunk);
                self.chunks.set(next);
            } else {
                (*prev).next = next;
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
        }
    }

    fn place_user(&self, block: *mut BlockHeader, layout: Layout) -> Result<*mut u8, AllocError> {
        let chunk = unsafe { (*block).chunk };
        let base = unsafe { (*chunk).alloc_ptr };
        let payload = payload_ptr(block);
        let payload_end = payload as usize + payload_len(block);
        let user_addr = if layout.align() <= MIN_ALIGN {
            payload as usize
        } else {
            let min = (payload as usize)
                .checked_add(size_of::<usize>())
                .ok_or(AllocError)?;
            align_up(min, layout.align()).ok_or(AllocError)?
        };
        let user_end = user_addr.checked_add(layout.size()).ok_or(AllocError)?;
        if user_addr < payload as usize || user_end > payload_end {
            return Err(AllocError);
        }
        debug_assert!(user_addr % layout.align() == 0);
        if layout.align() > MIN_ALIGN {
            let offset = user_addr - block as usize;
            let word = user_addr - size_of::<usize>();
            unsafe { (base.with_addr(word) as *mut usize).write(offset) };
        }
        Ok(base.with_addr(user_addr))
    }

    fn resolve(&self, user: *mut u8, layout: Layout) -> Result<*mut BlockHeader, ()> {
        let addr = user as usize;
        let mut chunk = self.chunks.get();
        let mut steps = 0;
        while !chunk.is_null() {
            steps += 1;
            if steps > 1_000_000 {
                panic!("цикл в списке чанков");
            }
            let start = unsafe { (*chunk).data as usize };
            let end = unsafe { (*chunk).footer as usize };
            if addr >= start && addr < end {
                return header_in(chunk, user, layout);
            }
            chunk = unsafe { (*chunk).next };
        }
        Err(())
    }

    fn snapshot(&self) -> AllocStats {
        let mut hist = empty_histogram();
        let mut free = 0;
        let mut largest = 0;
        for bin in &self.bins {
            self.walk_chain(bin.get(), &mut hist, &mut free, &mut largest);
        }
        self.walk_chain(self.large_free.get(), &mut hist, &mut free, &mut largest);
        AllocStats::from_parts(
            self.meters.reserved(),
            self.meters.user_bytes(),
            self.meters.used_bytes(),
            free,
            largest,
            self.meters.live(),
            self.meters.peak(),
            self.meters.chunks(),
            self.meters.cached_bytes(),
            hist,
        )
    }

    fn walk_chain(
        &self,
        mut node: *mut BlockHeader,
        hist: &mut [usize; HIST_BUCKETS],
        free: &mut usize,
        largest: &mut usize,
    ) {
        let mut steps = 0;
        while !node.is_null() {
            steps += 1;
            if steps > 1_000_000 {
                panic!("цикл в free-list");
            }
            let payload = payload_len(node);
            *free += payload;
            if payload > *largest {
                *largest = payload;
            }
            if let Some(index) = hist_index(payload) {
                hist[index] += 1;
            }
            node = unsafe { (*free_node(node)).next };
        }
    }

    fn reject_double_free(&self, header: *mut BlockHeader) {
        let flags = block_flags(header);
        if flags & (F_FREE | F_CACHED) != 0 {
            panic!("double free");
        }
    }

    fn ensure_id(&self) -> u64 {
        let current = self.list_id.get();
        if current != 0 {
            return current;
        }
        let fresh = NEXT_ID.fetch_add(1, Ordering::Relaxed).max(1);
        self.list_id.set(fresh);
        fresh
    }

    fn thread_id(&self) -> u64 {
        if self.system_backend { 0 } else { thread_tag() }
    }

    fn debug_check(&self) {
        if !self.checks {
            return;
        }
        let mut chunk = self.chunks.get();
        let mut steps = 0;
        while !chunk.is_null() {
            steps += 1;
            if steps > 1_000_000 {
                panic!("цикл в списке чанков");
            }
            self.check_chunk(chunk);
            chunk = unsafe { (*chunk).next };
        }
    }

    fn check_chunk(&self, chunk: *mut FlChunk) {
        let footer = unsafe { (*chunk).footer };
        let mut block = unsafe { (*chunk).data as *mut BlockHeader };
        let mut prev = 0usize;
        let mut guard = 0;
        while block != footer {
            guard += 1;
            if guard > 1_000_000 {
                panic!("цикл в блоках чанка");
            }
            unsafe {
                if (*block).magic != MAGIC {
                    panic!("повреждён magic заголовка");
                }
                if (*block).chunk != chunk {
                    panic!("блок указывает не на свой чанк");
                }
                if (*block).prev_size != prev {
                    panic!("prev_size не сходится с предыдущим блоком");
                }
            }
            let size = block_size(block);
            if size < HEADER + 16 || !size.is_multiple_of(16) {
                panic!("некорректный размер блока");
            }
            let next = next_block(block);
            if is_free(block) {
                if !is_footer(next) && is_free(next) {
                    panic!("соседние свободные блоки не слиты");
                }
                if !self.list_contains(block) {
                    panic!("свободный блок отсутствует в списке");
                }
            } else if self.list_contains(block) {
                panic!("занятый блок висит в free-list");
            }
            prev = size;
            block = next;
        }
        unsafe {
            if (*footer).magic != FOOTER_MAGIC || (*footer).prev_size != prev {
                panic!("повреждён footer чанка");
            }
        }
    }

    fn list_contains(&self, block: *mut BlockHeader) -> bool {
        for bin in &self.bins {
            if chain_contains(bin.get(), block) {
                return true;
            }
        }
        chain_contains(self.large_free.get(), block)
    }
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FreeList {
    fn drop(&mut self) {
        let mut chunk = self.chunks.get();
        while !chunk.is_null() {
            let next = unsafe { (*chunk).next };
            let (size, align, system, ptr) = unsafe {
                (
                    (*chunk).alloc_size,
                    (*chunk).alloc_align,
                    (*chunk).system,
                    (*chunk).alloc_ptr,
                )
            };
            let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
            unsafe { chunk::deallocate(NonNull::new_unchecked(ptr), layout, system) };
            chunk = next;
        }
    }
}

unsafe impl Alloc for FreeList {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_at(layout, Location::caller())
    }

    #[track_caller]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.deallocate_at(ptr, layout, Location::caller());
    }

    fn stats(&self) -> AllocStats {
        self.snapshot()
    }
}

/// Pointer that returns its block to [`FreeList`] on drop.
pub struct FreeBox<'a, T> {
    ptr: NonNull<T>,
    alloc: &'a FreeList,
    layout: Layout,
}

impl<T> Deref for FreeBox<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for FreeBox<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for FreeBox<'_, T> {
    fn drop(&mut self) {
        unsafe {
            if self.layout.size() == 0 {
                ptr::drop_in_place(NonNull::<T>::dangling().as_ptr());
            } else {
                ptr::drop_in_place(self.ptr.as_ptr());
            }
            self.alloc.deallocate(self.ptr.cast(), self.layout);
        }
    }
}

fn slice_ptr(ptr: *mut u8, len: usize) -> NonNull<[u8]> {
    NonNull::slice_from_raw_parts(unsafe { NonNull::new_unchecked(ptr) }, len)
}

fn payload_ptr(block: *mut BlockHeader) -> *mut u8 {
    unsafe { (block as *mut u8).add(HEADER) }
}

fn payload_len(block: *mut BlockHeader) -> usize {
    block_size(block) - HEADER
}

fn free_node(block: *mut BlockHeader) -> *mut FreeNode {
    payload_ptr(block) as *mut FreeNode
}

fn block_size(block: *mut BlockHeader) -> usize {
    unsafe { (*block).size_and_flags & !FLAG_MASK }
}

fn block_flags(block: *mut BlockHeader) -> usize {
    unsafe { (*block).size_and_flags & FLAG_MASK }
}

fn is_free(block: *mut BlockHeader) -> bool {
    block_flags(block) & F_FREE != 0
}

fn is_footer(block: *mut BlockHeader) -> bool {
    unsafe { (*block).magic == FOOTER_MAGIC }
}

fn set_size(block: *mut BlockHeader, size: usize, flags: usize) {
    debug_assert!(size.is_multiple_of(16));
    debug_assert!(flags & !FLAG_MASK == 0);
    unsafe { (*block).size_and_flags = size | flags };
}

fn set_flags(block: *mut BlockHeader, flags: usize) {
    set_size(block, block_size(block), flags);
}

unsafe fn write_header(
    block: *mut BlockHeader,
    size: usize,
    flags: usize,
    prev: usize,
    user: usize,
    magic: usize,
    chunk: *mut FlChunk,
) {
    unsafe {
        ptr::write(
            block,
            BlockHeader {
                size_and_flags: size | flags,
                prev_size: prev,
                user_size: user,
                magic,
                chunk,
                _pad: 0,
            },
        );
    }
}

fn next_block(block: *mut BlockHeader) -> *mut BlockHeader {
    let size = block_size(block);
    let chunk = unsafe { (*block).chunk };
    if chunk.is_null() {
        return unsafe { (block as *mut u8).add(size) as *mut BlockHeader };
    }
    let base = unsafe { (*chunk).alloc_ptr };
    base.with_addr(block as usize + size) as *mut BlockHeader
}

fn prev_block(block: *mut BlockHeader) -> Option<*mut BlockHeader> {
    let prev_size = unsafe { (*block).prev_size };
    if prev_size == 0 {
        return None;
    }
    let chunk = unsafe { (*block).chunk };
    let base = unsafe { (*chunk).alloc_ptr };
    Some(base.with_addr(block as usize - prev_size) as *mut BlockHeader)
}

fn header_in(chunk: *mut FlChunk, user: *mut u8, layout: Layout) -> Result<*mut BlockHeader, ()> {
    let base = unsafe { (*chunk).alloc_ptr };
    let data = unsafe { (*chunk).data as usize };
    let footer = unsafe { (*chunk).footer as usize };
    let header_addr = if layout.align() <= MIN_ALIGN {
        let addr = (user as usize).checked_sub(HEADER).ok_or(())?;
        if addr < data || addr >= footer {
            return Err(());
        }
        addr
    } else {
        let word = (user as usize).checked_sub(size_of::<usize>()).ok_or(())?;
        if word < data || user as usize >= footer {
            return Err(());
        }
        let offset = unsafe { (base.with_addr(word) as *const usize).read() };
        let addr = (user as usize).checked_sub(offset).ok_or(())?;
        if addr < data || addr >= footer || offset < HEADER {
            return Err(());
        }
        addr
    };
    if header_addr % align_of::<BlockHeader>() != 0 {
        return Err(());
    }
    let header = base.with_addr(header_addr) as *mut BlockHeader;
    unsafe {
        if (*header).magic != MAGIC || (*header).chunk != chunk {
            return Err(());
        }
    }
    Ok(header)
}

fn chain_contains(mut node: *mut BlockHeader, needle: *mut BlockHeader) -> bool {
    let mut steps = 0;
    while !node.is_null() {
        if node == needle {
            return true;
        }
        steps += 1;
        if steps > 1_000_000 {
            panic!("цикл в free-list");
        }
        node = unsafe { (*free_node(node)).next };
    }
    false
}

fn poison(ptr: *mut u8, len: usize) {
    if cfg!(debug_assertions) && len > 0 {
        unsafe { ptr::write_bytes(ptr, 0xDE, len) };
    }
}

fn align_of<T>() -> usize {
    std::mem::align_of::<T>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_fragmentation_rounds_to_class() {
        let list = FreeList::new();
        let layout = Layout::from_size_align(10, 1).unwrap();
        list.allocate(layout).unwrap();
        let stats = list.stats();
        assert_eq!(stats.user_bytes, 10);
        assert_eq!(stats.used_bytes, 16);
        assert_eq!(stats.internal_fragmentation, 6);
        assert_eq!(stats.live_allocations, 1);
    }

    #[test]
    fn coalesce_removes_external_holes() {
        let list = FreeList::new();
        let layout = Layout::from_size_align(16, 8).unwrap();
        let a = list.allocate(layout).unwrap();
        let b = list.allocate(layout).unwrap();
        let c = list.allocate(layout).unwrap();
        unsafe {
            list.deallocate(NonNull::new_unchecked(a.as_ptr() as *mut u8), layout);
            list.deallocate(NonNull::new_unchecked(c.as_ptr() as *mut u8), layout);
        }
        let split = list.stats();
        assert!(split.external_fragmentation > 0);
        assert!(split.free_bytes > split.largest_free_block);
        unsafe { list.deallocate(NonNull::new_unchecked(b.as_ptr() as *mut u8), layout) };
        let whole = list.stats();
        assert_eq!(whole.live_allocations, 0);
        assert_eq!(whole.user_bytes, 0);
        assert_eq!(whole.external_fragmentation, 0);
    }

    #[test]
    #[should_panic(expected = "double free")]
    fn double_free_panics() {
        let list = FreeList::new();
        let layout = Layout::from_size_align(16, 8).unwrap();
        let ptr = list.allocate(layout).unwrap();
        unsafe {
            let thin = NonNull::new_unchecked(ptr.as_ptr() as *mut u8);
            list.deallocate(thin, layout);
            list.deallocate(thin, layout);
        }
    }
}
