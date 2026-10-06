//! Pure state machine for the watcher: wake-delay and long-press decisions. No I/O.
#![allow(dead_code)]

use std::fmt;
use std::time::Duration;

/// Monotonic timestamp, as an offset from an arbitrary origin.
pub type Time = Duration;

/// How close the `BTN_MODE` release may come before `Gone` and still count as "held until
/// disconnect" (the probe saw release and ENODEV ~7ms apart).
pub const RELEASE_GRACE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Any input event; `btn_mode` is `Some(value)` if it was `BTN_MODE` (1 press, 0 release).
    Input { t: Time, btn_mode: Option<i32> },
    /// Periodic tick while waiting for the wake delay.
    Tick { t: Time },
    /// The device is gone (ENODEV).
    Gone { t: Time },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Wake,
    Off,
    Exit,
}

/// Why the machine returned its last non-`None` action, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `Wake`: an input event arrived before the wake delay.
    Input,
    /// `Wake`: the device was still there after the wake delay.
    StillConnected { after: Duration },
    /// `Exit`: gone before any input or the wake delay (idle-off ghost reconnect).
    GhostReconnect { after: Duration },
    /// `Off`: `BTN_MODE` still held at `Gone`, for at least `long_press`.
    LongPressHeld { held: Duration },
    /// `Off`: `BTN_MODE` held for at least `long_press`, released just before `Gone`.
    LongPressReleased {
        held: Duration,
        before_gone: Duration,
    },
    /// `Exit`: `BTN_MODE` still held at `Gone`, but not long enough.
    HeldTooShort { held: Duration },
    /// `Exit`: last `BTN_MODE` hold was shorter than `long_press`.
    ShortPress { held: Duration },
    /// `Exit`: long hold, but released too long before `Gone`.
    ReleasedEarly {
        held: Duration,
        before_gone: Duration,
    },
    /// `Exit`: gone with no `BTN_MODE` press seen.
    NoButtonHeld,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Reason::Input => write!(f, "input"),
            Reason::StillConnected { after } => write!(f, "still connected after {}", Dur(after)),
            Reason::GhostReconnect { after } => {
                write!(f, "ghost reconnect (gone after {}, no input)", Dur(after))
            }
            Reason::LongPressHeld { held } => {
                write!(f, "long press {} (held at disconnect)", Dur(held))
            }
            Reason::LongPressReleased { held, before_gone } => write!(
                f,
                "long press {} (released {} before disconnect)",
                Dur(held),
                Dur(before_gone)
            ),
            Reason::HeldTooShort { held } => write!(f, "button held only {}", Dur(held)),
            Reason::ShortPress { held } => write!(f, "short press {}", Dur(held)),
            Reason::ReleasedEarly { held, before_gone } => {
                write!(f, "press {} released {} ago", Dur(held), Dur(before_gone))
            }
            Reason::NoButtonHeld => write!(f, "no button held"),
        }
    }
}

/// Compact human duration: "7ms", "10.6s", "2m", "1h5m".
struct Dur(Duration);

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        let secs = d.as_secs();
        if d < Duration::from_secs(1) {
            write!(f, "{}ms", d.as_millis())
        } else if secs < 60 {
            write!(f, "{:.1}s", d.as_secs_f64())
        } else if secs < 3600 {
            write!(f, "{}m", secs / 60)
        } else {
            write!(f, "{}h{}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the first input or the wake delay.
    Pending,
    /// Woke (or decided to); tracking `BTN_MODE` until `Gone`.
    Running,
    /// Returned `Exit` (or `Off`); everything after is ignored.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    state: State,
    start: Time,
    wake_delay: Duration,
    long_press: Duration,
    /// Time of the last `BTN_MODE` press.
    pressed_at: Option<Time>,
    /// Time of the release following `pressed_at`, if any.
    released_at: Option<Time>,
    reason: Option<Reason>,
}

impl Machine {
    /// New machine for a device that appeared at `start`.
    pub fn new(start: Time, wake_delay: Duration, long_press: Duration) -> Self {
        Self {
            state: State::Pending,
            start,
            wake_delay,
            long_press,
            pressed_at: None,
            released_at: None,
            reason: None,
        }
    }

    /// Feed one event, get the action to take.
    pub fn step(&mut self, event: Event) -> Action {
        match (self.state, event) {
            (State::Done, _) => Action::None,

            (State::Pending, Event::Input { t, btn_mode }) => {
                self.track(t, btn_mode);
                self.state = State::Running;
                self.decide(Action::Wake, Reason::Input)
            }
            (State::Pending, Event::Tick { t }) => {
                let after = t.saturating_sub(self.start);
                if after >= self.wake_delay {
                    self.state = State::Running;
                    self.decide(Action::Wake, Reason::StillConnected { after })
                } else {
                    Action::None
                }
            }
            (State::Pending, Event::Gone { t }) => {
                self.state = State::Done;
                let after = t.saturating_sub(self.start);
                self.decide(Action::Exit, Reason::GhostReconnect { after })
            }

            (State::Running, Event::Input { t, btn_mode }) => {
                self.track(t, btn_mode);
                Action::None
            }
            (State::Running, Event::Tick { .. }) => Action::None,
            (State::Running, Event::Gone { t }) => {
                self.state = State::Done;
                let (action, reason) = self.on_gone(t);
                self.decide(action, reason)
            }
        }
    }

    /// Whether the I/O layer still needs to send `Tick`s (only while waiting for the wake delay).
    pub fn needs_tick(&self) -> bool {
        self.state == State::Pending
    }

    /// Whether the machine has returned its final action (`Exit` or `Off`).
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Why the last non-`None` action was returned, for logging.
    pub fn reason(&self) -> Option<Reason> {
        self.reason
    }

    fn decide(&mut self, action: Action, reason: Reason) -> Action {
        self.reason = Some(reason);
        action
    }

    fn track(&mut self, t: Time, btn_mode: Option<i32>) {
        match btn_mode {
            Some(1) => {
                self.pressed_at = Some(t);
                self.released_at = None;
            }
            Some(0) if self.pressed_at.is_some() => self.released_at = Some(t),
            // Autorepeat (2), stray releases and other events don't change the press.
            _ => {}
        }
    }

    fn on_gone(&self, t: Time) -> (Action, Reason) {
        let Some(pressed_at) = self.pressed_at else {
            return (Action::Exit, Reason::NoButtonHeld);
        };
        match self.released_at {
            None => {
                let held = t.saturating_sub(pressed_at);
                if held >= self.long_press {
                    (Action::Off, Reason::LongPressHeld { held })
                } else {
                    (Action::Exit, Reason::HeldTooShort { held })
                }
            }
            Some(released_at) => {
                let held = released_at.saturating_sub(pressed_at);
                let before_gone = t.saturating_sub(released_at);
                if held < self.long_press {
                    (Action::Exit, Reason::ShortPress { held })
                } else if before_gone <= RELEASE_GRACE {
                    (Action::Off, Reason::LongPressReleased { held, before_gone })
                } else {
                    (Action::Exit, Reason::ReleasedEarly { held, before_gone })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAKE: Duration = Duration::from_secs(5);
    const LONG: Duration = Duration::from_secs(5);

    fn ms(n: u64) -> Time {
        Duration::from_millis(n)
    }

    fn machine() -> Machine {
        Machine::new(ms(1_000), WAKE, LONG)
    }

    fn input(t: u64) -> Event {
        Event::Input {
            t: ms(t),
            btn_mode: None,
        }
    }
    fn press(t: u64) -> Event {
        Event::Input {
            t: ms(t),
            btn_mode: Some(1),
        }
    }
    fn release(t: u64) -> Event {
        Event::Input {
            t: ms(t),
            btn_mode: Some(0),
        }
    }
    fn tick(t: u64) -> Event {
        Event::Tick { t: ms(t) }
    }
    fn gone(t: u64) -> Event {
        Event::Gone { t: ms(t) }
    }

    /// A machine that has already woken via the wake delay (start 1s, woke at 6s).
    fn running() -> Machine {
        let mut m = machine();
        assert_eq!(m.step(tick(6_000)), Action::Wake);
        m
    }

    #[test]
    fn press_within_wake_delay_wakes_on_that_event() {
        let mut m = machine();
        assert!(m.needs_tick());
        assert_eq!(m.step(tick(1_250)), Action::None);
        assert_eq!(m.step(tick(3_000)), Action::None);
        assert_eq!(m.step(press(3_100)), Action::Wake);
        assert_eq!(m.reason(), Some(Reason::Input));
        assert!(!m.needs_tick());
        // No second wake.
        assert_eq!(m.step(release(3_200)), Action::None);
        assert_eq!(m.step(tick(7_000)), Action::None);
        assert_eq!(m.step(input(8_000)), Action::None);
    }

    #[test]
    fn non_button_input_also_wakes() {
        let mut m = machine();
        assert_eq!(m.step(input(1_500)), Action::Wake);
    }

    #[test]
    fn no_input_still_there_at_wake_delay_wakes() {
        let mut m = machine();
        assert_eq!(m.step(tick(5_750)), Action::None);
        assert_eq!(m.step(tick(6_000)), Action::Wake); // exactly start + 5s
        assert_eq!(m.reason(), Some(Reason::StillConnected { after: WAKE }));
        assert!(!m.needs_tick());
        assert_eq!(m.step(tick(6_250)), Action::None);
    }

    #[test]
    fn gone_at_3s_with_no_input_exits_without_wake() {
        let mut m = machine();
        assert_eq!(m.step(tick(2_000)), Action::None);
        assert_eq!(m.step(tick(3_500)), Action::None);
        assert_eq!(m.step(gone(4_000)), Action::Exit);
        assert_eq!(
            m.reason(),
            Some(Reason::GhostReconnect { after: ms(3_000) })
        );
        assert!(m.is_done());
        assert!(!m.needs_tick());
    }

    #[test]
    fn long_hold_release_then_gone_7ms_later_is_off() {
        let mut m = machine();
        assert_eq!(m.step(press(2_000)), Action::Wake);
        assert_eq!(m.step(input(5_000)), Action::None);
        assert_eq!(m.step(release(12_600)), Action::None);
        assert_eq!(m.step(gone(12_607)), Action::Off);
        let reason = m.reason().unwrap();
        assert_eq!(
            reason,
            Reason::LongPressReleased {
                held: ms(10_600),
                before_gone: ms(7)
            }
        );
        assert_eq!(
            reason.to_string(),
            "long press 10.6s (released 7ms before disconnect)"
        );
    }

    #[test]
    fn held_and_gone_while_held_is_off() {
        let mut m = running();
        assert_eq!(m.step(press(10_000)), Action::None);
        assert_eq!(m.step(gone(16_000)), Action::Off);
        assert_eq!(m.reason(), Some(Reason::LongPressHeld { held: ms(6_000) }));
    }

    #[test]
    fn held_briefly_and_gone_while_held_exits() {
        let mut m = running();
        assert_eq!(m.step(press(10_000)), Action::None);
        assert_eq!(m.step(gone(12_000)), Action::Exit);
        assert_eq!(m.reason(), Some(Reason::HeldTooShort { held: ms(2_000) }));
    }

    #[test]
    fn power_menu_hold_then_idle_gone_exits() {
        let mut m = running();
        assert_eq!(m.step(press(10_000)), Action::None);
        assert_eq!(m.step(release(12_600)), Action::None);
        // Still connected, other input later.
        assert_eq!(m.step(input(20_000)), Action::None);
        assert_eq!(m.step(gone(2_600_000)), Action::Exit);
        assert_eq!(m.reason(), Some(Reason::ShortPress { held: ms(2_600) }));
    }

    #[test]
    fn power_menu_hold_released_right_before_gone_still_exits() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(12_600));
        assert_eq!(m.step(gone(12_607)), Action::Exit);
    }

    #[test]
    fn long_hold_released_two_minutes_before_gone_exits() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(16_000));
        assert_eq!(m.step(gone(136_000)), Action::Exit);
        let reason = m.reason().unwrap();
        assert_eq!(
            reason,
            Reason::ReleasedEarly {
                held: ms(6_000),
                before_gone: ms(120_000)
            }
        );
        assert_eq!(reason.to_string(), "press 6.0s released 2m ago");
    }

    #[test]
    fn idle_gone_with_no_button_exits() {
        let mut m = running();
        m.step(input(7_000));
        assert_eq!(m.step(gone(2_500_000)), Action::Exit);
        assert_eq!(m.reason(), Some(Reason::NoButtonHeld));
    }

    #[test]
    fn release_exactly_250ms_before_gone_is_off() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(20_000));
        assert_eq!(m.step(gone(20_250)), Action::Off);
    }

    #[test]
    fn release_251ms_before_gone_exits() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(20_000));
        assert_eq!(m.step(gone(20_251)), Action::Exit);
    }

    #[test]
    fn hold_of_exactly_long_press_counts() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(15_000));
        assert_eq!(m.step(gone(15_007)), Action::Off);

        let mut m = running();
        m.step(press(10_000));
        assert_eq!(m.step(gone(15_000)), Action::Off);
    }

    #[test]
    fn hold_just_under_long_press_does_not_count() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(14_999));
        assert_eq!(m.step(gone(15_006)), Action::Exit);

        let mut m = running();
        m.step(press(10_000));
        assert_eq!(m.step(gone(14_999)), Action::Exit);
    }

    #[test]
    fn press_that_woke_is_tracked_for_long_press() {
        // Controller turned on, then the button held right away until power-off.
        let mut m = machine();
        assert_eq!(m.step(press(1_100)), Action::Wake);
        assert_eq!(m.step(gone(12_000)), Action::Off);
    }

    #[test]
    fn later_press_replaces_earlier_hold() {
        let mut m = running();
        m.step(press(10_000));
        m.step(release(20_000)); // long hold, released
        m.step(press(30_000));
        m.step(release(30_100)); // short tap
        assert_eq!(m.step(gone(30_105)), Action::Exit);
    }

    #[test]
    fn autorepeat_and_stray_release_are_ignored() {
        let mut m = running();
        m.step(release(9_000)); // release with no press
        m.step(press(10_000));
        m.step(Event::Input {
            t: ms(12_000),
            btn_mode: Some(2),
        });
        assert_eq!(m.step(gone(16_000)), Action::Off);
    }

    #[test]
    fn events_after_exit_return_none() {
        let mut m = machine();
        assert_eq!(m.step(gone(2_000)), Action::Exit);
        let reason = m.reason();
        assert_eq!(m.step(press(2_100)), Action::None);
        assert_eq!(m.step(tick(10_000)), Action::None);
        assert_eq!(m.step(gone(11_000)), Action::None);
        assert_eq!(m.reason(), reason);
    }

    #[test]
    fn events_after_off_return_none() {
        let mut m = running();
        m.step(press(10_000));
        assert_eq!(m.step(gone(20_000)), Action::Off);
        assert!(m.is_done());
        assert_eq!(m.step(gone(20_001)), Action::None);
    }

    #[test]
    fn reason_display() {
        assert_eq!(
            Reason::GhostReconnect { after: ms(3_000) }.to_string(),
            "ghost reconnect (gone after 3.0s, no input)"
        );
        assert_eq!(
            Reason::LongPressHeld { held: ms(10_600) }.to_string(),
            "long press 10.6s (held at disconnect)"
        );
        assert_eq!(Reason::NoButtonHeld.to_string(), "no button held");
        assert_eq!(
            Reason::ShortPress { held: ms(2_600) }.to_string(),
            "short press 2.6s"
        );
        assert_eq!(
            Reason::StillConnected {
                after: ms(3_725_000)
            }
            .to_string(),
            "still connected after 1h2m"
        );
    }
}
