//! The speed governor (SPEC.md §16): lanes, chunk and bundle size from what the link and
//! the receiver report, once per second. Pure: the sender feeds it samples.
use crate::gen::{BN_CREDIT, BN_DISK, BN_NETWORK, BN_SOURCE, BN_WORKERS};

pub const MIN_CHUNK: u32 = 1 << 20;
/// 16 MiB frames include the MAC and the Chunk header, so the largest chunk is 15 MiB.
pub const MAX_CHUNK: u32 = 15 << 20;
pub const START_CHUNK: u32 = 4 << 20;
pub const MIN_BUNDLE: u32 = 256 << 10;
pub const MAX_BUNDLE: u32 = 15 << 20;
pub const START_BUNDLE: u32 = 1 << 20;
pub const START_LANES: u8 = 2;
pub const MAX_LANES: u8 = 8;
const GAIN: f64 = 1.10;
const HOLD_TICKS: u32 = 30;
const STABLE_TICKS: u32 = 10;
const PROBE_TICKS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Bundle,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Mixed,
    StreamOnly,
    BundleOnly,
}

#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub secs: f64,
    /// Bytes the receiver confirmed `Received` during this tick.
    pub bytes_acked: u64,
    pub lanes: u8,
    /// Lane deaths plus requeued frames this tick.
    pub stalls: u32,
    /// The sender had frames ready but no credit at some point this tick.
    pub credit_starved: bool,
    /// Lanes had room but the readers had nothing ready.
    pub source_starved: bool,
    /// From the receiver's last Status.
    pub receiver_bottleneck: u8,
    /// Bytes made durable this tick, per class, and what is left of each class.
    pub small_durable: u64,
    pub large_durable: u64,
    pub small_left: u64,
    pub large_left: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub lanes: u8,
    pub chunk: u32,
    pub bundle: u32,
    pub bottleneck: u8,
    pub mode: Mode,
    pub prefer: Class,
    pub sequential: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LaneStep {
    Steady,
    Trying { before: f64, wait: u32 },
    Hold(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Probe {
    Warmup(u32),
    Run { mode: Mode, left: u32 },
    Done,
}

pub struct Governor {
    lanes: u8,
    chunk: u32,
    bundle: u32,
    step: LaneStep,
    stable: u32,
    probe: Probe,
    rates: [(f64, f64); 3], // (small, large) durable B/s for Mixed, StreamOnly, BundleOnly
    acc: (f64, f64, f64),   // small bytes, large bytes, seconds in the current probe phase
    sequential: bool,
    mode: Mode,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new()
    }
}

fn idx(m: Mode) -> usize {
    match m {
        Mode::Mixed => 0,
        Mode::StreamOnly => 1,
        Mode::BundleOnly => 2,
    }
}

pub fn inflight_cap(chunk: u32, lane_rate: f64) -> u64 {
    ((lane_rate * 2.0) as u64).max(chunk as u64)
}

impl Governor {
    pub fn new() -> Self {
        Self {
            lanes: START_LANES,
            chunk: START_CHUNK,
            bundle: START_BUNDLE,
            step: LaneStep::Steady,
            stable: 0,
            probe: Probe::Warmup(3),
            rates: [(0.0, 0.0); 3],
            acc: (0.0, 0.0, 0.0),
            sequential: false,
            mode: Mode::Mixed,
        }
    }

    fn decision(&self, bottleneck: u8, s: &Sample) -> Decision {
        let small_rate = self.rates[0].0.max(1.0);
        let large_rate = self.rates[0].1.max(1.0);
        let prefer = if s.small_left as f64 / small_rate >= s.large_left as f64 / large_rate {
            Class::Bundle
        } else {
            Class::Stream
        };
        let mode = if self.sequential {
            if s.small_left > 0 {
                Mode::BundleOnly
            } else {
                Mode::StreamOnly
            }
        } else {
            self.mode
        };
        Decision {
            lanes: self.lanes,
            chunk: self.chunk,
            bundle: self.bundle,
            bottleneck,
            mode,
            prefer,
            sequential: self.sequential,
        }
    }

    pub fn tick(&mut self, s: &Sample) -> Decision {
        if s.secs <= 0.0 {
            return self.decision(BN_NETWORK, s);
        }
        let rate = s.bytes_acked as f64 / s.secs;
        let lanes_now = s.lanes.max(1);
        let lane_rate = rate / lanes_now as f64;
        let bottleneck = if s.source_starved {
            BN_SOURCE
        } else if s.credit_starved {
            match s.receiver_bottleneck {
                BN_DISK | BN_WORKERS => s.receiver_bottleneck,
                _ => BN_CREDIT,
            }
        } else {
            BN_NETWORK
        };

        // Lanes and chunk.
        if s.stalls > 0 {
            self.lanes = self.lanes.saturating_sub(1).max(1);
            self.chunk = (self.chunk / 2).max(MIN_CHUNK) & !((1 << 20) - 1);
            self.stable = 0;
            self.step = LaneStep::Hold(HOLD_TICKS / 3);
        } else {
            self.stable += 1;
            if self.stable >= STABLE_TICKS {
                self.stable = 0;
                self.chunk = (self.chunk * 2).min(MAX_CHUNK);
            }
            self.step = match self.step {
                LaneStep::Hold(0) => LaneStep::Steady,
                LaneStep::Hold(n) => LaneStep::Hold(n - 1),
                LaneStep::Trying { before, wait: 0 } => {
                    if rate >= before * GAIN {
                        LaneStep::Steady
                    } else {
                        self.lanes = self.lanes.saturating_sub(1).max(1);
                        LaneStep::Hold(HOLD_TICKS)
                    }
                }
                LaneStep::Trying { before, wait } => LaneStep::Trying {
                    before,
                    wait: wait - 1,
                },
                LaneStep::Steady if bottleneck == BN_NETWORK && self.lanes < MAX_LANES => {
                    self.lanes += 1;
                    LaneStep::Trying {
                        before: rate,
                        wait: 1,
                    }
                }
                st => st,
            };
        }
        let half_second = ((lane_rate * 0.5) as u32) & !((1 << 20) - 1);
        self.chunk = self.chunk.min(half_second.max(MIN_CHUNK));
        self.bundle = ((lane_rate * 0.25) as u32).clamp(MIN_BUNDLE, MAX_BUNDLE);

        // Mixing check.
        self.probe = match self.probe {
            Probe::Warmup(0) if s.small_left > 0 && s.large_left > 0 => {
                self.acc = (0.0, 0.0, 0.0);
                self.mode = Mode::Mixed;
                Probe::Run {
                    mode: Mode::Mixed,
                    left: PROBE_TICKS,
                }
            }
            Probe::Warmup(0) => Probe::Done,
            Probe::Warmup(n) => Probe::Warmup(n - 1),
            Probe::Run { mode, left } => {
                self.acc.0 += s.small_durable as f64;
                self.acc.1 += s.large_durable as f64;
                self.acc.2 += s.secs;
                if left > 1 {
                    Probe::Run {
                        mode,
                        left: left - 1,
                    }
                } else {
                    self.rates[idx(mode)] = (self.acc.0 / self.acc.2, self.acc.1 / self.acc.2);
                    self.acc = (0.0, 0.0, 0.0);
                    match mode {
                        Mode::Mixed => {
                            self.mode = Mode::StreamOnly;
                            Probe::Run {
                                mode: Mode::StreamOnly,
                                left: PROBE_TICKS,
                            }
                        }
                        Mode::StreamOnly => {
                            self.mode = Mode::BundleOnly;
                            Probe::Run {
                                mode: Mode::BundleOnly,
                                left: PROBE_TICKS,
                            }
                        }
                        Mode::BundleOnly => {
                            let (ms, ml) = self.rates[0];
                            let (sl, ll) = (s.small_left as f64, s.large_left as f64);
                            let mixed = (sl / ms.max(1.0)).max(ll / ml.max(1.0));
                            let seq = sl / self.rates[2].0.max(1.0) + ll / self.rates[1].1.max(1.0);
                            self.sequential = seq < mixed * 0.9;
                            self.mode = Mode::Mixed;
                            Probe::Done
                        }
                    }
                }
            }
            Probe::Done => Probe::Done,
        };
        self.decision(bottleneck, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link where each lane carries `per_lane` B/s up to a `cap`.
    fn net(g: &mut Governor, per_lane: f64, cap: f64, ticks: usize) -> Decision {
        let mut d = g.tick(&Sample::default());
        for _ in 0..ticks {
            let rate = (d.lanes as f64 * per_lane).min(cap);
            d = g.tick(&Sample {
                secs: 1.0,
                bytes_acked: rate as u64,
                lanes: d.lanes,
                ..Default::default()
            });
        }
        d
    }

    #[test]
    fn lanes_grow_while_throughput_rises_and_stop_at_the_ceiling() {
        let mut g = Governor::new();
        let d = net(&mut g, 30e6, 110e6, 40);
        assert_eq!(d.lanes, 4, "3→4 gains 22 %, 4→5 gains nothing");
        assert_eq!(d.bottleneck, crate::gen::BN_NETWORK);
    }

    #[test]
    fn a_single_window_limited_lane_still_gets_company() {
        // Wi-Fi: 52 MB/s per lane (512 KiB rcvbuf / 10 ms), 300 MB/s link.
        let mut g = Governor::new();
        assert!(net(&mut g, 52e6, 300e6, 60).lanes >= 5);
    }

    #[test]
    fn a_stall_drops_a_lane_and_halves_the_chunk() {
        let mut g = Governor::new();
        let d0 = net(&mut g, 30e6, 110e6, 40);
        // 200 MB/s keeps the half-second cap out of the way, so only the halving shows.
        let d = g.tick(&Sample {
            secs: 1.0,
            bytes_acked: 200_000_000,
            lanes: d0.lanes,
            stalls: 1,
            ..Default::default()
        });
        assert_eq!(d.lanes, d0.lanes - 1);
        assert_eq!(d.chunk, (d0.chunk / 2).max(MIN_CHUNK) & !((1 << 20) - 1));
    }

    #[test]
    fn the_chunk_grows_back_when_stable_and_stays_whole_groups() {
        let mut g = Governor::new();
        let _ = g.tick(&Sample {
            secs: 1.0,
            bytes_acked: 1,
            lanes: 2,
            stalls: 1,
            ..Default::default()
        });
        let d = net(&mut g, 60e6, 110e6, 40);
        assert!(d.chunk > MIN_CHUNK);
        assert_eq!(d.chunk % (1 << 20), 0);
        assert!(d.chunk <= MAX_CHUNK);
    }

    #[test]
    fn the_bottleneck_follows_who_waits() {
        let mut g = Governor::new();
        let s = Sample {
            secs: 1.0,
            bytes_acked: 10,
            lanes: 2,
            source_starved: true,
            ..Default::default()
        };
        assert_eq!(g.tick(&s).bottleneck, crate::gen::BN_SOURCE);
        let s = Sample {
            secs: 1.0,
            bytes_acked: 10,
            lanes: 2,
            credit_starved: true,
            receiver_bottleneck: crate::gen::BN_DISK,
            ..Default::default()
        };
        let d = g.tick(&s);
        assert_eq!(d.bottleneck, crate::gen::BN_DISK);
        assert_eq!(
            d.lanes, 2,
            "lanes never grow while the receiver is the limit"
        );
    }

    #[test]
    fn bundles_scale_with_the_link() {
        let mut g = Governor::new();
        let fast = net(&mut g, 100e6, 110e6, 10).bundle;
        let mut g = Governor::new();
        let slow = net(&mut g, 2e6, 2e6, 10).bundle;
        assert!(fast > slow);
        assert!(
            (MIN_BUNDLE..=MAX_BUNDLE).contains(&fast) && (MIN_BUNDLE..=MAX_BUNDLE).contains(&slow)
        );
    }

    /// A disk where running both classes at once halves each one's rate (a seeking HDD).
    fn mixing(penalty: f64) -> Decision {
        let mut g = Governor::new();
        let mut d = g.tick(&Sample::default());
        let (mut small_left, mut large_left) = (400e6, 4e9);
        for _ in 0..60 {
            let (sr, lr) = match d.mode {
                Mode::Mixed => (20e6 * penalty, 80e6 * penalty),
                Mode::StreamOnly => (0.0, 80e6),
                Mode::BundleOnly => (20e6, 0.0),
            };
            small_left -= sr;
            large_left -= lr;
            d = g.tick(&Sample {
                secs: 1.0,
                bytes_acked: (sr + lr) as u64,
                lanes: d.lanes,
                small_durable: sr as u64,
                large_durable: lr as u64,
                small_left: small_left as u64,
                large_left: large_left as u64,
                ..Default::default()
            });
        }
        d
    }

    #[test]
    fn mixing_that_slows_the_disk_switches_to_sequential() {
        assert!(mixing(0.4).sequential);
        assert!(!mixing(1.0).sequential);
    }

    #[test]
    fn the_inflight_cap_is_two_seconds_of_a_lane_but_at_least_a_chunk() {
        assert_eq!(inflight_cap(4 << 20, 10e6), 20_000_000);
        assert_eq!(inflight_cap(4 << 20, 1e6), 4 << 20);
    }

    /// Ruling answer 1: both classes queued at the gate, one runs dry during the probe —
    /// the probe still completes (no panic, `Probe` ends in `Done`) and never starts twice.
    #[test]
    fn a_class_running_dry_during_the_probe_still_reaches_a_decision() {
        let mut g = Governor::new();
        let (mut small_left, mut large_left) = (100_000_000u64, 4_000_000_000u64);
        for _ in 0..25 {
            // The small class drains during the stream-only phase of the probe.
            let sr = small_left.min(10_000_000);
            let lr = large_left.min(10_000_000);
            small_left -= sr;
            large_left -= lr;
            g.tick(&Sample {
                secs: 1.0,
                bytes_acked: sr + lr,
                lanes: 2,
                small_durable: sr,
                large_durable: lr,
                small_left,
                large_left,
                ..Default::default()
            });
        }
        assert!(
            matches!(g.probe, Probe::Done),
            "the probe completes instead of hanging"
        );
        // A later tick with both classes queued again must not start a second probe.
        g.tick(&Sample {
            secs: 1.0,
            bytes_acked: 1,
            lanes: 2,
            small_left: 1,
            large_left: 1,
            ..Default::default()
        });
        assert!(matches!(g.probe, Probe::Done), "the probe never runs twice");
    }
}
