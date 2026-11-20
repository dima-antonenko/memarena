//! `memarena` — a bump pointer, a growing arena, and a free-list.
//!
//! # Safety model
//!
//! `alloc` takes `&self` and returns `&mut T` for the allocator's lifetime.
//! That is sound because each allocation is a distinct object: the references
//! do not alias. `reset` and `rewind` take `&mut self`, so the compiler rejects
//! a rewind while those references are still live.
//!
//! Raw `deallocate` is `unsafe`: the pointer must come from the same allocator
//! with the same [`Layout`], and it may be freed only once. The arena and the
//! bump do not free individual objects; their `deallocate` is a no-op.
//!
//! Destructors of values placed by `alloc` do not run on their own. The
//! free-list does that through [`FreeBox`]. A ZST does not move the cursor and
//! receives a dangling aligned pointer, as with the system allocator.
//!
//! # Fragmentation
//!
//! * internal — rounding and alignment bytes inside live blocks;
//! * external — free bytes that are not part of the largest contiguous hole.
//!
//! A bump has a single free tail, so its external fragmentation is zero.
//! An arena counts the tails of chunks it has already left.

#![deny(unsafe_op_in_unsafe_fn)]

mod arena;
mod bump;
mod chunk;
mod freelist;
#[cfg(feature = "global")]
mod global;
mod layout;
mod profile;
mod stats;
mod sync;
mod tls;

pub use arena::{Arena, ArenaCheckpoint};
pub use bump::{Bump, BumpCheckpoint};
pub use freelist::{FreeBox, FreeList};
#[cfg(feature = "global")]
pub use global::GlobalFreeList;
pub use layout::{SIZE_CLASSES, allocation_ptr};
pub use profile::{CallsiteStat, Event, EventKind, ProfileReport};
pub use stats::AllocStats;
pub use tls::{SharedFreeList, ThreadCache, ThreadLocalArena};

use std::alloc::Layout;
use std::fmt;
use std::ptr::NonNull;

/// The system refused the memory, or the request does not fit in the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocError;

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("не удалось выделить память")
    }
}

impl std::error::Error for AllocError {}

/// Shared interface of the allocators in this crate.
///
/// # Safety
///
/// An implementation must:
/// * return a pointer aligned to `layout.align()` and covering `layout.size()`
///   bytes, or [`AllocError`];
/// * for a zero size, return an aligned dangling pointer and not move the cursor;
/// * in `deallocate`, accept only the matching pointer and the same layout;
/// * not alias live allocations.
pub unsafe trait Alloc {
    #[track_caller]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError>;

    /// # Safety
    ///
    /// `ptr` came from `allocate` on this same value with the same `layout` and has not been freed yet.
    #[track_caller]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout);

    fn stats(&self) -> AllocStats;
}
