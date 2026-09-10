use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(300);
const PHASES: u32 = 6;

pub(super) struct Flash {
    pub pr: String,
    started: Instant,
}

impl Flash {
    pub fn new(pr: String) -> Self {
        Self {
            pr,
            started: Instant::now(),
        }
    }

    /// Three blue pulses separated by the default color, then restore live state.
    pub fn phase(&self, now: Instant) -> Option<(bool, Instant)> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= INTERVAL * PHASES {
            return None;
        }
        let phase = (elapsed.as_millis() / INTERVAL.as_millis()) as u32;
        Some((
            phase.is_multiple_of(2),
            self.started + INTERVAL * (phase + 1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flashes_blue_three_times_and_finishes_at_the_deadline() {
        let flash = Flash::new("pr".into());
        for phase in 0..PHASES {
            let now = flash.started + INTERVAL * phase;
            let expected = Some((phase % 2 == 0, now + INTERVAL));
            assert_eq!(flash.phase(now), expected);
            assert_eq!(
                flash.phase(now + INTERVAL - Duration::from_nanos(1)),
                expected
            );
        }
        assert_eq!(flash.phase(flash.started + INTERVAL * PHASES), None);
        assert_eq!(flash.phase(flash.started + Duration::from_secs(60)), None);
    }
}
