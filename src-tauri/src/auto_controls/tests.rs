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

#[test]
fn case_03_darker_than_the_limits_allow() {
    // The scene needs -1.0; the limits give E = -5 and Most gain 75 = 2.25 stops: -2.75 at most.
    let mut sim = Sim::new(SimCfg::default(), dim(-1.0));
    let mut ctrl = engage(&sim, resume(-6, 0), false);
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
    let mut ctrl = engage(&sim, resume(-5, 0), true);
    sim.run(&mut ctrl, 8.0);
    assert_eq!(sim.device(), (-5, 75), "the gain did not climb to Most gain first");
    assert_eq!(state(&ctrl), ExposureState::LimitGain);
    // The user drags Most gain to its minimum.
    let mut cfg = AutoCfg::default();
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
                Some(Resume {
                    exposure: Some(-5),
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
            start_e: -5,
            start_g: 75,
            ..SimCfg::default()
        },
        dim(-1.0),
    );
    let ctrl2 = engage(&sim2, resume(-5, 75), true);
    assert_eq!(ctrl2.seeds(30.0).exposure, Some((-5, Some(75))));
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

fn wb_step_check(alpha_true: f64, to: f64) -> AutoController {
    // 6500 K light, matched; at t = 5 s the light turns to `to` Kelvin.
    let temp: Light = Box::new(move |t| if t < 5.0 { 6500.0 } else { to });
    let mut sim = wb_sim(alpha_true, 6500, temp);
    let mut ctrl = wb_ctrl(&sim, 6500, true);
    sim.run(&mut ctrl, 25.0);
    assert!(wb_writes(&sim).iter().all(|w| w.started >= 5.0), "wrote before the light changed");
    let at = wb_settled_at(&sim, 5.0, 25.0, 0.03)
        .unwrap_or_else(|| panic!("α_true {alpha_true}: never settled: {:?}", sim.history.last()));
    assert!(at - 5.0 <= 6.0, "α_true {alpha_true}: |e_wb| < 0.03 only after {} s", at - 5.0);
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

#[test]
fn case_15_white_balance_step() {
    let ctrl = wb_step_check(0.008, 3000.0);
    // The text of 4.2 with the Kelvin the loop holds.
    let st = ctrl.status().white_balance.unwrap();
    assert_eq!(st.message, Some(format!("Holding · {} K", st.kelvin)));
    assert!((2900..=3100).contains(&st.kelvin), "{}", st.kelvin);
    // Learned α within 30 % of the truth, for a camera half as sensitive and one 1.5 times as sensitive. At 0.012 a
    // 3000 K light would be a cast of ln(B/R) = -2.15 (blue a tenth of red): every cell saturated, nothing neutral to
    // trust, and the loop rightly holds (conf_wb < 0.3). That camera gets a 4000 K light.
    for (alpha_true, to) in [(0.004, 3000.0), (0.012, 4000.0)] {
        let ctrl = wb_step_check(alpha_true, to);
        let a = ctrl.learned().wb_alpha.expect("nothing learned");
        assert!((a - alpha_true).abs() <= 0.3 * alpha_true, "learned α {a} for a true {alpha_true}");
    }
}

#[test]
fn a_cast_with_nothing_neutral_to_trust_is_held() {
    // The 3000 K light on a very sensitive camera: ln(B/R) = -2.15. The loop does not guess.
    let temp: Light = Box::new(|t| if t < 5.0 { 6500.0 } else { 3000.0 });
    let mut sim = wb_sim(0.012, 6500, temp);
    let mut ctrl = wb_ctrl(&sim, 6500, true);
    sim.run(&mut ctrl, 20.0);
    assert!(wb_writes(&sim).is_empty(), "{:?}", wb_writes(&sim));
    assert!(sim.handbacks.is_empty());
}

#[test]
fn case_16_light_warmer_than_the_camera_can_correct() {
    // A 2000 K lamp: the camera's warmest setting (2800 K) is still too cool for it.
    let mut sim = wb_sim(0.008, 3500, kelvin(2000.0));
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
    assert_eq!(
        ctrl.status().white_balance.unwrap().message.as_deref(),
        Some("White balance at its limit (2800 K): the light is warmer than this camera can correct. Auto Adjust can finish it.")
    );
}

#[test]
fn case_16_light_cooler_than_the_camera_can_correct() {
    let mut sim = wb_sim(0.008, 5500, kelvin(12_000.0));
    let mut ctrl = wb_ctrl(&sim, 5500, true);
    sim.run(&mut ctrl, 14.0);
    assert_eq!(sim.device_k(), 6500);
    assert_eq!(wb_state(&ctrl), WhiteBalanceState::AtLimit);
    assert!(ctrl
        .status()
        .white_balance
        .unwrap()
        .message
        .unwrap()
        .contains("the light is cooler than this camera can correct"));
}

#[test]
fn case_17_white_balance_waits_for_exposure() {
    // One stop brighter and a 3500 K light at t = 8 s: exposure has to move twice; white balance must wait for it.
    let base = (4.0f64).exp2();
    let light: Light = Box::new(move |t| if t < 8.0 { base } else { base * 4.0 });
    let temp: Light = Box::new(|t| if t < 8.0 { 6500.0 } else { 3500.0 });
    let cfg = SimCfg {
        wb: Some(WbSim::default()),
        start_e: -5,
        start_g: 33,
        ..SimCfg::default()
    };
    let cfg = SimCfg {
        wb: cfg.wb.map(|w| WbSim { start_k: 6500, ..w }),
        ..cfg
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
    for i in 0..60 {
        ctrl.on_sample(
            &MeterSample {
                token: 1,
                capture_qpc: 0,
                stats: red.clone(),
            },
            0.5 + f64::from(i) * 0.066,
        );
    }
    let actions = ctrl.tick(2.0, &sim.facts());
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Write { .. })),
        "{actions:?}"
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
