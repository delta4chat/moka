// License and Copyright Notice:
//
// Some of the code and doc comments in this module were ported or copied from
// a Java class `com.github.benmanes.caffeine.cache.TimerWheel` of Caffeine.
// https://github.com/ben-manes/caffeine/blob/master/caffeine/src/main/java/com/github/benmanes/caffeine/cache/TimerWheel.java
//
// The original code/comments from Caffeine are licensed under the Apache License,
// Version 2.0 <https://github.com/ben-manes/caffeine/blob/master/LICENSE>
//
// Copyrights of the original code/comments are retained by their contributors.
// For full authorship information, see the version control history of
// https://github.com/ben-manes/caffeine/

#![allow(unused)] // TODO: Remove this.

use std::{convert::TryInto, ptr::NonNull, time::Duration};

use super::{
    concurrent::entry_info::EntryInfo,
    deque::{DeqNode, Deque},
    time::{CheckedTimeOps, Instant},
};

use triomphe::Arc as TrioArc;

const BUCKET_COUNTS: &[u64] = &[
    64, // roughly seconds
    64, // roughly minutes
    32, // roughly hours
    4,  // roughly days
    1,  // overflow (> ~6.5 days)
];

const OVERFLOW_QUEUE_INDEX: usize = BUCKET_COUNTS.len() - 1;
const NUM_LEVELS: usize = OVERFLOW_QUEUE_INDEX - 1;

const DAY: Duration = Duration::from_secs(60 * 60 * 24);

const SPANS: &[u64] = &[
    aligned_duration(Duration::from_secs(1)),       // 1.07s
    aligned_duration(Duration::from_secs(60)),      // 1.14m
    aligned_duration(Duration::from_secs(60 * 60)), // 1.22h
    aligned_duration(DAY),                          // 1.63d
    BUCKET_COUNTS[3] * aligned_duration(DAY),       // 6.5d
    BUCKET_COUNTS[3] * aligned_duration(DAY),       // 6.5d
];

const SHIFT: &[u64] = &[
    SPANS[0].trailing_zeros() as u64,
    SPANS[1].trailing_zeros() as u64,
    SPANS[2].trailing_zeros() as u64,
    SPANS[3].trailing_zeros() as u64,
    SPANS[4].trailing_zeros() as u64,
];

/// Returns the next power of two of the duration in nanoseconds.
const fn aligned_duration(duration: Duration) -> u64 {
    // NOTE: as_nanos() returns u128, so convert it to u64 by using `as`.
    // We cannot call TryInto::try_into() here because it is not a const fn.
    (duration.as_nanos() as u64).next_power_of_two()
}

pub(crate) struct TimerNode<K> {
    level: u8,
    index: u8,
    entry_info: TrioArc<EntryInfo<K>>,
}

impl<K> TimerNode<K> {
    fn new(entry_info: TrioArc<EntryInfo<K>>, level: usize, index: usize) -> Self {
        Self {
            level: level.try_into().unwrap(),
            index: index.try_into().unwrap(),
            entry_info,
        }
    }

    fn entry_info(&self) -> &TrioArc<EntryInfo<K>> {
        &self.entry_info
    }
}

type Bucket<K> = Deque<TimerNode<K>>;

/// A hierarchical timer wheel to add, remove, and fire expiration events in
/// amortized O(1) time.
///
/// The expiration events are deferred until the timer is advanced, which is
/// performed as part of the cache's housekeeping cycle.
pub(crate) struct TimerWheel<K> {
    wheels: Box<[Box<[Bucket<K>]>]>,
    /// The time when this timer wheel was created.
    origin: Instant,
    /// The time when this timer wheel was last advanced.
    current: Instant,
}

impl<K> TimerWheel<K> {
    fn new(now: Instant) -> Self {
        let wheels = BUCKET_COUNTS
            .iter()
            .map(|b| {
                (0..*b)
                    .map(|_| Deque::new(super::CacheRegion::Other))
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            wheels,
            origin: now,
            current: now,
        }
    }

    /// Schedules a timer event for the node.
    pub(crate) fn schedule(
        &mut self,
        entry_info: TrioArc<EntryInfo<K>>,
    ) -> Option<NonNull<DeqNode<TimerNode<K>>>> {
        if let Some(t) = entry_info.expiration_time() {
            let (level, index) = self.bucket_indices(t);
            dbg!(level, index);
            let node = Box::new(DeqNode::new(TimerNode::new(entry_info, level, index)));
            let node = self.wheels[level][index].push_back(node);
            Some(node)
        } else {
            None
        }
    }

    /// Reschedules an active timer event for the node.
    pub(crate) fn reschedule(
        &mut self,
        node: NonNull<DeqNode<TimerNode<K>>>,
    ) -> Option<NonNull<DeqNode<TimerNode<K>>>> {
        let p = unsafe { node.as_ref() };
        let entry_info = TrioArc::clone(&p.element.entry_info);
        self.deschedule(node);
        self.schedule(entry_info)
    }

    /// Removes a timer event for this node if present.
    pub(crate) fn deschedule(&mut self, node: NonNull<DeqNode<TimerNode<K>>>) {
        let p = unsafe { node.as_ref() };
        let level = p.element.level as usize;
        let index = p.element.index as usize;
        unsafe { self.wheels[level][index].unlink_and_drop(node) };
    }

    /// Advances the timer wheel to the current time, and returns an iterator over
    /// expired cache entries.
    pub(crate) fn advance(
        &mut self,
        current_time: Instant,
    ) -> impl Iterator<Item = TrioArc<EntryInfo<K>>> + '_ {
        let previous_time = self.current;
        self.current = current_time;
        ExpiredEntries::new(self, previous_time, current_time)
    }

    /// Returns a reference to the timer event (cache entry) at the front of the
    /// queue.
    pub(crate) fn pop_timer(
        &mut self,
        level: usize,
        index: usize,
    ) -> Option<TrioArc<EntryInfo<K>>> {
        self.wheels[level][index]
            .pop_front()
            .map(|node| TrioArc::clone(&node.element.entry_info))
    }

    /// Returns the bucket indices to locate the bucket that the timer event
    /// should be added to.
    fn bucket_indices(&self, time: Instant) -> (usize, usize) {
        let duration = time
            .checked_duration_since(self.current)
            // FIXME: unwrap will panic if the time is earlier than self.current.
            .unwrap()
            .as_nanos() as u64;
        let time_nanos = self.time_nanos(time);
        for level in 0..=NUM_LEVELS {
            if duration < SPANS[level + 1] {
                let ticks = time_nanos >> SHIFT[level];
                let index = ticks & (BUCKET_COUNTS[level] - 1);
                return (level, index as usize);
            }
        }
        (OVERFLOW_QUEUE_INDEX, 0)
    }

    // Nano-seconds since the timer wheel was created.
    fn time_nanos(&self, time: Instant) -> u64 {
        // ENHANCEME: Check overflow? (u128 -> u64)
        // FIXME: unwrap will panic if the time is earlier than self.origin.
        time.checked_duration_since(self.origin).unwrap().as_nanos() as u64
    }
}

/// An iterator over expired cache entries.
pub(crate) struct ExpiredEntries<'iter, K> {
    timer_wheel: &'iter mut TimerWheel<K>,
    previous_time: Instant,
    current_time: Instant,
    is_done: bool,
    level: usize,
    // TODO: u8 should be enough.
    index: u64,
    end_index: u64,
    index_mask: u64,
    is_index_set: bool,
}

impl<'iter, K> ExpiredEntries<'iter, K> {
    fn new(
        timer_wheel: &'iter mut TimerWheel<K>,
        previous_time: Instant,
        current_time: Instant,
    ) -> Self {
        Self {
            timer_wheel,
            previous_time,
            current_time,
            is_done: false,
            level: 0,
            index: 0,
            end_index: 0,
            index_mask: 0,
            is_index_set: false,
        }
    }
}

impl<'iter, K> Drop for ExpiredEntries<'iter, K> {
    fn drop(&mut self) {
        // If dropped without completely consuming this iterator, reset the timer
        // wheel's current time to the previous time.
        if !self.is_done {
            self.timer_wheel.current = self.previous_time;
        }
    }
}

impl<'iter, K> Iterator for ExpiredEntries<'iter, K> {
    type Item = TrioArc<EntryInfo<K>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_done {
            return None;
        }

        loop {
            // Ensure that the index for the current level is set.
            if !self.is_index_set {
                let previous_time_nanos = self.timer_wheel.time_nanos(self.previous_time);
                let current_time_nanos = self.timer_wheel.time_nanos(self.current_time);
                let previous_ticks = previous_time_nanos >> SHIFT[self.level];
                let current_ticks = current_time_nanos >> SHIFT[self.level];

                if current_ticks <= previous_ticks {
                    self.is_done = true;
                    return None;
                }

                self.index_mask = BUCKET_COUNTS[self.level] - 1;
                self.index = previous_ticks & self.index_mask;
                let steps = (current_ticks - previous_ticks + 1).min(BUCKET_COUNTS[self.level]);
                self.end_index = self.index + steps;

                self.is_index_set = true;

                // dbg!(self.level, self.index, self.end_index);
            }

            // Pop the next timer event (cache entry) from the current level and
            // index.
            let i = self.index & self.index_mask;
            match self.timer_wheel.pop_timer(self.level, i as usize) {
                Some(entry_info) => {
                    if let Some(t) = entry_info.expiration_time() {
                        if t <= self.current_time {
                            // The cache entry has expired. Return it.
                            return Some(entry_info);
                        } else {
                            // The cache entry has not expired. Reschedule it.
                            self.timer_wheel.schedule(entry_info);
                        }
                    }
                }
                // Done with the current level and index. Move to the next index or
                // next level.
                None => {
                    self.index += 1;
                    if self.index >= self.end_index {
                        self.level += 1;
                        // No more levels to process. We are done.
                        if self.level >= BUCKET_COUNTS.len() {
                            self.is_done = true;
                            return None;
                        }
                        self.is_index_set = false;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{f32::MIN, sync::Arc, time::Duration};

    use super::{TimerWheel, SPANS};
    use crate::common::{
        concurrent::{entry_info::EntryInfo, KeyHash},
        time::{CheckedTimeOps, Clock, Instant, Mock},
    };

    use triomphe::Arc as TrioArc;

    #[test]
    fn bucket_indices() {
        fn bi(timer: &TimerWheel<()>, now: Instant, dur: Duration) -> (usize, usize) {
            let t = now.checked_add(dur).unwrap();
            timer.bucket_indices(t)
        }

        let (clock, mock) = Clock::mock();
        let now = now(&clock);

        let mut timer = TimerWheel::<()>::new(now);
        assert_eq!(timer.bucket_indices(now), (0, 0));

        // Level 0: 1.07s
        assert_eq!(bi(&timer, now, n2d(SPANS[0] - 1)), (0, 0));
        assert_eq!(bi(&timer, now, n2d(SPANS[0])), (0, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 63)), (0, 63));

        // Level 1: 1.14m
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 64)), (1, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[1])), (1, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[1] * 63 + SPANS[0] * 63)), (1, 63));

        // Level 2: 1.22h
        assert_eq!(bi(&timer, now, n2d(SPANS[1] * 64)), (2, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[2])), (2, 1));
        assert_eq!(
            bi(
                &timer,
                now,
                n2d(SPANS[2] * 31 + SPANS[1] * 63 + SPANS[0] * 63)
            ),
            (2, 31)
        );

        // Level 3: 1.63dh
        assert_eq!(bi(&timer, now, n2d(SPANS[2] * 32)), (3, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[3])), (3, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[3] * 3)), (3, 3));

        // Overflow
        assert_eq!(bi(&timer, now, n2d(SPANS[3] * 4)), (4, 0));
        assert_eq!(bi(&timer, now, n2d(SPANS[4])), (4, 0));
        assert_eq!(bi(&timer, now, n2d(SPANS[4] * 100)), (4, 0));

        // Increment the clock by 5 ticks. (1 tick ~= 1.07s)
        let now = advance_clock(&clock, &mock, n2d(SPANS[0] * 5));
        timer.current = now;

        // Level 0: 1.07s
        assert_eq!(bi(&timer, now, n2d(SPANS[0] - 1)), (0, 5));
        assert_eq!(bi(&timer, now, n2d(SPANS[0])), (0, 6));
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 63)), (0, 4));

        // Level 1: 1.14m
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 64)), (1, 1));
        assert_eq!(bi(&timer, now, n2d(SPANS[1])), (1, 1));
        assert_eq!(
            bi(&timer, now, n2d(SPANS[1] * 63 + SPANS[0] * (63 - 5))),
            (1, 63)
        );

        // Increment the clock by 61 ticks. (total 66 ticks)
        let now = advance_clock(&clock, &mock, n2d(SPANS[0] * 61));
        timer.current = now;

        // Level 0: 1.07s
        assert_eq!(bi(&timer, now, n2d(SPANS[0] - 1)), (0, 2));
        assert_eq!(bi(&timer, now, n2d(SPANS[0])), (0, 3));
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 63)), (0, 1));

        // Level 1: 1.14m
        assert_eq!(bi(&timer, now, n2d(SPANS[0] * 64)), (1, 2));
        assert_eq!(bi(&timer, now, n2d(SPANS[1])), (1, 2));
        assert_eq!(
            bi(&timer, now, n2d(SPANS[1] * 63 + SPANS[0] * (63 - 2))),
            (1, 0)
        );
    }

    #[test]
    fn advance() {
        fn schedule_timer(timer: &mut TimerWheel<u32>, key: u32, now: Instant, ttl: Duration) {
            let hash = key as u64;
            let key_hash = KeyHash::new(Arc::new(key), hash);
            let policy_weight = 0;
            let mut entry_info = TrioArc::new(EntryInfo::new(key_hash, now, policy_weight));
            entry_info.set_expiration_time(now.checked_add(ttl).unwrap());
            timer.schedule(entry_info);
        }

        fn key(maybe_entry: Option<TrioArc<EntryInfo<u32>>>) -> u32 {
            *maybe_entry.expect("entry is none").key_hash().key
        }

        let (clock, mock) = Clock::mock();
        let now = advance_clock(&clock, &mock, s2d(10));

        let mut timer = TimerWheel::<u32>::new(now);

        // Add timers that will expire in some seconds.
        schedule_timer(&mut timer, 1, now, s2d(5));
        schedule_timer(&mut timer, 2, now, s2d(1));
        schedule_timer(&mut timer, 3, now, s2d(63));
        schedule_timer(&mut timer, 4, now, s2d(3));

        let now = advance_clock(&clock, &mock, s2d(4));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 2);
        assert_eq!(key(expired_entries.next()), 4);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(4));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 1);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(64 - 8));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 3);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        // Add timers that will expire in some minutes.
        const MINUTES: u64 = 60;
        schedule_timer(&mut timer, 1, now, s2d(5 * MINUTES));
        #[allow(clippy::identity_op)]
        schedule_timer(&mut timer, 2, now, s2d(1 * MINUTES));
        schedule_timer(&mut timer, 3, now, s2d(63 * MINUTES));
        schedule_timer(&mut timer, 4, now, s2d(3 * MINUTES));

        let now = advance_clock(&clock, &mock, s2d(4 * MINUTES));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 2);
        assert_eq!(key(expired_entries.next()), 4);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(4 * MINUTES));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 1);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d((64 - 8) * MINUTES));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 3);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        // Add timers that will expire in some hours.
        const HOURS: u64 = 60 * 60;
        schedule_timer(&mut timer, 1, now, s2d(5 * HOURS));
        #[allow(clippy::identity_op)]
        schedule_timer(&mut timer, 2, now, s2d(1 * HOURS));
        schedule_timer(&mut timer, 3, now, s2d(31 * HOURS));
        schedule_timer(&mut timer, 4, now, s2d(3 * HOURS));

        let now = advance_clock(&clock, &mock, s2d(4 * HOURS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 2);
        assert_eq!(key(expired_entries.next()), 4);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(4 * HOURS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 1);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d((32 - 8) * HOURS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 3);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        // Add timers that will expire in a few days.
        const DAYS: u64 = 24 * 60 * 60;
        schedule_timer(&mut timer, 1, now, s2d(5 * DAYS));
        #[allow(clippy::identity_op)]
        schedule_timer(&mut timer, 2, now, s2d(1 * DAYS));
        schedule_timer(&mut timer, 3, now, s2d(2 * DAYS));
        // Longer than ~6.5 days, so this should be stored in the overflow area.
        schedule_timer(&mut timer, 4, now, s2d(8 * DAYS));

        let now = advance_clock(&clock, &mock, s2d(3 * DAYS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 2);
        assert_eq!(key(expired_entries.next()), 3);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(3 * DAYS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 1);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);

        let now = advance_clock(&clock, &mock, s2d(3 * DAYS));
        let mut expired_entries = timer.advance(now);
        assert_eq!(key(expired_entries.next()), 4);
        assert!(expired_entries.next().is_none());
        drop(expired_entries);
    }

    //
    // Utility functions
    //

    fn now(clock: &Clock) -> Instant {
        Instant::new(clock.now())
    }

    fn advance_clock(clock: &Clock, mock: &Arc<Mock>, duration: Duration) -> Instant {
        mock.increment(duration);
        now(clock)
    }

    /// Convert nano-seconds to duration.
    fn n2d(nanos: u64) -> Duration {
        Duration::from_nanos(nanos)
    }

    /// Convert seconds to duration.
    fn s2d(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }
}
