use crate::{Error, Result};
use std::sync::Mutex;
use tokio::time::Instant;
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClockObservation {
    pub utc_ms: u64,
    pub trusted: bool,
}
struct Anchor {
    utc_ms: u64,
    monotonic: Instant,
    highwater: u64,
    trusted: bool,
}
pub(crate) struct GoalClock {
    anchor: Mutex<Anchor>,
}
impl GoalClock {
    pub fn new(highwater: u64) -> Self {
        Self::at(highwater, crate::runtime::now_ms())
    }
    fn at(highwater: u64, now: u64) -> Self {
        Self {
            anchor: Mutex::new(Anchor {
                utc_ms: now,
                monotonic: Instant::now(),
                highwater,
                trusted: now >= highwater,
            }),
        }
    }
    pub fn observe(&self) -> Result<ClockObservation> {
        let mut a = self.anchor.lock().map_err(Error::io)?;
        if !a.trusted {
            let now = crate::runtime::now_ms();
            if now >= a.highwater {
                a.utc_ms = now;
                a.monotonic = Instant::now();
                a.trusted = true;
            }
        }
        let elapsed = u64::try_from(a.monotonic.elapsed().as_millis()).ok();
        let utc = elapsed
            .and_then(|e| a.utc_ms.checked_add(e))
            .filter(|v| *v <= i64::MAX as u64);
        match utc {
            Some(utc) => {
                if a.trusted {
                    a.highwater = a.highwater.max(utc);
                }
                Ok(ClockObservation {
                    utc_ms: utc,
                    trusted: a.trusted,
                })
            }
            None => {
                a.trusted = false;
                Ok(ClockObservation {
                    utc_ms: a.highwater,
                    trusted: false,
                })
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn monotonic_elapsed_maps_to_the_fixed_utc_anchor() {
        let clock = GoalClock::at(1000, 2000);
        tokio::time::advance(std::time::Duration::from_millis(300)).await;
        let observed = clock.observe().unwrap();
        assert!(observed.trusted);
        assert_eq!(observed.utc_ms, 2300);
    }
    #[test]
    fn a_future_persisted_highwater_never_creates_new_business_time() {
        let clock = GoalClock::at(i64::MAX as u64 - 1, 1000);
        assert!(!clock.observe().unwrap().trusted);
    }
}
