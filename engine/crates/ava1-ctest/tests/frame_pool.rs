//! The lane frame-buffer pool (review 003 section 3): size classes, reuse without
//! zeroing, the idle cap, and no leak / double free.
#![cfg(unix)]
use ava1_ctest as _; // links the payload C
use std::os::raw::{c_int, c_void};
use std::sync::Mutex;

extern "C" {
    fn ava1_frame_class(len: usize) -> usize;
    fn ava1_frame_alloc(len: usize, cap: *mut usize) -> *mut c_void;
    fn ava1_frame_free(p: *mut c_void, cap: usize) -> c_int;
    fn ava1_frame_pool_set_budget(bytes: u64);
    fn ava1_frame_pool_idle(cls: c_int) -> usize;
    fn ava1_frame_pool_outstanding() -> usize;
    fn ava1_frame_pool_trim();
}

const MIB: usize = 1 << 20;
/// The pool is one global: tests take turns.
static TURN: Mutex<()> = Mutex::new(());

fn fresh() -> std::sync::MutexGuard<'static, ()> {
    let g = TURN.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        ava1_frame_pool_set_budget(0);
        ava1_frame_pool_trim();
    }
    g
}

#[test]
fn a_length_maps_to_the_smallest_class_that_holds_it() {
    unsafe {
        assert_eq!(ava1_frame_class(100), 0, "a small body is plain malloc");
        assert_eq!(ava1_frame_class(MIB / 2), 0);
        assert_eq!(ava1_frame_class(MIB / 2 + 1), MIB);
        assert_eq!(ava1_frame_class(MIB), MIB);
        // A chunk of N MiB is N MiB plus its message header: still class N.
        assert_eq!(ava1_frame_class(MIB + 40), MIB);
        assert_eq!(ava1_frame_class(4 * MIB + 40), 4 * MIB);
        assert_eq!(ava1_frame_class(8 * MIB + 40), 8 * MIB);
        assert_eq!(ava1_frame_class(MIB + 64 * 1024 + 1), 4 * MIB);
        assert_eq!(ava1_frame_class(4 * MIB), 4 * MIB);
        assert_eq!(ava1_frame_class(5 * MIB), 8 * MIB);
        assert_eq!(ava1_frame_class(9 * MIB), 16 * MIB);
        assert_eq!(ava1_frame_class(16 * MIB), 16 * MIB);
        assert_eq!(ava1_frame_class(16 * MIB + 64 * 1024), 16 * MIB);
        assert_eq!(ava1_frame_class(16 * MIB + 64 * 1024 + 1), 0);
    }
}

#[test]
fn a_freed_buffer_is_reused_without_zeroing() {
    let _t = fresh();
    unsafe {
        let mut cap = 0usize;
        let p = ava1_frame_alloc(4 * MIB + 40, &mut cap) as *mut u8;
        assert!(!p.is_null());
        assert_eq!(cap, 4 * MIB);
        // Past the intrusive free-list link at the start of the buffer.
        *p.add(4096) = 0xAB;
        *p.add(4 * MIB + 100) = 0xCD; // the header slack is real, writable memory
        assert_eq!(ava1_frame_free(p as *mut c_void, cap), 0);
        assert_eq!(ava1_frame_pool_idle(1), 1);
        let q = ava1_frame_alloc(4 * MIB, &mut cap) as *mut u8;
        assert_eq!(q, p, "the same block comes back");
        assert_eq!(*q.add(4096), 0xAB, "and it was not zeroed");
        assert_eq!(ava1_frame_pool_idle(1), 0);
        assert_eq!(ava1_frame_free(q as *mut c_void, cap), 0);
    }
}

#[test]
fn the_pool_never_holds_more_than_the_budget() {
    let _t = fresh();
    unsafe {
        ava1_frame_pool_set_budget(24 * MIB as u64);
        let mut cap = 0usize;
        let ps: Vec<_> = (0..10)
            .map(|_| ava1_frame_alloc(4 * MIB, &mut cap))
            .collect();
        for p in ps {
            assert_eq!(ava1_frame_free(p, cap), 0);
        }
        assert_eq!(ava1_frame_pool_idle(1), 6, "24 MiB / 4 MiB");
        // Mixed classes share the one ceiling.
        let a = ava1_frame_alloc(16 * MIB, &mut cap);
        assert_eq!(ava1_frame_free(a, cap), 0);
        assert_eq!(
            ava1_frame_pool_idle(3),
            0,
            "16 MiB does not fit beside 24 MiB idle"
        );
        // Shrinking the budget trims what is idle.
        ava1_frame_pool_set_budget(8 * MIB as u64);
        assert_eq!(ava1_frame_pool_idle(1), 2);
        ava1_frame_pool_set_budget(0);
    }
}

#[test]
fn each_class_obeys_budget_over_class() {
    let _t = fresh();
    unsafe {
        let mut cap = 0usize;
        // Default 96 MiB: six 16 MiB buffers at most.
        let ps: Vec<_> = (0..9)
            .map(|_| ava1_frame_alloc(15 * MIB, &mut cap))
            .collect();
        assert_eq!(cap, 16 * MIB);
        for p in ps {
            assert_eq!(ava1_frame_free(p, cap), 0);
        }
        assert_eq!(ava1_frame_pool_idle(3), 6);
    }
}

#[test]
fn nothing_leaks_and_a_double_free_is_refused() {
    let _t = fresh();
    unsafe {
        let base = ava1_frame_pool_outstanding();
        let mut cap = 0usize;
        let big = ava1_frame_alloc(2 * MIB, &mut cap);
        let mut small_cap = 1usize;
        let small = ava1_frame_alloc(1000, &mut small_cap);
        assert_eq!(small_cap, 0, "unpooled");
        assert_eq!(ava1_frame_pool_outstanding(), base + 2);
        assert_eq!(ava1_frame_free(small, small_cap), 0);
        assert_eq!(ava1_frame_free(big, cap), 0);
        assert_eq!(ava1_frame_pool_outstanding(), base);
        assert_eq!(ava1_frame_free(big, cap), -1, "already idle in the pool");
        assert_eq!(
            ava1_frame_pool_outstanding(),
            base,
            "refusal does not unbalance the count"
        );
        assert_eq!(ava1_frame_pool_idle(1), 1, "and the pool list is intact");
        assert_eq!(ava1_frame_free(std::ptr::null_mut(), 0), 0);
    }
}

#[test]
fn threads_hammering_the_pool_balance_out() {
    let _t = fresh();
    let base = unsafe { ava1_frame_pool_outstanding() };
    let hs: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                for n in 0..200 {
                    let mut cap = 0usize;
                    let len = [100, MIB, 3 * MIB, 7 * MIB][(i + n) % 4];
                    unsafe {
                        let p = ava1_frame_alloc(len, &mut cap);
                        assert!(!p.is_null());
                        *(p as *mut u8).add(len - 1) = 1;
                        assert_eq!(ava1_frame_free(p, cap), 0);
                    }
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(unsafe { ava1_frame_pool_outstanding() }, base);
}
