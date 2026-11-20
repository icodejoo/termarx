//! 分配失败时先等待重试、而不是立刻 abort 的全局分配器包装。
//!
//! Rust 默认在分配失败时直接 abort。系统提交量（commit）被占满时这通常只是
//! 短暂的尖峰，而 multiplexer host 一旦 abort，所有终端里的任务会一起丢失。
//! 这里在底层分配器返回空指针后退避重试一段时间，等别的进程释放内存。

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    thread,
    time::Duration,
};

/// 首次分配失败之后最多重试的次数。
const DEFAULT_MAX_RETRIES: u32 = 24;
/// 「耐心模式」下最多重试的次数；按每次 500 毫秒算约 10 分钟，给用户腾内存留出时间。
const PATIENT_MAX_RETRIES: u32 = 1200;
/// 第一次重试前等待的毫秒数，之后每次翻倍直到 [`DEFAULT_MAX_DELAY_MS`]。
const DEFAULT_INITIAL_DELAY_MS: u64 = 10;
/// 单次重试等待的毫秒数上限；默认策略下总共最多等待约 10 秒。
const DEFAULT_MAX_DELAY_MS: u64 = 500;

/// 包装底层分配器，分配或扩容失败时退避重试，重试用尽后才返回空指针（由 Rust 照常 abort）。
///
/// 用法：
///
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: RetryingAlloc<System> = RetryingAlloc::new(System);
/// ```
pub struct RetryingAlloc<A = System> {
    inner: A,
    max_retries: AtomicU32,
    initial_delay_ms: u64,
    max_delay_ms: u64,
    recovered: AtomicU64,
}

impl<A> RetryingAlloc<A> {
    /// 用默认重试策略（最多约 10 秒）包装 `inner`。
    pub const fn new(inner: A) -> Self {
        Self::with_policy(
            inner,
            DEFAULT_MAX_RETRIES,
            DEFAULT_INITIAL_DELAY_MS,
            DEFAULT_MAX_DELAY_MS,
        )
    }

    /// 用自定义策略包装 `inner`：最多重试 `max_retries` 次，等待从 `initial_delay_ms`
    /// 起每次翻倍，封顶 `max_delay_ms`。
    pub const fn with_policy(
        inner: A,
        max_retries: u32,
        initial_delay_ms: u64,
        max_delay_ms: u64,
    ) -> Self {
        Self {
            inner,
            max_retries: AtomicU32::new(max_retries),
            initial_delay_ms,
            max_delay_ms,
            recovered: AtomicU64::new(0),
        }
    }

    /// 切到耐心模式：分配失败后最多等待约 10 分钟再放弃。
    ///
    /// 给承载终端任务的 multiplexer host 用：卡住几分钟好过 abort 后所有任务一起丢失，
    /// 用户腾出内存后它能自己继续跑。
    pub fn set_patient_mode(&self) {
        self.max_retries
            .store(PATIENT_MAX_RETRIES, Ordering::Relaxed);
    }

    /// 返回「首次失败、重试后成功」的分配次数，可用来确认这层保护确实生效过。
    pub fn recovered_allocations(&self) -> u64 {
        self.recovered.load(Ordering::Relaxed)
    }

    /// 首次失败后反复调用 `attempt` 直到拿到非空指针或重试用尽，返回最后一次的结果。
    #[cold]
    #[inline(never)]
    fn retry(&self, mut attempt: impl FnMut() -> *mut u8) -> *mut u8 {
        let mut delay_ms = self.initial_delay_ms;
        for _ in 0..self.max_retries.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(delay_ms));
            let pointer = attempt();
            if !pointer.is_null() {
                self.recovered.fetch_add(1, Ordering::Relaxed);
                return pointer;
            }
            delay_ms = delay_ms.saturating_mul(2).min(self.max_delay_ms);
        }
        std::ptr::null_mut()
    }
}

// SAFETY: 每个方法都把调用方给的指针、布局和大小原样转交给 `inner`；重试只是用相同
// 参数再次调用 `inner` 的分配方法，没有改变任何已分配块的所有权，等待期间也不持有锁。
unsafe impl<A: GlobalAlloc> GlobalAlloc for RetryingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` 由 `GlobalAlloc::alloc` 的调用方保证合法。
        let pointer = unsafe { self.inner.alloc(layout) };
        if !pointer.is_null() {
            return pointer;
        }
        // SAFETY: 同上，重试使用相同的 `layout`。
        self.retry(|| unsafe { self.inner.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` 由 `GlobalAlloc::alloc_zeroed` 的调用方保证合法。
        let pointer = unsafe { self.inner.alloc_zeroed(layout) };
        if !pointer.is_null() {
            return pointer;
        }
        // SAFETY: 同上，重试使用相同的 `layout`。
        self.retry(|| unsafe { self.inner.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` 与 `layout` 描述一块由本分配器分配的存活内存。
        unsafe { self.inner.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: 参数满足 `GlobalAlloc::realloc` 的约定。
        let new_pointer = unsafe { self.inner.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            return new_pointer;
        }
        // SAFETY: 扩容失败时原块保持有效且未被释放，因此可以用相同参数重试。
        self.retry(|| unsafe { self.inner.realloc(pointer, layout, new_size) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// 前 `failures_left` 次分配类调用返回空指针，之后委托给系统分配器的测试用分配器。
    struct FlakyAlloc {
        failures_left: AtomicU32,
        calls: AtomicU32,
    }

    impl FlakyAlloc {
        /// 创建一个会先失败 `failures` 次的分配器。
        const fn new(failures: u32) -> Self {
            Self {
                failures_left: AtomicU32::new(failures),
                calls: AtomicU32::new(0),
            }
        }

        /// 记录一次调用，并返回这次是否应当模拟失败。
        fn should_fail(&self) -> bool {
            self.calls.fetch_add(1, Ordering::Relaxed);
            // 测试里每个分配器只在单线程使用，先读后写即可。
            let left = self.failures_left.load(Ordering::Relaxed);
            if left == 0 {
                return false;
            }
            self.failures_left.store(left - 1, Ordering::Relaxed);
            true
        }
    }

    // SAFETY: 成功时完全委托给 `System`，失败时只返回空指针。
    unsafe impl GlobalAlloc for FlakyAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if self.should_fail() {
                return std::ptr::null_mut();
            }
            // SAFETY: 调用方保证 `layout` 合法。
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if self.should_fail() {
                return std::ptr::null_mut();
            }
            // SAFETY: 调用方保证 `layout` 合法。
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: 调用方保证指针来自 `System` 且布局匹配。
            unsafe { System.dealloc(pointer, layout) };
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if self.should_fail() {
                return std::ptr::null_mut();
            }
            // SAFETY: 调用方保证参数满足 `realloc` 的约定。
            unsafe { System.realloc(pointer, layout, new_size) }
        }
    }

    /// 测试用布局：64 字节、按 8 对齐。
    fn layout() -> Layout {
        Layout::from_size_align(64, 8).expect("valid layout")
    }

    /// 不等待的重试策略，让测试瞬间完成。
    fn instant(failures: u32, max_retries: u32) -> RetryingAlloc<FlakyAlloc> {
        RetryingAlloc::with_policy(FlakyAlloc::new(failures), max_retries, 0, 0)
    }

    #[test]
    fn first_try_success_does_not_retry() {
        let allocator = instant(0, 3);
        // SAFETY: 布局合法，分配后立即用同一布局释放。
        unsafe {
            let pointer = allocator.alloc(layout());
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, layout());
        }
        assert_eq!(allocator.inner.calls.load(Ordering::Relaxed), 1);
        assert_eq!(allocator.recovered_allocations(), 0);
    }

    #[test]
    fn alloc_recovers_after_transient_failures() {
        let allocator = instant(3, 5);
        // SAFETY: 布局合法，分配后立即用同一布局释放。
        unsafe {
            let pointer = allocator.alloc(layout());
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, layout());
        }
        assert_eq!(allocator.inner.calls.load(Ordering::Relaxed), 4);
        assert_eq!(allocator.recovered_allocations(), 1);
    }

    #[test]
    fn alloc_gives_up_after_max_retries() {
        let allocator = instant(100, 2);
        // SAFETY: 布局合法；返回空指针时没有需要释放的内存。
        let pointer = unsafe { allocator.alloc(layout()) };
        assert!(pointer.is_null());
        // 首次 + 2 次重试。
        assert_eq!(allocator.inner.calls.load(Ordering::Relaxed), 3);
        assert_eq!(allocator.recovered_allocations(), 0);
    }

    #[test]
    fn alloc_zeroed_recovers_and_stays_zeroed() {
        let allocator = instant(1, 3);
        // SAFETY: 布局合法，读取范围在分配的 64 字节内，之后用同一布局释放。
        unsafe {
            let pointer = allocator.alloc_zeroed(layout());
            assert!(!pointer.is_null());
            assert!(
                std::slice::from_raw_parts(pointer, 64)
                    .iter()
                    .all(|byte| *byte == 0)
            );
            allocator.dealloc(pointer, layout());
        }
        assert_eq!(allocator.recovered_allocations(), 1);
    }

    #[test]
    fn realloc_recovers_and_keeps_contents() {
        let allocator = instant(0, 3);
        // SAFETY: 布局合法；先正常分配并写入数据，再让 realloc 失败两次后成功，
        // 最后用新大小的布局释放。
        unsafe {
            let pointer = allocator.alloc(layout());
            assert!(!pointer.is_null());
            pointer.write(0xAB);
            allocator.inner.failures_left.store(2, Ordering::Relaxed);
            let grown = allocator.realloc(pointer, layout(), 128);
            assert!(!grown.is_null());
            assert_eq!(grown.read(), 0xAB);
            allocator.dealloc(
                grown,
                Layout::from_size_align(128, 8).expect("valid layout"),
            );
        }
        assert_eq!(allocator.recovered_allocations(), 1);
    }

    #[test]
    fn patient_mode_survives_long_outages() {
        let allocator = instant(100, 2);
        allocator.set_patient_mode();
        // SAFETY: 布局合法，分配后立即用同一布局释放。
        unsafe {
            let pointer = allocator.alloc(layout());
            assert!(!pointer.is_null());
            allocator.dealloc(pointer, layout());
        }
        assert_eq!(allocator.inner.calls.load(Ordering::Relaxed), 101);
        assert_eq!(allocator.recovered_allocations(), 1);
    }

    #[test]
    fn delay_doubles_up_to_the_cap() {
        let allocator = RetryingAlloc::with_policy(FlakyAlloc::new(100), 4, 1, 2);
        let started = std::time::Instant::now();
        // SAFETY: 布局合法；返回空指针时没有需要释放的内存。
        let pointer = unsafe { allocator.alloc(layout()) };
        assert!(pointer.is_null());
        // 等待序列 1ms、2ms、2ms、2ms，总共至少 7ms。
        assert!(started.elapsed() >= Duration::from_millis(7));
    }
}
