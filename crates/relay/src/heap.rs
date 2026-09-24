//! Keeps the delay buffer's memory out of glibc's heap.
//!
//! The buffer lives in 1 MiB blocks (see `ArenaPool` in the rtmp crate), which
//! glibc maps straight from the OS and gives back when freed, until the first one
//! is freed: that raises the size from which it maps (to more than a block), and
//! later blocks come from its heaps instead, one per thread, which keep what is
//! freed. In 12-hour tests the process then held up to twice the buffered data,
//! slowly growing for hours. Fixing the threshold keeps every block mapped.
//! Other systems' allocators map allocations this large already.

/// Below a block, so blocks are always mapped (smaller allocations use the heap).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MMAP_THRESHOLD: libc::c_int = 512 * 1024;

/// Sets the threshold, once per process. Call before the first block is made.
pub(crate) fn keep_blocks_mapped() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: mallopt only changes allocator settings; it is safe to call
            // at any time, from any thread.
            #[allow(unsafe_code)]
            let ok = unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD) };
            if ok != 1 {
                tracing::warn!("could not set the allocator's mapping threshold");
            }
        });
    }
}
