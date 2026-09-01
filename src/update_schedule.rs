use crate::settings::UpdateCheckMode;
use std::time::{Duration, Instant, SystemTime};

/// How often the running event loop re-evaluates the persisted last-check time
/// against the configured interval. The interval itself is user-configured; this
/// only bounds how stale the decision can get while the window stays open.
const REEVALUATION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A deadline owned by the GUI event loop; no timer task survives window exit.
/// The last successful check time is persisted by the caller, so a check that
/// became due while the program was closed is caught up on the next launch.
#[derive(Debug)]
pub struct UpdateCheckSchedule {
    mode: UpdateCheckMode,
    interval_minutes: u16,
    last_check: Option<SystemTime>,
    next_check: Option<Instant>,
}

impl UpdateCheckSchedule {
    pub fn new(
        mode: UpdateCheckMode,
        interval_minutes: u16,
        last_check: Option<SystemTime>,
        now: Instant,
    ) -> Self {
        let mut schedule = Self {
            mode,
            interval_minutes: interval_minutes.max(1),
            last_check,
            next_check: None,
        };
        schedule.reschedule(now, SystemTime::now());
        schedule
    }

    /// Reschedule only when the saved mode, interval, or last-check time changes.
    pub fn configure(
        &mut self,
        mode: UpdateCheckMode,
        interval_minutes: u16,
        last_check: Option<SystemTime>,
        now: Instant,
    ) -> bool {
        let interval_minutes = interval_minutes.max(1);
        if self.mode == mode
            && self.interval_minutes == interval_minutes
            && self.last_check == last_check
        {
            return false;
        }
        self.mode = mode;
        self.interval_minutes = interval_minutes;
        self.last_check = last_check;
        self.reschedule(now, SystemTime::now());
        true
    }

    pub fn next_check(&self) -> Option<Instant> {
        self.next_check
    }

    /// Whether the configured interval has elapsed since the last recorded check.
    /// With no recorded check, a periodic schedule is due immediately.
    pub fn is_due(&self, now: SystemTime) -> bool {
        if self.mode != UpdateCheckMode::Periodic {
            return false;
        }
        self.last_check.is_none_or(|last_check| {
            now.duration_since(last_check)
                .is_ok_and(|elapsed| elapsed >= self.interval())
        })
    }

    pub fn take_due(&mut self, now: Instant, wall_now: SystemTime) -> bool {
        if self.next_check.is_none_or(|deadline| now < deadline) {
            return false;
        }
        let due = self.is_due(wall_now);
        // The deadline only means "reconsider now". Rescheduling from the last
        // recorded check (rather than from this wake) keeps a check that is
        // not yet due from being pushed further into the future.
        self.reschedule(now, wall_now);
        if due {
            // The caller records the check it starts, and `configure` then
            // reschedules from that time. Until then, a due result must not
            // leave the deadline at `now`, or the event loop wakes in a burst.
            self.next_check = Some(now + self.interval().min(REEVALUATION_INTERVAL));
        }
        due
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.interval_minutes) * 60)
    }

    fn reschedule(&mut self, now: Instant, wall_now: SystemTime) {
        self.next_check = (self.mode == UpdateCheckMode::Periodic).then(|| {
            let until_interval = self
                .last_check
                .and_then(|last_check| wall_now.duration_since(last_check).ok())
                .map(|elapsed| self.interval().saturating_sub(elapsed))
                .unwrap_or(Duration::ZERO);
            // Never sleep past the next hourly reconsideration, so a check that
            // becomes due is noticed within an hour even when the interval is longer.
            now + until_interval.min(REEVALUATION_INTERVAL)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(seconds: u64) -> SystemTime {
        SystemTime::now()
            .checked_sub(Duration::from_secs(seconds))
            .unwrap()
    }

    #[test]
    fn startup_and_disabled_modes_have_no_periodic_deadline() {
        let now = Instant::now();
        for mode in [UpdateCheckMode::Startup, UpdateCheckMode::Disabled] {
            let mut schedule = UpdateCheckSchedule::new(mode, 1, None, now);
            assert_eq!(schedule.next_check(), None);
            assert!(!schedule.is_due(SystemTime::now()));
            assert!(!schedule.take_due(now + Duration::from_secs(3600), SystemTime::now()));
        }
    }

    #[test]
    fn a_fresh_periodic_schedule_is_due_immediately() {
        let now = Instant::now();
        let mut schedule = UpdateCheckSchedule::new(UpdateCheckMode::Periodic, 60, None, now);
        assert_eq!(schedule.next_check(), Some(now));
        assert!(schedule.is_due(SystemTime::now()));
        assert!(schedule.take_due(now, SystemTime::now()));
    }

    #[test]
    fn a_recent_check_waits_out_the_remaining_interval() {
        let now = Instant::now();
        let mut schedule =
            UpdateCheckSchedule::new(UpdateCheckMode::Periodic, 60, Some(ago(20 * 60)), now);
        let deadline = schedule.next_check().unwrap();
        let remaining = deadline.saturating_duration_since(now);
        assert!(
            (Duration::from_secs(39 * 60)..Duration::from_secs(41 * 60)).contains(&remaining),
            "remaining was {remaining:?}"
        );
        assert!(!schedule.is_due(SystemTime::now()));
        assert!(!schedule.take_due(now, SystemTime::now()));
    }

    #[test]
    fn an_elapsed_interval_is_due_even_after_a_restart() {
        let now = Instant::now();
        let schedule =
            UpdateCheckSchedule::new(UpdateCheckMode::Periodic, 60, Some(ago(2 * 60 * 60)), now);
        assert_eq!(schedule.next_check(), Some(now));
        assert!(schedule.is_due(SystemTime::now()));
    }

    #[test]
    fn the_deadline_never_exceeds_the_hourly_reevaluation() {
        let now = Instant::now();
        let schedule =
            UpdateCheckSchedule::new(UpdateCheckMode::Periodic, 24 * 60, Some(ago(60)), now);
        let wait = schedule
            .next_check()
            .unwrap()
            .saturating_duration_since(now);
        assert!(wait <= REEVALUATION_INTERVAL);
        assert!(wait > Duration::from_secs(59 * 60));
    }

    #[test]
    fn changing_modes_or_intervals_reschedules_and_disabling_cancels() {
        let start = Instant::now();
        let mut schedule = UpdateCheckSchedule::new(UpdateCheckMode::Startup, 60, None, start);
        let now = start + Duration::from_secs(10);
        let recent = Some(SystemTime::now());
        assert!(schedule.configure(UpdateCheckMode::Periodic, 5, recent, now));
        assert!(!schedule.is_due(SystemTime::now()));
        assert!(!schedule.configure(UpdateCheckMode::Periodic, 5, recent, start));
        assert!(schedule.configure(UpdateCheckMode::Periodic, 1, None, now));
        assert!(schedule.is_due(SystemTime::now()));
        assert!(schedule.configure(UpdateCheckMode::Disabled, 1, None, now));
        assert_eq!(schedule.next_check(), None);
        assert!(!schedule.is_due(SystemTime::now()));
    }

    #[test]
    fn a_late_tick_does_not_produce_a_burst_of_checks() {
        let start = Instant::now();
        let mut schedule =
            UpdateCheckSchedule::new(UpdateCheckMode::Periodic, 1, Some(ago(60 * 60)), start);
        let resumed = start + Duration::from_secs(3600);
        assert!(schedule.take_due(resumed, SystemTime::now()));
        // The second wake waits at least one interval, so the same instant is not due again.
        assert!(!schedule.take_due(resumed, SystemTime::now()));
    }
}
