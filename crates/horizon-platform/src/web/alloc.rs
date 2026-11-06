//! The threaded web build's allocator. std's on wasm is one dlmalloc behind
//! one lock, taken by spinning (a browser's page thread may not wait), by
//! every allocation of every thread: the workers' allocations held the
//! page's frames up and their spinning took the cores. Here each thread
//! keeps the blocks up to 64 KiB it frees, by size class, and hands them out
//! again without a lock; a miss, a larger or over-aligned block, or a full
//! class goes to a heap of its own ([`Heap`]). A class keeps at most 512
//! blocks and, past 1 KiB, 256 KiB of them.
//!
//! The heaps are dlmalloc's, each behind its own lock: the page's thread has
//! one to itself, and the workers share the others, a worker taking the next
//! one round. Blocks of 1 MiB and more come from one heap all threads share
//! ([`BIG`]): what a heap takes it keeps, and with every heap keeping the big
//! blocks it ever held (chunk arrays, far terrain, region files) the memory
//! grew by 600 MB in a minute's flight. A block goes back to the heap whose
//! memory it is in: each heap takes the browser's memory a page (64 KiB) at a
//! time, by the MiB, and marks the pages its own ([`OWNER`]). What a heap
//! frees at its top, and segments it no longer uses, go to a pool every heap
//! takes from before the memory grows ([`POOL`]): kept by each heap, what a
//! worker generating terrain had held at its peak stayed out of every other
//! thread's reach. (With one heap for every thread, a
//! Chromium profile of a laptop had the page's thread waiting on its lock
//! for a third of its time while chunks loaded, the chunk workers for 40%,
//! and the save workers for half; and std's dlmalloc grows the memory 64 KiB
//! at a time, holding its lock.)
//!
//! A thread's lists and heap live in its thread-local storage, which
//! wasm-bindgen sets up by allocating it (and the thread's stack): until
//! then this allocator passes straight through to std's, and frees of what
//! std's gave go back to it.
//!
//! Every lock here is [`Lock`]: workers sleep until the holder wakes them;
//! only the page's thread, which may not, spins. (On std's own, everyone
//! spins, and with a worker loading the world's generation data while the
//! page and the other workers allocated, the spinners kept the holder from
//! running: Chromium took 143 s over what Firefox did in 2.)
#[cfg(target_arch = "wasm32")]
use core::arch::wasm32::{memory_atomic_notify, memory_atomic_wait32, memory_grow};
#[cfg(target_arch = "wasm64")]
use core::arch::wasm64::{memory_atomic_notify, memory_atomic_wait32, memory_grow};
use dlmalloc::Dlmalloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, UnsafeCell},
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicI32, AtomicU8, AtomicUsize, Ordering},
};

/// Size classes, in bytes: a block up to the largest comes from a class.
const CLASSES: [usize; 24] = [
    16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192,
    12288, 16384, 24576, 32768, 49152, 65536,
];
/// Blocks a thread keeps of a class before its frees go back to std's.
const KEEP: u32 = 512;
/// Bytes a thread keeps of a class past 1 KiB: a worker that frees its meshes
/// keeps no more than this of each size for itself.
const KEEP_BYTES: usize = 256 << 10;
/// Blocks a thread keeps of class `c`.
const fn keep(c: usize) -> u32 {
    let by_bytes = KEEP_BYTES / CLASSES[c];
    if CLASSES[c] <= 1024 || by_bytes >= KEEP as usize {
        KEEP
    } else {
        by_bytes as u32
    }
}
/// Every class's blocks are aligned this far: as far as std's allocator aligns
/// everything (dlmalloc's 8 bytes on wasm32), as a block aligned further took
/// its `memalign`, which allocates more and splits it, under the lock.
#[cfg(target_arch = "wasm32")]
const ALIGN: usize = 8;
#[cfg(not(target_arch = "wasm32"))]
const ALIGN: usize = 16;
/// Below this address a thread-local's address is only its offset in the
/// block: the thread's storage isn't set up yet (its base is 0).
const SET_UP: usize = 64 << 10;

/// Heaps: the page's thread's (0), the workers', and the big blocks' (the last).
const HEAPS: usize = 5;
/// The heap of the blocks of [`BIG`] bytes or more, every thread's.
const SHARED: usize = HEAPS - 1;
/// From this size a block is the shared heap's.
const BIG: usize = 1 << 20;
/// What a heap takes of the browser's memory at a time.
const GRANULARITY: usize = 1 << 20;
/// A page of wasm memory.
const PAGE: usize = 64 << 10;
/// Pages the memory can have: 4 GiB on wasm32, the 8 GiB web/threads.toml
/// declares on wasm64.
#[cfg(target_arch = "wasm32")]
const MAX_PAGES: usize = 1 << 16;
#[cfg(target_arch = "wasm64")]
const MAX_PAGES: usize = 1 << 17;

/// The heap each page of memory is in, plus one; 0 for memory no heap gave
/// out (std's allocator's, the module's data, the stacks).
static OWNER: [AtomicU8; MAX_PAGES] = [const { AtomicU8::new(0) }; MAX_PAGES];

/// The heap `block` is in, if a heap gave it out.
fn owner(block: *mut u8) -> Option<usize> {
    let page = block as usize / PAGE;
    let owner = OWNER.get(page)?.load(Ordering::Acquire);
    (owner != 0).then(|| usize::from(owner) - 1)
}

/// A free region of the pool, written at its start.
#[repr(C)]
struct Region {
    size: usize,
    /// The next region's address (higher), 0 for none.
    next: usize,
}

/// Pages the heaps gave back, whole: free regions in address order, a list
/// through the regions themselves, neighbours merged.
struct Pool {
    lock: Lock,
    head: UnsafeCell<usize>,
}
// SAFETY: the list is only touched with the lock held.
unsafe impl Sync for Pool {}
static POOL: Pool = Pool {
    lock: Lock(AtomicI32::new(0)),
    head: UnsafeCell::new(0),
};
impl Pool {
    /// Takes back `[base, base + size)`, whole pages no heap uses any more.
    fn give(&self, base: usize, size: usize) {
        for page in base / PAGE..((base + size) / PAGE).min(MAX_PAGES) {
            OWNER[page].store(0, Ordering::Release);
        }
        self.lock.lock();
        // SAFETY: the lock is held; every region on the list is free memory
        // only the pool touches, as is the one given.
        unsafe {
            let head = &mut *self.head.get();
            let (mut prev, mut next) = (0, *head);
            while next != 0 && next < base {
                prev = next;
                next = (*(next as *const Region)).next;
            }
            let mut size = size;
            if next == base + size {
                let after = &*(next as *const Region);
                size += after.size;
                next = after.next;
            }
            if prev != 0 && prev + (*(prev as *const Region)).size == base {
                let before = &mut *(prev as *mut Region);
                before.size += size;
                before.next = next;
            } else {
                (base as *mut Region).write(Region { size, next });
                if prev == 0 {
                    *head = base;
                } else {
                    (*(prev as *mut Region)).next = base;
                }
            }
        }
        self.lock.unlock();
    }
    /// `size` bytes (whole pages) from the lowest region that has them.
    fn take(&self, size: usize) -> Option<usize> {
        self.lock.lock();
        // SAFETY: as in `give`.
        let taken = unsafe {
            let head = &mut *self.head.get();
            let (mut prev, mut at) = (0, *head);
            while at != 0 && (*(at as *const Region)).size < size {
                prev = at;
                at = (*(at as *const Region)).next;
            }
            if at != 0 {
                let Region { size: has, next } = (at as *const Region).read();
                let rest = if has > size {
                    let rest = at + size;
                    (rest as *mut Region).write(Region {
                        size: has - size,
                        next,
                    });
                    rest
                } else {
                    next
                };
                if prev == 0 {
                    *head = rest;
                } else {
                    (*(prev as *mut Region)).next = rest;
                }
            }
            (at != 0).then_some(at)
        };
        self.lock.unlock();
        taken
    }
}

/// A heap's memory: pages from the pool or `memory.grow`, marked as the
/// heap's; what it gives back goes to the pool (wasm memory never shrinks).
struct Pages {
    heap: u8,
}
// SAFETY: the regions handed out are whole pages of the memory, of the size
// given, which nothing else uses until the heap gives them back.
unsafe impl dlmalloc::Allocator for Pages {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let size = size.div_ceil(PAGE) * PAGE;
        let base = match POOL.take(size) {
            Some(base) => base,
            None => {
                let first = memory_grow::<0>(size / PAGE);
                if first == usize::MAX {
                    return (ptr::null_mut(), 0, 0);
                }
                first * PAGE
            }
        };
        for page in base / PAGE..((base + size) / PAGE).min(MAX_PAGES) {
            OWNER[page].store(self.heap + 1, Ordering::Release);
        }
        (base as *mut u8, size, 0)
    }
    fn remap(&self, _: *mut u8, _: usize, _: usize, _: bool) -> *mut u8 {
        ptr::null_mut()
    }
    fn free_part(&self, base: *mut u8, old: usize, new: usize) -> bool {
        // The tail of a segment: whole pages, as segments and the trims are.
        let (start, end) = (
            (base as usize + new).next_multiple_of(PAGE),
            base as usize + old,
        );
        if end > start {
            POOL.give(start, (end - start) / PAGE * PAGE);
        }
        true
    }
    fn free(&self, base: *mut u8, size: usize) -> bool {
        POOL.give(base as usize, size / PAGE * PAGE);
        true
    }
    fn can_release_part(&self, _: u32) -> bool {
        true
    }
    /// Pages from the pool hold what they held.
    fn allocates_zeros(&self) -> bool {
        false
    }
    fn page_size(&self) -> usize {
        PAGE
    }
}

/// One of the heaps: dlmalloc, behind a lock of its own.
struct Heap {
    lock: Lock,
    dl: UnsafeCell<Dlmalloc<Pages>>,
}
// SAFETY: the dlmalloc is only touched with the lock held (`Heap::with`).
unsafe impl Sync for Heap {}
impl Heap {
    const fn new(heap: u8) -> Self {
        let mut dl = Dlmalloc::new_with_allocator(Pages { heap });
        let _ = dl.set_granularity(GRANULARITY);
        Self {
            lock: Lock(AtomicI32::new(0)),
            dl: UnsafeCell::new(dl),
        }
    }
    /// `f` on the dlmalloc with the lock held. Nothing in it allocates, so
    /// no thread holds two heaps' locks.
    fn with<R>(&self, f: impl FnOnce(&mut Dlmalloc<Pages>) -> R) -> R {
        self.lock.lock();
        // SAFETY: the lock is held.
        let out = f(unsafe { &mut *self.dl.get() });
        self.lock.unlock();
        out
    }
}
static HEAP: [Heap; HEAPS] = heaps();
const fn heaps() -> [Heap; HEAPS] {
    let mut heaps = [const { MaybeUninit::<Heap>::uninit() }; HEAPS];
    let mut i = 0;
    while i < HEAPS {
        heaps[i] = MaybeUninit::new(Heap::new(i as u8));
        i += 1;
    }
    // SAFETY: every element was written above.
    unsafe { std::mem::transmute::<[MaybeUninit<Heap>; HEAPS], [Heap; HEAPS]>(heaps) }
}
/// The workers' next heap.
static NEXT_HEAP: AtomicUsize = AtomicUsize::new(0);

/// This thread's heap, once its storage is set up: the page's thread's own
/// (and a worker's until it has started), or the one a worker took.
fn my_heap() -> Option<usize> {
    MY_HEAP
        .try_with(|mine| {
            if (mine as *const Cell<u8> as usize) < SET_UP {
                return None;
            }
            if !may_sleep() {
                return Some(0);
            }
            let mut heap = mine.get();
            if heap == 0 {
                heap = 1 + (NEXT_HEAP.fetch_add(1, Ordering::Relaxed) % (SHARED - 1)) as u8;
                mine.set(heap);
            }
            Some(usize::from(heap))
        })
        .ok()
        .flatten()
}

/// The heap a block of `layout` comes from: the shared one for a big block,
/// else this thread's (none before its storage: std's).
fn heap_for(layout: Layout) -> Option<usize> {
    if layout.size() >= BIG {
        Some(SHARED)
    } else {
        my_heap()
    }
}

/// A block of `layout` from its heap ([`heap_for`]).
fn heap_alloc(layout: Layout, zeroed: bool) -> *mut u8 {
    match heap_for(layout) {
        // SAFETY: GlobalAlloc's contract on `layout`, passed on.
        Some(h) => HEAP[h].with(|dl| unsafe {
            if zeroed {
                dl.calloc(layout.size(), layout.align())
            } else {
                dl.malloc(layout.size(), layout.align())
            }
        }),
        // SAFETY: as above.
        None => locked(|| unsafe {
            if zeroed {
                System.alloc_zeroed(layout)
            } else {
                System.alloc(layout)
            }
        }),
    }
}
/// Frees `block`, of `layout`, into the heap it came from.
///
/// # Safety
/// `block` was allocated with `layout` (by `heap_alloc`, or std's).
unsafe fn heap_dealloc(block: *mut u8, layout: Layout) {
    match owner(block) {
        // SAFETY: the heap gave it out, with this layout.
        Some(h) => HEAP[h].with(|dl| unsafe { dl.free(block, layout.size(), layout.align()) }),
        // SAFETY: std's gave it out, with this layout.
        None => locked(|| unsafe { System.dealloc(block, layout) }),
    }
}

/// One thread's free blocks of each class: a list through the blocks' first
/// word, and its length.
struct Lists {
    head: [*mut u8; CLASSES.len()],
    len: [u32; CLASSES.len()],
}

thread_local! {
    static LISTS: UnsafeCell<Lists> = const {
        UnsafeCell::new(Lists { head: [ptr::null_mut(); CLASSES.len()], len: [0; CLASSES.len()] })
    };
    /// This thread is a worker, which may sleep ([`worker_started`]).
    static SLEEPS: Cell<bool> = const { Cell::new(false) };
    /// The heap this worker took, 0 before ([`my_heap`]).
    static MY_HEAP: Cell<u8> = const { Cell::new(0) };
}

/// Called on a worker once its thread-local storage is set up: from then
/// on it sleeps while another thread holds std's allocator.
pub(crate) fn worker_started() {
    let _ = SLEEPS.try_with(|s| s.set(true));
}
fn may_sleep() -> bool {
    SLEEPS
        .try_with(|s| (s as *const Cell<bool> as usize) >= SET_UP && s.get())
        .unwrap_or(false)
}

/// The lock in front of std's allocator: 0 free, 1 held, 2 held with
/// threads waiting (whose release wakes one). Ulrich Drepper's futex mutex.
struct Lock(AtomicI32);
static LOCK: Lock = Lock(AtomicI32::new(0));
/// Looks at a held lock before going to sleep: holds are short.
const SPINS: u32 = 64;
/// A sleeper looks again after this long whatever happens (nanoseconds).
const NAP: i64 = 1_000_000;
impl Lock {
    fn lock(&self) {
        if self
            .0
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        for _ in 0..SPINS {
            std::hint::spin_loop();
            if self.0.load(Ordering::Relaxed) == 0
                && self
                    .0
                    .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
        }
        let sleep = may_sleep();
        while self.0.swap(2, Ordering::Acquire) != 0 {
            if sleep {
                // SAFETY: the lock's word, a valid aligned i32 for as long as
                // the module runs; waiting is allowed off the page's thread.
                unsafe { memory_atomic_wait32(self.0.as_ptr(), 2, NAP) };
            } else {
                for _ in 0..SPINS {
                    std::hint::spin_loop();
                }
            }
        }
    }
    fn unlock(&self) {
        if self.0.swap(0, Ordering::Release) == 2 {
            // SAFETY: as in `lock`; notifying is allowed on any thread.
            unsafe { memory_atomic_notify(self.0.as_ptr(), 1) };
        }
    }
}
/// `f` (a call of std's allocator) with the lock held.
fn locked<R>(f: impl FnOnce() -> R) -> R {
    LOCK.lock();
    let out = f();
    LOCK.unlock();
    out
}

/// The class a layout's blocks come from.
fn class(layout: Layout) -> Option<usize> {
    if layout.align() > ALIGN {
        return None;
    }
    CLASSES.iter().position(|&size| layout.size() <= size)
}

/// What std's allocator gives a class's blocks.
fn block(class: usize) -> Layout {
    // SAFETY: the classes are nonzero multiples of ALIGN, a power of two.
    unsafe { Layout::from_size_align_unchecked(CLASSES[class], ALIGN) }
}

/// This thread's lists, once its storage is set up.
fn with_lists<R>(f: impl FnOnce(&mut Lists) -> R) -> Option<R> {
    LISTS
        .try_with(|lists| {
            if (lists.get() as usize) < SET_UP {
                return None;
            }
            // SAFETY: a thread's lists are only touched by that thread, and
            // never reentrantly (nothing here allocates).
            Some(f(unsafe { &mut *lists.get() }))
        })
        .ok()
        .flatten()
}

pub struct Cached;

// SAFETY: every block handed out is one a heap (or std's allocator) gave for
// its class's layout (or the caller's, for the rest), and goes back to the
// one that gave it with that same layout.
unsafe impl GlobalAlloc for Cached {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(c) = class(layout) else {
            return heap_alloc(layout, false);
        };
        let kept = with_lists(|lists| {
            let head = lists.head[c];
            if !head.is_null() {
                // SAFETY: a kept block's first word links to the next one.
                lists.head[c] = unsafe { *head.cast::<*mut u8>() };
                lists.len[c] -= 1;
            }
            head
        });
        match kept {
            Some(block) if !block.is_null() => block,
            _ => heap_alloc(block(c), false),
        }
    }

    unsafe fn dealloc(&self, block_ptr: *mut u8, layout: Layout) {
        let Some(c) = class(layout) else {
            // SAFETY: allocated with this layout.
            return unsafe { heap_dealloc(block_ptr, layout) };
        };
        let kept = with_lists(|lists| {
            if lists.len[c] >= keep(c) {
                return false;
            }
            // SAFETY: the block is at least 16 bytes, aligned for a pointer, and ours now.
            unsafe { *block_ptr.cast::<*mut u8>() = lists.head[c] };
            lists.head[c] = block_ptr;
            lists.len[c] += 1;
            true
        });
        if kept != Some(true) {
            // SAFETY: the class's blocks are allocated with its layout.
            unsafe { heap_dealloc(block_ptr, block(c)) };
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if class(layout).is_none() {
            return heap_alloc(layout, true);
        }
        // A kept block holds what it last held.
        // SAFETY: as `alloc`; the block has at least `layout.size()` bytes.
        unsafe {
            let block = self.alloc(layout);
            if !block.is_null() {
                ptr::write_bytes(block, 0, layout.size());
            }
            block
        }
    }

    unsafe fn realloc(&self, block_ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc promises a nonzero size that doesn't overflow
        // with this alignment.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (class(layout), class(new)) {
            (None, None) => match (owner(block_ptr), heap_for(layout), heap_for(new)) {
                // This thread's own heap's block, staying there: dlmalloc grows or shrinks it
                // in place where it can, the other threads' frees into the heap waiting
                // meanwhile.
                // SAFETY: the heap gave it out, with this layout.
                (Some(h), Some(from), Some(to)) if h == from && from == to && h != SHARED => {
                    HEAP[h].with(|dl| unsafe {
                        dl.realloc(block_ptr, layout.size(), layout.align(), new_size)
                    })
                }
                // SAFETY: std's block, with its layout.
                (None, None, None) => {
                    locked(|| unsafe { System.realloc(block_ptr, layout, new_size) })
                }
                // Another heap's, or a big block (whose copy would hold every thread's big
                // blocks up): moved into its new heap, the bytes copied outside the locks.
                // SAFETY: a fresh block for the new layout, the old bytes copied into it and
                // the old block given back with its layout.
                _ => unsafe {
                    let moved = heap_alloc(new, false);
                    if !moved.is_null() {
                        ptr::copy_nonoverlapping(block_ptr, moved, layout.size().min(new_size));
                        heap_dealloc(block_ptr, layout);
                    }
                    moved
                },
            },
            // The class's block has room for it already.
            (Some(a), Some(b)) if a == b => block_ptr,
            // SAFETY: a fresh block for the new layout, the old one's bytes
            // copied into it and the old one freed with its layout.
            _ => unsafe {
                let moved = self.alloc(new);
                if !moved.is_null() {
                    ptr::copy_nonoverlapping(block_ptr, moved, layout.size().min(new_size));
                    self.dealloc(block_ptr, layout);
                }
                moved
            },
        }
    }
}

#[global_allocator]
static ALLOCATOR: Cached = Cached;
