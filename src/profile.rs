//! Allocation profiling.
//!
//! Counters and the callsite table live inside the allocator and allocate nothing.
//! The ring log, when enabled, comes from [`std::alloc::System`], so the
//! accounting does not allocate from the arena or the free-list again.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::cell::UnsafeCell;
use std::fmt;
use std::panic::Location;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

const CALLSITES: usize = 32;
const RING: usize = 1024;
const RECENT: usize = 8;
const SIZE_BUCKETS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    Alloc = 0,
    Dealloc = 1,
    Reset = 2,
    Oom = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
    pub size: usize,
    pub align: usize,
    pub ptr: usize,
    pub file: &'static str,
    pub line: u32,
    pub col: u32,
    pub thread_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallsiteStat {
    pub file: &'static str,
    pub line: u32,
    pub col: u32,
    pub count: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct ProfileReport {
    pub enabled: bool,
    pub events_recorded: u64,
    pub peak_live_bytes: usize,
    pub size_histogram: [u64; SIZE_BUCKETS],
    pub top_by_bytes: [Option<CallsiteStat>; RECENT],
    pub top_by_count: [Option<CallsiteStat>; RECENT],
    pub recent: [Option<Event>; RECENT],
}

impl fmt::Display for ProfileReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "профилирование: {}",
            if self.enabled { "вкл" } else { "выкл" }
        )?;
        writeln!(f, "событий:        {}", self.events_recorded)?;
        writeln!(f, "пик живых байт: {}", self.peak_live_bytes)?;
        writeln!(f, "топ по байтам:")?;
        for entry in self.top_by_bytes.into_iter().flatten() {
            writeln!(
                f,
                "  {}:{}:{}  {} байт, {} вызовов",
                entry.file, entry.line, entry.col, entry.bytes, entry.count
            )?;
        }
        writeln!(f, "топ по числу вызовов:")?;
        for entry in self.top_by_count.into_iter().flatten() {
            writeln!(
                f,
                "  {}:{}:{}  {} вызовов, {} байт",
                entry.file, entry.line, entry.col, entry.count, entry.bytes
            )?;
        }
        write!(
            f,
            "последние события: {}",
            self.recent.iter().flatten().count()
        )
    }
}

struct Callsite {
    loc: Cell<*const Location<'static>>,
    count: Cell<u64>,
    bytes: Cell<u64>,
}

struct Ring {
    slots: SysBuf<Slot>,
}

#[derive(Clone, Copy)]
struct Slot {
    seq: u64,
    kind: u8,
    size: usize,
    align: usize,
    ptr: usize,
    loc: *const Location<'static>,
    thread_id: u64,
}

/// Callsite accounting. The log may be touched only under exclusive access to
/// the allocator (one thread, or the spinlock).
pub struct Profiler {
    enabled: Cell<bool>,
    allow_log: bool,
    seq: Cell<u64>,
    live: Cell<usize>,
    peak_live: Cell<usize>,
    hist: [Cell<u64>; SIZE_BUCKETS],
    callsites: [Callsite; CALLSITES],
    overflow: Cell<u64>,
    ring: UnsafeCell<Option<Ring>>,
}

impl Profiler {
    pub const fn new(allow_log: bool) -> Self {
        Self {
            enabled: Cell::new(false),
            allow_log,
            seq: Cell::new(0),
            live: Cell::new(0),
            peak_live: Cell::new(0),
            hist: [const { Cell::new(0) }; SIZE_BUCKETS],
            callsites: [const { Callsite::new() }; CALLSITES],
            overflow: Cell::new(0),
            ring: UnsafeCell::new(None),
        }
    }

    pub fn enable(&self) {
        self.enabled.set(true);
        if self.allow_log
            && unsafe { (*self.ring.get()).is_none() }
            && let Some(slots) = SysBuf::new(RING)
        {
            unsafe { *self.ring.get() = Some(Ring { slots }) };
        }
    }

    pub fn disable(&self) {
        self.enabled.set(false);
    }

    pub fn record_alloc(
        &self,
        size: usize,
        align: usize,
        ptr: usize,
        caller: &'static Location<'static>,
        thread_id: u64,
    ) {
        if size > 0 {
            self.live.set(self.live.get().saturating_add(size));
            self.peak_live
                .set(self.peak_live.get().max(self.live.get()));
        }
        self.record(EventKind::Alloc, size, align, ptr, caller, thread_id, true);
    }

    pub fn record_dealloc(
        &self,
        size: usize,
        align: usize,
        ptr: usize,
        caller: &'static Location<'static>,
        thread_id: u64,
    ) {
        self.live.set(self.live.get().saturating_sub(size));
        self.record(
            EventKind::Dealloc,
            size,
            align,
            ptr,
            caller,
            thread_id,
            false,
        );
    }

    pub fn set_live(&self, live: usize) {
        self.live.set(live);
        self.peak_live.set(self.peak_live.get().max(live));
    }

    pub fn record_reset(&self, caller: &'static Location<'static>, thread_id: u64) {
        self.record(EventKind::Reset, 0, 1, 0, caller, thread_id, false);
    }

    /// Adjust the live-byte accounting after an in-place `realloc`, with no new event.
    pub fn retarget_live(&self, old: usize, new: usize) {
        if !self.enabled.get() {
            return;
        }
        let live = self.live.get().saturating_sub(old).saturating_add(new);
        self.live.set(live);
        self.peak_live.set(self.peak_live.get().max(live));
    }

    pub fn record_oom(
        &self,
        size: usize,
        align: usize,
        caller: &'static Location<'static>,
        thread_id: u64,
    ) {
        self.record(EventKind::Oom, size, align, 0, caller, thread_id, false);
    }

    pub fn report(&self) -> ProfileReport {
        let mut sites = [None; CALLSITES];
        let mut n = 0;
        for site in &self.callsites {
            let loc = site.loc.get();
            if loc.is_null() {
                continue;
            }
            let loc = unsafe { &*loc };
            sites[n] = Some(CallsiteStat {
                file: loc.file(),
                line: loc.line(),
                col: loc.column(),
                count: site.count.get(),
                bytes: site.bytes.get(),
            });
            n += 1;
        }
        let filled = &mut sites[..n];
        ProfileReport {
            enabled: self.enabled.get(),
            events_recorded: self.seq.get(),
            peak_live_bytes: self.peak_live.get(),
            size_histogram: std::array::from_fn(|i| self.hist[i].get()),
            top_by_bytes: top(filled, |a, b| a.bytes > b.bytes),
            top_by_count: top(filled, |a, b| a.count > b.count),
            recent: self.recent(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        kind: EventKind,
        size: usize,
        align: usize,
        ptr: usize,
        caller: &'static Location<'static>,
        thread_id: u64,
        count_site: bool,
    ) {
        if !self.enabled.get() {
            return;
        }
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        if count_site {
            let bucket = size_bucket(size);
            self.hist[bucket].set(self.hist[bucket].get() + 1);
            self.note_callsite(caller, size);
        }
        let ring = unsafe { &mut *self.ring.get() };
        if let Some(ring) = ring {
            let index = (seq as usize - 1) % ring.slots.len;
            ring.slots.write(
                index,
                Slot {
                    seq,
                    kind: kind as u8,
                    size,
                    align,
                    ptr,
                    loc: caller,
                    thread_id,
                },
            );
        }
    }

    fn note_callsite(&self, caller: &'static Location<'static>, size: usize) {
        let needle = caller as *const Location<'static>;
        for site in &self.callsites {
            let loc = site.loc.get();
            if loc == needle {
                site.count.set(site.count.get() + 1);
                site.bytes.set(site.bytes.get() + size as u64);
                return;
            }
            if loc.is_null() {
                site.loc.set(needle);
                site.count.set(1);
                site.bytes.set(size as u64);
                return;
            }
        }
        self.overflow.set(self.overflow.get() + 1);
    }

    fn recent(&self) -> [Option<Event>; RECENT] {
        let mut out = [None; RECENT];
        let ring = unsafe { &*self.ring.get() };
        let Some(ring) = ring else {
            return out;
        };
        let mut best = [0u64; RECENT];
        for index in 0..ring.slots.len {
            let slot = ring.slots.read(index);
            if slot.seq == 0 {
                continue;
            }
            if let Some(pos) = best.iter().position(|&seq| slot.seq > seq) {
                let mut cursor = RECENT - 1;
                while cursor > pos {
                    best[cursor] = best[cursor - 1];
                    out[cursor] = out[cursor - 1];
                    cursor -= 1;
                }
                best[pos] = slot.seq;
                out[pos] = Some(slot_event(slot));
            }
        }
        out
    }
}

impl Callsite {
    const fn new() -> Self {
        Self {
            loc: Cell::new(std::ptr::null()),
            count: Cell::new(0),
            bytes: Cell::new(0),
        }
    }
}

unsafe impl Send for Profiler {}

fn size_bucket(size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    (usize::BITS as usize - 1)
        .saturating_sub(size.leading_zeros() as usize)
        .min(SIZE_BUCKETS - 1)
}

fn top(
    sites: &mut [Option<CallsiteStat>],
    better: impl Fn(CallsiteStat, CallsiteStat) -> bool,
) -> [Option<CallsiteStat>; RECENT] {
    let mut out = [None; RECENT];
    for site in sites.iter().flatten().copied() {
        if let Some(pos) = out.iter().position(|slot| match slot {
            None => true,
            Some(current) => better(site, *current),
        }) {
            let mut cursor = RECENT - 1;
            while cursor > pos {
                out[cursor] = out[cursor - 1];
                cursor -= 1;
            }
            out[pos] = Some(site);
        }
    }
    out
}

fn slot_event(slot: Slot) -> Event {
    let (file, line, col) = if slot.loc.is_null() {
        ("", 0, 0)
    } else {
        let loc = unsafe { &*slot.loc };
        (loc.file(), loc.line(), loc.column())
    };
    Event {
        seq: slot.seq,
        kind: match slot.kind {
            0 => EventKind::Alloc,
            1 => EventKind::Dealloc,
            2 => EventKind::Reset,
            _ => EventKind::Oom,
        },
        size: slot.size,
        align: slot.align,
        ptr: slot.ptr,
        file,
        line,
        col,
        thread_id: slot.thread_id,
    }
}

struct SysBuf<T> {
    ptr: NonNull<T>,
    len: usize,
    layout: Layout,
}

impl<T> SysBuf<T> {
    fn new(len: usize) -> Option<Self> {
        let layout = Layout::array::<T>(len).ok()?;
        if layout.size() == 0 {
            return None;
        }
        let raw = unsafe { System.alloc(layout) };
        let raw = NonNull::new(raw)?;
        unsafe { ptr::write_bytes(raw.as_ptr(), 0, layout.size()) };
        Some(Self {
            ptr: raw.cast(),
            len,
            layout,
        })
    }

    fn read(&self, index: usize) -> T
    where
        T: Copy,
    {
        unsafe { self.ptr.add(index).read() }
    }

    fn write(&mut self, index: usize, value: T) {
        unsafe { self.ptr.add(index).write(value) };
    }
}

impl<T> Drop for SysBuf<T> {
    fn drop(&mut self) {
        unsafe { System.dealloc(self.ptr.as_ptr().cast(), self.layout) };
    }
}

static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);

/// Stable thread number inside the process. Does not allocate after the first call.
pub fn thread_tag() -> u64 {
    thread_local! {
        static TAG: u64 = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
    }
    TAG.with(|tag| *tag)
}
