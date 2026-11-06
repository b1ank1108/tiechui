use rquickjs::allocator::{Allocator, RustAllocator};
use std::{cell::Cell, ptr, rc::Rc};

const LIMIT: usize = 64 * 1024 * 1024;

pub(super) struct Heap {
    used: usize,
    pub exhausted: Rc<Cell<bool>>,
}
impl Heap {
    pub fn new(exhausted: Rc<Cell<bool>>) -> Self {
        Self { used: 0, exhausted }
    }
    fn allows(&self, size: usize, previous: usize) -> bool {
        // Reserve room for the backing allocator's alignment and allocation header.
        let fits = size
            .checked_add(32)
            .and_then(|size| (self.used - previous).checked_add(size))
            .is_some_and(|size| size <= LIMIT);
        if !fits {
            self.exhausted.set(true);
        }
        fits
    }
    fn allocated(&mut self, ptr: *mut u8, previous: usize) -> *mut u8 {
        if ptr.is_null() {
            self.exhausted.set(true);
        } else {
            // This pointer was just allocated by RustAllocator.
            self.used = self.used - previous + unsafe { RustAllocator::usable_size(ptr) } + 16;
        }
        ptr
    }
}

// All allocations and pointer operations delegate to one backing allocator. Accounting only
// rejects allocations; it does not change their layout, alignment, or ownership.
unsafe impl Allocator for Heap {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.allows(size, 0) {
            return ptr::null_mut();
        }
        self.allocated(RustAllocator.alloc(size), 0)
    }
    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            self.exhausted.set(true);
            return ptr::null_mut();
        };
        if !self.allows(total, 0) {
            return ptr::null_mut();
        }
        self.allocated(RustAllocator.calloc(count, size), 0)
    }
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if !ptr.is_null() {
            // The allocator contract supplies a live pointer allocated by this instance.
            unsafe {
                self.used -= RustAllocator::usable_size(ptr) + 16;
                RustAllocator.dealloc(ptr);
            }
        }
    }
    unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(size);
        }
        if size == 0 {
            unsafe {
                self.dealloc(ptr);
            }
            return ptr::null_mut();
        }
        // The allocator contract supplies a live pointer allocated by this instance.
        let previous = unsafe { RustAllocator::usable_size(ptr) } + 16;
        if !self.allows(size, previous) {
            return ptr::null_mut();
        }
        let replacement = unsafe { RustAllocator.realloc(ptr, size) };
        self.allocated(replacement, previous)
    }
    unsafe fn usable_size(ptr: *mut u8) -> usize {
        if ptr.is_null() {
            0
        } else {
            // Every non-null allocation is owned by the backing allocator.
            unsafe { RustAllocator::usable_size(ptr) }
        }
    }
}
