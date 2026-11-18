//! Status texts (docs/AUTO-CONTROLS.md 4.2), made in Rust so tests can check them. A1: the exposure loop's texts.
//! Phase A2 adds the white balance texts and the hand-back texts (`ignored`, `no_response`, `not_answering`,
//! `hunting`, `conflict`).

use super::{CapReason, ExposureState};

/// An exposure of `2^e` seconds as text: `1/32 s`, `1/2 s`, `1 s`, `2 s`.
pub fn exposure_time(e: i32) -> String {
    if e < 0 {
        format!("1/{} s", 1u64 << (-e).min(40))
    } else {
        format!("{} s", 1u64 << e.min(40))
    }
}

/// Everything the exposure texts read.
pub struct ExposureFacts {
    pub state: ExposureState,
    pub exposure: i32,
    pub gain: Option<i32>,
    pub max_gain: Option<i32>,
    pub e_min: i32,
    pub gain_min: i32,
    pub cap: i32,
    pub cap_reason: CapReason,
    pub fps: f64,
    pub target_fps: f64,
    /// The rate the camera dropped to when the learned cap was set.
    pub dropped_to: Option<f64>,
}

/// The text of 4.2 for the state, `None` when the state has none of its own.
pub fn exposure_message(f: &ExposureFacts) -> Option<String> {
    match f.state {
        ExposureState::Starting | ExposureState::Adjusting => Some("Adjusting…".to_string()),
        ExposureState::Holding => {
            if f.cap_reason == CapReason::Learned {
                let fps = f.dropped_to.unwrap_or(f.fps).round();
                return Some(format!(
                    "The camera dropped to {fps:.0} fps at {}, so Better Cam keeps exposure at {} or shorter.",
                    exposure_time(f.cap + 1),
                    exposure_time(f.cap)
                ));
            }
            let fps = f.fps.round();
            Some(match (f.gain, f.max_gain) {
                (Some(g), Some(m)) => format!("Holding {fps:.0} fps · gain {g} of {m}"),
                _ => format!("Holding {fps:.0} fps · {}", exposure_time(f.exposure)),
            })
        }
        ExposureState::LimitGain => Some(format!(
            "Gain at its maximum ({}). Add light, allow more gain, or allow a longer exposure.",
            f.max_gain.unwrap_or_default()
        )),
        ExposureState::LimitExposure => Some(format!(
            "Exposure at the {:.0} fps limit and this camera has no gain. Add light or allow a longer exposure.",
            f.target_fps.round()
        )),
        ExposureState::TooBright => Some(format!(
            "Too bright: exposure at its shortest ({}) and gain at {}.",
            exposure_time(f.e_min),
            f.gain_min
        )),
        ExposureState::NoSignal => {
            Some("Nothing to measure (lens covered?). Brightening slowly.".to_string())
        }
        ExposureState::Paused => Some("Paused.".to_string()),
        ExposureState::Closed => Some("Starts when the camera opens.".to_string()),
        // A2: the watchdog's texts. `no_picture` has none before 10 s.
        ExposureState::NoPicture
        | ExposureState::Conflict
        | ExposureState::Ignored
        | ExposureState::NoResponse
        | ExposureState::NotAnswering
        | ExposureState::Hunting => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_times() {
        assert_eq!(exposure_time(-5), "1/32 s");
        assert_eq!(exposure_time(-13), "1/8192 s");
        assert_eq!(exposure_time(-1), "1/2 s");
        assert_eq!(exposure_time(0), "1 s");
        assert_eq!(exposure_time(2), "4 s");
    }

    fn facts(state: ExposureState) -> ExposureFacts {
        ExposureFacts {
            state,
            exposure: -5,
            gain: Some(48),
            max_gain: Some(75),
            e_min: -13,
            gain_min: 0,
            cap: -5,
            cap_reason: CapReason::Fps,
            fps: 30.0,
            target_fps: 30.0,
            dropped_to: None,
        }
    }

    #[test]
    fn texts_of_section_4_2() {
        let t = |s| exposure_message(&facts(s)).unwrap();
        assert_eq!(t(ExposureState::Holding), "Holding 30 fps · gain 48 of 75");
        assert_eq!(
            t(ExposureState::LimitGain),
            "Gain at its maximum (75). Add light, allow more gain, or allow a longer exposure."
        );
        assert_eq!(
            t(ExposureState::TooBright),
            "Too bright: exposure at its shortest (1/8192 s) and gain at 0."
        );
        let mut f = facts(ExposureState::Holding);
        f.gain = None;
        f.max_gain = None;
        assert_eq!(exposure_message(&f).unwrap(), "Holding 30 fps · 1/32 s");
        let mut f = facts(ExposureState::Holding);
        f.cap = -6;
        f.cap_reason = CapReason::Learned;
        f.dropped_to = Some(24.2);
        assert_eq!(
            exposure_message(&f).unwrap(),
            "The camera dropped to 24 fps at 1/32 s, so Better Cam keeps exposure at 1/64 s or shorter."
        );
    }
}
