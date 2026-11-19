//! The cases of docs/AUTO-CONTROLS.md 6.2 against the simulated camera: A1's (1-14, 23, 24) and A2's (15-22, the
//! white balance loop and the watchdog), the `support()` outcomes and a few cases of the watchdog the table does not
//! list (hunting, no picture, a hand-back to a manual value, a write that never returns).
//!
//! Times are simulated seconds. "Within the band" is judged by the oracle (`Sim::true_error`: the landed state of
//! the camera under the scripted light, no noise), never by what the controller believes.

use super::sim::*;
use super::*;

fn resume(e: i32, g: i32) -> Resume {
    Resume {
        exposure: Some(e),
        gain: Some(g),
        white_balance: None,
    }
}

fn engage_with(sim: &Sim, cfg: AutoCfg, resume: Resume, seeded: bool) -> AutoController {
    if seeded {
        // The hub wrote the resume point before the loop starts: the camera must hold it.
        assert_eq!(
            (Some(sim.device().0), sim.cfg.gain.then_some(sim.device().1)),
            (resume.exposure, resume.gain.filter(|_| sim.cfg.gain)),
            "the sim does not start at the resume point"
        );
    }
    AutoController::new(
        &sim.caps(),
        cfg,
        Learned::default(),
        Modes {
            exposure: Mode::App,
            resume,
            seeded,
            ..Modes::default()
        },
    )
}

fn engage(sim: &Sim, resume: Resume, seeded: bool) -> AutoController {
    engage_with(sim, AutoCfg::default(), resume, seeded)
}

/// Run in 100 ms steps and log the state after each.
fn run_states(sim: &mut Sim, ctrl: &mut AutoController, until: f64) -> Vec<(f64, ExposureState)> {
    let mut v = Vec::new();
    while sim.t < until - 1e-9 {
        sim.run(ctrl, (sim.t + 0.1).min(until));
        v.push((sim.t, ctrl.status().exposure.expect("engaged").state));
    }
    v
}

/// The earliest tick time from which the true error stays within `band` up to `until`.
fn settled_at(sim: &Sim, from: f64, until: f64, band: f64) -> Option<f64> {
    let mut at = None;
    for s in sim.history.iter().filter(|s| s.t >= from && s.t <= until) {
        if s.err.abs() <= band {
            at.get_or_insert(s.t);
        } else {
            at = None;
        }
    }
    at
}

fn errs(sim: &Sim, from: f64, until: f64) -> impl Iterator<Item = f64> + '_ {
    sim.history
        .iter()
        .filter(move |s| s.t >= from && s.t <= until)
        .map(|s| s.err)
}

/// One decision of the controller: the writes returned by one tick and the change of total light they make.
#[derive(Debug)]
struct Batch {
    t: f64,
    dx: f64,
    keys: Vec<ControlKey>,
    values: Vec<i32>,
}

fn batches(sim: &Sim, start: (i32, i32)) -> Vec<Batch> {
    let (mut e, mut g) = start;
    let mut out: Vec<Batch> = Vec::new();
    let mut x_before = f64::from(e) + sim.true_gs(g);
    for &(t, key, value) in &sim.issued {
        let new = out.last().is_none_or(|b| b.t != t);
        if new {
            x_before = f64::from(e) + sim.true_gs(g);
            out.push(Batch {
                t,
                dx: 0.0,
                keys: Vec::new(),
                values: Vec::new(),
            });
        }
        match key {
            ControlKey::Exposure => e = value,
            _ => g = value,
        }
        let b = out.last_mut().expect("pushed");
        b.keys.push(key);
        b.values.push(value);
        b.dx = f64::from(e) + sim.true_gs(g) - x_before;
    }
    out
}

fn reversals(b: &[Batch]) -> usize {
    let signs: Vec<f64> = b.iter().map(|b| b.dx).filter(|d| d.abs() > 1e-9).collect();
    signs.windows(2).filter(|w| w[0].signum() != w[1].signum()).count()
}

fn state(ctrl: &AutoController) -> ExposureState {
    ctrl.status().exposure.expect("engaged").state
}

// ---------------------------------------------------------------------------------------------------------------

#[test]
fn case_01_dim_room_30fps() {
    // Needs E = -5 and 1.5 stops of gain; cold start E = -6, G = 0 (the first write is the slow one).
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 10.0);
    let at = settled_at(&sim, 0.0, 10.0, 0.33).expect("never settled");
    assert!(at <= 3.0, "within the band only at {at}");
    assert!(sim.max_e_applied <= -5, "exposure went to {}", sim.max_e_applied);
    let first = sim.writes[0].done;
    let mut t = first;
    while t + 1.0 <= 10.0 {
        let fps = sim.fps_between(t, t + 1.0);
        assert!(fps >= 29.5, "{fps} fps in [{t}, {}]", t + 1.0);
        t += 1.0;
    }
    assert!(sim.writes.len() <= 15, "{} writes", sim.writes.len());
    let n = sim.writes.len();
    sim.run(&mut ctrl, 70.0);
    assert_eq!(sim.writes.len(), n, "writes in the next 60 s");
    assert_eq!(state(&ctrl), ExposureState::Holding);
    assert!(settled_at(&sim, 10.0, 70.0, 0.33).is_some());
}

#[test]
fn case_02_light_steps() {
    let base = (3.5f64).exp2(); // need -3.5
    let light: Light = Box::new(move |t| if (10.0..20.0).contains(&t) { base * 4.0 } else { base });
    let cfg = SimCfg {
        start_e: -5,
        start_g: 50,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-5, 50), true);
    sim.run(&mut ctrl, 30.0);
    // Light on: +2 stops, too bright, err -2.
    let on = settled_at(&sim, 10.0, 19.9, 0.33).expect("no settle after light on");
    assert!(on - 10.0 <= 2.0, "light on settled after {}", on - 10.0);
    let over = errs(&sim, 10.0, 19.9).fold(f64::MIN, f64::max);
    assert!(over <= 0.5, "overshoot {over}");
    // Light off: -2 stops, too dark.
    let off = settled_at(&sim, 20.0, 30.0, 0.33).expect("no settle after light off");
    assert!(off - 20.0 <= 2.0, "light off settled after {}", off - 20.0);
    let under = errs(&sim, 20.0, 30.0).fold(f64::MAX, f64::min);
    assert!(under >= -0.5, "overshoot {under}");
}

/// "Longest exposure" set to the frame-rate limit of 30 fps (1/32 s): the frame rate is never lowered (2.5.12).
fn at_the_frame_rate() -> AutoCfg {
    let mut cfg = AutoCfg::default();
    cfg.exposure.longest = Longest::Stop(-5);
    cfg
}

#[test]
fn case_03_darker_than_the_limits_allow() {
    // The scene needs -1.0; the limits give E = -5 and Most gain 75 = 2.25 stops: -2.75 at most.
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage_with(&sim, at_the_frame_rate(), resume(-6, 0), false);
    let states = run_states(&mut sim, &mut ctrl, 12.0);
    let (limit_at, _) = states
        .iter()
        .find(|(_, s)| *s == ExposureState::LimitGain)
        .expect("never limit_gain");
    assert!(*limit_at <= 5.0, "limit_gain at {limit_at}");
    assert_eq!(sim.device(), (-5, 75));
    assert!(
        states.iter().filter(|(t, _)| t >= limit_at).all(|(_, s)| *s == ExposureState::LimitGain),
        "left limit_gain"
    );
    assert!(sim.writes_between(*limit_at, 12.0).is_empty(), "writes after the limit");
    assert!(sim.max_e_applied <= -5);
    assert!(sim.fps_between(sim.writes[0].done, 12.0) >= 29.5);
    assert_eq!(
        ctrl.status().exposure.unwrap().message.as_deref(),
        Some("Gain at its maximum (75). Add light, allow more gain, or allow a longer exposure.")
    );
}

#[test]
fn most_gain_off_drives_the_gain_to_its_minimum() {
    // F1: the slider says Off, so the device gain must follow (it stayed at its last driven value, 75), and the
    // status must not claim the camera has no gain.
    let cfg = SimCfg {
        start_e: -5,
        start_g: 0,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-1.0));
    let mut ctrl = engage_with(&sim, at_the_frame_rate(), resume(-5, 0), true);
    sim.run(&mut ctrl, 8.0);
    assert_eq!(sim.device(), (-5, 75), "the gain did not climb to Most gain first");
    assert_eq!(state(&ctrl), ExposureState::LimitGain);
    // The user drags Most gain to its minimum.
    let mut cfg = at_the_frame_rate();
    cfg.exposure.max_gain = Some(0);
    ctrl.set_cfg(cfg);
    let off_at = sim.t;
    sim.run(&mut ctrl, off_at + 8.0);
    assert_eq!(sim.device().1, 0, "device gain stays at {}", sim.device().1);
    let gain_writes: Vec<_> = sim
        .writes_between(off_at, off_at + 8.0)
        .into_iter()
        .filter(|w| w.key == ControlKey::Gain)
        .collect();
    assert_eq!(gain_writes.len(), 1, "{gain_writes:?}");
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::LimitExposure);
    assert_eq!(st.max_gain, None);
    assert_eq!(
        st.message.as_deref(),
        Some("Exposure at the 30 fps limit and gain is off. Add light, allow more gain, or allow a longer exposure.")
    );
    // Quiet afterwards: no further writes.
    let n = sim.writes.len();
    sim.run(&mut ctrl, off_at + 16.0);
    assert_eq!(sim.writes.len(), n);
}

#[test]
fn most_gain_off_from_the_start_never_raises_the_gain() {
    let mut cfg = AutoCfg::default();
    cfg.exposure.max_gain = Some(0);
    let sim_cfg = SimCfg {
        start_e: -5,
        start_g: 40,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(sim_cfg, dim(-1.0));
    // A resume point from the days Most gain was 75.
    let mut ctrl = engage_with(&sim, cfg, resume(-5, 40), true);
    sim.run(&mut ctrl, 8.0);
    assert_eq!(sim.device().1, 0);
    assert!(sim.writes.iter().filter(|w| w.key == ControlKey::Gain).all(|w| w.value == 0));
}

#[test]
fn the_resume_point_is_stored_after_three_seconds_at_a_limit() {
    // F2: a dark room never reaches `holding`; each reopen climbed from the seed again.
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let (mut limit_at, mut stored_at) = (None, None);
    while sim.t < 20.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        let st = ctrl.status();
        if limit_at.is_none() && st.exposure.as_ref().unwrap().state == ExposureState::LimitGain {
            limit_at = Some(sim.t);
        }
        if stored_at.is_none() && st.resume.is_some() {
            stored_at = Some(sim.t);
            assert_eq!(
                st.resume,
                // The limit of this room is the lowered frame rate's: one stop past 1/32 s (2.5.12).
                Some(Resume {
                    exposure: Some(-4),
                    gain: Some(75),
                    white_balance: None
                })
            );
            assert_eq!(st.exposure.unwrap().state, ExposureState::LimitGain);
        }
    }
    let (limit_at, stored_at) = (limit_at.expect("no limit_gain"), stored_at.expect("resume point never stored"));
    assert!(
        (3.0..=3.4).contains(&(stored_at - limit_at)),
        "limit_gain at {limit_at}, stored at {stored_at}"
    );
    // It rides an emitted status (the runner hands it to the store).
    assert!(sim.emitted.iter().any(|s| s.resume.is_some()));
    // A reopen with it starts at the limit: no climb, the seed is the stored point.
    let sim2 = Sim::new(
        SimCfg {
            start_e: -4,
            start_g: 75,
            ..SimCfg::default()
        },
        dim(-1.0),
    );
    let ctrl2 = engage(&sim2, resume(-4, 75), true);
    assert_eq!(ctrl2.seeds(30.0).exposure, Some((-4, Some(75))));
}

#[test]
fn the_resume_point_at_a_limit_is_stored_at_most_every_ten_seconds() {
    // The point is stored once for the stay; a stay that ends and begins again within 10 s waits for the 10 s.
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let mut firsts = Vec::new();
    let mut had = false;
    while sim.t < 30.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        let has = ctrl.status().resume.is_some();
        if has && !had {
            firsts.push(sim.t);
        }
        had = has;
    }
    assert_eq!(firsts.len(), 1, "stored again at {firsts:?}");
}

#[test]
fn case_04_bright_scene() {
    // Needs E = -11 with gain at its minimum.
    let mut sim = Sim::new(SimCfg::default(), dim(-11.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 20.0);
    assert!(settled_at(&sim, 0.0, 20.0, 0.33).is_some(), "not settled: {:?}", sim.history.last());
    let b = batches(&sim, (-6, 0));
    let biggest = b.iter().map(|b| b.dx.abs()).fold(0.0, f64::max);
    assert!(biggest <= 1.0 + 0.05, "a decision moved {biggest} stops");
    let (e, g) = sim.device();
    assert_eq!(e, -11, "exposure {e}, gain {g}");
    // A downward crossover lands the gain 0.25 stop (8 units) above its minimum, and it only falls to 0 when the
    // need sits exactly on a stop: "gain at G_min" means within that landing offset.
    assert!(g <= 8, "gain {g} at E {e}");
}

#[test]
fn case_05_mains_flicker() {
    let cfg = SimCfg {
        flicker: Some((0.3, 100.0)),
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 10.0);
    assert!(settled_at(&sim, 0.0, 10.0, 0.33).is_some());
    sim.run(&mut ctrl, 70.0);
    let after = sim.writes_between(10.0, 70.0);
    assert!(after.len() <= 12, "{} writes in 60 s", after.len());
    assert!(
        !after.iter().any(|w| w.key == ControlKey::Exposure),
        "an exposure write after convergence"
    );
    let b: Vec<Batch> = batches(&sim, (-6, 0)).into_iter().filter(|b| b.t >= 10.0).collect();
    assert!(reversals(&b) <= 1, "reversals: {b:?}");
}

fn converge_check(cfg: SimCfg) {
    let cfg = SimCfg { start_e: -5, ..cfg };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut ctrl = engage(&sim, resume(-5, 0), true);
    sim.run(&mut ctrl, 15.0);
    let at = settled_at(&sim, 0.0, 15.0, 0.33).expect("never settled");
    assert!(at <= 5.0, "settled at {at}");
    let b = batches(&sim, (-5, 0));
    assert!(reversals(&b) <= 2, "reversals {}: {b:?}", reversals(&b));
}

#[test]
fn case_06_gain_curve_mismatch() {
    converge_check(SimCfg {
        s_true: 1.5,
        ..SimCfg::default()
    });
    converge_check(SimCfg {
        s_true: 5.0,
        ..SimCfg::default()
    });
    converge_check(SimCfg {
        bent: true,
        ..SimCfg::default()
    });
}

fn learn_check(s_true: f64) {
    // The light alternates between two levels the gain alone can cover (E stays at -5).
    let cfg = SimCfg {
        s_true,
        start_e: -5,
        ..SimCfg::default()
    };
    let light: Light = Box::new(|t| {
        let need: f64 = if ((t / 5.0) as u32).is_multiple_of(2) { -4.0 } else { -4.8 };
        (-need).exp2()
    });
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-5, 0), true);
    sim.run(&mut ctrl, 120.0);
    let s = ctrl.learned().gain_stops.expect("nothing learned");
    assert!(
        (s - s_true).abs() <= 0.3 * s_true,
        "learned S {s} for a true {s_true}"
    );
    let gain_moves = batches(&sim, (-5, 0))
        .iter()
        .filter(|b| b.keys == [ControlKey::Gain])
        .count();
    assert!(gain_moves >= 20, "only {gain_moves} gain moves");
    assert!(!sim.writes.iter().any(|w| w.key == ControlKey::Exposure));
}

#[test]
fn case_06_learned_gain_curve() {
    learn_check(1.5);
    learn_check(5.0);
}

#[test]
fn case_07_whole_stops_camera() {
    let cfg = SimCfg {
        gain: false,
        start_e: -8,
        ..SimCfg::default()
    };
    // E = -5 is 0.4 stop short of the scene's need.
    let mut sim = Sim::new(cfg, dim(-5.4));
    let mut ctrl = engage(&sim, resume(-8, 0), false);
    sim.run(&mut ctrl, 20.0);
    assert!(settled_at(&sim, 0.0, 20.0, 0.6).is_some(), "{:?}", sim.history.last());
    assert_eq!(sim.device().0, -5);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::Holding);
    assert_eq!(st.gain, None);
    assert_eq!(st.message.as_deref(), Some("Holding 30 fps · 1/32 s"));
    assert!(!sim.writes.iter().any(|w| w.key == ControlKey::Gain));
    assert!(sim.max_e_applied <= -5);
}

#[test]
fn case_08_rate_drop_learns_a_cap() {
    let cfg = SimCfg {
        rate_drop: true,
        start_e: -5,
        start_g: 33,
        ..SimCfg::default()
    };
    // Converged already (need -4.0 = E -5 and 1 stop of gain): the guard is the only thing that acts.
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut ctrl = engage(&sim, resume(-5, 33), true);
    let mut learned_at = None;
    while sim.t < 14.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if learned_at.is_none() && ctrl.learned().rate_caps.contains_key("30") {
            learned_at = Some(sim.t);
        }
    }
    let at = learned_at.expect("no cap learned");
    assert!(at <= 4.0, "cap learned at {at}");
    assert_eq!(ctrl.learned().rate_caps.get("30"), Some(&-6));
    let fps = sim.fps_between(8.0, 14.0);
    assert!(fps >= 0.95 * 30.0, "{fps} fps after the cap");
    assert_eq!(sim.device().0, -6);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!((st.cap, st.cap_reason), (-6, CapReason::Learned));
    assert_eq!(
        st.message.as_deref().map(|m| m.starts_with("The camera dropped to ")),
        Some(true),
        "{:?}",
        st.message
    );
    assert!(settled_at(&sim, 8.0, 14.0, 0.33).is_some());
}

/// Live on W4GS at 60 fps (A6): 1/64 s gives 55 fps, 92 % of the request, so the guard must trigger above 90 %.
#[test]
fn case_08b_rate_a_little_low_still_learns_a_cap() {
    let cfg = SimCfg {
        fps: 60.0,
        rate_drop: true,
        rate_drop_fill: 0.86,
        start_e: -6,
        start_g: 33,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-5.0));
    let mut ctrl = engage(&sim, resume(-6, 33), true);
    let mut learned_at = None;
    while sim.t < 14.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if learned_at.is_none() && ctrl.learned().rate_caps.contains_key("60") {
            learned_at = Some(sim.t);
        }
    }
    let fps_before = sim.fps_between(0.5, 3.0);
    assert!((0.9 * 60.0..0.95 * 60.0).contains(&fps_before), "{fps_before} fps before the cap");
    let at = learned_at.expect("no cap learned at 55 fps");
    assert!(at <= 5.0, "cap learned at {at}");
    assert_eq!(ctrl.learned().rate_caps.get("60"), Some(&-7));
    assert!(sim.fps_between(8.0, 14.0) >= 59.0);
}

#[test]
fn case_09_rate_low_for_another_reason() {
    let cfg = SimCfg {
        fps_cap: Some(20.0),
        start_e: -5,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut ctrl = engage(&sim, resume(-5, 0), true);
    let (mut present, mut sets) = (false, 0);
    while sim.t < 40.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        let now = ctrl.learned().rate_caps.contains_key("30");
        if now && !present {
            sets += 1;
        }
        present = now;
    }
    assert_eq!(sets, 1, "the cap was tried {sets} times");
    assert!(ctrl.learned().rate_caps.is_empty(), "the cap stayed");
    let exposure_writes = sim.writes.iter().filter(|w| w.key == ControlKey::Exposure).count();
    assert_eq!(exposure_writes, 1, "only the one constraint move: {:?}", sim.writes);
    assert!(settled_at(&sim, 20.0, 40.0, 0.33).is_some());
}

#[test]
fn case_10_lens_covered() {
    let base = (5.5f64).exp2(); // need -5.5
    let light: Light = Box::new(move |t| if (15.0..25.0).contains(&t) { 0.0 } else { base });
    let cfg = SimCfg {
        start_g: 16,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-6, 16), true);
    sim.run(&mut ctrl, 14.9);
    assert!(settled_at(&sim, 0.0, 14.9, 0.33).is_some());
    let states = run_states(&mut sim, &mut ctrl, 25.0);
    assert!(
        sim.writes_between(15.0, 16.5).is_empty(),
        "moved while covered for less than 1.5 s: {:?}",
        sim.writes_between(15.0, 16.5)
    );
    let drift = sim.writes_between(16.5, 25.0);
    assert!(!drift.is_empty(), "never brightened");
    let b: Vec<Batch> = batches(&sim, (-6, 16)).into_iter().filter(|b| b.t >= 15.0 && b.t < 25.0).collect();
    for w in b.windows(2) {
        assert!(w[1].t - w[0].t >= 0.95, "moves {} s apart", w[1].t - w[0].t);
    }
    assert!(b.iter().all(|b| b.dx > 0.0 && b.dx <= 1.05), "{b:?}");
    assert!(states.iter().any(|(_, s)| *s == ExposureState::NoSignal));
    assert_eq!(sim.device(), (-5, 75), "ends at the upper limits");
    // Uncovered.
    sim.run(&mut ctrl, 40.0);
    let at = settled_at(&sim, 25.0, 40.0, 0.33).expect("no recovery");
    assert!(at - 25.0 <= 3.0, "recovered after {}", at - 25.0);
}

#[test]
fn case_11_flashlight() {
    let base = (3.5f64).exp2();
    let light: Light = Box::new(move |t| if (15.0..25.0).contains(&t) { base * 1024.0 } else { base });
    let cfg = SimCfg {
        start_e: -5,
        start_g: 50,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-5, 50), true);
    sim.run(&mut ctrl, 35.0);
    let b = batches(&sim, (-5, 50));
    let mut checked = 0;
    for batch in b.iter().filter(|b| b.t >= 15.2 && b.t < 25.0) {
        let snap = sim
            .history
            .iter()
            .rev()
            .find(|s| s.t <= batch.t - 0.25)
            .expect("history");
        if snap.clip >= 0.6 && snap.e > -13 {
            assert!(
                (-1.05..=-0.95).contains(&batch.dx),
                "at {} (clip {}): moved {}",
                batch.t,
                snap.clip,
                batch.dx
            );
            checked += 1;
        }
    }
    assert!(checked >= 8, "only {checked} decisions under a clipped picture");
    let at25 = sim.history.iter().find(|s| s.t >= 24.9).unwrap();
    assert!(at25.clip < 0.5 || (at25.e, at25.g) == (-13, 0), "{at25:?}");
    assert!(
        settled_at(&sim, 25.0, 35.0, 0.33).is_some(),
        "no recovery: {:?} {:?} {:?}",
        ctrl.status().exposure,
        sim.handbacks,
        sim.history.last()
    );
}

#[test]
fn case_12_slow_ramp() {
    let cfg = SimCfg {
        start_e: -9,
        start_g: 20,
        jitter: 0.1,
        ..SimCfg::default()
    };
    // The scene needs -8.4, then -5.4 over 30 s from t = 5: three crossovers, and the end sits 0.65 stop inside the
    // last one.
    let light: Light = Box::new(|t| {
        let need: f64 = -8.4 + 3.0 * ((t - 5.0) / 30.0).clamp(0.0, 1.0);
        (-need).exp2()
    });
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-9, 20), true);
    sim.run(&mut ctrl, 45.0);
    let exposure: Vec<i32> = sim
        .writes
        .iter()
        .filter(|w| w.key == ControlKey::Exposure)
        .map(|w| w.value)
        .collect();
    assert_eq!(exposure, vec![-8, -7, -6], "exposure writes");
    // Not counted: the 1-3 frames of a crossover between its two writes (2.5.6), a dip by design.
    let in_a_move = |t: f64| sim.writes.iter().any(|w| w.started <= t && t < w.done + 0.3);
    let worst = sim
        .history
        .iter()
        .filter(|s| s.t >= 8.0 && !in_a_move(s.t))
        .max_by(|a, b| a.err.abs().total_cmp(&b.err.abs()))
        .unwrap();
    assert!(worst.err.abs() <= 0.6, "off target: {worst:?}");
}

#[test]
fn case_13_crossover_order() {
    // Brighter crossover: exposure up one stop, gain down a little: the gain write (darker) comes first.
    let cfg = SimCfg {
        start_e: -7,
        start_g: 40,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.5));
    let mut ctrl = engage(&sim, resume(-7, 40), true);
    sim.run(&mut ctrl, 3.0);
    let b = batches(&sim, (-7, 40));
    let first = &b[0];
    assert_eq!(first.keys, vec![ControlKey::Gain, ControlKey::Exposure], "{first:?}");
    assert_eq!(first.values[1], -6);
    assert!(first.values[0] < 40);
    // Darker crossover: exposure down, gain up: the exposure write comes first.
    let cfg = SimCfg {
        start_e: -6,
        start_g: 7,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-7.5));
    let mut ctrl = engage(&sim, resume(-6, 7), true);
    sim.run(&mut ctrl, 3.0);
    let b = batches(&sim, (-6, 7));
    let first = &b[0];
    assert_eq!(first.keys, vec![ControlKey::Exposure, ControlKey::Gain], "{first:?}");
    assert!(first.values[0] < -6 && first.values[1] > 7);
}

#[test]
fn case_14_format_change_30_to_60() {
    let cfg = SimCfg {
        start_e: -5,
        start_g: 33,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut ctrl = engage(&sim, resume(-5, 33), true);
    sim.run(&mut ctrl, 10.0);
    assert!(sim.writes.is_empty(), "writes before the change: {:?}", sim.writes);
    sim.cfg.fps = 60.0;
    sim.run(&mut ctrl, 15.0);
    let w = sim.writes_between(10.0, 15.0);
    let b = batches(&sim, (-5, 33));
    let first = &b[0];
    assert!(first.t <= 10.5, "first decision at {}", first.t);
    assert_eq!(first.keys, vec![ControlKey::Exposure, ControlKey::Gain]);
    assert_eq!(first.values[0], -6);
    let dgain = sim.true_gs(first.values[1]) - sim.true_gs(33);
    assert!((dgain - 1.0).abs() <= 0.15, "gain rose {dgain} stops");
    assert!(w.len() <= 2, "{} writes: {w:?}", w.len());
    let fps = sim.fps_between(12.0, 15.0);
    assert!(fps >= 59.0, "{fps} fps after the change");
    assert_eq!(state(&ctrl), ExposureState::Holding);
}

#[test]
fn case_23_pause_mid_adjustment() {
    let base = (3.5f64).exp2();
    // One stop more light while paused.
    let light: Light = Box::new(move |t| if (3.0..).contains(&t) { base * 2.0 } else { base });
    let mut sim = Sim::new(SimCfg::default(), light);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 1.5);
    assert!(!sim.writes.is_empty(), "nothing to interrupt");
    sim.pause();
    let issued_before = sim.issued.len();
    sim.run(&mut ctrl, 6.5);
    // Whatever was in flight finished; nothing new was started while paused.
    let late: Vec<_> = sim.issued[issued_before..].iter().filter(|(t, _, _)| *t > 1.7).collect();
    assert!(late.is_empty(), "writes issued while paused: {late:?}");
    assert_eq!(state(&ctrl), ExposureState::Paused);
    sim.resume();
    // Two stale samples captured before the resume, claiming the picture is far too dark.
    let stale = |stats: Stats| MeterSample {
        token: 1,
        capture_qpc: 0,
        stats,
    };
    let dark = Stats {
        cells: vec![[0.02, 0.02, 0.02, 1.0]; 256],
    };
    ctrl.on_sample(&stale(dark.clone()), 6.0);
    ctrl.on_sample(&stale(dark), 6.03);
    let n = sim.issued.len();
    sim.run(&mut ctrl, 6.7);
    assert_eq!(sim.issued.len(), n, "decided on samples from before the resume");
    sim.run(&mut ctrl, 14.0);
    let first = sim.issued[n..].first().expect("no write after the resume");
    assert!(first.0 >= 6.5 + 0.2, "first decision at {}", first.0);
    assert!(settled_at(&sim, 6.5, 14.0, 0.33).is_some_and(|t| t <= 11.0), "{:?}", sim.history.last());
}

#[test]
fn case_24_engagement_from_a_stale_read() {
    // The camera reports exposure -2 with Camera auto on: a stale value. The seed comes from the device default
    // (-6), never from the read.
    let cfg = SimCfg {
        stale_exposure: Some(-2),
        start_e: -9,
        ..SimCfg::default()
    };
    let sim = Sim::new(cfg.clone(), dim(-3.5));
    let mut ctrl = AutoController::new(
        &sim.caps(),
        AutoCfg::default(),
        Learned::default(),
        Modes {
            exposure: Mode::App,
            ..Modes::default()
        },
    );
    ctrl.on_readback(&sim.caps()[0], 0.0);
    let actions = ctrl.tick(0.0, &sim.facts());
    assert_eq!(
        actions[..2],
        [
            Action::Write {
                key: ControlKey::Exposure,
                value: -6
            },
            Action::Write {
                key: ControlKey::Gain,
                value: 0
            }
        ]
    );
    // The resume point wins over the default.
    let mut ctrl = engage(&sim, resume(-7, 20), false);
    let actions = ctrl.tick(0.0, &sim.facts());
    assert_eq!(
        actions[..2],
        [
            Action::Write {
                key: ControlKey::Exposure,
                value: -7
            },
            Action::Write {
                key: ControlKey::Gain,
                value: 20
            }
        ]
    );
    // A default longer than the frame limit is cut to it.
    let mut caps = sim.caps();
    caps[0].default = -2;
    let mut ctrl = AutoController::new(
        &caps,
        AutoCfg::default(),
        Learned::default(),
        Modes {
            exposure: Mode::App,
            ..Modes::default()
        },
    );
    let actions = ctrl.tick(0.0, &sim.facts());
    assert_eq!(
        actions[0],
        Action::Write {
            key: ControlKey::Exposure,
            value: -5
        }
    );
    // The whole run: the first write is the exposure's manual write, then the loop converges.
    let mut sim = Sim::new(cfg, dim(-3.5));
    let mut ctrl = engage(&sim, Resume::default(), false);
    sim.run(&mut ctrl, 10.0);
    assert_eq!((sim.issued[0].1, sim.issued[0].2), (ControlKey::Exposure, -6));
    assert!(settled_at(&sim, 0.0, 10.0, 0.33).is_some_and(|t| t <= 4.0), "{:?}", sim.history.last());
}

// ---------------------------------------------------------------------------------------------------------------
// Phase A2: support, white balance (cases 15-17), the watchdog (cases 18-22)

fn set(caps: &mut [ControlInfo], key: ControlKey) -> &mut ControlInfo {
    caps.iter_mut().find(|c| c.key == key).expect("control in the set")
}

fn without(caps: &[ControlInfo], key: ControlKey) -> Vec<ControlInfo> {
    caps.iter().filter(|c| c.key != key).cloned().collect()
}

#[test]
fn support_lists_every_outcome() {
    use NoOffer::*;
    use Offer::*;
    let none = Learned::default();
    let both = |caps: &[ControlInfo], l: &Learned| {
        let s = AutoController::support(caps, l);
        (s.exposure, s.white_balance)
    };
    let w4gs = w4gs();
    // W4GS: exposure -13..-1 step 1, gain 0..100, white balance 2800..6500.
    assert_eq!(both(&w4gs, &none), (Full, Full));
    // The camera's own sim agrees.
    let sim = Sim::new(
        SimCfg {
            wb: Some(WbSim::default()),
            ..SimCfg::default()
        },
        dim(-3.5),
    );
    assert_eq!(both(&sim.caps(), &none), (Full, Full));
    // No gain control, or gain that is not manual, or gain too coarse to trim with: whole stops.
    assert_eq!(both(&without(&w4gs, ControlKey::Gain), &none), (Stops, Full));
    let mut caps = w4gs.clone();
    set(&mut caps, ControlKey::Gain).can_manual = false;
    assert_eq!(both(&caps, &none), (Stops, Full));
    let mut caps = w4gs.clone();
    set(&mut caps, ControlKey::Gain).max = 3;
    assert_eq!(both(&caps, &none), (Stops, Full));
    // Exposure in units that are not log2 seconds.
    let mut caps = w4gs.clone();
    let e = set(&mut caps, ControlKey::Exposure);
    (e.min, e.max) = (0, 10_000);
    assert_eq!(both(&caps, &none), (No(UnknownUnits), Full));
    let mut caps = w4gs.clone();
    set(&mut caps, ControlKey::Exposure).step = 10;
    assert_eq!(both(&caps, &none).0, No(UnknownUnits));
    // White balance in units that are not Kelvin.
    let mut caps = w4gs.clone();
    let w = set(&mut caps, ControlKey::WhiteBalance);
    (w.min, w.max) = (0, 100);
    assert_eq!(both(&caps, &none), (Full, No(UnknownUnits)));
    let mut caps = w4gs.clone();
    let w = set(&mut caps, ControlKey::WhiteBalance);
    (w.min, w.max) = (4000, 4500);
    assert_eq!(both(&caps, &none).1, No(UnknownUnits), "Kelvin but a range of 500");
    // No manual values.
    let mut caps = w4gs.clone();
    set(&mut caps, ControlKey::Exposure).can_manual = false;
    set(&mut caps, ControlKey::WhiteBalance).can_manual = false;
    assert_eq!(both(&caps, &none), (No(NoManual), No(NoManual)));
    // Absent.
    let caps = without(&without(&w4gs, ControlKey::Exposure), ControlKey::WhiteBalance);
    assert_eq!(both(&caps, &none), (No(NoManual), No(NoManual)));
    // Blocked by the watchdog, each loop on its own.
    let exposure_blocked = Learned {
        blocked: vec![ControlKey::Exposure],
        ..Learned::default()
    };
    assert_eq!(both(&w4gs, &exposure_blocked), (No(Blocked), Full));
    let wb_blocked = Learned {
        blocked: vec![ControlKey::WhiteBalance],
        ..Learned::default()
    };
    assert_eq!(both(&w4gs, &wb_blocked), (Full, No(Blocked)));
    // The reasons the card shows.
    let s = AutoController::support(&w4gs, &exposure_blocked);
    assert_eq!(
        s.exposure.why(ControlKey::Exposure).as_deref(),
        Some("This camera ignored Better Cam auto.")
    );
    assert_eq!(s.white_balance.why(ControlKey::WhiteBalance), None);
    assert!(Full.offered() && Stops.offered() && !No(Blocked).offered());
}

#[test]
fn a_loop_is_not_built_for_a_control_that_is_not_offered() {
    let mut caps = w4gs();
    let w = set(&mut caps, ControlKey::WhiteBalance);
    (w.min, w.max) = (0, 100);
    let mut ctrl = AutoController::new(
        &caps,
        AutoCfg::default(),
        Learned::default(),
        Modes {
            white_balance: Mode::App,
            ..Modes::default()
        },
    );
    assert!(!ctrl.wants_meter());
    let facts = Sim::new(SimCfg::default(), dim(-3.5)).facts();
    assert!(ctrl.tick(0.0, &facts).is_empty());
    assert!(ctrl.status().white_balance.is_none());
}

// --- white balance ---

fn wb_sim(alpha_true: f64, start_k: i32, temp: Light) -> Sim {
    wb_sim_in(None, alpha_true, start_k, temp)
}

/// The same in a coloured scene of the kit (`None`: the grey wall).
fn wb_sim_in(scene: Option<&'static str>, alpha_true: f64, start_k: i32, temp: Light) -> Sim {
    let cfg = SimCfg {
        wb: Some(WbSim {
            alpha_true,
            start_k,
            can_auto: true,
        }),
        start_e: -5,
        start_g: 33,
        // These runs start seeded: `apply_on_open` has made the first (slow, 200-700 ms) write already.
        slow_first_write: false,
        scene,
        ..SimCfg::default()
    };
    Sim::new(cfg, dim(-4.0)).with_temperature(temp)
}

fn wb_modes(k: i32, seeded: bool) -> Modes {
    Modes {
        white_balance: Mode::App,
        resume: Resume {
            white_balance: Some(k),
            ..Resume::default()
        },
        seeded,
        ..Modes::default()
    }
}

fn wb_ctrl(sim: &Sim, k: i32, seeded: bool) -> AutoController {
    AutoController::new(&sim.caps(), AutoCfg::default(), Learned::default(), wb_modes(k, seeded))
}

fn wb_state(ctrl: &AutoController) -> WhiteBalanceState {
    ctrl.status().white_balance.expect("engaged").state
}

fn wb_settled_at(sim: &Sim, from: f64, until: f64, band: f64) -> Option<f64> {
    let mut at = None;
    for s in sim.history.iter().filter(|s| s.t >= from && s.t <= until) {
        if s.wb_err.abs() <= band {
            at.get_or_insert(s.t);
        } else {
            at = None;
        }
    }
    at
}

fn wb_writes(sim: &Sim) -> Vec<WriteRec> {
    sim.writes.iter().filter(|w| w.key == ControlKey::WhiteBalance).copied().collect()
}

fn mired(k: i32) -> f64 {
    1e6 / f64::from(k)
}

/// The estimator's constant error on the kit's office scene, in `ln(B/R)` (4 mired: docs/AUTO-WHITE-BALANCE.md 3.5).
const ESTIMATOR_BIAS: f64 = 0.042;

fn wb_step_check_in(scene: Option<&'static str>, alpha_true: f64, to: f64) -> AutoController {
    // 6500 K light, matched; at t = 5 s the light turns to `to` Kelvin.
    let temp: Light = Box::new(move |t| if t < 5.0 { 6500.0 } else { to });
    let mut sim = wb_sim_in(scene, alpha_true, 6500, temp);
    let mut ctrl = wb_ctrl(&sim, 6500, true);
    sim.run(&mut ctrl, 25.0);
    // A scene with colours has the estimator's own bias (the office reads 4 mired too orange, `ESTIMATOR_BIAS`): the
    // loop may settle that before the light changes, by a few mired, and ends within it.
    let bias = if scene.is_some() { ESTIMATOR_BIAS } else { 0.0 };
    let before: Vec<WriteRec> = wb_writes(&sim).into_iter().filter(|w| w.started < 5.0).collect();
    if scene.is_none() {
        assert!(before.is_empty(), "wrote before the light changed");
    } else {
        let moved = before.last().map_or(0.0, |w| (mired(w.value) - mired(6500)).abs());
        assert!(moved <= 12.0, "moved {moved} mired before the light changed: {before:?}");
    }
    let at = wb_settled_at(&sim, 5.0, 25.0, 0.03 + bias)
        .unwrap_or_else(|| panic!("α_true {alpha_true}: never settled: {:?}", sim.history.last()));
    assert!(at - 5.0 <= 6.0, "α_true {alpha_true}: |e_wb| < {} only after {} s", 0.03 + bias, at - 5.0);
    let writes = wb_writes(&sim);
    let mut k = 6500;
    let mut signs = Vec::new();
    for w in &writes {
        let dm = mired(w.value) - mired(k);
        // ΔM_max 20 mired at Normal; a kelvin of rounding is 1.1 mired at 3000 K.
        assert!(dm.abs() <= 20.0 + 1.2, "α_true {alpha_true}: a write moved {dm} mired");
        signs.push(dm.signum());
        k = w.value;
    }
    let reversals = signs.windows(2).filter(|s| s[0] != s[1]).count();
    assert!(reversals <= 2, "α_true {alpha_true}: {reversals} reversals: {writes:?}");
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::Holding);
    ctrl
}

fn case_15(scene: Option<&'static str>) {
    let ctrl = wb_step_check_in(scene, 0.008, 3000.0);
    // The text of 4.2 with the Kelvin the loop holds.
    let st = ctrl.status().white_balance.unwrap();
    assert_eq!(st.message, Some(format!("Holding · {} K", st.kelvin)));
    assert!((2900..=3100).contains(&st.kelvin), "{}", st.kelvin);
    // Learned α within 30 % of the truth, for a camera half as sensitive and one 1.5 times as sensitive. At 0.012 the
    // cast of a 3000 K light is so strong that W3 follows it (B4), and the 4000 K light keeps this case as it was.
    for (alpha_true, to) in [(0.004, 3000.0), (0.012, 4000.0)] {
        let ctrl = wb_step_check_in(scene, alpha_true, to);
        let a = ctrl.learned().wb_alpha.expect("nothing learned");
        assert!((a - alpha_true).abs() <= 0.3 * alpha_true, "learned α {a} for a true {alpha_true}");
    }
}

#[test]
fn case_15_white_balance_step() {
    case_15(None);
}

#[test]
fn case_15_white_balance_step_in_the_office() {
    // B6: the same criteria with colours in the picture.
    case_15(Some("office"));
}

#[test]
fn a_strong_cast_is_followed() {
    // B4, reversed from `a_cast_with_nothing_neutral_to_trust_is_held`: the 3000 K light on a very sensitive camera
    // is a cast of ln(B/R) = -2.15 (blue a tenth of red). The old meter judged its cells before the correction, found
    // everything saturated and held; the shared estimator judges them after it, and the loop walks to the light.
    for scene in [None, Some("office")] {
        let temp: Light = Box::new(|t| if t < 5.0 { 6500.0 } else { 3000.0 });
        let mut sim = wb_sim_in(scene, 0.012, 6500, temp);
        let mut ctrl = wb_ctrl(&sim, 6500, true);
        sim.run(&mut ctrl, 25.0);
        let k = sim.device_k();
        let bias = if scene.is_some() { ESTIMATOR_BIAS } else { 0.0 };
        assert!(
            wb_settled_at(&sim, 5.0, 25.0, 0.03 + bias).is_some() || wb_state(&ctrl) == WhiteBalanceState::AtLimit,
            "{scene:?}: not followed: device {k} K, {:?}",
            sim.history.last()
        );
        assert!((2800..=3300).contains(&k), "{scene:?}: device at {k} K");
        assert!(sim.handbacks.is_empty());
    }
}

fn case_16_warm(scene: Option<&'static str>) {
    // A 2000 K lamp: the camera's warmest setting (2800 K) is still too cool for it.
    let mut sim = wb_sim_in(scene, 0.008, 3500, kelvin(2000.0));
    let mut ctrl = wb_ctrl(&sim, 3500, true);
    let mut limit_at = None;
    while sim.t < 14.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if limit_at.is_none() && wb_state(&ctrl) == WhiteBalanceState::AtLimit {
            limit_at = Some(sim.t);
        }
    }
    let at = limit_at.expect("never at_limit");
    assert!(at <= 8.0, "at_limit at {at}");
    assert_eq!(sim.device_k(), 2800);
    assert!(wb_writes(&sim).iter().all(|w| w.started < at), "writes after at_limit");
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::AtLimit);
    // W3: the text names the end of the range; the card says what finishes the colour.
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("White balance at its warmest (2800 K): the light is warmer than this camera can correct.")
    );
}

#[test]
fn case_16_light_warmer_than_the_camera_can_correct() {
    case_16_warm(None);
}

#[test]
fn case_16_light_warmer_than_the_camera_can_correct_in_the_office() {
    case_16_warm(Some("office"));
}

fn case_16_cool(scene: Option<&'static str>) {
    let mut sim = wb_sim_in(scene, 0.008, 5500, kelvin(12_000.0));
    let mut ctrl = wb_ctrl(&sim, 5500, true);
    sim.run(&mut ctrl, 14.0);
    assert_eq!(sim.device_k(), 6500);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::AtLimit);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("White balance at its coolest (6500 K): the light is cooler than this camera can correct.")
    );
}

#[test]
fn case_16_light_cooler_than_the_camera_can_correct() {
    case_16_cool(None);
}

#[test]
fn case_16_light_cooler_than_the_camera_can_correct_in_the_office() {
    case_16_cool(Some("office"));
}

#[test]
fn case_17_white_balance_waits_for_exposure() {
    case_17(None);
}

#[test]
fn case_17_white_balance_waits_for_exposure_in_the_office() {
    case_17(Some("office"));
}

fn case_17(scene: Option<&'static str>) {
    // One stop brighter and a 3500 K light at t = 8 s: exposure has to move twice; white balance must wait for it.
    let base = (4.0f64).exp2();
    let light: Light = Box::new(move |t| if t < 8.0 { base } else { base * 4.0 });
    let temp: Light = Box::new(|t| if t < 8.0 { 6500.0 } else { 3500.0 });
    let cfg = SimCfg {
        wb: Some(WbSim {
            start_k: 6500,
            ..WbSim::default()
        }),
        start_e: -5,
        start_g: 33,
        scene,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, light).with_temperature(temp);
    let mut ctrl = AutoController::new(
        &sim.caps(),
        AutoCfg::default(),
        Learned::default(),
        Modes {
            exposure: Mode::App,
            white_balance: Mode::App,
            resume: Resume {
                exposure: Some(-5),
                gain: Some(33),
                white_balance: Some(6500),
            },
            seeded: true,
            ..Modes::default()
        },
    );
    sim.run(&mut ctrl, 30.0);
    let wb = wb_writes(&sim);
    assert!(wb.len() >= 5, "white balance never moved: {wb:?}");
    let exposure: Vec<WriteRec> = sim
        .writes
        .iter()
        .filter(|w| matches!(w.key, ControlKey::Exposure | ControlKey::Gain))
        .copied()
        .collect();
    assert!(exposure.iter().any(|w| w.started >= 8.0), "exposure never moved");
    for w in &wb {
        // No white balance write within 0.3 s after an exposure batch, and none while one is in flight.
        for x in &exposure {
            assert!(
                w.started < x.started || w.started >= x.done + 0.3 - 1e-9,
                "white balance write at {} is {} s after the exposure write that returned at {}",
                w.started,
                w.started - x.done,
                x.done
            );
        }
        // None while the exposure is more than a stop off (judged by the true error 0.1 s before the write).
        let before = sim.history.iter().rev().find(|s| s.t <= w.started - 0.1 + 1e-9).expect("history");
        assert!(before.err.abs() <= 1.25, "white balance wrote at {} with the exposure {} stops off", w.started, before.err);
    }
    assert!(settled_at(&sim, 8.0, 30.0, 0.33).is_some_and(|t| t <= 20.0), "{:?}", sim.history.last());
    assert!(wb_settled_at(&sim, 8.0, 30.0, 0.05).is_some_and(|t| t <= 25.0), "{:?}", sim.history.last());
}

#[test]
fn white_balance_warmth_moves_the_target() {
    // Warmth +0.5 asks for an orange picture: the loop settles where the signal is c*(+0.5), not zero.
    let mut sim = wb_sim(0.008, 5000, kelvin(5000.0));
    let mut ctrl = AutoController::new(
        &sim.caps(),
        AutoCfg {
            white_balance: WhiteBalanceCfg {
                warmth: 0.5,
                ..WhiteBalanceCfg::default()
            },
            ..AutoCfg::default()
        },
        Learned::default(),
        wb_modes(5000, true),
    );
    sim.run(&mut ctrl, 20.0);
    assert!(wb_settled_at(&sim, 0.0, 20.0, 0.05).is_some(), "{:?}", sim.history.last());
    // Warmer than the light: a higher Kelvin than the light's (lower mired).
    assert!(sim.device_k() > 5000, "{}", sim.device_k());
    // And as far as the aim says: c* = α (M_set - M_light).
    let want = 1e6 / (1e6 / 5000.0 + meter::warmth_target(0.5) / 0.008);
    assert!((f64::from(sim.device_k()) - want).abs() < 150.0, "{} vs {want}", sim.device_k());
}

#[test]
fn white_balance_holds_on_a_picture_of_one_colour() {
    // Saturated everywhere: nothing neutral to trust, so the loop must not move.
    let mut sim = wb_sim(0.008, 5000, kelvin(6500.0));
    let mut ctrl = wb_ctrl(&sim, 5000, true);
    sim.run(&mut ctrl, 0.5);
    let red = Stats {
        cells: vec![[0.4, 0.02, 0.02, 1.0]; 256],
    };
    let actions = drive(&mut ctrl, &sim, &red, 0.5, 4.0);
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Write { .. })),
        "{actions:?}"
    );
    // W3: the text says why (it is not the dark).
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("Mostly one colour (a face or a wall): nothing neutral to measure. Staying at 5000 K; Warmth still applies.")
    );
}

#[test]
fn white_balance_on_a_black_picture_says_nothing_to_measure() {
    // F4: a picture too dark for colour is not "holding": nothing was checked. The Kelvin is kept, and the text says so.
    let cfg = SimCfg {
        wb: Some(WbSim {
            alpha_true: 0.008,
            start_k: 4600,
            can_auto: true,
        }),
        start_e: -5,
        start_g: 33,
        slow_first_write: false,
        ..SimCfg::default()
    };
    let light: Light = Box::new(|t| if t < 6.0 { (-8.0f64).exp2() } else { 16.0 });
    let mut sim = Sim::new(cfg, light).with_temperature(kelvin(4600.0));
    let mut ctrl = wb_ctrl(&sim, 4600, true);
    sim.run(&mut ctrl, 4.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("Nothing to measure (too dark). Keeping 4600 K.")
    );
    assert!(wb_writes(&sim).is_empty());
    // Light: the loop measures again and holds.
    sim.run(&mut ctrl, 12.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::Holding);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("Holding · 4600 K")
    );
}

/// Drive the controller from `from` for `secs` s on a picture that never changes, as the runner does: a sample every
/// 66 ms, a tick every 100 ms. Returns everything it asked for.
fn drive(ctrl: &mut AutoController, sim: &Sim, stats: &Stats, from: f64, secs: f64) -> Vec<Action> {
    let mut actions = Vec::new();
    for ms in 0..(secs * 1000.0) as u64 {
        let t = from + ms as f64 / 1000.0;
        if ms % 66 == 0 {
            ctrl.on_sample(
                &MeterSample {
                    token: 1,
                    capture_qpc: 0,
                    stats: stats.clone(),
                },
                t,
            );
        }
        if ms % 100 == 0 {
            actions.extend(ctrl.tick(t, &sim.facts()));
        }
    }
    actions
}

#[test]
fn a_lifted_black_says_too_dark_not_holding() {
    // F4's leftover: gain 75 on a black picture is a flat grey around 17 of 255. Flat grey looks like a neutral
    // light, so the loop said "Holding". A picture this dark has nothing to measure.
    let mut sim = wb_sim(0.008, 4600, kelvin(4600.0));
    let mut ctrl = wb_ctrl(&sim, 4600, true);
    sim.run(&mut ctrl, 0.5);
    let lin = meter::srgb_decode(17.0 / 255.0) as f32;
    let grey = Stats {
        cells: vec![[lin, lin, lin, 1.0]; 256],
    };
    let actions = drive(&mut ctrl, &sim, &grey, 0.5, 4.0);
    assert!(!actions.iter().any(|a| matches!(a, Action::Write { .. })), "{actions:?}");
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("Nothing to measure (too dark). Keeping 4600 K.")
    );
}

// --- W3: coloured scenes (docs/AUTO-WHITE-BALANCE.md 5.2, cases B1-B8) ---

/// A coloured scene on a camera with the physical sensitivity (`α_true = 0.0105`, the blackbody slope).
fn kit(scene: &'static str, start_k: i32, temp: Light) -> (Sim, AutoController) {
    let sim = wb_sim_in(Some(scene), 0.0105, start_k, temp);
    let ctrl = wb_ctrl(&sim, start_k, true);
    (sim, ctrl)
}

/// What Auto Adjust makes of the picture the loop leaves it (it measures the loop's output): the angle of a neutral
/// surface to its aim, in degrees, with its gains `0.25 ..= 4`.
fn after_auto_adjust(sim: &Sim) -> f64 {
    let aim = crate::fx::auto_adjust::white_aim(0.0, 0.0);
    let est = crate::fx::awb::estimate_light(&sim.picture());
    let g = est.gains(aim, (0.25, 4.0));
    let n = sim.neutral_now();
    let out = [g[0] * n[0], g[1] * n[1], g[2] * n[2]];
    crate::fx::awb::scenes::angle(out, aim.map(f64::from))
}

/// The true residual of the landed state in mired, + is too orange (what `wb_err` says, in the doc's unit).
fn residual_mired(sim: &Sim) -> f64 {
    -sim.history.last().expect("ran").wb_err / 0.0105
}

fn at_or_holding(ctrl: &AutoController) -> bool {
    matches!(wb_state(ctrl), WhiteBalanceState::Holding | WhiteBalanceState::AtLimit)
}

#[test]
fn b1_the_office_follows_a_light_that_turns_orange() {
    // 6500 K matched, then a 2700 K lamp at 5 s: the device goes to its warm end within 10 s.
    let temp: Light = Box::new(|t| if t < 5.0 { 6500.0 } else { 2700.0 });
    let (mut sim, mut ctrl) = kit("office", 6500, temp);
    sim.run(&mut ctrl, 15.0);
    assert!(sim.device_k() <= 2850, "device at {} K", sim.device_k());
    assert!(at_or_holding(&ctrl), "{:?}", wb_state(&ctrl));
    let a = after_auto_adjust(&sim);
    assert!(a <= 2.0, "{a}° after Auto Adjust");
}

#[test]
fn b2_a_2200_k_light_ends_at_the_limit_and_auto_adjust_finishes_it() {
    let (mut sim, mut ctrl) = kit("office", 4600, kelvin(2200.0));
    sim.run(&mut ctrl, 10.0);
    assert_eq!(sim.device_k(), 2800);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::AtLimit);
    let a = after_auto_adjust(&sim);
    assert!(a <= 2.0, "{a}° after Auto Adjust");
}

#[test]
fn b3_a_warm_light_is_not_held_at_the_seed() {
    // Today these hold at 4600 K (+158 mired): nothing neutral to trust under the old gate.
    for scene in ["warm_room", "grey_room"] {
        let (mut sim, mut ctrl) = kit(scene, 4600, kelvin(2700.0));
        sim.run(&mut ctrl, 10.0);
        assert!(sim.device_k() <= 2900, "{scene}: device at {} K", sim.device_k());
        assert!(!wb_writes(&sim).is_empty());
    }
}

#[test]
fn b5_a_face_or_a_wall_filling_the_frame_is_held_with_its_own_text() {
    for scene in ["face_fill", "red_wall"] {
        let (mut sim, mut ctrl) = kit(scene, 4600, kelvin(5500.0));
        sim.run(&mut ctrl, 10.0);
        assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal, "{scene}");
        assert!(wb_writes(&sim).len() <= 1, "{scene}: {:?}", wb_writes(&sim));
        assert_eq!(
            ctrl.status().white_balance.unwrap().message.as_deref(),
            Some("Mostly one colour (a face or a wall): nothing neutral to measure. Staying at 4600 K; Warmth still applies."),
            "{scene}"
        );
    }
}

#[test]
fn b7_an_office_in_daylight_holds_close_to_neutral() {
    // Today's meter ends 28 mired too blue here (the face pulls the gray world).
    let (mut sim, mut ctrl) = kit("office", 4600, kelvin(5500.0));
    let mut holding_at = None;
    while sim.t < 6.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if holding_at.is_none() && wb_state(&ctrl) == WhiteBalanceState::Holding {
            holding_at = Some(sim.t);
        }
    }
    assert!(holding_at.is_some(), "not holding within 6 s: {:?}", wb_state(&ctrl));
    let r = residual_mired(&sim);
    assert!(r.abs() <= 10.0, "residual {r} mired at {} K", sim.device_k());
}

#[test]
fn b8_noise_does_not_make_the_loop_hunt() {
    // 60 s of holding in an office with sensor noise and a flickering light level: at most one write per 10 s and no
    // reversal pattern. 2700 K sits at the device's warm end; 3000 K is inside its range.
    for (light, seed) in [(2700.0, 3200), (3000.0, 3400)] {
        let cfg = SimCfg {
            wb: Some(WbSim {
                alpha_true: 0.0105,
                start_k: seed,
                can_auto: true,
            }),
            start_e: -5,
            start_g: 33,
            slow_first_write: false,
            scene: Some("office"),
            jitter: 0.1,
            ..SimCfg::default()
        };
        let mut sim = Sim::new(cfg, dim(-4.0)).with_temperature(kelvin(light));
        let mut ctrl = wb_ctrl(&sim, seed, true);
        sim.run(&mut ctrl, 20.0);
        assert!(at_or_holding(&ctrl), "{light} K: {:?}", wb_state(&ctrl));
        let n = wb_writes(&sim).len();
        sim.run(&mut ctrl, 80.0);
        let later: Vec<WriteRec> = wb_writes(&sim).into_iter().skip(n).collect();
        assert!(later.len() <= 6, "{light} K: {} writes in 60 s: {later:?}", later.len());
        let mut k = sim.writes.iter().rev().find(|w| w.started < 20.0 && w.key == ControlKey::WhiteBalance).map_or(seed, |w| w.value);
        let mut signs = Vec::new();
        for w in &later {
            signs.push((mired(w.value) - mired(k)).signum());
            k = w.value;
        }
        let reversals = signs.windows(2).filter(|s| s[0] != s[1]).count();
        assert!(reversals <= 1, "{light} K: {reversals} reversals: {later:?}");
    }
}

/// The recorded room (`fx::awb::real`: a face in a dim room, a bright window, 6 s of frames at the camera's own rate)
/// played to the loop on a camera that starts at the Kelvin the recording had.
fn real_room(speed: Speed, seconds: f64) -> (Sim, AutoController) {
    real_room_with(speed, 0.009, 6261, seconds)
}

fn real_room_with(speed: Speed, alpha_true: f64, start_k: i32, seconds: f64) -> (Sim, AutoController) {
    let cfg = SimCfg {
        wb: Some(WbSim { alpha_true, start_k, can_auto: true }),
        start_e: -5,
        start_g: 33,
        slow_first_write: false,
        replay: Some(Replay::new(crate::fx::awb::real::between(108.0, 116.0))),
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut autocfg = AutoCfg::default();
    autocfg.white_balance.speed = speed;
    let mut ctrl = AutoController::new(&sim.caps(), autocfg, Learned::default(), wb_modes(start_k, true));
    sim.run(&mut ctrl, seconds);
    (sim, ctrl)
}

/// The moves after `settled` seconds (2 mired and more) and the direction changes among them.
fn wb_after(sim: &Sim, start_k: i32, settled: f64) -> (usize, usize) {
    let writes = wb_writes(sim);
    let mut k = writes.iter().rev().find(|w| w.started < settled).map_or(start_k, |w| w.value);
    let mut signs = Vec::new();
    for w in writes.iter().filter(|w| w.started >= settled) {
        let dm = mired(w.value) - mired(k);
        if dm.abs() >= 2.0 {
            signs.push(dm.signum());
        }
        k = w.value;
    }
    (signs.len(), signs.windows(2).filter(|s| s[0] != s[1]).count())
}

#[test]
fn a_face_in_a_dim_room_does_not_make_the_loop_hunt_or_hand_back() {
    // The owner's desk (a W4GS on Better Cam auto white balance, 2026-11-19): 8 s of real frames played forward and
    // back for 3 minutes. For about a second of each pass the estimator has two answers on frames that look alike
    // (the walls, `t` 0, or the face with half of the walls, -0.27 and -0.55), and the loop followed each one, turned
    // around and, after the damping, handed back to Camera auto: Normal at 43 s, Fast at 28 s; Slow wrote 160 times
    // in 3 minutes. Now it reads the doubt (`LightEstimate::apart`), measures with both starts and holds.
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        let (sim, ctrl) = real_room(speed, 180.0);
        let (writes, reversals) = wb_after(&sim, 6261, 20.0);
        assert!(sim.handbacks.is_empty(), "{speed:?}: {:?}", sim.handbacks);
        assert!(at_or_holding(&ctrl), "{speed:?}: {:?}", wb_state(&ctrl));
        assert!(writes <= 1 && reversals == 0, "{speed:?}: {writes} writes, {reversals} reversals after 20 s");
        assert!(wb_writes(&sim).len() <= 3, "{speed:?}: {} writes in all", wb_writes(&sim).len());
        // Where it ended is the room's: neutral to within the band.
        let est = crate::fx::awb::estimate_light(&sim.picture());
        assert!(est.t.abs() <= 0.08, "{speed:?}: {} K, t {:+.3}", sim.device_k(), est.t);
    }
}

#[test]
fn the_real_room_with_a_camera_that_answers_stronger_or_weaker_than_expected_stays_calm() {
    // alpha_true 0.006 / 0.012 against the loop's 0.006 start, and cameras that start far from the light.
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        for (alpha, start) in [(0.006, 6261), (0.012, 6261), (0.009, 5200), (0.009, 6500), (0.009, 4600)] {
            let (sim, ctrl) = real_room_with(speed, alpha, start, 180.0);
            let (writes, reversals) = wb_after(&sim, start, 30.0);
            let what = format!("{speed:?}, alpha {alpha}, from {start} K");
            assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
            assert!(!matches!(wb_state(&ctrl), WhiteBalanceState::Hunting), "{what}");
            // At most three moves a minute once it has settled, and never a run of turns.
            assert!(writes <= 8, "{what}: {writes} writes in 150 s");
            assert!(reversals <= 6, "{what}: {reversals} reversals");
        }
    }
}

#[test]
fn the_white_balance_resume_point_is_stored_after_three_seconds_at_its_limit() {
    // F2 for white balance: a light the camera cannot correct sits in `at_limit`.
    let mut sim = wb_sim(0.008, 3500, kelvin(2000.0));
    let mut ctrl = wb_ctrl(&sim, 3500, true);
    let (mut limit_at, mut stored_at) = (None, None);
    while sim.t < 20.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if limit_at.is_none() && wb_state(&ctrl) == WhiteBalanceState::AtLimit {
            limit_at = Some(sim.t);
        }
        if stored_at.is_none() && ctrl.status().resume.is_some() {
            stored_at = Some(sim.t);
            assert_eq!(ctrl.status().resume.unwrap().white_balance, Some(2800));
        }
    }
    let (limit_at, stored_at) = (limit_at.expect("no at_limit"), stored_at.expect("never stored"));
    assert!(
        (3.0..=3.4).contains(&(stored_at - limit_at)),
        "at_limit at {limit_at}, stored at {stored_at}"
    );
}

// --- warmth that always applies (docs/AUTO-WHITE-BALANCE.md 10.4.3) ---

fn set_warmth(ctrl: &mut AutoController, warmth: f64) {
    let mut cfg = ctrl.cfg;
    cfg.white_balance.warmth = warmth;
    ctrl.set_cfg(cfg);
}

/// The Kelvin the feed-forward writes for a warmth change from `from` to `to` on a camera at `k` (10.4.3).
fn feed_forward_kelvin(k: i32, from: f64, to: f64, learned: &Learned) -> f64 {
    let alpha = learned.wb_alpha.unwrap_or(0.006).max(0.0105);
    1e6 / (mired(k) + (meter::warmth_target(to) - meter::warmth_target(from)) / alpha)
}

/// The white balance writes that started in `from..until`.
fn wb_writes_between(sim: &Sim, from: f64, until: f64) -> Vec<WriteRec> {
    wb_writes(sim).into_iter().filter(|w| w.started >= from && w.started < until).collect()
}

#[test]
fn a_small_warmth_change_moves_the_camera_at_once() {
    // A grey wall under a 5000 K light on a camera at 5000 K, holding: a step of 0.05 is inside the loop's own band
    // (`c*` moves 0.034 against `B_out` 0.06), so before the feed-forward it was swallowed half of the time.
    let mut sim = wb_sim(0.0105, 5000, kelvin(5000.0));
    let mut ctrl = wb_ctrl(&sim, 5000, true);
    sim.run(&mut ctrl, 10.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::Holding);
    let mut warmth = 0.0;
    for to in [0.05, -0.05, 0.1] {
        let (t0, k) = (sim.t, sim.device_k());
        let want = feed_forward_kelvin(k, warmth, to, ctrl.learned());
        set_warmth(&mut ctrl, to);
        sim.run(&mut ctrl, t0 + 0.5);
        let first = *wb_writes_between(&sim, t0, t0 + 0.5).first().unwrap_or_else(|| panic!("{warmth} -> {to}: no write"));
        assert!((f64::from(first.value) - want).abs() <= 1.0, "{warmth} -> {to}: wrote {} K, want {want:.1} K", first.value);
        sim.run(&mut ctrl, t0 + 6.0);
        let err = sim.history.last().expect("ran").wb_err;
        assert!(err.abs() <= 0.03, "{warmth} -> {to}: |wb_err| {err}");
        assert_eq!(wb_state(&ctrl), WhiteBalanceState::Holding, "{warmth} -> {to}");
        // No reversal: whatever the loop adds after the feed-forward goes the same way.
        let mut prev = k;
        let signs: Vec<f64> = wb_writes_between(&sim, t0, t0 + 6.0)
            .iter()
            .map(|w| {
                let dm = mired(w.value) - mired(prev);
                prev = w.value;
                dm.signum()
            })
            .collect();
        assert!(signs.windows(2).all(|s| s[0] == s[1]), "{warmth} -> {to}: {signs:?}");
        warmth = to;
    }
}

#[test]
fn warmth_applies_while_the_picture_is_one_colour() {
    // The red wall holds (nothing neutral to measure): the Warmth setting still moves the camera, once.
    let (mut sim, mut ctrl) = kit("red_wall", 4600, kelvin(5500.0));
    sim.run(&mut ctrl, 10.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    let (t0, k) = (sim.t, sim.device_k());
    let want = feed_forward_kelvin(k, 0.0, 0.5, ctrl.learned());
    set_warmth(&mut ctrl, 0.5);
    sim.run(&mut ctrl, t0 + 0.5);
    let first = wb_writes_between(&sim, t0, t0 + 0.5);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!((f64::from(first[0].value) - want).abs() <= 1.0, "wrote {} K, want {want:.1} K", first[0].value);
    sim.run(&mut ctrl, t0 + 4.5);
    assert_eq!(wb_writes_between(&sim, t0, t0 + 4.5).len(), 1, "{:?}", wb_writes_between(&sim, t0, t0 + 4.5));
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    let st = ctrl.status().white_balance.unwrap();
    assert_eq!(
        st.message,
        Some(format!("Mostly one colour (a face or a wall): nothing neutral to measure. Staying at {} K; Warmth still applies.", st.kelvin))
    );
}

#[test]
fn warmth_applies_while_the_picture_reads_two_ways() {
    // The recorded face that gives two answers for a second or two, played forward and back (the doubt room: from
    // 6500 K the picture reads `TwoAnswers`, from 5200 K it is `OneColour`, see 10.9). The loop is in `slow` at 30 s
    // and the warmth changes there: the move is made all the same. A cooler aim is used: a warmer one would ask for
    // more than the 6500 K the camera has.
    let (mut sim, mut ctrl) = doubt_room(Speed::Normal, 0.009, 6500, 30.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::Slow, "{:?}", ctrl.status().white_balance);
    let k = sim.device_k();
    let want = feed_forward_kelvin(k, 0.0, -0.3, ctrl.learned());
    set_warmth(&mut ctrl, -0.3);
    sim.run(&mut ctrl, 30.5);
    let first = *wb_writes_between(&sim, 30.0, 30.5).first().expect("no write within 0.5 s");
    assert!((f64::from(first.value) - want).abs() <= 1.0, "wrote {} K, want {want:.1} K", first.value);
    // Nothing the lane or the loop writes afterwards takes the move back.
    sim.run(&mut ctrl, 90.0);
    let mut prev = k;
    for w in wb_writes_between(&sim, 30.0, 90.0) {
        assert!(w.value <= prev, "a write that raises K: {prev} -> {} at {:.1} s", w.value, w.started);
        prev = w.value;
    }
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
    assert_ne!(wb_state(&ctrl), WhiteBalanceState::Hunting);
}

/// The doubtful 1.4 s of the recorded desk (two answers on most frames), played forward and back for `seconds`: a
/// doubt that lasts, on a camera of `alpha_true` that starts at `start_k`.
fn doubt_room(speed: Speed, alpha_true: f64, start_k: i32, seconds: f64) -> (Sim, AutoController) {
    let cfg = SimCfg {
        wb: Some(WbSim { alpha_true, start_k, can_auto: true }),
        start_e: -5,
        start_g: 33,
        slow_first_write: false,
        replay: Some(Replay::new(crate::fx::awb::real::between(112.4, 113.8))),
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut autocfg = AutoCfg::default();
    autocfg.white_balance.speed = speed;
    let mut ctrl = AutoController::new(&sim.caps(), autocfg, Learned::default(), wb_modes(start_k, true));
    sim.run(&mut ctrl, seconds);
    (sim, ctrl)
}

#[test]
fn a_face_that_stays_in_doubt_is_followed_slowly() {
    // The doubtful 1.4 s of the desk, on a camera at 6500 K (the picture 0.08-0.24 too orange on the middle of its two
    // answers) and two answers for as long as the face stays: the loop used to hold at 6500 K for good (`no_signal`).
    // Now it says it is unsure and follows the middle of the two answers, slowly, toward a higher mired.
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        let what = format!("{speed:?}");
        let (mut sim, mut ctrl) = doubt_room(speed, 0.009, 6500, 3.0);
        let st = ctrl.status().white_balance.unwrap();
        assert_eq!(st.state, WhiteBalanceState::Slow, "{what}: {st:?}");
        assert_eq!(
            st.message,
            Some(format!("Unsure of the light (a bright face or two lights): adjusting slowly · {} K", st.kelvin)),
            "{what}"
        );
        assert!(wb_writes_between(&sim, 0.0, 5.0).is_empty(), "{what}: {:?}", wb_writes_between(&sim, 0.0, 5.0));
        sim.run(&mut ctrl, 150.0);
        let writes = wb_writes(&sim);
        assert!(!writes.is_empty(), "{what}: the lane never wrote");
        let mut k = 6500;
        for w in &writes {
            assert!(w.value < k, "{what}: a write that raises K: {k} -> {} at {:.1} s", w.value, w.started);
            k = w.value;
        }
        // The middle of the two answers over one more pass of the replay (a single frame flips between the answers).
        let (mut sum, mut n) = (0.0, 0);
        let mut t = 150.0;
        while t < 152.8 {
            t += 0.05;
            sim.run(&mut ctrl, t);
            let r = meter::measure_white_balance(&sim.picture(), 0.0);
            if r.reason == meter::Reason::TwoAnswers {
                sum += r.error;
                n += 1;
            }
        }
        let mean = sum / f64::from(n.max(1));
        assert!(n >= 20 && mean.abs() <= 0.07, "{what}: mean error {mean:+.3} over {n} frames at {} K, {writes:?}", sim.device_k());
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
        assert_ne!(wb_state(&ctrl), WhiteBalanceState::Hunting, "{what}");
    }
}

#[test]
fn a_face_that_stays_in_doubt_at_its_kelvin_stays_close() {
    // The same room on a camera that has the recording's own Kelvin (the walls are neutral): the middle of the two
    // answers sits about 0.12 off, so the slow lane moves a little toward it, a compromise of the two answers, one
    // way and never back. Measured (180 s, the lane working with `α` 0.0105): Slow 5 writes, 8.6 mired; Normal 3,
    // 8.6; Fast 2, 8.0 (steps of 2, 3, 4 mired).
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        let (sim, _) = doubt_room(speed, 0.009, 6261, 180.0);
        let writes = wb_writes(&sim);
        let (mut k, mut total, mut signs) = (6261, 0.0, Vec::new());
        for w in &writes {
            let dm = mired(w.value) - mired(k);
            total += dm.abs();
            signs.push(dm.signum());
            k = w.value;
        }
        assert!(writes.len() <= 5, "{speed:?}: {} writes in 180 s: {writes:?}", writes.len());
        assert!(total <= 10.0, "{speed:?}: {total:.1} mired in all: {writes:?}");
        assert!(signs.windows(2).all(|s| s[0] == s[1]), "{speed:?}: {signs:?}");
        assert!(sim.handbacks.is_empty(), "{speed:?}: {:?}", sim.handbacks);
    }
}

#[test]
fn a_warmth_drag_is_not_a_swing() {
    // The slider dragged back and forth every 1.5 s: each step is the user's move, not the loop's reversal. The camera
    // answers more strongly (0.014) than the feed-forward's 0.0105 assumes, so the loop takes back a third of each
    // move before the next one; without `forget_direction` those take-backs count as a swing and the loop hands back.
    let mut sim = wb_sim(0.014, 5000, kelvin(5000.0));
    let mut ctrl = wb_ctrl(&sim, 5000, true);
    sim.run(&mut ctrl, 10.0);
    let mut never_hunting = true;
    let mut t = sim.t;
    for warmth in [0.3, -0.3, 0.3, -0.3, 0.3, -0.3, 0.3, -0.3, 0.3] {
        set_warmth(&mut ctrl, warmth);
        for _ in 0..15 {
            t += 0.1;
            sim.run(&mut ctrl, t);
            never_hunting &= wb_state(&ctrl) != WhiteBalanceState::Hunting;
        }
    }
    sim.run(&mut ctrl, t + 20.0);
    assert!(never_hunting);
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
    assert_ne!(wb_state(&ctrl), WhiteBalanceState::Hunting);
    let err = sim.history.last().expect("ran").wb_err;
    assert!(err.abs() <= 0.03, "|wb_err| {err} on the last aim");
}

#[test]
fn a_warmth_change_whose_write_failed_is_written_again() {
    // The red wall holds, so only the feed-forward moves the camera. The camera refuses its write once: the change is
    // not lost, the next try writes it.
    let (mut sim, mut ctrl) = kit("red_wall", 4600, kelvin(5500.0));
    sim.run(&mut ctrl, 10.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    let (t0, k) = (sim.t, sim.device_k());
    let want = feed_forward_kelvin(k, 0.0, 0.5, ctrl.learned());
    sim.faults.fail = true;
    set_warmth(&mut ctrl, 0.5);
    sim.run(&mut ctrl, t0 + 0.3);
    assert!(sim.attempts.iter().any(|w| w.key == ControlKey::WhiteBalance && w.started >= t0), "no attempt");
    assert_eq!(sim.device_k(), k, "the failed write must not land");
    sim.faults.fail = false;
    sim.run(&mut ctrl, t0 + 4.0);
    let writes = wb_writes_between(&sim, t0, t0 + 4.0);
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!((f64::from(writes[0].value) - want).abs() <= 1.0, "wrote {} K, want {want:.1} K", writes[0].value);
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
}

#[test]
fn a_warmth_drag_writes_at_most_four_times_a_second_and_loses_nothing() {
    // The slider sends a value every 80 ms for 2 s: the writes are 0.25 s apart at least, and the last one carries
    // what the ones before held back.
    let (mut sim, mut ctrl) = kit("red_wall", 4600, kelvin(5500.0));
    sim.run(&mut ctrl, 10.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    let (t0, k0) = (sim.t, sim.device_k());
    let steps = 25;
    let mut warmth = 0.0;
    for i in 1..=steps {
        warmth = 0.3 * f64::from(i) / f64::from(steps);
        set_warmth(&mut ctrl, warmth);
        sim.run(&mut ctrl, t0 + 0.08 * f64::from(i));
    }
    sim.run(&mut ctrl, t0 + 4.0);
    let writes = wb_writes_between(&sim, t0, t0 + 4.0);
    assert!(!writes.is_empty() && writes.len() <= 9, "{} writes: {writes:?}", writes.len());
    for pair in writes.windows(2) {
        assert!(pair[1].started - pair[0].done >= 0.25 - 1e-9, "{pair:?}");
    }
    // The camera's Kelvin is the whole change at the feed-forward's `α` (one camera step is 1 K here).
    let alpha = 0.006f64.max(0.0105);
    let want = 1e6 / (mired(k0) + (meter::warmth_target(warmth) - meter::warmth_target(0.0)) / alpha);
    assert!((f64::from(sim.device_k()) - want).abs() <= 1.0, "{} K, want {want:.1} K", sim.device_k());
}

#[test]
fn a_warmth_change_on_a_held_picture_is_the_resume_point() {
    // A closed camera must reopen at the Kelvin the user's warmth change made, though the loop (holding on a picture
    // of one colour) stores nothing of its own.
    let (mut sim, mut ctrl) = kit("red_wall", 4600, kelvin(5500.0));
    sim.run(&mut ctrl, 10.0);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    assert_eq!(ctrl.status().resume, None);
    let t0 = sim.t;
    set_warmth(&mut ctrl, 0.5);
    sim.run(&mut ctrl, t0 + 0.4);
    // Not at once: the drag may still be going.
    assert_eq!(ctrl.status().resume, None);
    sim.run(&mut ctrl, t0 + 3.0);
    let first = sim.device_k();
    assert_ne!(first, 4600);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::NoSignal);
    assert_eq!(ctrl.status().resume.and_then(|r| r.white_balance), Some(first));
    // A second change soon after is written at once but stored only when `RESUME_EVERY` allows.
    set_warmth(&mut ctrl, -0.5);
    sim.run(&mut ctrl, t0 + 6.0);
    let second = sim.device_k();
    assert_ne!(second, first);
    assert_eq!(ctrl.status().resume.and_then(|r| r.white_balance), Some(first));
    sim.run(&mut ctrl, t0 + 16.0);
    assert_eq!(ctrl.status().resume.and_then(|r| r.white_balance), Some(second));
}

// --- the watchdog ---

fn engage_both(sim: &Sim, resume: Resume, seeded: bool) -> AutoController {
    AutoController::new(
        &sim.caps(),
        AutoCfg::default(),
        Learned::default(),
        Modes {
            exposure: Mode::App,
            resume,
            seeded,
            ..Modes::default()
        },
    )
}

fn handed_back(sim: &Sim, key: ControlKey) -> Vec<HandBack> {
    sim.handbacks.iter().filter(|h| h.1 == key).map(|h| h.2).collect()
}

#[test]
fn case_18_a_camera_that_ignores_writes() {
    let cfg = SimCfg {
        start_e: -9,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-3.5));
    sim.faults.ignore = vec![ControlKey::Exposure];
    let mut ctrl = engage_both(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 15.0);
    let exposure_writes: Vec<_> = sim.attempts.iter().filter(|w| w.key == ControlKey::Exposure).collect();
    assert_eq!(exposure_writes.len(), 3, "W2 after 3 writes: {exposure_writes:?}");
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    // Gain goes back to its prior value (the camera default without a recorded one).
    assert_eq!(handed_back(&sim, ControlKey::Gain), vec![HandBack::Manual(0)]);
    let at = sim.handbacks[0].0;
    assert!(sim.attempts.iter().all(|w| w.started < at), "writes after the hand-back");
    assert_eq!(ctrl.learned().blocked, vec![ControlKey::Exposure]);
    assert!(!ctrl.wants_meter());
    let st = ctrl.status().exposure.expect("the hand-back stays in the status");
    assert_eq!(st.state, ExposureState::Ignored);
    assert_eq!(st.handed_back, Some(HandBackKind::Camera));
    assert_eq!(
        st.message.as_deref(),
        Some("The camera ignored Better Cam auto for Exposure. Back to Camera auto.")
    );
    // The card learns it from the emit, and the mode is no longer offered.
    let last = sim.emitted.last().expect("emitted");
    assert_eq!(last.exposure.as_ref().map(|e| e.state), Some(ExposureState::Ignored));
    assert!(sim.emitted.iter().any(|s| s.learned.as_ref().is_some_and(|l| !l.blocked.is_empty())));
    assert_eq!(
        AutoController::support(&sim.caps(), ctrl.learned()).exposure,
        Offer::No(NoOffer::Blocked)
    );
    // "Try again" clears `blocked`; choosing Better Cam auto again starts the loop.
    ctrl.resume(ControlKey::Exposure);
    assert!(ctrl.learned().blocked.is_empty());
    ctrl.set_modes(Modes {
        exposure: Mode::Camera,
        ..Modes::default()
    });
    ctrl.set_modes(Modes {
        exposure: Mode::App,
        resume: resume(-6, 0),
        ..Modes::default()
    });
    assert!(ctrl.wants_meter());
    assert_eq!(ctrl.status().exposure.map(|e| e.state), Some(ExposureState::Starting));
}

#[test]
fn a_hand_back_stays_until_the_mode_changes() {
    let cfg = SimCfg {
        start_e: -9,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-3.5));
    sim.faults.ignore = vec![ControlKey::Exposure];
    let mut ctrl = engage_both(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 15.0);
    let ignored = |c: &AutoController| c.status().exposure.map(|e| e.state);
    assert_eq!(ignored(&ctrl), Some(ExposureState::Ignored));
    // The runner confirms the hand-back with the mode the control went to: the status stays.
    ctrl.set_modes(Modes {
        exposure: Mode::Camera,
        ..Modes::default()
    });
    assert_eq!(ignored(&ctrl), Some(ExposureState::Ignored));
    // The user picks Manual: it goes.
    ctrl.set_modes(Modes {
        exposure: Mode::Manual,
        ..Modes::default()
    });
    assert_eq!(ignored(&ctrl), None);
}

#[test]
fn case_19_a_picture_that_does_not_answer() {
    // A camera without gain: every move is an exposure move of a whole stop; the picture ignores them.
    let cfg = SimCfg {
        gain: false,
        start_e: -8,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-5.5));
    sim.faults.no_effect = vec![ControlKey::Exposure];
    let mut ctrl = engage_both(&sim, resume(-8, 0), true);
    sim.run(&mut ctrl, 12.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    let at = sim.handbacks[0].0;
    assert!(at <= 5.0, "handed back at {at}");
    assert!(sim.attempts.iter().all(|w| w.started < at), "writes after the hand-back: {:?}", sim.attempts);
    assert_eq!(sim.attempts.len(), 3, "three batches: {:?}", sim.attempts);
    assert!(ctrl.learned().blocked.is_empty(), "no_effect is not `ignored`");
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::NoResponse);
    assert_eq!(
        st.message.as_deref(),
        Some("The picture does not change when Better Cam changes Exposure. Back to Camera auto.")
    );
}

#[test]
fn case_19_with_gain_the_exposure_steps_are_still_caught() {
    // Far from the cap the moves are gain first (the picture answers), then exposure steps (it does not): three of
    // those in a row are caught. Near the cap the loop would fill with gain instead and the picture would answer.
    let cfg = SimCfg {
        start_e: -12,
        start_g: 0,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-8.5));
    sim.faults.no_effect = vec![ControlKey::Exposure];
    let mut ctrl = engage_both(&sim, resume(-12, 0), true);
    sim.run(&mut ctrl, 25.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    assert_eq!(handed_back(&sim, ControlKey::Gain), vec![HandBack::Manual(0)]);
    let at = sim.handbacks[0].0;
    assert!(at <= 25.0 && sim.writes.iter().all(|w| w.started < at));
    assert_eq!(ctrl.status().exposure.unwrap().state, ExposureState::NoResponse);
}

#[test]
fn case_20_a_camera_that_errors() {
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    sim.faults.fail = true;
    let mut ctrl = engage_both(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 10.0);
    assert!(sim.attempts.len() <= 4, "{} write attempts: {:?}", sim.attempts.len(), sim.attempts);
    assert!(sim.writes.is_empty());
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::NotAnswering);
    assert_eq!(
        st.message.as_deref(),
        Some("The camera stopped answering. Exposure went back to Camera auto.")
    );
    // The retry waits half a second after each failure.
    let times: Vec<f64> = sim.issued.iter().map(|i| i.0).collect();
    assert!(times.windows(2).all(|w| w[1] == w[0] || w[1] - w[0] >= 0.5), "{times:?}");
    // An error is not "ignored".
    assert!(ctrl.learned().blocked.is_empty());
}

#[test]
fn a_write_that_never_returns_is_a_fault_after_3_seconds() {
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    sim.faults.hang = true;
    let mut ctrl = engage_both(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 2.5);
    assert!(sim.handbacks.is_empty());
    sim.run(&mut ctrl, 5.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    assert!(sim.handbacks[0].0 >= 3.0, "{:?}", sim.handbacks);
    assert_eq!(ctrl.status().exposure.unwrap().state, ExposureState::NotAnswering);
}

#[test]
fn case_21_another_program_keeps_changing_the_control() {
    // Converged (E -5, one stop of gain); another program nudges the gain four times in 30 s: 0.2 stop, inside the
    // band, so only the read-back notices.
    let cfg = SimCfg {
        start_e: -5,
        start_g: 33,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    sim.faults.meddles = [10.0, 20.0, 30.0, 40.0]
        .into_iter()
        .map(|at| Meddle {
            at,
            key: ControlKey::Gain,
            value: 40,
        })
        .collect();
    let mut ctrl = engage_both(&sim, resume(-5, 33), true);
    sim.run(&mut ctrl, 70.0);
    let retakes: Vec<_> = sim
        .writes
        .iter()
        .filter(|w| w.key == ControlKey::Gain && w.value == 33)
        .collect();
    assert_eq!(retakes.len(), 3, "re-takes: {:?}", sim.writes);
    for (w, at) in retakes.iter().zip([10.0, 20.0, 30.0]) {
        assert!(w.started >= at && w.started < at + 1.6, "re-take at {} for a change at {at}", w.started);
    }
    // The fourth change: conflict, nothing handed back, nothing written until the user resumes.
    assert!(sim.handbacks.is_empty());
    assert!(sim.writes.iter().all(|w| w.started < 40.0), "{:?}", sim.writes);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::Conflict);
    assert_eq!(
        st.message.as_deref(),
        Some("Another program keeps changing Exposure. Better Cam auto paused.")
    );
    assert_eq!(st.handed_back, None);
    assert!(!ctrl.wants_meter());
    assert_eq!(sim.device().1, 40, "the other program's value stays");
    // "Resume": the loop starts again (from the values it holds).
    ctrl.resume(ControlKey::Exposure);
    assert!(ctrl.wants_meter());
    assert_ne!(ctrl.status().exposure.map(|e| e.state), Some(ExposureState::Conflict));
}

#[test]
fn case_22_slow_writes() {
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    sim.faults.slow = Some(0.3);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 14.0);
    let at = settled_at(&sim, 0.0, 14.0, 0.33).expect("never settled");
    assert!(at <= 6.0, "settled at {at}");
    // Never more than one batch per two write times.
    let mut starts: Vec<f64> = sim.issued.iter().map(|i| i.0).collect();
    starts.dedup();
    for w in starts.windows(2) {
        assert!(w[1] - w[0] >= 0.6 - 1e-9, "batches {} s apart: {starts:?}", w[1] - w[0]);
    }
    // The camera is slow but works: not handed back.
    assert!(sim.handbacks.is_empty());
    assert_eq!(state(&ctrl), ExposureState::Holding);
}

#[test]
fn a_camera_too_slow_for_a_live_loop_is_handed_back() {
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    sim.faults.slow = Some(1.2);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 30.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    assert_eq!(
        ctrl.status().exposure.unwrap().message.as_deref(),
        Some("This camera is too slow to adjust live.")
    );
}

#[test]
fn hunting_damps_then_hands_back() {
    // The light swings two stops every 1.2 s: the loop follows it, reverses, and is stopped.
    let base = (3.5f64).exp2();
    let light: Light = Box::new(move |t| if ((t / 1.2) as u32).is_multiple_of(2) { base } else { base * 4.0 });
    let cfg = SimCfg {
        start_e: -5,
        start_g: 50,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-5, 50), true);
    sim.run(&mut ctrl, 60.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::Hunting);
    assert_eq!(st.message.as_deref(), Some("Exposure kept swinging. Back to Camera auto."));
    // Damped first (30 s), handed back on the second round: not within the first 10 s window.
    assert!(sim.handbacks[0].0 > 8.0, "{:?}", sim.handbacks);
    assert!(sim.writes.iter().all(|w| w.started < sim.handbacks[0].0));
}

#[test]
fn no_picture_holds_and_speaks_after_ten_seconds() {
    let cfg = SimCfg {
        start_e: -5,
        start_g: 33,
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-4.0));
    let mut ctrl = engage(&sim, resume(-5, 33), true);
    sim.run(&mut ctrl, 8.0);
    assert_eq!(state(&ctrl), ExposureState::Holding);
    sim.no_samples = true;
    sim.run(&mut ctrl, 9.0);
    assert_eq!(state(&ctrl), ExposureState::Holding, "less than 2 s of silence");
    sim.run(&mut ctrl, 11.0);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!((st.state, st.message), (ExposureState::NoPicture, None));
    sim.run(&mut ctrl, 20.0);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.state, ExposureState::NoPicture);
    assert_eq!(st.message.as_deref(), Some("No picture to measure."));
    assert!(sim.writes.is_empty(), "wrote without a picture: {:?}", sim.writes);
    // The picture comes back: the loop goes on, nothing was handed back.
    sim.no_samples = false;
    sim.run(&mut ctrl, 24.0);
    assert_eq!(state(&ctrl), ExposureState::Holding);
    assert!(sim.handbacks.is_empty());
}

// --- where a hand-back goes ---

#[test]
fn a_camera_without_camera_auto_goes_back_to_the_prior_value() {
    let cfg = SimCfg {
        exposure_auto: false,
        wb: Some(WbSim {
            can_auto: false,
            ..WbSim::default()
        }),
        ..SimCfg::default()
    };
    let mut sim = Sim::new(cfg, dim(-3.5));
    sim.faults.fail = true;
    let modes = Modes {
        exposure: Mode::App,
        white_balance: Mode::App,
        prior: Prior {
            exposure: Some(-7),
            gain: Some(12),
            gain_auto: false,
            white_balance: Some(5200),
        },
        ..Modes::default()
    };
    let mut ctrl = AutoController::new(&sim.caps(), AutoCfg::default(), Learned::default(), modes);
    sim.run(&mut ctrl, 10.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::Manual(-7)]);
    assert_eq!(handed_back(&sim, ControlKey::Gain), vec![HandBack::Manual(12)]);
    assert_eq!(handed_back(&sim, ControlKey::WhiteBalance), vec![HandBack::Manual(5200)]);
    let st = ctrl.status().exposure.unwrap();
    assert_eq!(st.handed_back, Some(HandBackKind::Manual));
    assert_eq!(
        st.message.as_deref(),
        Some("The camera stopped answering. Exposure went back to its earlier value.")
    );
    let wb = ctrl.status().white_balance.unwrap();
    assert_eq!((wb.state, wb.handed_back), (WhiteBalanceState::NotAnswering, Some(HandBackKind::Manual)));
}

#[test]
fn white_balance_is_handed_back_on_its_own() {
    // Exposure keeps running; only the white balance write is ignored.
    let temp: Light = Box::new(|t| if t < 2.0 { 5000.0 } else { 3500.0 });
    let mut sim = wb_sim(0.008, 5000, temp);
    sim.faults.ignore = vec![ControlKey::WhiteBalance];
    let mut ctrl = AutoController::new(
        &sim.caps(),
        AutoCfg::default(),
        Learned::default(),
        Modes {
            exposure: Mode::App,
            white_balance: Mode::App,
            resume: Resume {
                exposure: Some(-5),
                gain: Some(33),
                white_balance: Some(5000),
            },
            seeded: true,
            ..Modes::default()
        },
    );
    sim.run(&mut ctrl, 20.0);
    assert_eq!(handed_back(&sim, ControlKey::WhiteBalance), vec![HandBack::CameraAuto]);
    assert!(handed_back(&sim, ControlKey::Exposure).is_empty());
    assert_eq!(ctrl.learned().blocked, vec![ControlKey::WhiteBalance]);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::Ignored);
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("The camera ignored Better Cam auto for White balance. Back to Camera auto.")
    );
    assert!(ctrl.status().exposure.is_some_and(|e| e.state == ExposureState::Holding));
    assert!(ctrl.wants_meter(), "exposure still needs the meter");
    assert_eq!(
        AutoController::support(&sim.caps(), ctrl.learned()).white_balance,
        Offer::No(NoOffer::Blocked)
    );
}

#[test]
fn a_white_balance_that_does_not_answer_is_handed_back() {
    let temp: Light = Box::new(|t| if t < 2.0 { 5000.0 } else { 3500.0 });
    let mut sim = wb_sim(0.008, 5000, temp);
    sim.faults.no_effect = vec![ControlKey::WhiteBalance];
    let mut ctrl = wb_ctrl(&sim, 5000, true);
    sim.run(&mut ctrl, 20.0);
    assert_eq!(handed_back(&sim, ControlKey::WhiteBalance), vec![HandBack::CameraAuto]);
    assert!(ctrl.learned().blocked.is_empty());
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("The picture does not change when Better Cam changes White balance. Back to Camera auto.")
    );
    assert!(sim.writes.iter().all(|w| w.started < sim.handbacks[0].0));
}

// --- the stripes note and `learned` in the status ---

#[test]
fn stripes_note_follows_the_exposure_and_the_powerline_control() {
    let sim = Sim::new(SimCfg::default(), dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), true);
    ctrl.tick(0.0, &sim.facts());
    assert_eq!(ctrl.status().exposure.unwrap().note, None, "powerline unknown");
    let mut line = w4gs().into_iter().find(|c| c.key == ControlKey::PowerlineFrequency).unwrap();
    line.value = 1; // 50 Hz
    ctrl.on_readback(&line, 0.1);
    // E = -6 is 1/64 s = 15.6 ms: longer than 10 ms, the period of 100 Hz light. The note starts at -7.
    assert_eq!(ctrl.status().exposure.unwrap().note, None);
    let sim = Sim::new(
        SimCfg {
            start_e: -9,
            ..SimCfg::default()
        },
        dim(-3.5),
    );
    let mut ctrl = engage(&sim, resume(-9, 0), true);
    ctrl.on_readback(&line, 0.0);
    ctrl.tick(0.0, &sim.facts());
    assert_eq!(
        ctrl.status().exposure.unwrap().note.as_deref(),
        Some("Short exposures under mains light can show stripes.")
    );
    line.value = 3; // Auto: no frequency known
    ctrl.on_readback(&line, 0.2);
    assert_eq!(ctrl.status().exposure.unwrap().note, None);
}

#[test]
fn learned_values_ride_the_status_that_follows_their_change() {
    // The gain curve is learned while the light alternates (as in case 6): some emitted status carries it, the next
    // does not, and the last carried value is what the controller holds.
    let cfg = SimCfg {
        s_true: 5.0,
        start_e: -5,
        ..SimCfg::default()
    };
    let light: Light = Box::new(|t| {
        let need: f64 = if ((t / 5.0) as u32).is_multiple_of(2) { -4.0 } else { -4.8 };
        (-need).exp2()
    });
    let mut sim = Sim::new(cfg, light);
    let mut ctrl = engage(&sim, resume(-5, 0), true);
    sim.run(&mut ctrl, 30.0);
    let carried: Vec<&Learned> = sim.emitted.iter().filter_map(|s| s.learned.as_ref()).collect();
    assert!(!carried.is_empty(), "no status carried the learned values");
    assert!(carried.windows(2).all(|w| w[0] != w[1]), "an unchanged value was sent again");
    let last = carried.last().unwrap();
    assert!(last.gain_stops.is_some());
    assert_eq!(ctrl.status().learned, None, "nothing changed since the last emit");
}

#[test]
fn cfg_and_learned_round_trip_as_json() {
    let cfg = AutoCfg {
        exposure: ExposureCfg {
            longest: Longest::Stop(-4),
            max_gain: Some(60),
            ..ExposureCfg::default()
        },
        ..AutoCfg::default()
    };
    let json = serde_json::to_string(&cfg).unwrap();
    assert_eq!(serde_json::from_str::<AutoCfg>(&json).unwrap(), cfg);
    assert_eq!(
        serde_json::from_str::<AutoCfg>(r#"{"exposure":{"longest":"fit"}}"#).unwrap(),
        AutoCfg::default()
    );
    assert!(serde_json::from_str::<AutoCfg>(r#"{"exposure":{"longest":"never"}}"#).is_err());
    let l: Learned = serde_json::from_str(r#"{"rate_caps":{"30":-6},"gain_stops":2.5}"#).unwrap();
    assert_eq!(l.rate_caps.get("30"), Some(&-6));
    assert_eq!(serde_json::from_str::<Learned>(&serde_json::to_string(&l).unwrap()).unwrap(), l);
}

/// The owner's settings on the W4GS (Most gain 100, Response Fast, 30 fps), from the two ways a loop starts: the seed
/// (-6, 0) and the resume point a limit state stored on an earlier run (-5, 100, the one the owner's record holds).
/// A scene the full-rate limits can reach ends within the band and at the lowest gain that gets there. One that is up
/// to a stop short of them stays at E = -5, G = 100 in `limit_gain`. One that is more than a stop short goes one stop
/// past (E = -4, 16 fps, 2.5.12) and, if that is still not enough, ends there at G = 100 in `limit_gain`.
#[test]
fn owner_settings_settle_inside_the_limits_and_sit_at_them_outside() {
    // The most light the limits give: -5 + 3 stops of gain, and with the frame rate lowered -4 + 3.
    let (top, top_lowered) = (-2.0, -1.0);
    for need in [-9.0, -7.0, -5.0, -4.0, -3.0, -2.5, -2.0, -1.5, -1.2, -0.5, 0.0, 1.0] {
        for (start, seeded) in [((-6, 0), false), ((-5, 100), true)] {
            let cfg = SimCfg {
                start_e: start.0,
                start_g: start.1,
                ..SimCfg::default()
            };
            let mut sim = Sim::new(cfg, dim(need));
            let mut ac = AutoCfg::default();
            ac.exposure.max_gain = Some(100);
            ac.exposure.speed = Speed::Fast;
            let mut ctrl = engage_with(&sim, ac, resume(start.0, start.1), seeded);
            sim.run(&mut ctrl, 20.0);
            let last = sim.history.last().expect("ticks");
            let what = format!("need {need} from {start:?}: device {:?}, err {:+.2}", sim.device(), last.err);
            assert!(sim.max_e_applied <= -4, "{what}: never past one stop over the frame rate");
            if need <= top {
                assert!(last.err.abs() <= 0.33, "{what}");
                assert!(sim.max_e_applied <= -5, "{what}: full frame rate");
                // Gain is not bought where a longer exposure gives the same light: at most the crossover's stop and a
                // quarter on top of what the scene needs.
                let gs = sim.true_gs(sim.device().1);
                assert!(gs <= (need - f64::from(sim.device().0)).max(0.0) + 1.5, "{what}: {gs} stops of gain");
            } else if need < top + 1.0 {
                // Under a stop short: the full frame rate is kept.
                assert_eq!(sim.device(), (-5, 100), "{what}");
                assert_eq!(state(&ctrl), ExposureState::LimitGain, "{what}");
                assert!((0.0..=1.0).contains(&last.err), "{what}");
            } else {
                assert_eq!(sim.device(), (-4, 100), "{what}");
                assert!(sim.fps_between(sim.t - 5.0, sim.t) < 20.0, "{what}");
                if need <= top_lowered {
                    assert!(last.err.abs() <= 0.33, "{what}");
                } else {
                    assert_eq!(state(&ctrl), ExposureState::LimitGain, "{what}");
                    assert!(last.err > 0.0, "{what}");
                }
            }
        }
    }
}

/// The owner's settings, 30 fps: Most gain 100, Response Fast, target 0.45.
fn owner_cfg() -> AutoCfg {
    let mut ac = AutoCfg::default();
    ac.exposure.max_gain = Some(100);
    ac.exposure.speed = Speed::Fast;
    ac
}

fn owner_sim(light: Light) -> Sim {
    Sim::new(
        SimCfg {
            start_e: -5,
            start_g: 100,
            ..SimCfg::default()
        },
        light,
    )
}

/// (a) A scene two stops under what 1/32 s and full gain give: the exposure goes longer than the frame-rate limit, one
/// stop (the ceiling for `fit`), and the picture is that much brighter. The frame-rate guard does not take the low rate
/// for a camera that drops frames.
#[test]
fn a_room_two_stops_too_dark_lowers_the_frame_rate() {
    let mut sim = owner_sim(dim(0.0));
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    let mut states = Vec::new();
    while sim.t < 30.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        states.push(ctrl.status().exposure.unwrap());
    }
    let last = sim.history.last().unwrap();
    assert_eq!(sim.device(), (-4, 100));
    assert_eq!(sim.max_e_applied, -4, "never past the ceiling");
    // Two stops under at (-5, 100); one stop under at the ceiling. The ceiling is as far as `fit` goes.
    assert!((last.err - 1.0).abs() <= 0.1, "err {}", last.err);
    assert!(sim.fps_between(20.0, 30.0) < 20.0, "{}", sim.fps_between(20.0, 30.0));
    // Not before 3 s of it (not a single frame), and the card says what the loop is doing.
    let first = sim.writes.iter().find(|w| w.key == ControlKey::Exposure && w.value == -4).expect("never went longer");
    assert!((3.0..=5.0).contains(&first.started), "went longer at {}", first.started);
    assert!(states.iter().any(|s| s.message.as_deref() == Some("Lowering frame rate for light…")));
    let st = states.last().unwrap();
    assert_eq!((st.state, st.cap, st.cap_reason), (ExposureState::LimitGain, -5, CapReason::Fps));
    let message = st.message.as_deref().unwrap();
    assert!(message.starts_with("Gain at its maximum (100) and exposure at its longest (1/16 s, 1"), "{message}");
    // The frame-rate guard (2.5.8) learned nothing from the low rate it was asked for.
    assert!(sim.emitted.iter().filter_map(|s| s.learned.as_ref()).all(|l| l.rate_caps.is_empty()));
    // And the loop is quiet.
    assert!(sim.writes_between(15.0, 30.0).is_empty());

    // 1.4 stops short at (-5, 100): the error is under half a stop at the ceiling.
    let mut sim = owner_sim(dim(-0.6));
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device(), (-4, 100));
    assert!(sim.history.last().unwrap().err.abs() <= 0.5, "err {}", sim.history.last().unwrap().err);
}

/// With the frame rate lowered, gain is not kept at its limit where exposure alone covers the need.
#[test]
fn a_brighter_room_needs_less_gain_at_the_longer_exposure() {
    let light: Light = Box::new(|t| if t < 15.0 { 1.0 } else { 1.8_f64.exp2() });
    // need = 0 until 15 s, then -1.8: inside the dead band (the exit is at -2.5), so the exposure stays at 1/16 s.
    let mut sim = owner_sim(light);
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 15.0);
    assert_eq!(sim.device(), (-4, 100));
    // The gain coming down at the lowered rate is plain adjusting: the card says "Lowering" only while the exposure
    // goes past the frame-rate limit.
    let mut texts = Vec::new();
    while sim.t < 40.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        texts.push(ctrl.status().exposure.unwrap().message);
    }
    assert!(texts.iter().any(|m| m.as_deref() == Some("Adjusting…")));
    assert!(!texts.iter().flatten().any(|m| m.starts_with("Lowering")), "{texts:?}");
    let (e, g) = sim.device();
    assert_eq!(e, -4);
    // 2.2 stops of gain of 3: well under the limit.
    assert!(sim.true_gs(g) < 2.5, "gain {g} = {} stops", sim.true_gs(g));
    assert!(sim.history.last().unwrap().err.abs() <= 0.33);
}

/// (b) A normally lit scene never leaves the frame-rate limit, from either start.
#[test]
fn a_lit_room_keeps_the_frame_rate() {
    for need in [-9.0, -6.0, -4.0, -2.5] {
        for (start, seeded) in [((-6, 0), false), ((-5, 100), true)] {
            let mut sim = Sim::new(
                SimCfg {
                    start_e: start.0,
                    start_g: start.1,
                    ..SimCfg::default()
                },
                dim(need),
            );
            let mut ctrl = engage_with(&sim, owner_cfg(), resume(start.0, start.1), seeded);
            sim.run(&mut ctrl, 30.0);
            let what = format!("need {need} from {start:?}");
            assert!(sim.max_e_applied <= -5, "{what}: exposure {}", sim.max_e_applied);
            assert!(sim.history.last().unwrap().err.abs() <= 0.33, "{what}");
            assert!(sim.fps_between(10.0, 30.0) >= 29.5, "{what}");
            assert_eq!(state(&ctrl), ExposureState::Holding, "{what}");
        }
    }
}

/// (c) Dark, then bright: the exposure returns to the frame-rate limit with gain still high (the full frame rate
/// first, then less gain), and nothing swings afterwards.
#[test]
fn light_back_restores_the_frame_rate_before_it_cuts_gain() {
    // need 0 until 20 s, then -4 (the exit is at -2.5).
    let light: Light = Box::new(|t| if t < 20.0 { 1.0 } else { 4.0_f64.exp2() });
    let mut sim = owner_sim(light);
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 20.0);
    assert_eq!(sim.device(), (-4, 100));
    sim.run(&mut ctrl, 60.0);
    assert_eq!(sim.device().0, -5, "back at 1/32 s");
    let g_end = sim.device().1;
    assert!(sim.history.last().unwrap().err.abs() <= 0.33);
    // The first moment the exposure is short again, gain has not gone below where it ends.
    let back = sim.history.iter().find(|s| s.t > 20.0 && s.e == -5).expect("never back");
    assert!(back.g >= g_end, "gain {} when the exposure came back, {g_end} at the end", back.g);
    // After that the exposure never goes longer again, and a settled loop does not write.
    assert!(sim.history.iter().filter(|s| s.t >= back.t).all(|s| s.e == -5));
    assert!(sim.writes_between(40.0, 60.0).is_empty());
    assert!(sim.fps_between(40.0, 60.0) >= 29.5);
    assert_eq!(state(&ctrl), ExposureState::Holding);
}

/// No flipping at the entry boundary: a light that wobbles across the point where the frame rate would be lowered
/// does not move the exposure back and forth. (The exit boundary is `a_light_at_the_exit_boundary_...`, which first
/// lets the loop enter for real.)
#[test]
fn a_light_at_the_boundary_does_not_flip_the_frame_rate() {
    // At (-5, 100): err = need + 2. The entry needs more than +1 for 3 s; the exit needs need under -2.5.
    for (low, high, start) in [(-1.3, -0.7, (-5, 100)), (-2.4, -1.6, (-5, 100))] {
        let light: Light = Box::new(move |t| {
            let need: f64 = if ((t / 7.0) as u32).is_multiple_of(2) { high } else { low };
            (-need).exp2()
        });
        let mut sim = Sim::new(
            SimCfg {
                start_e: start.0,
                start_g: start.1,
                ..SimCfg::default()
            },
            light,
        );
        let mut ctrl = engage_with(&sim, owner_cfg(), resume(start.0, start.1), true);
        sim.run(&mut ctrl, 150.0);
        let e_writes = sim.writes.iter().filter(|w| w.key == ControlKey::Exposure).count();
        assert!(e_writes <= 1, "{low}/{high}: the exposure was written {e_writes} times: {:?}", sim.writes);
    }
}

/// (d) `Longest exposure` at the frame-rate limit: gain only, as before. A longer user stop is the ceiling.
#[test]
fn the_longest_exposure_setting_is_never_passed() {
    let mut cfg = owner_cfg();
    cfg.exposure.longest = Longest::Stop(-5);
    let mut sim = owner_sim(dim(1.0));
    let mut ctrl = engage_with(&sim, cfg, resume(-5, 100), true);
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device(), (-5, 100));
    assert_eq!(sim.max_e_applied, -5);
    assert_eq!(state(&ctrl), ExposureState::LimitGain);
    assert!(sim.fps_between(5.0, 30.0) >= 29.5);
    assert!(sim.writes.is_empty(), "{:?}", sim.writes);

    // The user allows 1/8 s: exposure goes there first, before gain, as before (no 3 s wait, no ceiling of one stop).
    let mut cfg = owner_cfg();
    cfg.exposure.longest = Longest::Stop(-3);
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
    sim.run(&mut ctrl, 20.0);
    assert!(sim.max_e_applied <= -3);
    assert!(sim.history.last().unwrap().err.abs() <= 0.33);
    let mut cfg = owner_cfg();
    cfg.exposure.longest = Longest::Stop(-4);
    let mut sim = owner_sim(dim(4.0));
    let mut ctrl = engage_with(&sim, cfg, resume(-5, 100), true);
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device(), (-4, 100));
    assert_eq!(sim.max_e_applied, -4);
}

/// A reopen from the resume point of a lowered frame rate stays there: no climb back to 1/32 s, no write.
#[test]
fn a_reopen_keeps_the_lowered_frame_rate() {
    let mut sim = Sim::new(
        SimCfg {
            start_e: -4,
            start_g: 100,
            ..SimCfg::default()
        },
        dim(0.0),
    );
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-4, 100), true);
    assert_eq!(ctrl.seeds(30.0).exposure, Some((-4, Some(100))));
    sim.run(&mut ctrl, 10.0);
    assert!(sim.writes.is_empty(), "{:?}", sim.writes);
    assert_eq!(state(&ctrl), ExposureState::LimitGain);
    // A frame-rate limit that does not allow it (the user's own stop) cuts the seed to it.
    let mut cfg = owner_cfg();
    cfg.exposure.longest = Longest::Stop(-5);
    let ctrl = engage_with(&sim, cfg, resume(-4, 100), false);
    assert_eq!(ctrl.seeds(30.0).exposure, Some((-5, Some(100))));
}

#[test]
fn the_log_line_says_where_the_loop_is_and_what_it_saw() {
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    // Not before the camera is open (the first tick).
    assert_eq!(ctrl.exposure_diag(), None);
    sim.run(&mut ctrl, 0.2);
    assert_eq!(ctrl.exposure_diag().as_deref().map(|l| l.contains("Starting")), Some(true));
    sim.run(&mut ctrl, 12.0);
    let line = ctrl.exposure_diag().expect("engaged");
    // The room is darker than 1/32 s and Most gain give: the loop went one stop past (2.5.12) and sits at that limit.
    for part in [
        "LimitGain", "E=-4", "G=75", "cap=-5 (Fps)", "ceil=-4", "over=true", "maxG=75", "err=+", "bl=+0.00",
        "src=0.00", "fl=+0.00 gap=",
    ] {
        assert!(line.contains(part), "{part} not in {line}");
    }
    // 13.4: the spread of the bright tenth follows the gap. The spec says `sd=0.00` here; the sim's grey wall has a
    // face patch six cells wide, which reads 0.17-0.18 in every normal room of the suite (13.10).
    let sd: f64 = line.split(" sd=").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).expect("sd=");
    assert!(sd < 0.25, "sd={sd} in {line}");
    assert!(line.contains("fps=15."), "{line}");
    // Held at the frame-rate limit: the same room, `over=false`.
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage_with(&sim, at_the_frame_rate(), resume(-6, 0), false);
    sim.run(&mut ctrl, 12.0);
    let line = ctrl.exposure_diag().expect("engaged");
    for part in ["LimitGain", "E=-5", "G=75", "cap=-5 (User)", "ceil=-5", "over=false", "fps=30.0/30"] {
        assert!(line.contains(part), "{part} not in {line}");
    }
    // Not engaged: no line.
    let off = AutoController::new(&sim.caps(), AutoCfg::default(), Learned::default(), Modes::default());
    assert_eq!(off.exposure_diag(), None);
}

// --- lowering the frame rate for light: the audit's cases (2.5.11, 2.5.12) ---

/// Whether the log line says the loop lowers the frame rate.
fn lowered(ctrl: &AutoController) -> bool {
    ctrl.exposure_diag().expect("running").contains("over=true")
}

/// The first exposure write of `value` at or after `from`.
fn first_exposure_write(sim: &Sim, value: i32, from: f64) -> Option<f64> {
    sim.writes
        .iter()
        .find(|w| w.key == ControlKey::Exposure && w.value == value && w.started >= from)
        .map(|w| w.started)
}

/// A reopen in a normal room: the resume point is a value from some earlier run (it carries no frame rate), so a
/// seed above the frame-rate limit is only a guess. The room needs 3.8 stops: 1/64 s (the limit at 60 fps) and the
/// gain it leaves, not the 1/32 s of the seed (31 fps).
#[test]
fn a_reopen_in_a_normal_room_leaves_the_lowered_frame_rate() {
    let mut sim = Sim::new(
        SimCfg {
            fps: 60.0,
            start_e: -5,
            start_g: 40,
            ..SimCfg::default()
        },
        dim(-3.8),
    );
    let mut ctrl = engage(&sim, resume(-5, 40), true);
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device().0, -6, "{:?}", sim.device());
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(sim.fps_between(20.0, 30.0) >= 59.0, "{}", sim.fps_between(20.0, 30.0));
    assert!(sim.history.last().unwrap().err.abs() <= 0.33);
    assert_eq!(state(&ctrl), ExposureState::Holding);

    // A room that does need the longer exposure keeps it (the seed was right).
    let mut sim = Sim::new(
        SimCfg {
            fps: 60.0,
            start_e: -5,
            start_g: 75,
            ..SimCfg::default()
        },
        dim(-1.0),
    );
    let mut ctrl = engage(&sim, resume(-5, 75), true);
    sim.run(&mut ctrl, 30.0);
    assert!(lowered(&ctrl));
    assert_eq!(sim.device(), (-5, 75));
    assert!(sim.writes.is_empty(), "{:?}", sim.writes);
}

/// The same with a frame-rate limit the loop learned: a stored value above it is not kept either, and the seed is the
/// stored value until the first decision says otherwise.
#[test]
fn a_reopen_above_a_learned_rate_cap_leaves_the_lowered_frame_rate() {
    let mut sim = Sim::new(
        SimCfg {
            start_e: -5,
            start_g: 40,
            ..SimCfg::default()
        },
        dim(-3.8),
    );
    let mut learned = Learned::default();
    learned.rate_caps.insert("30".to_string(), -6);
    let mut ctrl = AutoController::new(
        &sim.caps(),
        AutoCfg::default(),
        learned,
        Modes {
            exposure: Mode::App,
            resume: resume(-5, 40),
            seeded: true,
            ..Modes::default()
        },
    );
    assert_eq!(ctrl.seeds(30.0).exposure, Some((-5, Some(40))));
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device().0, -6, "{:?}", sim.device());
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(sim.history.last().unwrap().err.abs() <= 0.33);
    assert_eq!(ctrl.learned().rate_caps.get("30"), Some(&-6));
}

/// The entry timer runs only while the loop sits at its limits with the picture over a stop dark: a dip in between
/// (too bright, the lens covered) starts it again, so the loop does not lower the frame rate on the first reading
/// after it.
#[test]
fn a_dip_in_the_light_starts_the_entry_timer_again() {
    // (when the dip ends, the light of the dip)
    for (name, dip) in [("too bright", 4.0_f64.exp2()), ("lens covered", 0.0)] {
        let light: Light = Box::new(move |t| {
            if (2.0..8.0).contains(&t) {
                dip
            } else {
                1.0
            }
        });
        let mut sim = owner_sim(light);
        let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
        sim.run(&mut ctrl, 25.0);
        assert_eq!(sim.device(), (-4, 100), "{name}");
        let at = first_exposure_write(&sim, -4, 0.0).unwrap_or_else(|| panic!("{name}: never went longer"));
        // The picture is dark again from 8 s on and at its limits from the last gain write that brought it there:
        // 3 s of that, not the time before the dip.
        let back = sim
            .writes
            .iter()
            .filter(|w| w.key == ControlKey::Gain && w.value == 100 && w.started >= 8.0)
            .map(|w| w.done)
            .fold(8.0, f64::max);
        assert!(at - back >= 2.9, "{name}: went longer at {at}, back at the limits at {back}");
    }
}

/// No gain, no room above the frame-rate limit left once the light is back: the lowered state ends when the exposure
/// is back at the frame-rate limit, not only when the scene is half a stop under what the limit gives.
#[test]
fn a_camera_without_gain_ends_the_lowered_frame_rate() {
    let light: Light = Box::new(|t| if t < 10.0 { 3.0_f64.exp2() } else { 5.0_f64.exp2() });
    let mut sim = Sim::new(
        SimCfg {
            gain: false,
            start_e: -5,
            ..SimCfg::default()
        },
        light,
    );
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 0), false);
    sim.run(&mut ctrl, 10.0);
    assert_eq!(sim.device().0, -4, "one stop past the frame rate in the dark room");
    assert!(lowered(&ctrl));
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device().0, -5, "{:?}", sim.device());
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(sim.fps_between(20.0, 30.0) >= 29.5);
    assert_eq!(state(&ctrl), ExposureState::Holding);
}

/// A reading with no confidence (a mostly dark picture: under 20 % confidence) never lowers the frame rate, however
/// long it lasts and however far off its error is (2.5.12). The picture alternates so that the median of three has no
/// confidence while the window still has a sure sample (all unsure is the no-signal path of 2.5.5 step 1).
#[test]
fn an_unsure_picture_never_lowers_the_frame_rate() {
    use super::meter::{measure_exposure, GRID};
    // `lit` cells at 0.05 of the full signal (about 1.8 stops dark), the rest black.
    let picture = |lit: usize| Stats {
        cells: (0..GRID * GRID)
            .map(|i| if i < lit { [0.05, 0.05, 0.05, 1.0] } else { [0.0, 0.0, 0.0, 1.0] })
            .collect(),
    };
    let sure = (1..GRID * GRID).find(|&n| measure_exposure(&picture(n), 0.45).confidence >= 0.2).expect("a lit picture");
    let (unsure, sure) = (picture(sure - 1), picture(sure));
    let (a, b) = (measure_exposure(&unsure, 0.45), measure_exposure(&sure, 0.45));
    assert!(a.confidence < 0.2 && a.error > 1.0 && b.confidence >= 0.2 && b.error > 1.0, "{a:?} {b:?}");

    let sim = owner_sim(dim(0.0));
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    let facts = SessionFacts {
        token: Some(1),
        target_fps: 30.0,
        capture_fps: 30.0,
        paused: false,
        resumed_at: f64::NEG_INFINITY,
    };
    let mut writes = Vec::new();
    let mut next_sample = 0.0;
    for i in 0..=100 {
        let now = f64::from(i) * 0.1;
        while next_sample <= now {
            let n = (next_sample / 0.066).round() as usize;
            let stats = if n % 3 == 2 { sure.clone() } else { unsure.clone() };
            ctrl.on_sample(&MeterSample { token: 1, capture_qpc: 0, stats }, next_sample);
            next_sample += 0.066;
        }
        writes.extend(ctrl.tick(now, &facts).into_iter().filter(|a| matches!(a, Action::Write { .. })));
    }
    assert!(writes.is_empty(), "{writes:?}");
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    // The same picture with its confidence in the clear does lower it (the test is not vacuous).
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    let mut writes = Vec::new();
    let mut next_sample = 0.0;
    for i in 0..=100 {
        let now = f64::from(i) * 0.1;
        while next_sample <= now {
            ctrl.on_sample(&MeterSample { token: 1, capture_qpc: 0, stats: sure.clone() }, next_sample);
            next_sample += 0.066;
        }
        writes.extend(ctrl.tick(now, &facts).into_iter().filter(|a| matches!(a, Action::Write { .. })));
        if !writes.is_empty() {
            break;
        }
    }
    assert!(lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(writes.contains(&Action::Write { key: ControlKey::Exposure, value: -4 }), "{writes:?}");
}

/// No line in the log while the camera is closed or the loop paused (it has nothing to say then).
#[test]
fn the_log_line_is_silent_while_closed_or_paused() {
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 2.0);
    assert!(ctrl.exposure_diag().is_some());
    let mut facts = sim.facts();
    facts.paused = true;
    ctrl.tick(2.1, &facts);
    assert_eq!(ctrl.exposure_diag(), None, "paused");
    facts.paused = false;
    ctrl.tick(2.2, &facts);
    assert!(ctrl.exposure_diag().is_some());
    facts.token = None;
    ctrl.tick(2.3, &facts);
    assert_eq!(ctrl.exposure_diag(), None, "closed");
}

/// A change of `Most gain` ends the lowered frame rate (2.5.12): the loop works from the new limits, brightness-
/// neutral first, and lowers it again only after 3 s at them.
#[test]
fn a_most_gain_change_ends_the_lowered_frame_rate() {
    let mut sim = owner_sim(dim(0.0));
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 15.0);
    assert!(lowered(&ctrl));
    assert_eq!(sim.device(), (-4, 100));
    let mut cfg = owner_cfg();
    cfg.exposure.max_gain = Some(60);
    ctrl.set_cfg(cfg);
    sim.run(&mut ctrl, 16.5);
    assert_eq!(sim.device().0, -5, "{:?}", sim.device());
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(sim.device().1 <= 60);
    // Still too dark at the new limits: the exposure goes longer again, not before the timer is out.
    assert!(first_exposure_write(&sim, -4, 16.0).is_none());
    sim.run(&mut ctrl, 30.0);
    assert_eq!(sim.device(), (-4, 60));
    assert!(lowered(&ctrl));
}

/// No entry while the frame-rate guard (2.5.8) is on a trial: the camera that drops frames at 1/32 s is taken to
/// 1/64 s first, and that stays learned; only afterwards may the loop go longer for a dark room.
#[test]
fn the_frame_rate_is_not_lowered_while_the_guard_tries_a_cap() {
    let mut sim = Sim::new(
        SimCfg {
            rate_drop: true,
            start_e: -5,
            start_g: 100,
            ..SimCfg::default()
        },
        dim(0.0),
    );
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 20.0);
    assert_eq!(ctrl.learned().rate_caps.get("30"), Some(&-6), "the guard's cap was taken back");
    // The trial ran at 1/64 s; the loop went longer afterwards (the room is two stops dark).
    let trial = first_exposure_write(&sim, -6, 0.0).expect("the guard never tried");
    let longer = first_exposure_write(&sim, -5, trial).expect("never went longer");
    assert!(longer - trial >= 3.0, "trial {trial}, longer {longer}");
}

// --- lowering the frame rate for light: the second audit's cases (2.5.12) ---

/// The exposure writes the camera received, as values.
fn exposure_attempts(sim: &Sim) -> Vec<i32> {
    sim.attempts.iter().filter(|w| w.key == ControlKey::Exposure).map(|w| w.value).collect()
}

/// A camera that takes an exposure up to the frame time and no longer (it reads back the old value): the one stop past
/// the frame-rate limit is refused once, and that is no ignored control. The loop stays at the frame-rate limit for
/// the rest of the session, asks again after a new one, and is not handed back or blocked.
#[test]
fn a_camera_that_refuses_a_longer_exposure_keeps_the_frame_rate() {
    let mut sim = owner_sim(dim(0.0));
    sim.faults.ignore = vec![ControlKey::Exposure];
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 30.0);
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
    assert!(ctrl.learned().blocked.is_empty(), "{:?}", ctrl.learned().blocked);
    assert_eq!(exposure_attempts(&sim), vec![-4], "asked once, not again this session");
    assert_eq!(state(&ctrl), ExposureState::LimitGain);
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    assert!(ctrl.exposure_diag().unwrap().contains("ceil=-5"), "{:?}", ctrl.exposure_diag());
    let st = ctrl.status().exposure.expect("running");
    assert_eq!(st.handed_back, None);
    assert_eq!(
        st.message.as_deref(),
        Some("Gain at its maximum (100). Add light, allow more gain, or allow a longer exposure.")
    );

    // A new session asks again (the next camera may take it).
    ctrl.set_modes(Modes {
        exposure: Mode::Camera,
        ..Modes::default()
    });
    ctrl.set_modes(Modes {
        exposure: Mode::App,
        resume: resume(-5, 100),
        seeded: true,
        ..Modes::default()
    });
    sim.run(&mut ctrl, 45.0);
    assert_eq!(exposure_attempts(&sim), vec![-4, -4]);
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
    assert!(ctrl.learned().blocked.is_empty());
}

/// A camera that ignores the exposure at the frame-rate limit too is still an ignored control (the refusal is only for
/// the stop past it).
#[test]
fn a_camera_that_ignores_every_exposure_write_is_still_ignored() {
    let mut sim = Sim::new(
        SimCfg {
            start_e: -9,
            ..SimCfg::default()
        },
        dim(-3.5),
    );
    sim.faults.ignore = vec![ControlKey::Exposure];
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-6, 0), false);
    sim.run(&mut ctrl, 15.0);
    assert_eq!(handed_back(&sim, ControlKey::Exposure), vec![HandBack::CameraAuto]);
    assert_eq!(ctrl.learned().blocked, vec![ControlKey::Exposure]);
}

/// A gap of frames (2 s without a sample, 2.5.12 and W5) breaks the 3 s of the entry timer: the loop that has seen the
/// room dark for 1 s, then nothing for 5 s, does not lower the frame rate on the first picture after it.
#[test]
fn a_gap_in_the_frames_starts_the_entry_timer_again() {
    let mut sim = owner_sim(dim(0.0));
    let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
    sim.run(&mut ctrl, 1.0);
    sim.no_samples = true;
    sim.run(&mut ctrl, 6.0);
    sim.no_samples = false;
    sim.run(&mut ctrl, 14.0);
    let at = first_exposure_write(&sim, -4, 0.0).expect("never went longer");
    assert!(at >= 9.0, "went longer at {at}: the 3 s began again with the first picture after the gap");
    assert!(at <= 11.0, "went longer at {at}");
}

/// The room brightens between the entry and the move up (2.5.12): gain alone covers it at the full frame rate, and
/// the loop must not stay "lowered" there (the guard off, the text, the next darkening without its 3 s). Whenever in
/// the second after the entry the light changes.
#[test]
fn a_room_that_brightens_after_the_entry_does_not_stay_lowered() {
    let mut cfg = owner_cfg();
    cfg.exposure.max_gain = Some(50);
    let mut caught = 0;
    for step in 0..24 {
        let at = 3.0 + f64::from(step) * 0.05;
        // At (-5, 50) the limits give need -3.5: -2.0 is 1.5 stops short, -3.9 within the gain's reach.
        let light: Light = Box::new(move |t| if t < at { 2.0_f64.exp2() } else { 3.9_f64.exp2() });
        let mut sim = Sim::new(
            SimCfg {
                start_e: -5,
                start_g: 50,
                ..SimCfg::default()
            },
            light,
        );
        let mut ctrl = engage_with(&sim, cfg, resume(-5, 50), true);
        let mut texts = Vec::new();
        while sim.t < 20.0 {
            sim.run(&mut ctrl, sim.t + 0.1);
            texts.push(ctrl.status().exposure.unwrap().message.unwrap_or_default());
        }
        let what = format!("light changes at {at}: device {:?}, {:?}", sim.device(), ctrl.exposure_diag());
        if first_exposure_write(&sim, -4, 0.0).is_some() {
            // The move came first: inside the dead band (the exit is at need -4.0) the loop stays where it is.
            assert!(lowered(&ctrl), "{what}");
            continue;
        }
        caught += 1;
        assert_eq!(sim.device().0, -5, "{what}");
        assert!(!lowered(&ctrl), "{what}");
        assert_eq!(state(&ctrl), ExposureState::Holding, "{what}");
        let last = ctrl.status().exposure.unwrap().message.unwrap();
        assert!(last.starts_with("Holding 30 fps"), "{what}: {last}");
        // Not "Lowered" at any time after the light was gone.
        let after = (at / 0.1) as usize + 20;
        assert!(!texts[after..].iter().any(|m| m.starts_with("Lower")), "{what}: {texts:?}");
        assert!(sim.history.last().unwrap().err.abs() <= 0.33, "{what}");
    }
    assert!(caught > 0, "no light change fell between the entry and the move");
}

/// A room that brightens a little inside the dead band, and then darkens again, ends the state only where it should:
/// after the entry and a hold at the frame-rate limit the lowered state is over, and a later darkening waits for its
/// own 3 s.
#[test]
fn a_darkening_after_a_cancelled_entry_waits_for_its_own_timer() {
    let mut cfg = owner_cfg();
    cfg.exposure.max_gain = Some(50);
    // need -2.0 (1.5 short) until the entry (3.35 s, as in the case above), then -3.9 (reachable), then -2.0 again from
    // 12 s.
    let light: Light = Box::new(|t| {
        if !(3.35..12.0).contains(&t) {
            2.0_f64.exp2()
        } else {
            3.9_f64.exp2()
        }
    });
    let mut sim = Sim::new(
        SimCfg {
            start_e: -5,
            start_g: 50,
            ..SimCfg::default()
        },
        light,
    );
    let mut ctrl = engage_with(&sim, cfg, resume(-5, 50), true);
    sim.run(&mut ctrl, 12.0);
    assert!(!lowered(&ctrl), "{:?}", ctrl.exposure_diag());
    sim.run(&mut ctrl, 14.5);
    assert!(first_exposure_write(&sim, -4, 12.0).is_none(), "went longer within 2.5 s of the darkening");
    sim.run(&mut ctrl, 25.0);
    assert!(first_exposure_write(&sim, -4, 12.0).is_some(), "never went longer");
}

/// The exit's dead band: a light that wobbles across the exit point (`need` under -2.5 at E_rate -5 and full gain)
/// is taken back to the frame rate once, and not again. The loop enters for real first (need 0 for 12 s).
#[test]
fn a_light_at_the_exit_boundary_does_not_flip_the_frame_rate() {
    for (high, low) in [(-2.3, -2.7), (-2.45, -2.55)] {
        let light: Light = Box::new(move |t| {
            let need: f64 = if t < 12.0 {
                0.0
            } else if (((t - 12.0) / 7.0) as u32).is_multiple_of(2) {
                high
            } else {
                low
            };
            (-need).exp2()
        });
        let mut sim = owner_sim(light);
        let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
        sim.run(&mut ctrl, 12.0);
        assert!(lowered(&ctrl), "{high}/{low}: {:?}", ctrl.exposure_diag());
        assert_eq!(sim.device().0, -4, "{high}/{low}");
        sim.run(&mut ctrl, 150.0);
        let writes = exposure_attempts(&sim);
        let first_back = writes.iter().position(|&v| v == -5);
        assert!(writes.starts_with(&[-4]), "{high}/{low}: {writes:?}");
        // No exposure write after the first -5.
        if let Some(i) = first_back {
            assert_eq!(writes.len(), i + 1, "{high}/{low}: {writes:?}");
        }
        assert!(writes.len() <= 2, "{high}/{low}: {writes:?}");
    }
}

// --- 10. Backlight: subject-priority metering (docs/AUTO-CONTROLS.md 10.8) ---

/// A kit scene with a window, in the owner's preset (`AutoCfg::default()`: target 0.45, Longest "fit", Most gain 75 %,
/// Normal; F 30). `dim(-3.5)` is the room's light unless a test says otherwise.
fn backlit_sim(scene: &'static str, window: Window, jitter: f64) -> Sim {
    Sim::new(
        SimCfg {
            scene: Some(scene),
            window: Some(window),
            jitter,
            ..SimCfg::default()
        },
        dim(-3.5),
    )
}

fn const_window(shape: WindowShape, stops: f64) -> Window {
    Window::new(shape, WindowLevel::Const(stops))
}

fn over_need(sim: &Sim, from: f64, until: f64) -> Vec<(f64, f64)> {
    sim.history
        .iter()
        .filter(|s| s.t >= from && s.t <= until)
        .map(|s| (s.t, s.over_need))
        .collect()
}

#[test]
fn a_window_leaves_the_scene_calibration_and_the_oracle_alone() {
    let room = |window| {
        Sim::new(
            SimCfg {
                scene: Some("office"),
                window,
                ..SimCfg::default()
            },
            dim(-3.5),
        )
    };
    let (plain, windowed) = (room(None), room(Some(const_window(WindowShape::TopAndSides, 5.0))));
    assert_eq!(plain.calibration(), windowed.calibration());
    assert_eq!(plain.stops_over_need(), windowed.stops_over_need());
    // At the start state (-6, 0, need -3.5) the face is 2.5 stops under its rest.
    assert!((plain.stops_over_need() + 2.5).abs() < 1e-9, "{}", plain.stops_over_need());
    let (a, b) = (plain.picture(), windowed.picture());
    // Rows 12-15 are never window; the window is brighter than the wall it replaces, and neutral.
    for i in 12 * 16..256 {
        assert_eq!(a.cells[i], b.cells[i], "cell {i}");
    }
    assert!(b.cells[0][1] > a.cells[0][1] * 4.0, "{:?} over {:?}", b.cells[0], a.cells[0]);
    assert_eq!(b.cells[0][0], b.cells[0][1]);
    // The person is never window: a face cell is the same.
    assert_eq!(a.cells[8 * 16 + 7], b.cells[8 * 16 + 7]);
}

/// L1: a window behind the person, whichever side of the dark resting point the loop starts on: the face is on
/// target. The seed -6 / 0 of the spec starts the face 2.5 stops under, where the window (+5) is already white and
/// today's meter lands on the good point too; the seed -8 / 0 (4.5 stops under) is the dark side that it does not leave.
#[test]
fn a_backlit_face_is_on_target_after_open() {
    for seed in [-6, -8] {
        for (scene, shape) in [
            ("office", WindowShape::TopAndSides),
            ("office", WindowShape::Big),
            ("warm_room", WindowShape::TopAndSides),
        ] {
            let mut sim = backlit_sim(scene, const_window(shape, 5.0), 0.0);
            let mut ctrl = engage(&sim, resume(seed, 0), false);
            sim.run(&mut ctrl, 30.0);
            let what = format!("{scene} {shape:?} from {seed}");
            for (t, o) in over_need(&sim, 8.0, 30.0) {
                assert!((-0.5..=0.8).contains(&o), "{what}: face {o:+.2} stops from its rest at {t:.1} s");
            }
            for s in sim.history.iter().filter(|s| s.t >= 5.0) {
                assert!(s.clip < 0.5, "{what}: clipped share {:.2} at {:.1} s", s.clip, s.t);
            }
            assert!(sim.writes_between(20.0, 30.0).is_empty(), "{what}: {:?}", sim.writes_between(20.0, 30.0));
        }
    }
}

/// L2: daylight rises behind the person for two minutes: the face stays where it was.
#[test]
fn rising_daylight_keeps_the_face() {
    let window = Window::new(
        WindowShape::Big,
        WindowLevel::Ramp {
            from: 1.7,
            to: 6.7,
            secs: 120.0,
        },
    );
    let mut sim = backlit_sim("warm_room", window, 0.02);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 120.0);
    for (t, o) in over_need(&sim, 80.0, 120.0) {
        assert!(o >= -0.6, "face {o:+.2} stops from its rest at {t:.1} s");
    }
}

/// Batches (decisions) that step back from a clipped picture: the highlight guard (2.5.5 step 2) moves by
/// `-step_max` while at least half of the picture is white.
fn guard_batches(sim: &Sim, batches: &[Batch], step_max: f64) -> usize {
    batches
        .iter()
        .filter(|b| b.dx <= -0.95 * step_max)
        .filter(|b| sim.history.iter().any(|s| (s.t - b.t).abs() < 1e-6 && s.clip >= 0.5))
        .count()
}

fn in_window(b: &[Batch], from: f64, until: f64) -> Vec<&Batch> {
    b.iter().filter(|b| b.t >= from && b.t < until).collect()
}

/// L3: a window behind the person, a noisy light: the loop is as quiet as in any other room, at every speed.
#[test]
fn a_backlit_picture_holds_still() {
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        let mut cfg = AutoCfg::default();
        cfg.exposure.speed = speed;
        let mut sim = backlit_sim("office", const_window(WindowShape::TopAndSides, 5.0), 0.05);
        let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
        sim.run(&mut ctrl, 120.0);
        let all = batches(&sim, (-6, 0));
        let late: Vec<Batch> = all.into_iter().filter(|b| b.t >= 20.0).collect();
        let what = format!("{speed:?}: {late:?}");
        assert!(late.len() <= 2, "{what}");
        assert_eq!(guard_batches(&sim, &late, speed.exposure_pace().step_max), 0, "{what}");
        assert!(reversals(&late) <= 1, "{what}");
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
        assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
    }
}

/// L4: a white window that brightens and dims by a stop moves nothing: it weighs the same in the reading and counts
/// as a light source whatever its level.
#[test]
fn a_white_window_that_brightens_or_dims_moves_nothing() {
    let window = Window::new(
        WindowShape::TopAndSides,
        WindowLevel::Wave {
            mid: 5.5,
            amp: 1.0,
            period: 20.0,
        },
    );
    let mut sim = backlit_sim("office", window, 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 120.0);
    assert!(sim.writes_between(20.0, 120.0).is_empty(), "{:?}", sim.writes_between(20.0, 120.0));
}

/// L5: someone walks in front of the window and away again every 15 s: one move each way, no more.
#[test]
fn walking_in_front_of_the_window_moves_once_each_way() {
    let window = Window {
        alternate: Some((WindowShape::Top, 15.0)),
        ..const_window(WindowShape::TopAndSides, 5.0)
    };
    let mut sim = backlit_sim("office", window, 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 90.0);
    let all = batches(&sim, (-6, 0));
    // From the second period on: the first one is the loop finding the room.
    for from in [15.0, 30.0, 45.0, 60.0, 75.0] {
        let period: Vec<&Batch> = in_window(&all, from, from + 15.0);
        let moves: Vec<f64> = period.iter().map(|b| b.dx).collect();
        assert!(period.len() <= 3, "{from} s: {moves:?}");
        let signs: Vec<f64> = moves.iter().copied().filter(|d| d.abs() > 1e-9).collect();
        let swings = signs.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
        assert!(swings <= 1, "{from} s: {moves:?}");
    }
    // At most one reversal per period cannot be the four in 10 s that damp the loop; nothing was handed back.
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
}

/// L6: a window over more than half of the centre weighted picture is not forced white: the clip budget stops the
/// term before the highlight guard would step back.
#[test]
fn a_window_beyond_the_clip_budget_is_not_forced_white() {
    let mut sim = backlit_sim("warm_room", const_window(WindowShape::Wall, 5.0), 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let mut worst = f64::INFINITY;
    while sim.t < 30.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        if let Some(line) = ctrl.exposure_diag() {
            let bl: f64 = line.split("bl=").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).expect("bl=");
            worst = worst.min(bl);
        }
    }
    assert!(worst >= 0.0, "backlight {worst}");
    for s in sim.history.iter().filter(|s| s.t >= 5.0) {
        assert!(s.clip < 0.5, "clipped share {:.2} at {:.1} s", s.clip, s.t);
    }
    assert!(sim.writes_between(20.0, 30.0).is_empty(), "{:?}", sim.writes_between(20.0, 30.0));
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
}

/// L7: the clip budget holds with a camera whose gain gives more or less than the loop thinks.
#[test]
fn the_clip_budget_holds_with_a_wrong_gain_curve() {
    for s_true in [1.5, 5.0] {
        let mut sim = Sim::new(
            SimCfg {
                scene: Some("warm_room"),
                window: Some(const_window(WindowShape::Big, 5.0)),
                s_true,
                ..SimCfg::default()
            },
            dim(-3.5),
        );
        let mut ctrl = engage(&sim, resume(-6, 0), false);
        sim.run(&mut ctrl, 60.0);
        let all = batches(&sim, (-6, 0));
        let step_max = Speed::Normal.exposure_pace().step_max;
        let what = format!("S {s_true}: {all:?}");
        assert!(guard_batches(&sim, &all, step_max) <= 1, "{what}");
        assert!(reversals(&all.into_iter().filter(|b| b.t >= 10.0).collect::<Vec<_>>()) <= 2, "{what}");
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
    }
}

/// L8: the card says why while the term is on, and says nothing in an ordinary room; the log line carries `bl=`.
///
/// The spec names L1's scene (window +5). There the loop rests where the window is white and the term is 0 (the good
/// resting point), so the suffix is checked on a big window of +3 stops, which stays under white and keeps the term on
/// (13.7: a *top and sides* window of +3 now rests at -0.57 with the term at 0.17, below the flag's edge).
#[test]
fn a_backlit_hold_says_so() {
    const NOTE: &str = " · backlight: exposing for the middle";
    let hold = |sim: &mut Sim, ctrl: &mut AutoController| {
        sim.run(ctrl, 30.0);
        assert_eq!(state(ctrl), ExposureState::Holding, "{:?}", ctrl.exposure_diag());
        ctrl.status().exposure.unwrap().message.unwrap()
    };
    let mut sim = backlit_sim("office", const_window(WindowShape::Big, 3.0), 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let message = hold(&mut sim, &mut ctrl);
    assert!(message.ends_with(NOTE), "{message}");
    let line = ctrl.exposure_diag().unwrap();
    assert!(line.contains("bl=+0.") || line.contains("bl=+1."), "{line}");
    assert!(!line.contains("src=0.00"), "{line}");
    // At the good resting point (window white, +5) the term is 0 and the card does not say it.
    let mut sim = backlit_sim("office", const_window(WindowShape::TopAndSides, 5.0), 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let message = hold(&mut sim, &mut ctrl);
    assert!(!message.contains("backlight"), "{message}");
    // Case 1's room: no window, no suffix, `bl=+0.00`.
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let message = hold(&mut sim, &mut ctrl);
    assert!(!message.contains("backlight"), "{message}");
    assert!(ctrl.exposure_diag().unwrap().contains("bl=+0.00 src=0.00"), "{:?}", ctrl.exposure_diag());
}

/// The flag has a hysteresis: on from half a stop, off at a quarter, unchanged in between.
#[test]
fn the_backlight_flag_does_not_chatter() {
    use super::exposure::backlit_after;
    assert!(backlit_after(false, 0.5) && backlit_after(true, 0.5) && backlit_after(false, 3.0));
    assert!(!backlit_after(true, 0.25) && !backlit_after(false, 0.25) && !backlit_after(true, 0.0));
    for between in [0.26, 0.3, 0.4, 0.49] {
        assert!(backlit_after(true, between), "{between}");
        assert!(!backlit_after(false, between), "{between}");
    }
    // A reading that wobbles across 0.4 (inside the two edges) never flips it either way.
    let (mut on, mut off) = (true, false);
    for b in [0.45, 0.35, 0.48, 0.3, 0.27, 0.49] {
        on = backlit_after(on, b);
        off = backlit_after(off, b);
    }
    assert!(on && !off);
}

// --- 11. Front-lit subject in a dark room (docs/AUTO-CONTROLS.md 11.9) ---

/// A kit scene with a lamp-lit person (`LampLit`: the room darker than the evenly lit one) in the owner's preset
/// (`AutoCfg::default()`: target 0.45, Longest "fit", Most gain 75 %, Normal; F 30).
fn lamp_cfg(scene: &'static str, lamp: LampLit) -> SimCfg {
    SimCfg {
        scene: Some(scene),
        lamp_lit: Some(lamp),
        ..SimCfg::default()
    }
}

/// The two ways into a room: from the dark side (a camera far under what the room needs) and from the bright side (a
/// resume point with a lot of gain): `(exposure, gain, seeded)`.
const SEEDS: [(i32, i32, bool); 2] = [(-8, 0, false), (-5, 75, true)];

/// Whether W4 (2.7: four reversals of moves of 0.15 stop or more within 10 s) would have damped the loop on these
/// batches: the damping itself is not visible from outside.
fn would_damp(b: &[Batch]) -> bool {
    let (mut last, mut times) = (None::<f64>, std::collections::VecDeque::new());
    for b in b.iter().filter(|b| b.dx.abs() >= 0.15) {
        if last.is_some_and(|s| s != b.dx.signum()) {
            times.push_back(b.t);
        }
        last = Some(b.dx.signum());
        while times.front().is_some_and(|t| b.t - t > 10.0) {
            times.pop_front();
        }
        if times.len() >= 4 {
            return true;
        }
    }
    false
}

/// The replayed desk's face and room (coded 0-255) after `from` seconds: the medians and the face's 90th percentile.
fn desk_after(sim: &Sim, from: f64) -> (f64, f64, f64) {
    let (mut face, mut room): (Vec<f64>, Vec<f64>) =
        sim.history.iter().filter(|s| s.t >= from).filter_map(|s| s.face_room).unzip();
    face.sort_by(f64::total_cmp);
    room.sort_by(f64::total_cmp);
    let at = |v: &[f64], p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    (at(&face, 0.5), at(&room, 0.5), at(&face, 0.9))
}

/// The recorded desk (frames from `a` to `b` s) played to the exposure loop alone, 120 s, on a camera that starts at
/// `start`; the frames are what the camera made at `-5 + gs(25)` stops of light.
fn desk_run(a: f64, b: f64, start: (i32, i32, bool), speed: Speed) -> (Sim, AutoController) {
    let x_rec = -5.0 + SimCfg::default().s_true * 25.0 / 100.0;
    let mut sim = Sim::new(
        SimCfg {
            start_e: start.0,
            start_g: start.1,
            replay: Some(Replay::with_exposure(crate::fx::awb::real::between(a, b), x_rec)),
            ..SimCfg::default()
        },
        dim(-4.0),
    );
    let mut cfg = AutoCfg::default();
    cfg.exposure.speed = speed;
    let mut ctrl = engage_with(&sim, cfg, resume(start.0, start.1), start.2);
    sim.run(&mut ctrl, 120.0);
    (sim, ctrl)
}

#[test]
fn a_lamp_lit_room_of_zero_stops_leaves_the_scene_alone() {
    let room = |lamp| {
        Sim::new(
            SimCfg {
                scene: Some("office"),
                lamp_lit: lamp,
                ..SimCfg::default()
            },
            dim(-3.5),
        )
    };
    let (plain, lit) = (room(None), room(Some(LampLit::new(0.0))));
    assert_eq!(plain.calibration(), lit.calibration());
    assert_eq!(plain.picture().cells, lit.picture().cells);
    assert_eq!(plain.face_over_need(), plain.stops_over_need());
    assert_eq!(plain.face_over_need(), lit.face_over_need());
    // Room -3: the person's skin keeps its light, the wall has 3 stops less, the shirt 1.5 (cells at the start state
    // are under white, so the ratio is exact), and the face level moves `face_over_need` and nothing else.
    let dark = room(Some(LampLit::new(-3.0)));
    assert_eq!(plain.calibration(), dark.calibration());
    let (a, b) = (plain.picture(), dark.picture());
    let (skin, wall, shirt) = (8 * 16 + 7, 0, 12 * 16 + 6);
    assert_eq!(a.cells[skin], b.cells[skin]);
    let ratio = |i: usize| f64::from(b.cells[i][1]) / f64::from(a.cells[i][1]);
    assert!((ratio(wall) - 0.125).abs() < 1e-5, "{}", ratio(wall));
    assert!((ratio(shirt) - 0.125_f64.sqrt()).abs() < 1e-5, "{}", ratio(shirt));
    let lamp = LampLit {
        room: -3.0,
        face: WindowLevel::Toggle { a: 0.0, b: -3.0, every: 15.0 },
    };
    assert_eq!(room(Some(lamp)).face_over_need(), plain.stops_over_need());
}

/// L9: a lamp-lit face in a dark room, from the dark side and from the bright side of the room: the face ends near the
/// brightest part of a lit room (0.4-1.3 stops over where the evenly lit room rests), not white.
#[test]
fn a_lamp_lit_face_is_not_white_after_open() {
    for (scene, room) in [("office", -3.0), ("office", -4.0), ("warm_room", -3.0), ("warm_room", -4.0)] {
        let mut ends = Vec::new();
        for (e, g, seeded) in SEEDS {
            let cfg = SimCfg {
                start_e: e,
                start_g: g,
                ..lamp_cfg(scene, LampLit::new(room))
            };
            let mut sim = Sim::new(cfg, dim(-5.0));
            let mut ctrl = engage(&sim, resume(e, g), seeded);
            sim.run(&mut ctrl, 30.0);
            let what = format!("{scene}, room {room}, from {e}/{g}");
            for s in sim.history.iter().filter(|s| s.t >= 8.0) {
                assert!((0.4..=1.3).contains(&s.face_over_need), "{what}: face {:+.2} at {:.1} s", s.face_over_need, s.t);
                assert!(s.clip < 0.02, "{what}: clipped share {:.3} at {:.1} s", s.clip, s.t);
            }
            assert!(sim.writes_between(20.0, 30.0).is_empty(), "{what}: {:?}", sim.writes_between(20.0, 30.0));
            ends.push(sim.history.last().expect("ticks").face_over_need);
        }
        assert!((ends[0] - ends[1]).abs() <= 0.35, "{scene}, room {room}: the two sides rest at {ends:?}");
    }
}

/// L10: the recorded desk (a face lit by a lamp, a dark room) on a camera that starts anywhere: the face is lit, not
/// white, the room is darker than the face, and the loop is quiet.
#[test]
fn the_recorded_desk_settles_with_the_face_lit_not_white() {
    for (a, b) in [(111.0, 116.0), (40.0, 44.5)] {
        for start in [(-5, 0, false), (-5, 75, true)] {
            let (sim, ctrl) = desk_run(a, b, start, Speed::Normal);
            let (face, room, _) = desk_after(&sim, 30.0);
            let what = format!("frames {a}-{b} s from {start:?}: face {face:.0}, room {room:.0}");
            assert!(face <= 150.0 && room <= 60.0, "{what}");
            let late = sim.writes_between(30.0, 120.0);
            assert!(late.len() <= 2, "{what}: {late:?}");
            assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
            assert!(!would_damp(&batches(&sim, (start.0, start.1))), "{what}");
            assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
        }
    }
}

/// L11: the desk with the face moving between two lit positions and the edge of the middle: the loop follows without
/// hunting.
#[test]
fn a_moving_lit_face_is_followed_without_hunting() {
    for speed in [Speed::Normal, Speed::Fast] {
        for start in [(-5, 0, false), (-5, 75, true)] {
            let (sim, ctrl) = desk_run(108.0, 116.0, start, speed);
            let (_, _, face90) = desk_after(&sim, 30.0);
            let what = format!("{speed:?} from {start:?}: face 90th percentile {face90:.0}");
            assert!(face90 <= 175.0, "{what}");
            assert!(!would_damp(&batches(&sim, (start.0, start.1))), "{what}");
            assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
            assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
        }
    }
}

/// The batches of each `every`-second period from the second on (the first is the loop finding the room).
fn periods(b: &[Batch], every: f64, until: f64) -> Vec<(f64, Vec<&Batch>)> {
    let mut out = Vec::new();
    let mut from = every;
    while from + every <= until + 1e-9 {
        out.push((from, in_window(b, from, from + every)));
        from += every;
    }
    out
}

/// L12: the lamp is switched on and off every 15 s: one settle each way.
#[test]
fn lamp_on_and_off_moves_once_each_way() {
    let lamp = LampLit {
        room: -3.0,
        face: WindowLevel::Toggle { a: 0.0, b: -3.0, every: 15.0 },
    };
    let mut sim = Sim::new(lamp_cfg("office", lamp), dim(-6.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 120.0);
    let all = batches(&sim, (-6, 0));
    for (from, period) in periods(&all, 15.0, 120.0) {
        let moves: Vec<f64> = period.iter().map(|b| b.dx).collect();
        assert!(period.len() <= 6, "{from} s: {moves:?}");
        let signs: Vec<f64> = moves.iter().copied().filter(|d| d.abs() > 1e-9).collect();
        assert!(signs.windows(2).filter(|w| w[0].signum() != w[1].signum()).count() <= 1, "{from} s: {moves:?}");
        if let Some(last) = period.last() {
            assert!(last.t <= from + 4.0, "{from} s: the last batch at {:.1} s", last.t);
        }
    }
    assert!(!would_damp(&all));
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
    // At the end of each period with the lamp on the face is lit, not white.
    for end in [15.0, 45.0, 75.0, 105.0] {
        let at = sim.history.iter().find(|s| s.t >= end - 0.15).expect("ticks");
        assert!((0.4..=1.3).contains(&at.face_over_need), "lamp on, {:.1} s: face {:+.2}", at.t, at.face_over_need);
    }
}

/// L13: the user leans toward and away from the lamp (the face a stop either side): no pumping.
#[test]
fn leaning_toward_and_away_does_not_pump() {
    for (speed, every) in [(Speed::Slow, 15.0), (Speed::Normal, 15.0), (Speed::Fast, 15.0), (Speed::Normal, 4.0)] {
        let lamp = LampLit {
            room: -3.0,
            face: WindowLevel::Toggle { a: 1.0, b: -1.0, every },
        };
        let mut cfg = AutoCfg::default();
        cfg.exposure.speed = speed;
        let mut sim = Sim::new(lamp_cfg("office", lamp), dim(-6.0));
        let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
        sim.run(&mut ctrl, 120.0);
        let all = batches(&sim, (-6, 0));
        let what = format!("{speed:?}, every {every} s");
        assert!(!would_damp(&all), "{what}");
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
        assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
        if every >= 15.0 {
            for (from, period) in periods(&all, every, 120.0) {
                let moves: Vec<f64> = period.iter().map(|b| b.dx).collect();
                let signs: Vec<f64> = moves.iter().copied().filter(|d| d.abs() > 1e-9).collect();
                let swings = signs.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
                assert!(swings <= 1, "{what}, {from} s: {moves:?}");
            }
        }
    }
}

/// A lamp-lit office (room -3) whose face follows `face`, from a dark camera, at `speed`: run for `secs`. With
/// `front_off` the front light term is forced to 0 (the baseline of 13.13: where the loop goes without the term).
fn swinging_face_run(speed: Speed, face: WindowLevel, secs: f64, front_off: bool) -> (Sim, AutoController) {
    let lamp = LampLit { room: -3.0, face };
    let mut cfg = AutoCfg::default();
    cfg.exposure.speed = speed;
    let mut sim = Sim::new(lamp_cfg("office", lamp), dim(-6.0));
    let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
    if front_off {
        meter::hook::with_front_off(|| sim.run(&mut ctrl, secs));
    } else {
        sim.run(&mut ctrl, secs);
    }
    (sim, ctrl)
}

/// The highest `face_over_need` from 20 s on (the face at its brightest, in the loop's rest).
fn face_peak(sim: &Sim) -> f64 {
    sim.history.iter().filter(|s| s.t >= 20.0).map(|s| s.face_over_need).fold(f64::NEG_INFINITY, f64::max)
}

/// The faces of 13.11 and 13.13, `(label, level)`: a lamp that comes and goes every 2-3.5 s or a face a stop or half
/// a stop either side (a user leaning, a hand passing the lamp), and a face that swings smoothly.
fn swinging_faces() -> Vec<(String, WindowLevel)> {
    let mut v = Vec::new();
    for (a, b) in [(1.0, -1.0), (0.0, -3.0), (1.0, 0.0), (0.5, -0.5)] {
        for every in [2.0, 2.5, 3.0, 3.5] {
            v.push((format!("face {a:+}/{b:+} every {every} s"), WindowLevel::Toggle { a, b, every }));
        }
    }
    for period in [3.0, 5.0, 8.0, 12.0] {
        v.push((format!("face wave +-1 stop, period {period} s"), WindowLevel::Wave { mid: 0.0, amp: 1.0, period }));
    }
    v
}

/// L29 (13.11, 13.13): a face that comes and goes every 2-3.5 s (leaning, a hand passing the lamp), or stays lit and
/// changes by half a stop to two (a lean toward the lamp, a smooth wave of 3-12 s): the loop darkens at once and keeps
/// the lit reading while the face comes back, so it does not reverse often enough for W4 to hand back, damps no more
/// than it does with the front term off, and the face at its brightest rests near the evenly lit rest's brightest
/// part (0.4-1.35 stops over, L9's 0.4-1.3 and a step: not white, and not left dark). One test per speed (they run side by side); 60 s
/// are enough: the hand-backs came at 18-45 s.
fn swinging_faces_hold_still(speed: Speed) {
    let mut failed = Vec::new();
    for (what, face) in swinging_faces() {
        let what = format!("{speed:?}, {what}");
        let (sim, ctrl) = swinging_face_run(speed, face, 60.0, false);
        let all = batches(&sim, (-6, 0));
        let peak = face_peak(&sim);
        if let Some(&(t, _, _)) = sim.handbacks.first() {
            failed.push(format!("{what}: handed back at {t:.1} s, {} batches", all.len()));
        } else if state(&ctrl) == ExposureState::Hunting {
            failed.push(format!("{what}: Hunting"));
        } else if would_damp(&all) {
            // The damping the term-off loop does too is the baseline; more is not.
            let (base, _) = swinging_face_run(speed, face, 60.0, true);
            if !would_damp(&batches(&base, (-6, 0))) {
                failed.push(format!(
                    "{what}: would damp (not without the term), {} batches, {} reversals in 10 s",
                    all.len(),
                    most_reversals_in_10_s(&all)
                ));
            }
        }
        // Measured: 0.65-1.31 at the three speeds (one gain step of the loop's own following on top of L9's 1.3).
        if !(0.4..=1.35).contains(&peak) {
            failed.push(format!("{what}: the face peaks at {peak:+.2} over its rest"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn a_face_that_comes_and_goes_quickly_does_not_hunt_at_slow() {
    swinging_faces_hold_still(Speed::Slow);
}

#[test]
fn a_face_that_comes_and_goes_quickly_does_not_hunt_at_normal() {
    swinging_faces_hold_still(Speed::Normal);
}

#[test]
fn a_face_that_comes_and_goes_quickly_does_not_hunt_at_fast() {
    swinging_faces_hold_still(Speed::Fast);
}

/// L31 (13.13 C): a lamp-lit face in a room too dark for the frame-rate limit, on a camera that refuses the stop past it
/// (the sim's `ignore`: it reads back the old value): asked once, no hand-back, nothing blocked, the loop rests at the
/// limit with the face one stop under where a camera that takes the stop leaves it. The sim cannot tell the lit hold's
/// shift by the refused stop from none at the end of a run (the shift only matters for 6 s; the exposure loop unit
/// tests `a_refused_exposure_write_shifts_nothing` and the two after it pin it); this is the whole-loop guard.
#[test]
fn a_lamp_lit_face_on_a_camera_that_refuses_a_longer_exposure_rests_at_the_limit() {
    let rest = |refuses: bool| {
        let cfg = SimCfg {
            start_e: -5,
            start_g: 100,
            ..lamp_cfg("office", LampLit::new(-3.0))
        };
        let mut sim = Sim::new(cfg, dim(-1.5));
        if refuses {
            sim.faults.ignore = vec![ControlKey::Exposure];
        }
        let mut ctrl = engage_with(&sim, owner_cfg(), resume(-5, 100), true);
        sim.run(&mut ctrl, 40.0);
        (sim, ctrl)
    };
    let (taken, _) = rest(false);
    let (refused, ctrl) = rest(true);
    assert_eq!(exposure_attempts(&refused), vec![-4], "asked once");
    assert_eq!(refused.device(), (-5, 100));
    assert!(refused.handbacks.is_empty(), "{:?}", refused.handbacks);
    assert!(ctrl.learned().blocked.is_empty(), "{:?}", ctrl.learned().blocked);
    let (end_taken, end_refused) = (taken.history.last().unwrap().face_over_need, refused.history.last().unwrap().face_over_need);
    assert!((end_taken - end_refused - 1.0).abs() <= 0.1, "taken {end_taken:+.2}, refused {end_refused:+.2}");
}

/// L14: a window behind the person and a lamp-lit face: section 10's result (the ceiling is off, `G` is over 0.25).
#[test]
fn a_window_and_a_lamp_lit_face_keep_section_10() {
    let cfg = SimCfg {
        window: Some(const_window(WindowShape::TopAndSides, 4.0)),
        ..lamp_cfg("office", LampLit::new(-3.0))
    };
    let mut sim = Sim::new(cfg, dim(-5.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 5.0);
    while sim.t < 30.0 {
        sim.run(&mut ctrl, sim.t + 1.0);
        let r = crate::auto_controls::meter::measure_exposure(&sim.picture(), 0.45);
        assert_eq!(r.front, 0.0, "{:.0} s: {r:?}", sim.t);
    }
    for (t, o) in sim.history.iter().filter(|s| s.t >= 8.0).map(|s| (s.t, s.face_over_need)) {
        assert!((0.6..=1.3).contains(&o), "face {o:+.2} at {t:.1} s");
    }
}

/// L15: a corner window (the desk's kind) with a lamp-lit face: the ceiling acts; with an evenly lit room it does not.
#[test]
fn a_corner_window_with_a_lamp_lit_face() {
    let corner = Some(const_window(WindowShape::Corner, 4.0));
    let mut ends = Vec::new();
    for (e, g, seeded) in SEEDS {
        let cfg = SimCfg {
            start_e: e,
            start_g: g,
            window: corner,
            ..lamp_cfg("office", LampLit::new(-4.0))
        };
        let mut sim = Sim::new(cfg, dim(-5.0));
        let mut ctrl = engage(&sim, resume(e, g), seeded);
        sim.run(&mut ctrl, 30.0);
        for s in sim.history.iter().filter(|s| s.t >= 8.0) {
            assert!((0.2..=1.0).contains(&s.face_over_need), "from {e}/{g}: face {:+.2} at {:.1} s", s.face_over_need, s.t);
        }
        ends.push(sim.history.last().expect("ticks").face_over_need);
    }
    assert!((ends[0] - ends[1]).abs() <= 0.35, "the two sides rest at {ends:?}");
    // The even office with the same window: the term is 0 and the face rests where section 10 puts it.
    let mut sim = Sim::new(SimCfg { window: corner, scene: Some("office"), ..SimCfg::default() }, dim(-5.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 30.0);
    let r = crate::auto_controls::meter::measure_exposure(&sim.picture(), 0.45);
    assert_eq!(r.front, 0.0, "{r:?}");
    let o = sim.history.last().expect("ticks").face_over_need;
    assert!((-0.3..=0.3).contains(&o), "even office, corner window: face {o:+.2}");
}

/// L16: a lamp-lit room with a noisy light: as quiet as any other room, at every speed.
#[test]
fn a_lamp_lit_room_holds_still() {
    for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
        let mut cfg = AutoCfg::default();
        cfg.exposure.speed = speed;
        let mut sim = Sim::new(SimCfg { jitter: 0.05, ..lamp_cfg("office", LampLit::new(-4.0)) }, dim(-5.0));
        let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
        sim.run(&mut ctrl, 120.0);
        let all = batches(&sim, (-6, 0));
        let late: Vec<Batch> = all.into_iter().filter(|b| b.t >= 20.0).collect();
        let what = format!("{speed:?}: {late:?}");
        assert!(late.len() <= 2, "{what}");
        assert!(reversals(&late) <= 1, "{what}");
        assert!(!would_damp(&late), "{what}");
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
    }
}

/// L17: the card says why while the term is on, and says nothing in an ordinary room; the log line carries `fl=`.
#[test]
fn a_lamp_lit_hold_says_so() {
    const NOTE: &str = " · dark room: exposing for the lit middle";
    let hold = |sim: &mut Sim, ctrl: &mut AutoController| {
        sim.run(ctrl, 30.0);
        assert_eq!(state(ctrl), ExposureState::Holding, "{:?}", ctrl.exposure_diag());
        ctrl.status().exposure.unwrap().message.unwrap()
    };
    let mut sim = Sim::new(SimCfg { start_e: -8, ..lamp_cfg("office", LampLit::new(-4.0)) }, dim(-5.0));
    let mut ctrl = engage(&sim, resume(-8, 0), false);
    let message = hold(&mut sim, &mut ctrl);
    assert!(message.ends_with(NOTE), "{message}");
    let line = ctrl.exposure_diag().unwrap();
    assert!(line.contains("fl=-") && line.contains("gap=+"), "{line}");
    // Case 1's room: no suffix, `fl=+0.00`.
    let mut sim = Sim::new(SimCfg::default(), dim(-3.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    let message = hold(&mut sim, &mut ctrl);
    assert!(!message.contains("dark room"), "{message}");
    assert!(ctrl.exposure_diag().unwrap().contains("fl=+0.00 gap="), "{:?}", ctrl.exposure_diag());
}

/// A pause (a conflict, the user's Stop) forgets what the last decision saw: the first "Holding" text after the resume
/// has no "backlight" or "dark room" suffix left over from before it (13.11).
#[test]
fn a_resume_does_not_show_the_suffix_of_before_the_pause() {
    let lamp = lamp_cfg("office", LampLit::new(-4.0));
    let backlit = SimCfg {
        scene: Some("office"),
        window: Some(const_window(WindowShape::Big, 3.0)),
        ..SimCfg::default()
    };
    for (cfg, light, note) in [
        (lamp, -5.0, " · dark room: exposing for the lit middle"),
        (backlit, -3.5, " · backlight: exposing for the middle"),
    ] {
        let mut sim = Sim::new(SimCfg { start_e: -8, ..cfg }, dim(light));
        let mut ctrl = engage(&sim, resume(-8, 0), false);
        sim.run(&mut ctrl, 30.0);
        let message = |ctrl: &AutoController| ctrl.status().exposure.unwrap().message.unwrap();
        assert!(message(&ctrl).ends_with(note), "before the pause: {}", message(&ctrl));
        sim.pause();
        sim.run(&mut ctrl, 31.0);
        sim.resume();
        // The first ticks after the resume, before a picture of the new session reaches the loop: nothing has judged
        // the room yet.
        sim.no_samples = true;
        sim.run(&mut ctrl, sim.t + 0.3);
        assert_eq!(state(&ctrl), ExposureState::Holding, "{:?}", ctrl.exposure_diag());
        assert!(!message(&ctrl).contains(note), "after the resume: {}", message(&ctrl));
        // And the suffix is back once the loop has judged the picture again.
        sim.no_samples = false;
        sim.run(&mut ctrl, sim.t + 5.0);
        assert!(message(&ctrl).ends_with(note), "five seconds on: {}", message(&ctrl));
    }
}

/// L18: the lamp-lit face does not cost the frame rate: the loop needs less light, so it keeps 30 fps and does not
/// reach the gain limit (section 10 reads the room 1.5 stops over the limit and lowers the rate).
#[test]
fn a_lamp_lit_face_keeps_the_frame_rate() {
    let mut sim = Sim::new(lamp_cfg("office", LampLit::new(-5.0)), dim(-5.5));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 10.0);
    while sim.t < 40.0 {
        sim.run(&mut ctrl, sim.t + 0.1);
        assert!(!lowered(&ctrl), "{:.1} s: {:?}", sim.t, ctrl.exposure_diag());
    }
    assert!(sim.fps_between(10.0, 40.0) >= 29.5, "{} fps", sim.fps_between(10.0, 40.0));
    let (e, g) = sim.device();
    assert!(g < 75, "gain {g} at E {e}");
}

// --- 12. Big window behind the subject (docs/AUTO-CONTROLS.md 12.8) ---

/// L19: a big window 3.5-6 stops over the face, from the dark side: the face is on target, not 1.5 stops under, the
/// window does not clip, and the loop is quiet by 20 s.
#[test]
fn a_big_window_is_on_target_from_the_dark_side() {
    let cases = [
        ("office", 3.5),
        ("office", 4.0),
        ("office", 6.0),
        ("warm_room", 4.0),
        ("warm_room", 6.0),
    ];
    for (scene, stops) in cases {
        for seed in [-6, -8] {
            let mut sim = backlit_sim(scene, const_window(WindowShape::Big, stops), 0.0);
            let mut ctrl = engage(&sim, resume(seed, 0), false);
            sim.run(&mut ctrl, 30.0);
            let what = format!("{scene} Big +{stops} from {seed}");
            for (t, o) in over_need(&sim, 8.0, 30.0) {
                assert!((-0.5..=0.5).contains(&o), "{what}: face {o:+.2} stops from its rest at {t:.1} s");
            }
            for s in sim.history.iter().filter(|s| s.t >= 5.0) {
                assert!(s.clip < 0.5, "{what}: clipped share {:.2} at {:.1} s", s.clip, s.t);
            }
            assert!(sim.writes_between(20.0, 30.0).is_empty(), "{what}: {:?}", sim.writes_between(20.0, 30.0));
        }
    }
}

/// L20: the same, a noisy light, every speed, one minute (13.11: was two): as quiet as any other room.
#[test]
fn a_big_window_holds_still_at_every_speed() {
    for scene in ["office", "warm_room"] {
        for speed in [Speed::Slow, Speed::Normal, Speed::Fast] {
            for seed in [-6, -8] {
                let mut cfg = AutoCfg::default();
                cfg.exposure.speed = speed;
                let mut sim = backlit_sim(scene, const_window(WindowShape::Big, 4.0), 0.05);
                let mut ctrl = engage_with(&sim, cfg, resume(seed, 0), false);
                sim.run(&mut ctrl, 60.0);
                let all = batches(&sim, (seed, 0));
                let late: Vec<Batch> = all.into_iter().filter(|b| b.t >= 20.0).collect();
                let what = format!("{scene} {speed:?} from {seed}: {late:?}");
                assert!(late.len() <= 2, "{what}");
                assert_eq!(guard_batches(&sim, &late, speed.exposure_pace().step_max), 0, "{what}");
                assert!(reversals(&late) <= 1, "{what}");
                assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
                assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
                for (t, o) in over_need(&sim, 20.0, 120.0) {
                    assert!((-0.5..=0.5).contains(&o), "{what}: face {o:+.2} stops from its rest at {t:.1} s");
                }
            }
        }
    }
}

/// L21: daylight rises behind the person (L2's ramp): the face is back near its rest soon after the window passes
/// the point where it turns white, and stays there.
#[test]
fn rising_daylight_in_a_big_window_recovers_sooner() {
    let window = Window::new(
        WindowShape::Big,
        WindowLevel::Ramp {
            from: 1.7,
            to: 6.7,
            secs: 120.0,
        },
    );
    for scene in ["warm_room", "office"] {
        let mut sim = backlit_sim(scene, window, 0.02);
        let mut ctrl = engage(&sim, resume(-6, 0), false);
        sim.run(&mut ctrl, 120.0);
        let back = over_need(&sim, 10.0, 120.0).into_iter().find(|(_, o)| *o >= -0.5).map(|(t, _)| t);
        assert!(back.is_some_and(|t| t <= 48.0), "{scene}: face back within 0.5 stop at {back:?} s");
        for (t, o) in over_need(&sim, 60.0, 120.0) {
            assert!(o >= -0.6, "{scene}: face {o:+.2} stops from its rest at {t:.1} s");
        }
    }
}

/// L22: someone walks in front of a big window and away every 15 s: one move each way, no more.
#[test]
fn walking_in_front_of_a_big_window_moves_once_each_way() {
    let window = Window {
        alternate: Some((WindowShape::Top, 15.0)),
        ..const_window(WindowShape::Big, 4.0)
    };
    let mut sim = backlit_sim("office", window, 0.0);
    let mut ctrl = engage(&sim, resume(-6, 0), false);
    sim.run(&mut ctrl, 90.0);
    let all = batches(&sim, (-6, 0));
    for from in [15.0, 30.0, 45.0, 60.0, 75.0] {
        let period: Vec<&Batch> = in_window(&all, from, from + 15.0);
        let moves: Vec<f64> = period.iter().map(|b| b.dx).collect();
        assert!(period.len() <= 3, "{from} s: {moves:?}");
        let signs: Vec<f64> = moves.iter().copied().filter(|d| d.abs() > 1e-9).collect();
        let swings = signs.windows(2).filter(|w| w[0].signum() != w[1].signum()).count();
        assert!(swings <= 1, "{from} s: {moves:?}");
    }
    assert!(sim.handbacks.is_empty(), "{:?}", sim.handbacks);
}

// --- 13. Mid-bright windows and dark skin before a bright wall (docs/AUTO-CONTROLS.md 13.8) ---

/// The kit's three skin tones (`SimCfg::skin`): light, medium (-0.61 stop), dark (-1.79 stop).
const SKIN_TONES: [&str; 3] = ["skin_light", "skin_mid", "skin_dark"];

/// A kit scene with skin `skin`, an optional window and a camera that starts at `start_e` / 0.
fn skin_cfg(scene: &'static str, skin: &'static str, window: Option<Window>, start_e: i32) -> SimCfg {
    SimCfg {
        scene: Some(scene),
        window,
        skin: Some(skin),
        start_e,
        ..SimCfg::default()
    }
}

/// The two ways into a bright-wall room: from the dark side and from the bright side (the camera far over what the
/// room needs).
const WALL_SEEDS: [(i32, i32, bool); 2] = [(-8, 0, false), (-2, 0, false)];

/// The front light term of the picture the landed state shows now (what the meter says about the loop's own
/// picture).
fn front_now(sim: &Sim) -> f64 {
    crate::auto_controls::meter::measure_exposure(&sim.picture(), 0.45).front
}

#[test]
fn a_skin_override_keeps_the_scene_calibrated() {
    let room = |scene, skin| {
        Sim::new(
            SimCfg {
                scene: Some(scene),
                skin,
                ..SimCfg::default()
            },
            dim(-3.5),
        )
    };
    let plain = room("office", None);
    let light = room("office", Some("skin_light"));
    assert_eq!(plain.calibration(), light.calibration());
    assert_eq!(plain.picture().cells, light.picture().cells);
    assert_eq!(plain.stops_over_need(), light.stops_over_need());
    // The dark skin is the scene `office_dark_skin`, calibrated with it.
    let (kit, dark) = (room("office_dark_skin", None), room("office", Some("skin_dark")));
    assert_eq!(kit.calibration(), dark.calibration());
    assert_eq!(kit.picture().cells, dark.picture().cells);
    assert_ne!(plain.picture().cells, dark.picture().cells);
    assert_ne!(plain.calibration(), dark.calibration());
    // Medium skin: only the skin cells differ from the light-skinned picture.
    let mid = room("office", Some("skin_mid"));
    let (a, b) = (plain.picture(), mid.picture());
    let skin_cells = (0..256).filter(|&i| a.cells[i] != b.cells[i]).count();
    assert!(skin_cells > 0, "the medium skin changed nothing");
}

/// L23: a face before a bright wall with a darker part in the room is not darkened by the dark-room cap, whatever the
/// skin tone, and rests over its evenly lit level.
#[test]
fn a_face_before_a_bright_wall_is_not_darkened_for_any_skin_tone() {
    for scene in ["office", "warm_room"] {
        for skin in SKIN_TONES {
            for shape in [WindowShape::TopAndSides, WindowShape::Top, WindowShape::Big] {
                for (e, g, seeded) in WALL_SEEDS {
                    let mut sim = Sim::new(skin_cfg(scene, skin, Some(const_window(shape, -2.0)), e), dim(-6.0));
                    let mut ctrl = engage(&sim, resume(e, g), seeded);
                    let what = format!("{scene} {skin} {shape:?} -2 from {e}/{g}");
                    while sim.t < 20.0 - 1e-6 {
                        sim.run(&mut ctrl, sim.t + 1.0);
                        if sim.t >= 10.0 - 1e-6 {
                            let front = front_now(&sim);
                            assert!(front.abs() <= 0.01, "{what}: front {front:+.3} at {:.0} s", sim.t);
                        }
                    }
                    for (t, o) in over_need(&sim, 10.0, 20.0) {
                        assert!(o >= 0.4, "{what}: face {o:+.2} stops from its rest at {t:.1} s");
                    }
                }
            }
        }
    }
}

/// L24: a lamp-lit face of any skin tone in a dark room rests as before 13: the cap acts as it did. One test per skin
/// tone (13.11), so they run side by side.
fn a_lamp_lit_face_rests_as_before(skin: &'static str, band: (f64, f64)) {
    for scene in ["office", "warm_room"] {
        for room in [-3.0, -4.0, -5.0] {
            for (e, g, seeded) in SEEDS {
                let cfg = SimCfg {
                    skin: Some(skin),
                    start_e: e,
                    start_g: g,
                    ..lamp_cfg(scene, LampLit::new(room))
                };
                let mut sim = Sim::new(cfg, dim(-5.0));
                let mut ctrl = engage(&sim, resume(e, g), seeded);
                sim.run(&mut ctrl, 30.0);
                let what = format!("{scene} {skin} room {room} from {e}/{g}");
                for s in sim.history.iter().filter(|s| s.t >= 10.0) {
                    assert!(
                        (band.0..=band.1).contains(&s.face_over_need),
                        "{what}: face {:+.2} at {:.1} s",
                        s.face_over_need,
                        s.t
                    );
                }
                let front = front_now(&sim);
                let at_most = match (skin, room as i32) {
                    ("skin_light", _) => Some(-0.8),
                    ("skin_mid", -3) => Some(-0.4),
                    ("skin_mid", _) => Some(-0.9),
                    ("skin_dark", -5) => Some(-0.7),
                    _ => None,
                };
                if let Some(limit) = at_most {
                    assert!(front <= limit, "{what}: front {front:+.2} at 30 s (needs <= {limit})");
                }
                assert!(sim.writes_between(20.0, 30.0).is_empty(), "{what}: {:?}", sim.writes_between(20.0, 30.0));
            }
        }
    }
}

#[test]
fn a_lamp_lit_face_of_light_skin_rests_as_before() {
    a_lamp_lit_face_rests_as_before("skin_light", (0.4, 1.3));
}

#[test]
fn a_lamp_lit_face_of_medium_skin_rests_as_before() {
    a_lamp_lit_face_rests_as_before("skin_mid", (0.9, 1.7));
}

#[test]
fn a_lamp_lit_face_of_dark_skin_rests_as_before() {
    a_lamp_lit_face_rests_as_before("skin_dark", (1.7, 2.3));
}

/// L25: a window about 3 stops over the face, from the dark side: the term starts and the face comes up.
#[test]
fn a_window_three_stops_over_the_face_is_lifted_from_the_dark_side() {
    // (scene, skin, shape, window stops over the light-skin face, the face at least this far from its rest)
    let cases = [
        ("office", "skin_light", WindowShape::Big, 3.0, -1.0),
        ("warm_room", "skin_light", WindowShape::Big, 3.5, -0.3),
        ("warm_room", "skin_light", WindowShape::TopAndSides, 3.0, -0.7),
        ("office", "skin_light", WindowShape::Top, 2.5, -0.5),
        ("office", "skin_mid", WindowShape::Big, 3.0, -0.3),
        ("warm_room", "skin_mid", WindowShape::Big, 3.0, 0.0),
    ];
    for (scene, skin, shape, stops, floor) in cases {
        for seed in [-8, -6] {
            let mut sim = Sim::new(skin_cfg(scene, skin, Some(const_window(shape, stops)), seed), dim(-3.5));
            let mut ctrl = engage(&sim, resume(seed, 0), false);
            sim.run(&mut ctrl, 30.0);
            let what = format!("{scene} {skin} {shape:?} +{stops} from {seed}");
            for (t, o) in over_need(&sim, 10.0, 30.0) {
                assert!(o >= floor, "{what}: face {o:+.2} stops from its rest at {t:.1} s (needs >= {floor})");
            }
            for s in sim.history.iter().filter(|s| s.t >= 5.0) {
                assert!(s.clip < 0.5, "{what}: clipped share {:.2} at {:.1} s", s.clip, s.t);
            }
            assert!(sim.writes_between(20.0, 30.0).is_empty(), "{what}: {:?}", sim.writes_between(20.0, 30.0));
        }
    }
}

/// L26: bright walls and three-stop windows with a noisy light: as quiet as any other room, at every speed. The light
/// skin runs at all three speeds, the other two skin tones at Normal, 60 s each (13.11: the 45 runs of 120 s were most
/// of the suite's time; the loop is quiet from 20 s on or it is not); one test per run group, so they run side by side.
fn bright_walls_hold_still(skin: &'static str, speed: Speed) {
    let cases = [
        (WindowShape::TopAndSides, -2.0),
        (WindowShape::Top, -1.4),
        (WindowShape::OneSide, -2.0),
        (WindowShape::Big, 3.0),
        (WindowShape::TopAndSides, 3.0),
    ];
    for (shape, level) in cases {
        let mut cfg = AutoCfg::default();
        cfg.exposure.speed = speed;
        let cam = SimCfg {
            jitter: 0.05,
            ..skin_cfg("office", skin, Some(const_window(shape, level)), -6)
        };
        let mut sim = Sim::new(cam, dim(-3.5));
        let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
        sim.run(&mut ctrl, 60.0);
        let all = batches(&sim, (-6, 0));
        let late: Vec<Batch> = all.into_iter().filter(|b| b.t >= 20.0).collect();
        let what = format!("{skin} {shape:?} {level:+} {speed:?}: {late:?}");
        assert!(late.len() <= 2, "{what}");
        assert_eq!(guard_batches(&sim, &late, speed.exposure_pace().step_max), 0, "{what}");
        assert!(reversals(&late) <= 1, "{what}");
        assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
        assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
    }
}

#[test]
fn bright_walls_and_three_stop_windows_hold_still_light_skin_slow() {
    bright_walls_hold_still("skin_light", Speed::Slow);
}

#[test]
fn bright_walls_and_three_stop_windows_hold_still_light_skin_normal() {
    bright_walls_hold_still("skin_light", Speed::Normal);
}

#[test]
fn bright_walls_and_three_stop_windows_hold_still_light_skin_fast() {
    bright_walls_hold_still("skin_light", Speed::Fast);
}

#[test]
fn bright_walls_and_three_stop_windows_hold_still_medium_skin() {
    bright_walls_hold_still("skin_mid", Speed::Normal);
}

#[test]
fn bright_walls_and_three_stop_windows_hold_still_dark_skin() {
    bright_walls_hold_still("skin_dark", Speed::Normal);
}

/// The most reversals of moves of 0.15 stop or more in any 10 s (W4's count, 2.7).
fn most_reversals_in_10_s(b: &[Batch]) -> usize {
    let moves: Vec<&Batch> = b.iter().filter(|b| b.dx.abs() >= 0.15).collect();
    let mut most = 0;
    for (i, first) in moves.iter().enumerate() {
        let within: Vec<f64> = moves[i..].iter().take_while(|b| b.t - first.t <= 10.0).map(|b| b.dx.signum()).collect();
        most = most.max(within.windows(2).filter(|w| w[0] != w[1]).count());
    }
    most
}

/// L27: the user leans in front of a bright strip in a lamp-lit room: no pumping. One test per skin tone, 60 s per run
/// (four toggles at 15 s, fifteen at 4 s; 13.11).
fn leaning_before_a_strip(skin: &'static str) {
    for level in [-0.5, 0.0, 0.5] {
        for every in [15.0, 4.0] {
            let lamp = LampLit {
                room: -3.0,
                face: WindowLevel::Toggle { a: 1.0, b: -1.0, every },
            };
            let cfg = SimCfg {
                window: Some(const_window(WindowShape::TopAndSides, level)),
                skin: Some(skin),
                ..lamp_cfg("office", lamp)
            };
            let mut sim = Sim::new(cfg, dim(-6.0));
            let mut ctrl = engage(&sim, resume(-6, 0), false);
            sim.run(&mut ctrl, 60.0);
            let all = batches(&sim, (-6, 0));
            let what = format!("{skin}, strip {level:+}, every {every} s");
            assert!(!would_damp(&all), "{what}");
            assert!(most_reversals_in_10_s(&all) <= 3, "{what}: {} reversals in 10 s", most_reversals_in_10_s(&all));
            assert!(sim.handbacks.is_empty(), "{what}: {:?}", sim.handbacks);
            assert_ne!(state(&ctrl), ExposureState::Hunting, "{what}");
        }
    }
}

#[test]
fn leaning_in_front_of_a_bright_strip_does_not_pump_light_skin() {
    leaning_before_a_strip("skin_light");
}

#[test]
fn leaning_in_front_of_a_bright_strip_does_not_pump_medium_skin() {
    leaning_before_a_strip("skin_mid");
}

#[test]
fn leaning_in_front_of_a_bright_strip_does_not_pump_dark_skin() {
    leaning_before_a_strip("skin_dark");
}

/// L28: a bright patch on one side of the head only (the known limit of 13.6) is bounded: the cap takes little and
/// the face rests over its evenly lit level.
#[test]
fn a_one_sided_bright_wall_stays_bounded() {
    for scene in ["office", "warm_room"] {
        for skin in SKIN_TONES {
            for (shape, level) in [(WindowShape::OneSide, -2.0), (WindowShape::OneSide, -1.0), (WindowShape::Wall, -2.0)] {
                for (e, g, seeded) in WALL_SEEDS {
                    let mut sim = Sim::new(skin_cfg(scene, skin, Some(const_window(shape, level)), e), dim(-6.0));
                    let mut ctrl = engage(&sim, resume(e, g), seeded);
                    let what = format!("{scene} {skin} {shape:?} {level:+} from {e}/{g}");
                    while sim.t < 20.0 - 1e-6 {
                        sim.run(&mut ctrl, sim.t + 1.0);
                        if sim.t >= 10.0 - 1e-6 {
                            let front = front_now(&sim);
                            assert!(front >= -0.8, "{what}: front {front:+.3} at {:.0} s", sim.t);
                        }
                    }
                    for (t, o) in over_need(&sim, 10.0, 20.0) {
                        assert!(o >= 0.15, "{what}: face {o:+.2} stops from its rest at {t:.1} s");
                    }
                }
            }
        }
    }
}

// --- 13.12 Audit fixes, meter (docs/AUTO-CONTROLS.md 13.12) ---

/// Where an evenly lit room (a kit scene, no window) rests at `dim(-6.5)` with `target`: the face's offset in stops
/// from its rest at target 0.45 after 60 s, with the backlight term as built or forced to 0 (`meter::hook`), and the
/// meter's reading of the resting picture.
fn rest_of_an_even_room(
    scene: &'static str,
    skin: Option<&'static str>,
    target: f64,
    term_off: bool,
) -> (f64, meter::ExposureReading, Sim) {
    let mut sim = Sim::new(
        SimCfg {
            scene: Some(scene),
            skin,
            start_e: -6,
            ..SimCfg::default()
        },
        dim(-6.5),
    );
    let mut cfg = AutoCfg::default();
    cfg.exposure.target = target;
    let mut ctrl = engage_with(&sim, cfg, resume(-6, 0), false);
    if term_off {
        meter::hook::with_backlight_off(|| sim.run(&mut ctrl, 60.0));
    } else {
        sim.run(&mut ctrl, 60.0);
    }
    let reading = meter::measure_exposure(&sim.picture(), target);
    (sim.face_over_need(), reading, sim)
}

/// L30 (13.12): an evenly lit room at a high target rests where it rests without the backlight term. The audit's
/// three rows (sim rest at `dim(-6.5)`, as built / term forced to 0): office dark skin at 0.6 +1.74 / +0.89, office
/// medium skin at 0.7 +2.04 / +1.34, warm room medium skin at 0.7 +1.46 / +1.31. The fixed source levels (coded 0.90 /
/// 0.97) counted the wall as a light source there and the term lifted the face; the levels are stops over the target
/// now. The bound is the loop's own resolution: one gain step is 0.03 stop, an exposure step a whole stop that the
/// loop only takes when the error leaves its band.
#[test]
fn an_even_room_at_a_high_target_rests_without_the_backlight_term() {
    // Measured: the two runs rest at the same point (the gain step is 0.03); two steps of room.
    const BAND: f64 = 0.06;
    let rows = [
        ("office_dark_skin", None, 0.6),
        ("office", Some("skin_mid"), 0.7),
        ("warm_room", Some("skin_mid"), 0.7),
    ];
    for (scene, skin, target) in rows {
        let what = format!("{scene} {skin:?} at {target}");
        let (face, reading, sim) = rest_of_an_even_room(scene, skin, target, false);
        let (off_face, _, _) = rest_of_an_even_room(scene, skin, target, true);
        // The room is really one the term could lift (the audit's off-rests are +0.9 to +1.3), not an empty test.
        assert!(off_face >= 0.5, "{what}: the term-off run rests at {off_face:+.2}");
        assert!((face - off_face).abs() <= BAND, "{what}: face {face:+.2}, without the term {off_face:+.2}");
        assert_eq!((reading.backlight, reading.front), (0.0, 0.0), "{what}: {reading:?}");
        assert!(reading.sources <= 0.05, "{what}: {reading:?}");
        assert!(sim.writes_between(40.0, 60.0).is_empty(), "{what}: {:?}", sim.writes_between(40.0, 60.0));
    }
}