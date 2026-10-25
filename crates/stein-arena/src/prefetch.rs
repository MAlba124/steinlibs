//! CPU prefetch hint for a table slot.

/// Prefetch the cache line of `slice[index]`. Any index is fine, the hint cannot fault.
#[inline(always)]
pub fn prefetch<T>(slice: &[T], index: usize) {
    let p = slice.as_ptr().wrapping_add(index).cast::<i8>();
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a hint, touches no memory and never faults. SSE is x86_64 baseline.
    unsafe {
        core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(p);
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: PRFM is a hint, touches no memory or flags and never faults.
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_index_is_harmless() {
        let v = vec![7u32; 1000];
        prefetch(&v, 0);
        prefetch(&v, 999);
        prefetch(&v, 1000);
        prefetch(&v, usize::MAX);
        prefetch::<u64>(&[], 5);
        assert_eq!(v[999], 7);
    }
}
