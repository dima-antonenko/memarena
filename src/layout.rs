//! Alignment and size classes.

use std::alloc::Layout;

/// Minimum alignment of a free-list payload.
pub const MIN_ALIGN: usize = 16;

/// Payload size classes, in bytes. Anything larger than the last goes in its own chunk.
pub const SIZE_CLASSES: [usize; 16] = [
    16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096,
];

pub const HIST_BUCKETS: usize = SIZE_CLASSES.len() + 1;
pub const LARGE_HIST: usize = SIZE_CLASSES.len();

/// Round an address up to a multiple of `align`. `align` is a power of two.
#[inline]
pub fn align_up(addr: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    let mask = align - 1;
    Some(addr.checked_add(mask)? & !mask)
}

/// Round a size up to a multiple of 16.
#[inline]
pub fn round_up_min(size: usize) -> Option<usize> {
    align_up(size.max(1), MIN_ALIGN)
}

/// Class index for a request: the smallest class that fits `size`.
pub fn class_for_request(size: usize) -> Option<usize> {
    SIZE_CLASSES.iter().position(|&class| class >= size)
}

/// Bin index for an existing free block.
/// `None` means a large block (payload larger than 4096 bytes).
pub fn bin_for_payload(payload: usize) -> Option<usize> {
    if payload > SIZE_CLASSES[SIZE_CLASSES.len() - 1] {
        return None;
    }
    let mut found = None;
    for (index, &class) in SIZE_CLASSES.iter().enumerate() {
        if class <= payload {
            found = Some(index);
        } else {
            break;
        }
    }
    found
}

/// Histogram bucket for an arbitrary size. Zero bytes go in no bucket.
pub fn hist_index(size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    Some(bin_for_payload(size).unwrap_or(LARGE_HIST))
}

/// Thin pointer from an `allocate` result.
pub fn allocation_ptr(ptr: std::ptr::NonNull<[u8]>) -> std::ptr::NonNull<u8> {
    let thin = ptr.as_ptr() as *mut u8;
    debug_assert!(!thin.is_null());
    unsafe { std::ptr::NonNull::new_unchecked(thin) }
}

/// Dangling pointer for a ZST: the cursor does not move, as with the system allocator.
pub fn dangling_slice(layout: Layout) -> std::ptr::NonNull<[u8]> {
    let thin = unsafe { std::ptr::NonNull::new_unchecked(layout.align() as *mut u8) };
    std::ptr::NonNull::slice_from_raw_parts(thin, 0)
}

/// How many payload bytes are needed to place `layout`.
///
/// Alignment above [`MIN_ALIGN`] adds slack for the offset back to the header.
pub fn payload_needed(layout: Layout) -> Option<usize> {
    if layout.size() == 0 {
        return Some(0);
    }
    if layout.align() <= MIN_ALIGN {
        return Some(layout.size());
    }
    layout
        .size()
        .checked_add(layout.align())?
        .checked_add(std::mem::size_of::<usize>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_powers() {
        assert_eq!(align_up(0, 16), Some(0));
        assert_eq!(align_up(1, 16), Some(16));
        assert_eq!(align_up(16, 16), Some(16));
        assert_eq!(align_up(17, 64), Some(64));
    }

    #[test]
    fn classes_cover_requests() {
        assert_eq!(class_for_request(1), Some(0));
        assert_eq!(class_for_request(16), Some(0));
        assert_eq!(class_for_request(17), Some(1));
        assert_eq!(class_for_request(4096), Some(15));
        assert_eq!(class_for_request(4097), None);
        assert_eq!(bin_for_payload(20), Some(0));
        assert_eq!(bin_for_payload(4097), None);
    }
}
