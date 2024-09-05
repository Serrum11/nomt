use super::PAGE_SIZE;
use parking_lot::{RwLock, RwLockWriteGuard};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicPtr, AtomicU32, Ordering},
        Arc,
    },
};

// Region is 256 MiB. The choice is mostly arbitrary, but:
//
// 1. it's big enough so that we don't have to allocate too often.
// 2. it's a multiple of 2 MiB which is the size of huge-page on x86-64 and aarch64.
// 3. it fits 65k 4 KiB pages which requires 2 bytes to address making it nice round number.
//
// If we take 16 bits for the slot index, we are left with another 16 bit which we can use to address
// 64K regions. That makes up a total of 16 TiB of memory, way beyond of the current hardware limits.
// Because of that, we limit ourselves to 4096 regions which gives us only 1 TiB of memory.
const REGION_SLOT_BITS: u32 = 16;
const REGION_SLOT_MASK: u32 = (1 << REGION_SLOT_BITS) - 1;
const SLOTS_PER_REGION: usize = 1 << REGION_SLOT_BITS;
const REGION_BYTE_SIZE: usize = SLOTS_PER_REGION * PAGE_SIZE;
const REGION_COUNT: usize = 4096;

#[derive(Clone, Copy)]
struct PageIndex(u32);

impl PageIndex {
    /// Creates a new page index from a region and a slot.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the region index is less than `REGION_COUNT` and the slot index
    /// is less than `REGION_SLOT_MASK`.
    fn from_region_and_slot(region: u32, slot: u32) -> Self {
        assert!(region < REGION_COUNT as u32);
        assert!(slot < SLOTS_PER_REGION as u32);
        Self((region << REGION_SLOT_BITS) | slot)
    }

    /// Extracts the region index from the reference.
    fn region(&self) -> usize {
        (self.0 >> REGION_SLOT_BITS) as usize
    }

    /// Extracts the slot index within the region.
    fn slot_index(&self) -> usize {
        (self.0 & REGION_SLOT_MASK) as usize
    }
}

/// A page reference to the pool.
#[derive(Clone)]
pub struct Page(PageIndex);

impl Page {
    /// Returns a pointer to the page.
    pub fn as_ptr(&self, pool: &PagePool) -> *const u8 {
        pool.data_ptr(self.0) as *const u8
    }

    /// Returns a mutable pointer to the page.
    pub fn as_mut_ptr(&self, pool: &PagePool) -> *mut u8 {
        pool.data_ptr(self.0)
    }

    /// This is a convenience function that uses [`std::slice::from_raw_parts_mut`] to create a
    /// mutable slice.
    ///
    /// # Safety
    ///
    /// The caller is responsible for making sure:
    ///
    /// 1. that the page is not freed,
    /// 2. that the [`PagePool`] is the same that was used to allocate the page.
    /// 3. that the [`PagePool`] is not dropped while the slice is used.
    /// 4. that there is only a single mutable slice into the page at any given time.
    pub unsafe fn as_mut_slice(&self, pool: &PagePool) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.as_mut_ptr(pool), PAGE_SIZE)
    }
}

/// Provides a managed version of a [`Page`] by wrapping it and it's [`PagePool`].
///
/// Unlike [`Page`], this type handles deallocation for you upon dropping. It also provides a safe
/// way to access the contents of the page. However, the price for this convenience is that it is
/// heavier than the bare page type and it doesn't allow cloning.
pub struct FatPage {
    page_pool: PagePool,
    page: Page,
}

impl FatPage {
    /// See [`Page::as_ptr`].
    pub fn as_ptr(&self, pool: &PagePool) -> *const u8 {
        self.page.as_ptr(pool)
    }

    /// See [`Page::as_mut_ptr`].
    pub fn as_mut_ptr(&self, pool: &PagePool) -> *mut u8 {
        self.page.as_mut_ptr(pool)
    }
}

impl Deref for FatPage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { self.page.as_mut_slice(&self.page_pool) }
    }
}

impl DerefMut for FatPage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.page.as_mut_slice(&self.page_pool) }
    }
}

impl Drop for FatPage {
    fn drop(&mut self) {
        self.page_pool.dealloc(self.page.clone());
    }
}

/// [`PagePool`] is an efficient allocator for pages used in IO operations.
///
/// It allows for efficient allocation and deallocation of pages.
#[derive(Clone)]
pub struct PagePool {
    inner: Arc<Inner>,
}

struct Inner {
    // `regions` is a preallocated string of regions. `n_regions` is the number of regions that are
    // currently allocated and thus the index of the first unallocated region. An unallocated region
    // has the value of `null`. `n_regions` only grows, never shrinks and cannot exceed
    // [`REGION_COUNT`]. Once a region is allocated, it will not be freed until the pool is dropped.
    // Moreover, the pointer stored in `regions[i]` where `i < n_regions` is immutable once set.
    regions: [AtomicPtr<u8>; REGION_COUNT],
    n_regions: AtomicU32,
    freelist: RwLock<Vec<Page>>,
}

impl PagePool {
    /// Creates a new empty page pool.
    pub fn new() -> Self {
        let regions = std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut()));
        // The capacity is chosen to be large enough to fit 4 times as much as 50k pages.
        let freelist = RwLock::new(Vec::with_capacity(200000));
        Self {
            inner: Arc::new(Inner {
                regions,
                n_regions: AtomicU32::new(0),
                freelist,
            }),
        }
    }

    /// Allocates a new [`FatPage`].
    pub fn alloc_fat_page(&self) -> FatPage {
        let page = self.alloc_zeroed();
        FatPage {
            page_pool: self.clone(),
            page,
        }
    }

    /// Allocates a new [`Page`] and fills it with zeroes.
    pub fn alloc_zeroed(&self) -> Page {
        let page = {
            let mut freelist = self.inner.freelist.write();
            if freelist.is_empty() {
                self.grow(&mut freelist)
            } else {
                freelist.pop().unwrap()
            }
        };
        unsafe {
            // SAFETY: `page` is trivially a valid page that was allocated by this pool and not yet 
            //         freed.
            page.as_mut_slice(self).fill(0);
        }
        page
    }

    /// Deallocates a [`Page`].
    pub fn dealloc(&self, page: Page) {
        self.inner.freelist.write().push(page);
    }

    fn grow(&self, freelist_guard: &mut RwLockWriteGuard<Vec<Page>>) -> Page {
        assert!(freelist_guard.is_empty());

        // First step is to allocate a new region.
        let region_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION_BYTE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                /* fd */ -1,
                /* offset */ 0,
            )
        };
        if region_ptr == libc::MAP_FAILED {
            panic!("Failed to allocate memory");
        }
        assert!(!region_ptr.is_null());

        // Next, we need to store the region pointer in the regions array.
        //
        // We store the pointer in the regions array before incrementing n_regions. This is not
        // strictly necessary, because the freelist is empty and no page can refer to the new region
        // yet. Likewise, drop cannot happen during this operation. We still do it in this order
        // to just err on the safe side and avoid any potential issues.
        //
        // Also, note the ordering is not really important here since we own the lock.
        let region_ix = self.inner.n_regions.load(Ordering::Relaxed);
        self.inner.regions[region_ix as usize].store(region_ptr as *mut u8, Ordering::Relaxed);
        self.inner.n_regions.fetch_add(1, Ordering::Release);

        // Finally, we need to populate the freelist with the pages in the new region.
        for slot in 0..SLOTS_PER_REGION {
            freelist_guard.push(Page(PageIndex::from_region_and_slot(
                region_ix,
                slot as u32,
            )));
        }
        // UNWRAP: we know that the freelist is not empty, because we just filled it.
        freelist_guard.pop().unwrap()
    }

    fn data_ptr(&self, page_index: PageIndex) -> *mut u8 {
        let region = page_index.region();
        assert!(region < self.inner.n_regions.load(Ordering::Acquire) as usize);
        let region_ptr = self.inner.regions[region].load(Ordering::Relaxed);
        unsafe { region_ptr.add(page_index.slot_index() * PAGE_SIZE) }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for i in 0..self.n_regions.load(Ordering::Relaxed) as usize {
            let region_ptr = self.regions[i].load(Ordering::Relaxed);
            assert!(!region_ptr.is_null());
            unsafe {
                // SAFETY: `region_ptr` is a valid pointer to a region that was allocated and not
                // yet freed by this pool.
                libc::munmap(region_ptr as *mut libc::c_void, REGION_BYTE_SIZE);
            }
        }
    }
}

unsafe impl Send for PagePool {}
unsafe impl Sync for PagePool {}