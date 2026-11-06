//! Where the time goes, with `--stats`: one line in the log every few seconds. Off by default, and
//! then nothing here reads a clock.
//!
//! - `input`: from the kernel's timestamp of an event until the main loop looks at it. Anything
//!   long here means the loop was busy, whoever was at fault.
//! - `render`: one frame on the main loop, including the copy to another GPU.
//! - `gap`: between two frames that reached the screen, and how many vblanks went unused in spite
//!   of a redraw waiting. `late` counts them.
//! - `flip`: the same gap on the kernel's clock, as far as the driver tells. Where `gap` wanders
//!   and `flip` does not, the event came late, not the frame. `skipped` counts vblanks by their
//!   sequence numbers, for every frame and not only with a redraw waiting.
//! - `gpu`: from the end of `render` until the fence of the frame is signalled, so everything the
//!   GPUs still had to do: waiting for their turn, drawing, the copy to another GPU.
//! - `screen`: from a commit until the vblank that shows it, so what the compositor adds to a client.
//! - `scroll`: from a wheel event until the client under the pointer commits, so what the client adds.
//! - `tidy`: the housekeeping after every loop iteration.

use std::time::{Duration, Instant};

const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Durations of one kind since the last report.
#[derive(Debug, Default)]
pub struct Series(Vec<Duration>);

impl Series {
    fn add(&mut self, sample: Duration) {
        self.0.push(sample);
    }

    /// `name n=... mean=... p50=... p99=... max=...` in milliseconds, then forgets the samples.
    fn take(&mut self, name: &str) -> Option<String> {
        let text = Summary::of(&mut self.0).map(|summary| format!("{name} {summary}"));
        self.0.clear();
        text
    }
}

/// What a list of durations comes down to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub mean: Duration,
    pub p50: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Summary {
    pub fn of(samples: &mut [Duration]) -> Option<Self> {
        samples.sort_unstable();
        let count = samples.len();
        let at = |share: usize| samples[(count - 1) * share / 100];
        Some(Self {
            count,
            mean: samples.iter().sum::<Duration>() / u32::try_from(count.max(1)).ok()?,
            p50: *samples.get((count.checked_sub(1)?) / 2)?,
            p99: at(99),
            max: *samples.last()?,
        })
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |time: Duration| time.as_secs_f64() * 1_000.0;
        write!(
            f,
            "n={} mean={:.2} p50={:.2} p99={:.2} max={:.2}",
            self.count,
            ms(self.mean),
            ms(self.p50),
            ms(self.p99),
            ms(self.max)
        )
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    enabled: bool,
    last_report: Option<Instant>,
    input: Series,
    render: Series,
    gap: Series,
    flip: Series,
    gpu: Series,
    screen: Series,
    scroll: Series,
    tidy: Series,
    late: u32,
    skipped: u32,
    /// The oldest commit that is not on the screen yet.
    commit: Option<Instant>,
    /// The oldest commit of the frame on its way.
    queued_commit: Option<Instant>,
    /// The oldest wheel event without a commit of its client.
    wheel: Option<Instant>,
    last_vblank: Option<Instant>,
    /// Something asked for a redraw when the last vblank came.
    wanted: bool,
    /// The kernel's time and sequence number of the last vblank.
    last_flip: Option<(Duration, u32)>,
}

impl Stats {
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    /// An event of the kernel, with its timestamp on the monotonic clock, reached the main loop.
    pub fn input(&mut self, sent: Duration) {
        if !self.enabled {
            return;
        }
        let Ok(now) = monotonic() else {
            return;
        };
        self.input.add(now.saturating_sub(sent));
    }

    pub fn wheel(&mut self) {
        if self.enabled {
            self.wheel.get_or_insert_with(Instant::now);
        }
    }

    /// A client committed. `under_pointer`: the pointer is over a surface of this client.
    pub fn commit(&mut self, under_pointer: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.commit.get_or_insert(now);
        if under_pointer && let Some(wheel) = self.wheel.take() {
            self.scroll.add(now - wheel);
        }
    }

    pub fn rendered(&mut self, start: Option<Instant>) {
        if let Some(start) = start {
            self.render.add(start.elapsed());
        }
    }

    /// The fence of a frame was signalled, `start` being when its `render` ended.
    pub fn gpu_done(&mut self, start: Instant) {
        if self.enabled {
            self.gpu.add(start.elapsed());
        }
    }

    pub fn tidied(&mut self, start: Option<Instant>) {
        if let Some(start) = start {
            self.tidy.add(start.elapsed());
        }
        self.report();
    }

    /// A frame went on its way to the screen, with all commits up to here.
    pub fn queued(&mut self) {
        if self.enabled {
            self.queued_commit = self.queued_commit.take().or(self.commit.take());
        }
    }

    /// A frame reached the screen. `frame` is the length of one refresh, `wanted` says that a
    /// redraw is already waiting. A gap of more than one and a half frames after a vblank with a
    /// redraw waiting is a vblank we missed. `flip` is what the kernel says about it, if it does.
    pub fn vblank(&mut self, frame: Duration, wanted: bool, flip: Option<(Duration, u32)>) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if let (Some((before, count_before)), Some((time, count))) = (self.last_flip, flip) {
            self.skipped += count.wrapping_sub(count_before).saturating_sub(1);
            if self.wanted {
                self.flip.add(time.saturating_sub(before));
            }
        }
        self.last_flip = flip;
        if let Some(last) = self.last_vblank
            && self.wanted
        {
            let gap = now - last;
            self.gap.add(gap);
            if gap > frame.mul_f64(1.5) {
                self.late += 1;
            }
        }
        if let Some(commit) = self.queued_commit.take() {
            self.screen.add(now - commit);
        }
        self.last_vblank = Some(now);
        self.wanted = wanted;
    }

    fn report(&mut self) {
        if !self.enabled {
            return;
        }
        let last = *self.last_report.get_or_insert_with(Instant::now);
        if last.elapsed() < REPORT_EVERY {
            return;
        }
        self.last_report = Some(Instant::now());
        let parts = [
            self.input.take("input"),
            self.render.take("render"),
            self.gap.take("gap"),
            self.flip.take("flip"),
            self.gpu.take("gpu"),
            self.screen.take("screen"),
            self.scroll.take("scroll"),
            self.tidy.take("tidy"),
        ];
        let line: Vec<String> = parts.into_iter().flatten().collect();
        tracing::info!(
            late = std::mem::take(&mut self.late),
            skipped = std::mem::take(&mut self.skipped),
            "stats {}",
            line.join(" | ")
        );
    }
}

/// The clock of libinput's timestamps.
fn monotonic() -> Result<Duration, ()> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the pointer is valid for the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(());
    }
    Ok(Duration::new(
        u64::try_from(time.tv_sec).map_err(|_| ())?,
        u32::try_from(time.tv_nsec).map_err(|_| ())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn summary_of_samples() {
        let mut samples: Vec<Duration> = (1..=100).rev().map(ms).collect();
        let summary = Summary::of(&mut samples).unwrap();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.p50, ms(50));
        assert_eq!(summary.p99, ms(99));
        assert_eq!(summary.max, ms(100));
        assert_eq!(summary.mean, Duration::from_micros(50_500));
    }

    #[test]
    fn no_samples_no_summary() {
        assert_eq!(Summary::of(&mut []), None);
    }

    #[test]
    fn switched_off_reads_no_clock() {
        let mut stats = Stats::default();
        assert_eq!(stats.start(), None);
        stats.wheel();
        stats.commit(true);
        assert!(stats.wheel.is_none() && stats.commit.is_none());
    }

    #[test]
    fn a_gap_only_counts_with_a_redraw_waiting() {
        let mut stats = Stats::default();
        stats.enable();
        stats.vblank(ms(0), false, None);
        stats.vblank(ms(0), true, None);
        assert!(stats.gap.0.is_empty());
        stats.vblank(ms(0), false, None);
        assert_eq!(stats.gap.0.len(), 1);
        assert_eq!(stats.late, 1);
    }

    #[test]
    fn the_sequence_numbers_tell_skipped_vblanks() {
        let mut stats = Stats::default();
        stats.enable();
        stats.vblank(ms(6), true, Some((ms(100), u32::MAX)));
        stats.vblank(ms(6), true, Some((ms(106), 0)));
        assert_eq!(stats.skipped, 0);
        stats.vblank(ms(6), true, Some((ms(124), 3)));
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.flip.0, [ms(6), ms(18)]);
    }
}
