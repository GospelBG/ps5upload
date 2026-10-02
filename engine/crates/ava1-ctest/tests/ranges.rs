#![cfg(unix)]
use ava1::ranges::RangeSet;
use ava1::wire::SplitMix;
use ava1_ctest::*;

#[test]
fn c_range_sets_match_rust() {
    let mut rng = SplitMix(11);
    for _ in 0..300 {
        let ops: Vec<(u64, u64)> = (0..rng.below(40) + 1)
            .map(|_| {
                let s = rng.below(1000);
                (s, s + rng.below(60) + 1)
            })
            .collect();
        let mut r = RangeSet::new();
        for (s, e) in &ops {
            r.insert(*s, *e);
        }
        let want: Vec<(u64, u64)> = r.iter().collect();
        assert_eq!(c_rset_after(&ops), want, "{ops:?}");
    }
}

#[test]
fn c_file_runs_match_rust() {
    let mut rng = SplitMix(12);
    for _ in 0..200 {
        let n = rng.below(500) as u32 + 1;
        let set: std::collections::BTreeSet<u32> = (0..n).filter(|_| rng.below(3) == 0).collect();
        let rust = ava1::ranges::runs(&set);
        let c = c_bits_runs(n, &set.iter().copied().collect::<Vec<_>>());
        assert_eq!(
            c,
            rust.iter().map(|r| (r.first, r.count)).collect::<Vec<_>>()
        );
    }
}
