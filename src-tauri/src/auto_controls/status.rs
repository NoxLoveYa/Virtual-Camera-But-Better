//! Status texts (docs/AUTO-CONTROLS.md 4.2), made in Rust so tests can check them: the exposure loop's (A1), the
//! white balance loop's and the watchdog's hand-back texts (A2), the "not offered" reasons of 2.3 and the hints of
//! 2.9.
//!
//! The buttons of 4.2 (`[Resume]`, `[Try again]`) are not part of a text: the card draws them from the state.

use super::meter::Reason;
use super::watchdog::Fault;
use super::{CapReason, ExposureState, HandBackKind, NoOffer, WhiteBalanceState};
use crate::capture::controls::ControlKey;

/// A colour temperature as text: `4650 K`.
pub fn kelvin(k: i32) -> String {
    format!("{k} K")
}

/// The text of a watchdog fault (4.2: `ignored`, `no_response`, `not_answering`, `hunting`, `conflict`; 2.9: too
/// slow). `to` is where the control went; `None` for a conflict.
pub fn fault_message(fault: Fault, key: ControlKey, to: Option<HandBackKind>) -> String {
    let label = key.label();
    let back = match to {
        Some(HandBackKind::Camera) => "Back to Camera auto.",
        Some(HandBackKind::Manual) => "Back to its earlier value.",
        None => "",
    };
    match fault {
        Fault::NotAnswering => {
            let went = match to {
                Some(HandBackKind::Manual) => "its earlier value",
                _ => "Camera auto",
            };
            format!("The camera stopped answering. {label} went back to {went}.")
        }
        Fault::TooSlow => "This camera is too slow to adjust live.".to_string(),
        Fault::Ignored => format!("The camera ignored Better Cam auto for {label}. {back}"),
        Fault::NoResponse => {
            format!("The picture does not change when Better Cam changes {label}. {back}")
        }
        Fault::Hunting => format!("{label} kept swinging. {back}"),
        Fault::Conflict => format!("Another program keeps changing {label}. Better Cam auto paused."),
    }
}

/// Why Better Cam auto is not offered (2.3), for the tooltip of the disabled mode.
pub fn not_offered(key: ControlKey, why: NoOffer) -> String {
    let what = match key {
        ControlKey::WhiteBalance => "white balance",
        _ => "exposure",
    };
    match why {
        NoOffer::NoManual => format!("This camera has no manual {what}."),
        NoOffer::UnknownUnits => format!("This camera reports {what} in units Better Cam does not know."),
        NoOffer::Blocked => "This camera ignored Better Cam auto.".to_string(),
    }
}

/// The hint under the exposure row of a camera without a usable gain (2.9).
pub const WHOLE_STOPS_HINT: &str =
    "No gain control: Better Cam auto works in whole stops. Add Auto Adjust for the fine trim.";

/// The hint under the exposure row of a camera that can hold the frame rate itself (2.9).
pub const LOW_LIGHT_HINT: &str =
    "This camera can also hold the frame rate itself: Camera auto with Low light compensation off.";

/// The note of 2.5.10, shown while the exposure is shorter than two periods of the mains light.
pub const STRIPES_NOTE: &str = "Short exposures under mains light can show stripes.";

/// Whether an exposure of `2^e` s is short enough to show stripes under `mains` Hz light (2.5.10).
pub fn stripes(e: i32, mains: Option<u32>) -> bool {
    mains.is_some_and(|hz| matches!(hz, 50 | 60) && f64::from(e) < (1.0 / (2.0 * f64::from(hz))).log2())
}

/// Everything the white balance texts read.
pub struct WhiteBalanceFacts {
    pub state: WhiteBalanceState,
    pub kelvin: i32,
    /// `true` when the limit is the warm one (`K_min`): the light is warmer than the camera can correct.
    pub warm_limit: bool,
    /// How long there has been no picture, in seconds.
    pub no_picture_for: f64,
    /// Why the picture is held or followed slowly: picks the text of `NoSignal` (`Dark` or `OneColour`) and of `Slow`.
    pub reason: Reason,
    pub fault: Option<Fault>,
    pub handed_back: Option<HandBackKind>,
}

/// The text of 4.2 for a white balance state, `None` when the state has none of its own.
pub fn white_balance_message(f: &WhiteBalanceFacts) -> Option<String> {
    if let Some(fault) = f.fault {
        return Some(fault_message(fault, ControlKey::WhiteBalance, f.handed_back));
    }
    match f.state {
        WhiteBalanceState::Starting | WhiteBalanceState::Adjusting => Some("Adjusting…".to_string()),
        WhiteBalanceState::Holding => Some(format!("Holding · {}", kelvin(f.kelvin))),
        // The card adds the line about Auto Adjust under it (it knows whether the filter is in the chain).
        WhiteBalanceState::AtLimit => Some(format!(
            "White balance at its {} ({}): the light is {} than this camera can correct.",
            if f.warm_limit { "warmest" } else { "coolest" },
            kelvin(f.kelvin),
            if f.warm_limit { "warmer" } else { "cooler" }
        )),
        // "Too dark" only when it was the dark: a hold with any other reason (`TwoAnswers` among them, when the
        // doubtful samples were too few for the slow lane) is a picture that is bright but has nothing to measure.
        WhiteBalanceState::NoSignal if f.reason == Reason::Dark => {
            Some(format!("Nothing to measure (too dark). Keeping {}.", kelvin(f.kelvin)))
        }
        WhiteBalanceState::NoSignal => Some(format!(
            "Mostly one colour (a face or a wall): nothing neutral to measure. Staying at {}; Warmth still applies.",
            kelvin(f.kelvin)
        )),
        WhiteBalanceState::Slow => Some(format!(
            "Unsure of the light (a bright face or two lights): adjusting slowly · {}",
            kelvin(f.kelvin)
        )),
        WhiteBalanceState::NoPicture => (f.no_picture_for > super::watchdog::NO_PICTURE_TEXT_AFTER)
            .then(|| "No picture to measure.".to_string()),
        WhiteBalanceState::Paused => Some("Paused.".to_string()),
        WhiteBalanceState::Closed => Some("Starts when the camera opens.".to_string()),
        WhiteBalanceState::Conflict
        | WhiteBalanceState::Ignored
        | WhiteBalanceState::NoResponse
        | WhiteBalanceState::NotAnswering
        | WhiteBalanceState::Hunting => None,
    }
}

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
    /// The camera has a gain control but Most gain is at its minimum: the loop works in whole stops on purpose.
    pub gain_off: bool,
    pub cap: i32,
    pub cap_reason: CapReason,
    /// The loop goes past the frame-rate limit for light (2.5.12): the frame rate is lowered on purpose.
    pub lowered: bool,
    /// ... and the exposure is on its way past the frame-rate limit now (not a gain move while lowered).
    pub lowering: bool,
    pub fps: f64,
    pub target_fps: f64,
    /// The rate the camera dropped to when the learned cap was set.
    pub dropped_to: Option<f64>,
    /// The backlight term is on (10.7): the median backlight term of the readings is at least `BACKLIT_ON`, with
    /// hysteresis.
    pub backlit: bool,
    /// The front light term is on (11.8, 13.11): the loop's held reading is at least `FRONTLIT_ON` (0.5 stop) under
    /// its plain one, with hysteresis (off again from 0.25 stop). The flag only says so; `hold_note` decides what the
    /// text shows, and `backlit` goes first.
    pub frontlit: bool,
    /// Why the loop stopped, and where the control went.
    pub fault: Option<Fault>,
    pub handed_back: Option<HandBackKind>,
    /// How long there has been no picture, in seconds.
    pub no_picture_for: f64,
}

/// 10.7: the end of a holding text while the backlight term is on: why the window is white.
pub const BACKLIGHT_NOTE: &str = " · backlight: exposing for the middle";

/// 11.8: the end of a holding text while the front light term is on: why the face is not white and the room is dark.
pub const FRONTLIT_NOTE: &str = " · dark room: exposing for the lit middle";

/// The suffix of a holding text: at most one, 10's first.
fn hold_note(f: &ExposureFacts) -> &'static str {
    if f.backlit {
        BACKLIGHT_NOTE
    } else if f.frontlit {
        FRONTLIT_NOTE
    } else {
        ""
    }
}

/// The text of 4.2 for the state, `None` when the state has none of its own.
pub fn exposure_message(f: &ExposureFacts) -> Option<String> {
    if let Some(fault) = f.fault {
        return Some(fault_message(fault, ControlKey::Exposure, f.handed_back));
    }
    match f.state {
        ExposureState::Starting | ExposureState::Adjusting if f.lowering => {
            Some("Lowering frame rate for light…".to_string())
        }
        ExposureState::Starting | ExposureState::Adjusting => Some("Adjusting…".to_string()),
        ExposureState::Holding if f.lowered => {
            let fps = f.fps.round();
            let note = hold_note(f);
            Some(match (f.gain, f.max_gain) {
                (Some(g), Some(m)) => {
                    format!("Lowered frame rate for light: holding {fps:.0} fps · gain {g} of {m}{note}")
                }
                _ => format!(
                    "Lowered frame rate for light: holding {fps:.0} fps · {}{note}",
                    exposure_time(f.exposure)
                ),
            })
        }
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
            let note = hold_note(f);
            Some(match (f.gain, f.max_gain) {
                (Some(g), Some(m)) => format!("Holding {fps:.0} fps · gain {g} of {m}{note}"),
                _ if f.gain_off => format!("Holding {fps:.0} fps · {} · gain off{note}", exposure_time(f.exposure)),
                _ => format!("Holding {fps:.0} fps · {}{note}", exposure_time(f.exposure)),
            })
        }
        ExposureState::LimitGain if f.lowered => Some(format!(
            "Gain at its maximum ({}) and exposure at its longest ({}, {:.0} fps). Add light or allow a longer exposure.",
            f.max_gain.unwrap_or_default(),
            exposure_time(f.exposure),
            f.fps.round()
        )),
        ExposureState::LimitGain => Some(format!(
            "Gain at its maximum ({}). Add light, allow more gain, or allow a longer exposure.",
            f.max_gain.unwrap_or_default()
        )),
        ExposureState::LimitExposure if f.lowered => Some(format!(
            "Exposure at its longest ({}, {:.0} fps) and {}. Add light or allow a longer exposure.",
            exposure_time(f.exposure),
            f.fps.round(),
            if f.gain_off { "gain is off" } else { "this camera has no gain" }
        )),
        ExposureState::LimitExposure if f.gain_off => Some(format!(
            "Exposure at the {:.0} fps limit and gain is off. Add light, allow more gain, or allow a longer exposure.",
            f.target_fps.round()
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
        // `no_picture` has no text before 10 s.
        ExposureState::NoPicture => (f.no_picture_for > super::watchdog::NO_PICTURE_TEXT_AFTER)
            .then(|| "No picture to measure.".to_string()),
        // The watchdog's states carry their text in `fault`.
        ExposureState::Conflict
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
            gain_off: false,
            cap: -5,
            cap_reason: CapReason::Fps,
            lowered: false,
            lowering: false,
            fps: 30.0,
            target_fps: 30.0,
            dropped_to: None,
            backlit: false,
            frontlit: false,
            fault: None,
            handed_back: None,
            no_picture_for: 0.0,
        }
    }

    #[test]
    fn a_backlit_hold_says_why_the_window_is_white() {
        let say = |f: &dyn Fn(&mut ExposureFacts)| {
            let mut x = facts(ExposureState::Holding);
            x.backlit = true;
            f(&mut x);
            exposure_message(&x).unwrap()
        };
        let note = " · backlight: exposing for the middle";
        assert_eq!(BACKLIGHT_NOTE, note);
        assert_eq!(say(&|_| {}), format!("Holding 30 fps · gain 48 of 75{note}"));
        assert_eq!(
            say(&|x| {
                x.gain = None;
                x.max_gain = None;
            }),
            format!("Holding 30 fps · 1/32 s{note}")
        );
        assert_eq!(
            say(&|x| x.lowered = true),
            format!("Lowered frame rate for light: holding 30 fps · gain 48 of 75{note}")
        );
        // Not backlit: no suffix; and no suffix on a text that is not a hold.
        assert_eq!(exposure_message(&facts(ExposureState::Holding)).unwrap(), "Holding 30 fps · gain 48 of 75");
        let mut x = facts(ExposureState::LimitGain);
        x.backlit = true;
        assert!(!exposure_message(&x).unwrap().contains("backlight"));
    }

    #[test]
    fn a_front_lit_hold_says_the_room_is_dark() {
        let note = " · dark room: exposing for the lit middle";
        assert_eq!(FRONTLIT_NOTE, note);
        let say = |f: &dyn Fn(&mut ExposureFacts)| {
            let mut x = facts(ExposureState::Holding);
            x.frontlit = true;
            f(&mut x);
            exposure_message(&x).unwrap()
        };
        assert_eq!(say(&|_| {}), format!("Holding 30 fps · gain 48 of 75{note}"));
        assert_eq!(
            say(&|x| {
                x.gain = None;
                x.max_gain = None;
            }),
            format!("Holding 30 fps · 1/32 s{note}")
        );
        assert_eq!(
            say(&|x| x.lowered = true),
            format!("Lowered frame rate for light: holding 30 fps · gain 48 of 75{note}")
        );
        // At most one suffix, and 10's wins.
        assert_eq!(
            say(&|x| x.backlit = true),
            format!("Holding 30 fps · gain 48 of 75{BACKLIGHT_NOTE}")
        );
        // None on a text that is not a hold, none when the flag is off.
        let mut x = facts(ExposureState::LimitGain);
        x.frontlit = true;
        assert!(!exposure_message(&x).unwrap().contains("dark room"));
        assert_eq!(exposure_message(&facts(ExposureState::Holding)).unwrap(), "Holding 30 fps · gain 48 of 75");
    }

    #[test]
    fn every_text_of_section_4_2() {
        let t = |s| exposure_message(&facts(s));
        assert_eq!(t(ExposureState::Starting).as_deref(), Some("Adjusting…"));
        assert_eq!(t(ExposureState::Adjusting).as_deref(), Some("Adjusting…"));
        assert_eq!(
            t(ExposureState::LimitExposure).as_deref(),
            Some("Exposure at the 30 fps limit and this camera has no gain. Add light or allow a longer exposure.")
        );
        assert_eq!(
            t(ExposureState::NoSignal).as_deref(),
            Some("Nothing to measure (lens covered?). Brightening slowly.")
        );
        assert_eq!(t(ExposureState::Paused).as_deref(), Some("Paused."));
        assert_eq!(t(ExposureState::Closed).as_deref(), Some("Starts when the camera opens."));
        // `no_picture` says nothing before 10 s, then one text.
        let mut f = facts(ExposureState::NoPicture);
        f.no_picture_for = 9.0;
        assert_eq!(exposure_message(&f), None);
        f.no_picture_for = 10.5;
        assert_eq!(exposure_message(&f).as_deref(), Some("No picture to measure."));
        assert_eq!(STRIPES_NOTE, "Short exposures under mains light can show stripes.");
    }

    #[test]
    fn gain_off_has_its_own_words() {
        // Most gain at its minimum on a camera that has a gain: not "no gain".
        let mut f = facts(ExposureState::LimitExposure);
        f.gain = None;
        f.max_gain = None;
        f.gain_off = true;
        assert_eq!(
            exposure_message(&f).as_deref(),
            Some("Exposure at the 30 fps limit and gain is off. Add light, allow more gain, or allow a longer exposure.")
        );
        let mut f = facts(ExposureState::Holding);
        f.gain = None;
        f.max_gain = None;
        f.gain_off = true;
        assert_eq!(exposure_message(&f).as_deref(), Some("Holding 30 fps · 1/32 s · gain off"));
    }

    #[test]
    fn a_lowered_frame_rate_says_so() {
        let t = |s, f: &dyn Fn(&mut ExposureFacts)| {
            let mut x = facts(s);
            x.lowered = true;
            x.exposure = -4;
            x.fps = 15.9;
            f(&mut x);
            exposure_message(&x)
        };
        // Only while the exposure goes past the frame-rate limit; a gain move at the lowered rate is plain adjusting.
        assert_eq!(t(ExposureState::Adjusting, &|_| {}).as_deref(), Some("Adjusting…"));
        assert_eq!(t(ExposureState::Starting, &|_| {}).as_deref(), Some("Adjusting…"));
        assert_eq!(
            t(ExposureState::Adjusting, &|x| x.lowering = true).as_deref(),
            Some("Lowering frame rate for light…")
        );
        assert_eq!(
            t(ExposureState::Holding, &|_| {}).as_deref(),
            Some("Lowered frame rate for light: holding 16 fps · gain 48 of 75")
        );
        assert_eq!(
            t(ExposureState::Holding, &|x| {
                x.gain = None;
                x.max_gain = None;
            })
            .as_deref(),
            Some("Lowered frame rate for light: holding 16 fps · 1/16 s")
        );
        assert_eq!(
            t(ExposureState::LimitGain, &|_| {}).as_deref(),
            Some("Gain at its maximum (75) and exposure at its longest (1/16 s, 16 fps). Add light or allow a longer exposure.")
        );
        assert_eq!(
            t(ExposureState::LimitExposure, &|x| x.gain_off = true).as_deref(),
            Some("Exposure at its longest (1/16 s, 16 fps) and gain is off. Add light or allow a longer exposure.")
        );
    }

    #[test]
    fn fault_texts_of_section_4_2() {
        let x = ControlKey::Exposure;
        let cam = Some(HandBackKind::Camera);
        assert_eq!(
            fault_message(Fault::Conflict, x, None),
            "Another program keeps changing Exposure. Better Cam auto paused."
        );
        assert_eq!(
            fault_message(Fault::Ignored, x, cam),
            "The camera ignored Better Cam auto for Exposure. Back to Camera auto."
        );
        assert_eq!(
            fault_message(Fault::NoResponse, x, cam),
            "The picture does not change when Better Cam changes Exposure. Back to Camera auto."
        );
        assert_eq!(
            fault_message(Fault::NotAnswering, x, cam),
            "The camera stopped answering. Exposure went back to Camera auto."
        );
        assert_eq!(fault_message(Fault::Hunting, x, cam), "Exposure kept swinging. Back to Camera auto.");
        assert_eq!(fault_message(Fault::TooSlow, x, cam), "This camera is too slow to adjust live.");
        // A camera without Camera auto goes back to the earlier manual value.
        let man = Some(HandBackKind::Manual);
        assert_eq!(
            fault_message(Fault::Hunting, ControlKey::WhiteBalance, man),
            "White balance kept swinging. Back to its earlier value."
        );
        assert_eq!(
            fault_message(Fault::NotAnswering, ControlKey::WhiteBalance, man),
            "The camera stopped answering. White balance went back to its earlier value."
        );
        // The fault wins over the state.
        let mut f = facts(ExposureState::NotAnswering);
        f.fault = Some(Fault::NotAnswering);
        f.handed_back = cam;
        assert_eq!(
            exposure_message(&f).as_deref(),
            Some("The camera stopped answering. Exposure went back to Camera auto.")
        );
    }

    fn wb(state: WhiteBalanceState) -> WhiteBalanceFacts {
        WhiteBalanceFacts {
            state,
            kelvin: 4650,
            warm_limit: true,
            no_picture_for: 0.0,
            reason: Reason::Dark,
            fault: None,
            handed_back: None,
        }
    }

    #[test]
    fn white_balance_texts_of_section_4_2() {
        let t = |s| white_balance_message(&wb(s));
        assert_eq!(t(WhiteBalanceState::Starting).as_deref(), Some("Adjusting…"));
        assert_eq!(t(WhiteBalanceState::Adjusting).as_deref(), Some("Adjusting…"));
        assert_eq!(t(WhiteBalanceState::Holding).as_deref(), Some("Holding · 4650 K"));
        assert_eq!(t(WhiteBalanceState::Paused).as_deref(), Some("Paused."));
        assert_eq!(t(WhiteBalanceState::Closed).as_deref(), Some("Starts when the camera opens."));
        assert_eq!(
            t(WhiteBalanceState::NoSignal).as_deref(),
            Some("Nothing to measure (too dark). Keeping 4650 K.")
        );
        let mut f = wb(WhiteBalanceState::NoSignal);
        f.reason = Reason::OneColour;
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("Mostly one colour (a face or a wall): nothing neutral to measure. Staying at 4650 K; Warmth still applies.")
        );
        // A hold whose newest reason is two answers is not "too dark" (a bright face with too few doubtful samples).
        let mut f = wb(WhiteBalanceState::NoSignal);
        f.reason = Reason::TwoAnswers;
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("Mostly one colour (a face or a wall): nothing neutral to measure. Staying at 4650 K; Warmth still applies.")
        );
        let mut f = wb(WhiteBalanceState::Slow);
        f.reason = Reason::TwoAnswers;
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("Unsure of the light (a bright face or two lights): adjusting slowly · 4650 K")
        );
        let mut f = wb(WhiteBalanceState::AtLimit);
        f.kelvin = 2800;
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("White balance at its warmest (2800 K): the light is warmer than this camera can correct.")
        );
        f.kelvin = 6500;
        f.warm_limit = false;
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("White balance at its coolest (6500 K): the light is cooler than this camera can correct.")
        );
        let mut f = wb(WhiteBalanceState::NoPicture);
        f.no_picture_for = 12.0;
        assert_eq!(white_balance_message(&f).as_deref(), Some("No picture to measure."));
        f.no_picture_for = 3.0;
        assert_eq!(white_balance_message(&f), None);
        let mut f = wb(WhiteBalanceState::Ignored);
        f.fault = Some(Fault::Ignored);
        f.handed_back = Some(HandBackKind::Camera);
        assert_eq!(
            white_balance_message(&f).as_deref(),
            Some("The camera ignored Better Cam auto for White balance. Back to Camera auto.")
        );
    }

    #[test]
    fn reasons_and_hints_of_section_2_3_and_2_9() {
        let x = ControlKey::Exposure;
        assert_eq!(not_offered(x, NoOffer::NoManual), "This camera has no manual exposure.");
        assert_eq!(
            not_offered(x, NoOffer::UnknownUnits),
            "This camera reports exposure in units Better Cam does not know."
        );
        assert_eq!(not_offered(x, NoOffer::Blocked), "This camera ignored Better Cam auto.");
        let w = ControlKey::WhiteBalance;
        assert_eq!(not_offered(w, NoOffer::NoManual), "This camera has no manual white balance.");
        assert_eq!(
            not_offered(w, NoOffer::UnknownUnits),
            "This camera reports white balance in units Better Cam does not know."
        );
        assert_eq!(
            WHOLE_STOPS_HINT,
            "No gain control: Better Cam auto works in whole stops. Add Auto Adjust for the fine trim."
        );
        assert_eq!(
            LOW_LIGHT_HINT,
            "This camera can also hold the frame rate itself: Camera auto with Low light compensation off."
        );
    }

    #[test]
    fn stripes_note_applies_to_short_exposures_under_mains() {
        // 1/100 s is 2^-6.64: -7 and shorter. 1/120 s is 2^-6.9.
        assert!(stripes(-7, Some(50)) && stripes(-7, Some(60)));
        assert!(!stripes(-6, Some(50)) && !stripes(-6, Some(60)));
        assert!(!stripes(-9, None) && !stripes(-9, Some(0)));
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
