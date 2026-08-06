//! AIMD controller for live verbosity adjustment.
//!
//! The offline `learn --verbosity` pass sets the *starting* level. This controller
//! tracks drift during a session and nudges the level from runtime signals, using
//! the congestion-control intuition:
//!
//! - **Additive increase** toward terser output: only after *sustained* "the user
//!   isn't reading this" pressure (a streak of TOO_MUCH signals) do we step the
//!   level up by one.
//! - **Multiplicative-style decrease** on a TOO_LITTLE signal (the user asked for
//!   more): back off immediately by a level and enter a cooldown that suppresses
//!   re-escalation.

use serde::{Deserialize, Serialize};

/// An abstract feedback signal about the last response's verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Interrupted / fast-skipped → output went unread.
    TooMuch,
    /// User asked to explain / expand.
    TooLittle,
    /// Engaged normally.
    Neutral,
}

/// Per-conversation (or per-project) controller state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerState {
    pub level: i32,
    #[serde(default)]
    pub up_streak: i32,
    #[serde(default)]
    pub cooldown: i32,
}

/// Pure AIMD controller. `observe` maps (state, signal) → new state.
#[derive(Debug, Clone)]
pub struct VerbosityController {
    pub floor: i32,
    pub ceil: i32,
    /// Consecutive TOO_MUCH signals required before stepping up.
    pub probe_threshold: i32,
    /// Turns after a back-off during which we don't re-probe.
    pub cooldown_turns: i32,
}

impl Default for VerbosityController {
    fn default() -> Self {
        Self {
            floor: 1,
            ceil: 4,
            probe_threshold: 3,
            cooldown_turns: 5,
        }
    }
}

impl VerbosityController {
    pub fn observe(&self, state: &ControllerState, signal: Signal) -> ControllerState {
        let level = state.level;
        let up_streak = state.up_streak;
        let cooldown = (state.cooldown - 1).max(0);

        match signal {
            Signal::TooLittle => ControllerState {
                level: (level - 1).max(self.floor),
                up_streak: 0,
                cooldown: self.cooldown_turns,
            },
            Signal::TooMuch => {
                if cooldown > 0 {
                    ControllerState {
                        level,
                        up_streak: 0,
                        cooldown,
                    }
                } else {
                    let new_streak = up_streak + 1;
                    if new_streak >= self.probe_threshold && level < self.ceil {
                        ControllerState {
                            level: level + 1,
                            up_streak: 0,
                            cooldown: 0,
                        }
                    } else {
                        ControllerState {
                            level,
                            up_streak: new_streak,
                            cooldown,
                        }
                    }
                }
            }
            Signal::Neutral => ControllerState {
                level,
                up_streak: 0,
                cooldown,
            },
        }
    }
}

/// Load controller state from a JSON file, clamped to [floor, ceil].
pub fn load_state(
    path: &std::path::Path,
    default_level: i32,
    floor: i32,
    ceil: i32,
) -> ControllerState {
    let mut state = match std::fs::read_to_string(path) {
        Ok(content) => {
            serde_json::from_str::<ControllerState>(&content).unwrap_or(ControllerState {
                level: default_level,
                up_streak: 0,
                cooldown: 0,
            })
        }
        Err(_) => ControllerState {
            level: default_level,
            up_streak: 0,
            cooldown: 0,
        },
    };
    state.level = state.level.max(floor).min(ceil);
    state
}

/// Save controller state to a JSON file.
pub fn save_state(path: &std::path::Path, state: &ControllerState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> VerbosityController {
        VerbosityController {
            floor: 1,
            ceil: 4,
            probe_threshold: 3,
            cooldown_turns: 5,
        }
    }

    fn run(signals: &[Signal], start: i32) -> ControllerState {
        let c = ctrl();
        let mut state = ControllerState {
            level: start,
            up_streak: 0,
            cooldown: 0,
        };
        for s in signals {
            state = c.observe(&state, *s);
        }
        state
    }

    // ── Additive increase ──────────────────────────────────────────

    #[test]
    fn steps_up_only_after_threshold() {
        let state = run(&[Signal::TooMuch, Signal::TooMuch], 2);
        assert_eq!(state.level, 2);
        assert_eq!(state.up_streak, 2);
    }

    #[test]
    fn steps_up_at_threshold() {
        let state = run(&[Signal::TooMuch, Signal::TooMuch, Signal::TooMuch], 2);
        assert_eq!(state.level, 3);
        assert_eq!(state.up_streak, 0);
    }

    #[test]
    fn neutral_breaks_the_streak() {
        let state = run(
            &[
                Signal::TooMuch,
                Signal::TooMuch,
                Signal::Neutral,
                Signal::TooMuch,
            ],
            2,
        );
        assert_eq!(state.level, 2);
        assert_eq!(state.up_streak, 1);
    }

    #[test]
    fn does_not_exceed_ceiling() {
        let state = run(&vec![Signal::TooMuch; 30], 4);
        assert_eq!(state.level, 4);
    }

    // ── Multiplicative decrease ────────────────────────────────────

    #[test]
    fn too_little_backs_off_immediately() {
        let state = run(&[Signal::TooLittle], 3);
        assert_eq!(state.level, 2);
        assert_eq!(state.cooldown, 5);
    }

    #[test]
    fn does_not_go_below_floor() {
        let state = run(&vec![Signal::TooLittle; 10], 2);
        assert_eq!(state.level, 1);
    }

    #[test]
    fn cooldown_suppresses_reescalation() {
        let c = ctrl();
        let mut state = ControllerState {
            level: 3,
            up_streak: 0,
            cooldown: 0,
        };
        // Back off → level 2, cooldown 5
        state = c.observe(&state, Signal::TooLittle);
        assert_eq!(state.level, 2);
        // 3 TOO_MUCH signals — would normally step up, but cooldown active
        for _ in 0..3 {
            state = c.observe(&state, Signal::TooMuch);
        }
        assert_eq!(state.level, 2);
    }

    #[test]
    fn reescalates_after_cooldown_expires() {
        let c = ctrl();
        let mut state = ControllerState {
            level: 2,
            up_streak: 0,
            cooldown: 5,
        };
        // 5 neutral turns drain the cooldown
        for _ in 0..5 {
            state = c.observe(&state, Signal::Neutral);
        }
        assert_eq!(state.cooldown, 0);
        // Then sustained pressure steps up
        for _ in 0..3 {
            state = c.observe(&state, Signal::TooMuch);
        }
        assert_eq!(state.level, 3);
    }

    // ── Persistence ────────────────────────────────────────────────

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrl.json");
        let state = ControllerState {
            level: 3,
            up_streak: 2,
            cooldown: 1,
        };
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path, 2, 1, 4);
        assert_eq!(loaded.level, 3);
        assert_eq!(loaded.up_streak, 2);
    }

    #[test]
    fn missing_uses_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let state = load_state(&path, 2, 1, 4);
        assert_eq!(state.level, 2);
    }

    #[test]
    fn corrupt_uses_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{broken").unwrap();
        let state = load_state(&path, 3, 1, 4);
        assert_eq!(state.level, 3);
    }

    #[test]
    fn loaded_level_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctrl.json");
        let state = ControllerState {
            level: 9,
            up_streak: 0,
            cooldown: 0,
        };
        save_state(&path, &state).unwrap();
        let loaded = load_state(&path, 2, 1, 4);
        assert_eq!(loaded.level, 4);
    }
}
