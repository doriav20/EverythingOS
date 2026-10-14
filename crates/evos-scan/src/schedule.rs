//! Hot/cold scheduling of folder checks for network shares.
//!
//! Most folders on a big share never change, while a few change all the
//! time. Every folder gets its own check interval that doubles each time a
//! check finds it unchanged and snaps back to the minimum when it changed:
//! `interval = clamp(min * 2^streak, min, max)`. Busy folders are checked
//! every half minute, archives back off to the maximum staleness (a day by
//! default), and the per-check budget goes where changes actually happen.
//!
//! Folders the user is looking at can be boosted to the front of the queue,
//! and folders that were not re-listed for a long time are flagged for a
//! full metadata refresh, since file size/mtime changes do not touch the
//! folder's mtime.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use evos_core::{EntryId, NO_ENTRY};

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    /// Check interval of a folder that just changed.
    pub min_interval: Duration,
    /// Longest a folder may go unchecked (maximum staleness).
    pub max_interval: Duration,
    /// Re-list folders this long after their last listing to refresh file
    /// sizes and dates, which folder mtimes do not reflect.
    pub refresh_interval: Duration,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        ScheduleConfig {
            min_interval: Duration::from_secs(30),
            max_interval: Duration::from_secs(24 * 3600),
            refresh_interval: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    due: Instant,
    streak: u8,
    listed: Instant,
}

/// Per-folder check scheduler.
#[derive(Debug)]
pub struct Scheduler {
    cfg: ScheduleConfig,
    slots: HashMap<EntryId, Slot>,
    /// Min-heap of (due, folder). May hold stale entries; `slots` is truth.
    heap: BinaryHeap<Reverse<(Instant, EntryId)>>,
}

impl Scheduler {
    pub fn new(cfg: ScheduleConfig) -> Self {
        Scheduler {
            cfg,
            slots: HashMap::new(),
            heap: BinaryHeap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn is_tracked(&self, id: EntryId) -> bool {
        self.slots.contains_key(&id)
    }

    /// Starts tracking a folder that was just listed. Its first check is
    /// spread over the minimum interval to avoid a thundering herd.
    pub fn track(&mut self, id: EntryId, now: Instant) {
        if self.slots.contains_key(&id) {
            return;
        }
        let due = now + self.cfg.min_interval.mul_f64(0.5 + spread(id) * 0.5);
        self.set(
            id,
            Slot {
                due,
                streak: 0,
                listed: now,
            },
        );
    }

    /// Starts tracking many folders at once, with first checks spread over
    /// the minimum interval.
    pub fn track_all(&mut self, ids: impl IntoIterator<Item = EntryId>, now: Instant) {
        for id in ids {
            self.track(id, now);
        }
    }

    pub fn untrack(&mut self, id: EntryId) {
        self.slots.remove(&id);
    }

    /// Records the result of checking a folder.
    pub fn record(&mut self, id: EntryId, changed: bool, now: Instant) {
        let Some(mut slot) = self.slots.get(&id).copied() else {
            return;
        };
        slot.streak = if changed {
            0
        } else {
            slot.streak.saturating_add(1).min(30)
        };
        if changed {
            slot.listed = now;
        }
        let interval = self
            .cfg
            .min_interval
            .saturating_mul(1u32 << slot.streak.min(20))
            .clamp(self.cfg.min_interval, self.cfg.max_interval);
        // ±10 % jitter keeps folders checked together from staying in sync.
        slot.due = now + interval.mul_f64(0.9 + spread(id ^ slot.streak as u32) * 0.2);
        self.set(id, slot);
    }

    /// Records that a folder was fully re-listed (metadata refreshed).
    pub fn record_listed(&mut self, id: EntryId, now: Instant) {
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.listed = now;
        }
    }

    /// Moves a folder to the front of the queue.
    pub fn boost(&mut self, id: EntryId, now: Instant) {
        if let Some(mut slot) = self.slots.get(&id).copied() {
            slot.due = now;
            slot.streak = 0;
            self.set(id, slot);
        }
    }

    /// Moves every recently changed folder (at most `max_streak` unchanged
    /// checks since its last change) to the front of the queue. Used when
    /// the server says something changed without saying where.
    pub fn boost_hot(&mut self, max_streak: u8, now: Instant) {
        let hot: Vec<EntryId> = self
            .slots
            .iter()
            .filter(|(_, s)| s.streak <= max_streak && s.due > now)
            .map(|(&id, _)| id)
            .collect();
        for id in hot {
            if let Some(mut slot) = self.slots.get(&id).copied() {
                slot.due = now;
                self.set(id, slot);
            }
        }
    }

    /// Does this folder need a full re-list to refresh file metadata?
    pub fn needs_refresh(&self, id: EntryId, now: Instant) -> bool {
        self.slots
            .get(&id)
            .is_some_and(|s| now.duration_since(s.listed) >= self.cfg.refresh_interval)
    }

    /// Removes and returns up to `limit` folders whose check is due.
    pub fn pop_due(&mut self, now: Instant, limit: usize) -> Vec<EntryId> {
        let mut due = Vec::new();
        while due.len() < limit {
            let Some(&Reverse((when, id))) = self.heap.peek() else {
                break;
            };
            if when > now {
                break;
            }
            self.heap.pop();
            // Skip stale heap entries superseded by a later reschedule.
            if self.slots.get(&id).is_some_and(|s| s.due == when) {
                due.push(id);
            }
        }
        due
    }

    /// When the next check is due, if any folder is tracked.
    pub fn next_due(&mut self) -> Option<Instant> {
        while let Some(&Reverse((when, id))) = self.heap.peek() {
            if self.slots.get(&id).is_some_and(|s| s.due == when) {
                return Some(when);
            }
            self.heap.pop();
        }
        None
    }

    /// Translates ids after the index was compacted.
    pub fn remap(&mut self, map: &[EntryId]) {
        let slots = std::mem::take(&mut self.slots);
        self.heap.clear();
        for (id, slot) in slots {
            if let Some(&new) = map.get(id as usize).filter(|&&n| n != NO_ENTRY) {
                self.set(new, slot);
            }
        }
    }

    fn set(&mut self, id: EntryId, slot: Slot) {
        self.slots.insert(id, slot);
        self.heap.push(Reverse((slot.due, id)));
    }
}

/// Deterministic pseudo-random value in [0, 1) derived from an id.
fn spread(id: u32) -> f64 {
    let mut x = id.wrapping_mul(0x9e37_79b9) ^ 0x85eb_ca6b;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2c1b_3c6d);
    x ^= x >> 12;
    (x as f64) / (u32::MAX as f64 + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ScheduleConfig {
        ScheduleConfig {
            min_interval: Duration::from_secs(30),
            max_interval: Duration::from_secs(3600),
            refresh_interval: Duration::from_secs(7200),
        }
    }

    #[test]
    fn new_folders_are_due_within_the_min_interval() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(cfg());
        s.track_all(0..100, t0);
        assert!(s.pop_due(t0, 1000).is_empty());
        let due = s.pop_due(t0 + Duration::from_secs(30), 1000);
        assert_eq!(due.len(), 100);
        assert!(
            s.pop_due(t0 + Duration::from_secs(30), 1000).is_empty(),
            "popped once"
        );
    }

    #[test]
    fn stable_folders_back_off_and_changed_ones_snap_back() {
        let mut now = Instant::now();
        let mut s = Scheduler::new(cfg());
        s.track(1, now);
        let mut intervals = Vec::new();
        for _ in 0..10 {
            let next = s.next_due().unwrap();
            intervals.push(next.saturating_duration_since(now).as_secs());
            now = next;
            assert_eq!(s.pop_due(now, 10), vec![1]);
            s.record(1, false, now);
        }
        // 30s * 2^n with jitter, capped at one hour.
        assert!(intervals[3] >= 200 && intervals[3] <= 300, "{intervals:?}");
        assert!(intervals.last().unwrap() <= &3960, "{intervals:?}");
        assert!(intervals.last().unwrap() >= &3240, "{intervals:?}");
        s.pop_due(now + Duration::from_secs(4000), 10);
        s.record(1, true, now);
        let back = s.next_due().unwrap().duration_since(now).as_secs();
        assert!(
            (27..=33).contains(&back),
            "changed folder returns to the minimum: {back}"
        );
    }

    #[test]
    fn boost_and_budget() {
        let now = Instant::now();
        let mut s = Scheduler::new(cfg());
        s.track_all(0..10, now);
        s.boost(7, now);
        assert_eq!(s.pop_due(now, 5), vec![7]);
        let later = now + Duration::from_secs(60);
        assert_eq!(s.pop_due(later, 4).len(), 4, "budget limits a batch");
        assert_eq!(s.pop_due(later, 100).len(), 5);
    }

    #[test]
    fn boost_hot_only_moves_recently_changed_folders() {
        let now = Instant::now();
        let mut s = Scheduler::new(cfg());
        s.track_all([1, 2], now);
        let mut t = now;
        for _ in 0..5 {
            // Jump past every due time; 1 keeps changing, 2 never does.
            t += Duration::from_secs(100_000);
            for id in s.pop_due(t, 10) {
                s.record(id, id == 1, t);
            }
        }
        s.boost_hot(2, t);
        assert_eq!(s.pop_due(t, 10), vec![1]);
    }

    #[test]
    fn refresh_and_remap() {
        let now = Instant::now();
        let mut s = Scheduler::new(cfg());
        s.track(5, now);
        s.track(9, now);
        assert!(!s.needs_refresh(5, now + Duration::from_secs(60)));
        assert!(s.needs_refresh(5, now + Duration::from_secs(7200)));
        s.record_listed(5, now + Duration::from_secs(7000));
        assert!(!s.needs_refresh(5, now + Duration::from_secs(7200)));

        let mut map = vec![NO_ENTRY; 10];
        map[5] = 2;
        s.remap(&map);
        assert_eq!(s.len(), 1);
        assert!(s.is_tracked(2) && !s.is_tracked(5) && !s.is_tracked(9));
        assert_eq!(s.pop_due(now + Duration::from_secs(60), 10), vec![2]);
    }
}
