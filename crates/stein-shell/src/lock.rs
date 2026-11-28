//! Pointer lock as a pure state machine. The shell feeds it the facts each
//! frame after a surface's build and acts on the step it returns.
//!
//! ```text
//! Free --wanted, focused, pointer in, protocol--> Held
//! Held --not wanted, or unfocused, or no protocol--> Free
//! ```
//!
//! The pointer leaving does not unlock. The lock is persistent, so the
//! compositor deactivates it on leave and activates it again on return.

/// Whether a lock object is held for a surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Lock {
    #[default]
    Free,
    Held,
}

/// What the shell does about it this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Lock,
    Unlock,
    Nothing,
}

/// What the machine decides on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The scene asks for a lock.
    pub(crate) wanted: bool,
    /// The surface has keyboard focus.
    pub(crate) focused: bool,
    /// The pointer is over the surface.
    pub(crate) pointer_in: bool,
    /// Pointer constraints and relative pointer both bound.
    pub(crate) have_protocol: bool,
}

pub(crate) const fn step(state: Lock, f: Facts) -> Step {
    match state {
        Lock::Free if f.wanted && f.focused && f.pointer_in && f.have_protocol => Step::Lock,
        Lock::Held if !f.wanted || !f.focused || !f.have_protocol => Step::Unlock,
        _ => Step::Nothing,
    }
}

/// Add one relative motion event to a frame's motion, only while the lock
/// is active.
pub(crate) fn accumulate(motion: &mut [f32; 2], active: bool, dx: f64, dy: f64) {
    if active {
        motion[0] += dx as f32;
        motion[1] += dy as f32;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const ALL: Facts = Facts {
        wanted: true,
        focused: true,
        pointer_in: true,
        have_protocol: true,
    };

    /// The state after the step was carried out, as the shell does.
    fn run(state: Lock, f: Facts) -> Lock {
        match step(state, f) {
            Step::Lock => Lock::Held,
            Step::Unlock => Lock::Free,
            Step::Nothing => state,
        }
    }

    #[test]
    fn locks_only_when_every_fact_holds() {
        assert_eq!(step(Lock::Free, ALL), Step::Lock);
        let missing = [
            Facts {
                wanted: false,
                ..ALL
            },
            Facts {
                focused: false,
                ..ALL
            },
            Facts {
                pointer_in: false,
                ..ALL
            },
            Facts {
                have_protocol: false,
                ..ALL
            },
        ];
        for f in missing {
            assert_eq!(step(Lock::Free, f), Step::Nothing, "{f:?}");
        }
    }

    #[test]
    fn unlocks_when_unwanted_unfocused_or_protocol_gone() {
        assert_eq!(step(Lock::Held, ALL), Step::Nothing);
        for f in [
            Facts {
                wanted: false,
                ..ALL
            },
            Facts {
                focused: false,
                ..ALL
            },
            Facts {
                have_protocol: false,
                ..ALL
            },
        ] {
            assert_eq!(step(Lock::Held, f), Step::Unlock, "{f:?}");
        }
    }

    #[test]
    fn pointer_leaving_keeps_the_persistent_lock() {
        let f = Facts {
            pointer_in: false,
            ..ALL
        };
        assert_eq!(step(Lock::Held, f), Step::Nothing);
    }

    /// Facts with the protocol bound.
    const fn f(wanted: bool, focused: bool, pointer_in: bool) -> Facts {
        Facts {
            wanted,
            focused,
            pointer_in,
            have_protocol: true,
        }
    }

    #[test]
    fn a_cycle_locks_and_unlocks_only_on_the_edges() {
        use Lock::{Free, Held};
        let frames = [
            (f(true, false, false), Free),
            (f(true, false, true), Free),
            (f(true, true, true), Held),
            (f(true, true, true), Held),
            (f(true, true, false), Held),
            (f(true, false, false), Free),
            (f(true, true, true), Held),
            (f(false, true, true), Free),
            (f(false, true, true), Free),
        ];
        let mut state = Free;
        let mut steps = [0u32; 3];
        for (i, (facts, want)) in frames.into_iter().enumerate() {
            let s = step(state, facts);
            steps[s as usize] += 1;
            state = run(state, facts);
            assert_eq!(state, want, "frame {i}");
        }
        assert_eq!(steps, [2, 2, 5], "locks, unlocks, nothing");
    }

    #[test]
    fn motion_sums_while_active_and_ignores_otherwise() {
        let mut m = [0.0f32; 2];
        accumulate(&mut m, false, 5.0, 5.0);
        assert_eq!(m, [0.0, 0.0]);
        accumulate(&mut m, true, 1.5, -2.0);
        accumulate(&mut m, true, 0.25, 0.5);
        assert_eq!(m, [1.75, -1.5]);
    }

    #[test]
    fn taking_the_input_zeroes_motion() {
        let mut pending = stein_ui::Input::default();
        accumulate(&mut pending.motion, true, 3.0, 4.0);
        let frame = core::mem::take(&mut pending);
        assert_eq!(frame.motion, [3.0, 4.0]);
        assert_eq!(pending.motion, [0.0, 0.0]);
    }
}
