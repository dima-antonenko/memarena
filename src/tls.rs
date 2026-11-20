//! Per-thread arenas and the free-list magazine.
//!
//! Each thread has its own [`Arena`]. [`ThreadCache`] keeps that thread's hot
//! blocks in a magazine and hands a foreign free to the shared [`SharedFreeList`].

use crate::arena::Arena;
use crate::freelist::FreeList;
use crate::layout::{MIN_ALIGN, SIZE_CLASSES, class_for_request, payload_needed};
use crate::profile::thread_tag;
use crate::stats::AllocStats;
use crate::sync::SpinLock;
use crate::{Alloc, AllocError};
use std::alloc::Layout;
use std::cell::RefCell;
use std::panic::Location;
use std::ptr::{self, NonNull};
use std::sync::Arc;

const MAG_LIMIT: u16 = 32;

thread_local! {
    static LOCAL_ARENA: RefCell<Arena> = const { RefCell::new(Arena::new()) };
    static MAGAZINES: RefCell<Vec<MagazineSlot>> = const { RefCell::new(Vec::new()) };
}

/// An arena private to each thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadLocalArena;

impl ThreadLocalArena {
    pub const fn new() -> Self {
        Self
    }

    pub fn with<R>(&self, f: impl FnOnce(&Arena) -> R) -> R {
        LOCAL_ARENA.with(|cell| {
            let arena = cell.borrow();
            f(&arena)
        })
    }

    pub fn reset(&self) {
        LOCAL_ARENA.with(|cell| cell.borrow_mut().reset());
    }
}

/// Shared free-list. Every heap operation runs under the spinlock.
pub struct SharedFreeList {
    lock: SpinLock<FreeList>,
}

impl SharedFreeList {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            lock: SpinLock::new(FreeList::new()),
        })
    }

    pub fn lock(&self) -> crate::sync::SpinGuard<'_, FreeList> {
        self.lock.lock()
    }

    pub fn stats(&self) -> AllocStats {
        self.lock().stats()
    }

    pub fn enable_profiling(&self) {
        self.lock().enable_profiling();
    }

    pub fn disable_profiling(&self) {
        self.lock().disable_profiling();
    }

    pub fn profile_report(&self) -> crate::profile::ProfileReport {
        self.lock().profile_report()
    }
}

impl Default for SharedFreeList {
    fn default() -> Self {
        Self {
            lock: SpinLock::new(FreeList::new()),
        }
    }
}

unsafe impl Alloc for SharedFreeList {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.lock().allocate_at(layout, Location::caller())
    }

    #[track_caller]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.lock().deallocate_at(ptr, layout, Location::caller());
    }

    fn stats(&self) -> AllocStats {
        self.lock().stats()
    }
}

/// Per-thread cache on top of [`SharedFreeList`].
///
/// A block is cached only when its chunk belongs to the current thread. Freeing
/// a foreign block goes straight to the shared list under the spinlock.
pub struct ThreadCache {
    shared: Arc<SharedFreeList>,
    id: u64,
}

impl ThreadCache {
    pub fn new() -> Self {
        let shared = SharedFreeList::new();
        let id = shared.lock().id();
        Self { shared, id }
    }

    pub fn stats(&self) -> AllocStats {
        self.shared.stats()
    }

    pub fn enable_profiling(&self) {
        self.shared.enable_profiling();
    }

    pub fn disable_profiling(&self) {
        self.shared.disable_profiling();
    }

    pub fn profile_report(&self) -> crate::profile::ProfileReport {
        self.shared.profile_report()
    }

    #[track_caller]
    pub fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let caller = Location::caller();
        let thread = thread_tag();
        if let Some(class) = fast_class(layout)
            && let Some(user) = with_magazine(&self.shared, self.id, |mag| pop(mag, class))
        {
            let guard = self.shared.lock();
            let ptr = unsafe { NonNull::new_unchecked(user) };
            guard.reuse_cached(ptr, layout);
            guard.profiler().record_alloc(
                layout.size(),
                layout.align(),
                user as usize,
                caller,
                thread,
            );
            return Ok(NonNull::slice_from_raw_parts(ptr, layout.size()));
        }
        self.shared.lock().allocate_at(layout, caller)
    }

    /// # Safety
    /// `ptr` came from this cache (or its shared free-list) with the same `layout`.
    #[track_caller]
    pub unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let caller = Location::caller();
        let thread = thread_tag();
        with_magazine(&self.shared, self.id, |_| {});
        let guard = self.shared.lock();
        if let Some(class) = guard.cache_class(ptr, layout, thread)
            && magazine_len(&self.shared, self.id, class) < MAG_LIMIT
        {
            guard.mark_cached(ptr, layout);
            guard.profiler().record_dealloc(
                layout.size(),
                layout.align(),
                ptr.as_ptr() as usize,
                caller,
                thread,
            );
            drop(guard);
            with_magazine(&self.shared, self.id, |mag| push(mag, class, ptr.as_ptr()));
            return;
        }
        guard.deallocate_at(ptr, layout, caller);
    }
}

impl Clone for ThreadCache {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            id: self.id,
        }
    }
}

impl Default for ThreadCache {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl Alloc for ThreadCache {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        ThreadCache::allocate(self, layout)
    }

    #[track_caller]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { ThreadCache::deallocate(self, ptr, layout) };
    }

    fn stats(&self) -> AllocStats {
        ThreadCache::stats(self)
    }
}

struct Magazine {
    head: [*mut u8; SIZE_CLASSES.len()],
    len: [u16; SIZE_CLASSES.len()],
}

impl Magazine {
    fn empty() -> Self {
        Self {
            head: [ptr::null_mut(); SIZE_CLASSES.len()],
            len: [0; SIZE_CLASSES.len()],
        }
    }
}

struct MagazineSlot {
    id: u64,
    mag: Magazine,
    shared: Arc<SharedFreeList>,
}

impl Drop for MagazineSlot {
    fn drop(&mut self) {
        for class in 0..SIZE_CLASSES.len() {
            let mut user = self.mag.head[class];
            self.mag.head[class] = ptr::null_mut();
            self.mag.len[class] = 0;
            while !user.is_null() {
                let next = unsafe { (user as *const *mut u8).read() };
                self.shared
                    .lock()
                    .free_cached(unsafe { NonNull::new_unchecked(user) });
                user = next;
            }
        }
    }
}

fn with_magazine<R>(
    shared: &Arc<SharedFreeList>,
    id: u64,
    f: impl FnOnce(&mut Magazine) -> R,
) -> R {
    MAGAZINES.with(|cell| {
        let mut slots = cell.borrow_mut();
        if !slots.iter().any(|slot| slot.id == id) {
            slots.push(MagazineSlot {
                id,
                mag: Magazine::empty(),
                shared: Arc::clone(shared),
            });
        }
        let slot = slots.iter_mut().find(|slot| slot.id == id).expect("журнал");
        f(&mut slot.mag)
    })
}

fn magazine_len(shared: &Arc<SharedFreeList>, id: u64, class: usize) -> u16 {
    with_magazine(shared, id, |mag| mag.len[class])
}

fn pop(mag: &mut Magazine, class: usize) -> Option<*mut u8> {
    let head = mag.head[class];
    if head.is_null() {
        return None;
    }
    unsafe {
        mag.head[class] = (head as *const *mut u8).read();
    }
    mag.len[class] = mag.len[class].saturating_sub(1);
    Some(head)
}

fn push(mag: &mut Magazine, class: usize, user: *mut u8) {
    unsafe {
        (user as *mut *mut u8).write(mag.head[class]);
    }
    mag.head[class] = user;
    mag.len[class] = mag.len[class].saturating_add(1);
}

fn fast_class(layout: Layout) -> Option<usize> {
    if layout.size() == 0 || layout.align() > MIN_ALIGN {
        return None;
    }
    class_for_request(payload_needed(layout)?)
}
