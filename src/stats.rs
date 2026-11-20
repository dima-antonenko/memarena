//! Fragmentation counters.
//!
//! Internal fragmentation is the gap between the rounded block and the user's
//! request. External fragmentation is the free bytes that do not fit in the
//! largest contiguous hole.

use crate::layout::HIST_BUCKETS;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Snapshot of an allocator. All sizes are in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocStats {
    /// Bytes taken from the system.
    pub reserved: usize,
    /// Sum of the requested sizes of live objects.
    pub user_bytes: usize,
    /// Those same objects after rounding and alignment.
    pub used_bytes: usize,
    /// Free contiguous spans that can be allocated, plus the thread cache.
    pub free_bytes: usize,
    /// `used_bytes - user_bytes`.
    pub internal_fragmentation: usize,
    /// `free_bytes - largest_free_block`.
    pub external_fragmentation: usize,
    /// Largest free contiguous span.
    pub largest_free_block: usize,
    /// Number of live allocations.
    pub live_allocations: usize,
    /// Peak `user_bytes`.
    pub peak_user_bytes: usize,
    /// Number of chunks held from the system.
    pub chunk_count: usize,
    /// Bytes in the thread-local magazine (not yet returned to the shared free-list).
    pub cached_bytes: usize,
    /// Free-block counts by class. The last bucket is blocks larger than 4096.
    pub free_histogram: [usize; HIST_BUCKETS],
}

impl AllocStats {
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        reserved: usize,
        user_bytes: usize,
        used_bytes: usize,
        free_bytes: usize,
        largest_free_block: usize,
        live_allocations: usize,
        peak_user_bytes: usize,
        chunk_count: usize,
        cached_bytes: usize,
        free_histogram: [usize; HIST_BUCKETS],
    ) -> Self {
        Self {
            reserved,
            user_bytes,
            used_bytes,
            free_bytes,
            internal_fragmentation: used_bytes.saturating_sub(user_bytes),
            external_fragmentation: free_bytes.saturating_sub(largest_free_block),
            largest_free_block,
            live_allocations,
            peak_user_bytes,
            chunk_count,
            cached_bytes,
            free_histogram,
        }
    }
}

impl fmt::Display for AllocStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "зарезервировано у системы: {}", self.reserved)?;
        writeln!(f, "байты запросов:            {}", self.user_bytes)?;
        writeln!(f, "байты после округления:    {}", self.used_bytes)?;
        writeln!(f, "свободно:                  {}", self.free_bytes)?;
        writeln!(
            f,
            "внутренняя фрагментация:   {}",
            self.internal_fragmentation
        )?;
        writeln!(
            f,
            "внешняя фрагментация:      {}",
            self.external_fragmentation
        )?;
        writeln!(f, "крупнейшая дыра:           {}", self.largest_free_block)?;
        writeln!(f, "живых выдач:               {}", self.live_allocations)?;
        writeln!(f, "пик запросов:              {}", self.peak_user_bytes)?;
        writeln!(f, "чанков:                    {}", self.chunk_count)?;
        writeln!(f, "в журнале потока:          {}", self.cached_bytes)?;
        write!(f, "гистограмма свободных:     {:?}", self.free_histogram)
    }
}

/// Atomic counters. The peak is updated with `fetch_max`.
pub struct Meters {
    reserved: AtomicUsize,
    user_bytes: AtomicUsize,
    used_bytes: AtomicUsize,
    live: AtomicUsize,
    peak_user: AtomicUsize,
    chunks: AtomicUsize,
    cached_bytes: AtomicUsize,
}

impl Meters {
    pub const fn new() -> Self {
        Self {
            reserved: AtomicUsize::new(0),
            user_bytes: AtomicUsize::new(0),
            used_bytes: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            peak_user: AtomicUsize::new(0),
            chunks: AtomicUsize::new(0),
            cached_bytes: AtomicUsize::new(0),
        }
    }

    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Relaxed)
    }

    pub fn user_bytes(&self) -> usize {
        self.user_bytes.load(Ordering::Relaxed)
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed)
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    pub fn peak(&self) -> usize {
        self.peak_user.load(Ordering::Relaxed)
    }

    pub fn chunks(&self) -> usize {
        self.chunks.load(Ordering::Relaxed)
    }

    pub fn cached_bytes(&self) -> usize {
        self.cached_bytes.load(Ordering::Relaxed)
    }

    pub fn set_reserved(&self, value: usize) {
        self.reserved.store(value, Ordering::Relaxed);
    }

    pub fn add_reserved(&self, value: usize) {
        self.reserved.fetch_add(value, Ordering::Relaxed);
    }

    pub fn sub_reserved(&self, value: usize) {
        self.sub(&self.reserved, value);
    }

    pub fn set_chunks(&self, value: usize) {
        self.chunks.store(value, Ordering::Relaxed);
    }

    pub fn add_chunk(&self) {
        self.chunks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn sub_chunk(&self) {
        self.sub(&self.chunks, 1);
    }

    pub fn set_user_live(&self, user: usize, live: usize) {
        self.user_bytes.store(user, Ordering::Relaxed);
        self.live.store(live, Ordering::Relaxed);
    }

    pub fn note_alloc(&self, user: usize, used: usize) {
        self.add_user(user);
        self.used_bytes.fetch_add(used, Ordering::Relaxed);
        self.live.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_user(&self, user: usize) {
        let prev = self.user_bytes.fetch_add(user, Ordering::Relaxed);
        self.peak_user
            .fetch_max(prev.saturating_add(user), Ordering::Relaxed);
    }

    pub fn sub_user(&self, user: usize) {
        self.sub(&self.user_bytes, user);
    }

    pub fn set_used(&self, value: usize) {
        self.used_bytes.store(value, Ordering::Relaxed);
    }

    pub fn note_free(&self, user: usize, used: usize) {
        self.sub(&self.user_bytes, user);
        self.sub(&self.used_bytes, used);
        self.sub(&self.live, 1);
    }

    /// The block moved into the thread magazine: free for the user, not yet on the shared list.
    pub fn note_cache(&self, user: usize, payload: usize) {
        self.sub(&self.user_bytes, user);
        self.sub(&self.used_bytes, payload);
        self.sub(&self.live, 1);
        self.cached_bytes.fetch_add(payload, Ordering::Relaxed);
    }

    pub fn note_reuse(&self, user: usize, payload: usize) {
        self.sub(&self.cached_bytes, payload);
        self.note_alloc(user, payload);
    }

    pub fn note_uncache(&self, payload: usize) {
        self.sub(&self.cached_bytes, payload);
    }

    fn sub(&self, slot: &AtomicUsize, value: usize) {
        let prev = slot.fetch_sub(value, Ordering::Relaxed);
        debug_assert!(prev >= value, "счётчик ушёл ниже нуля");
    }
}

pub fn empty_histogram() -> [usize; HIST_BUCKETS] {
    [0; HIST_BUCKETS]
}
