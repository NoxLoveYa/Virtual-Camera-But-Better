//! The cases of docs/AUTO-CONTROLS.md 6.2 that phase A1 owns (1-14, 23, 24), against the simulated camera.
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
    assert!(settled_at(&sim, 25.0, 35.0, 0.33).is_some(), "no recovery");
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

#[test]
fn support_of_exposure() {
    let sim = Sim::new(SimCfg::default(), dim(-3.5));
    let l = Learned::default();
    assert_eq!(AutoController::support(&sim.caps(), &l).exposure, Offer::Full);
    let no_gain = Sim::new(
        SimCfg {
            gain: false,
            ..SimCfg::default()
        },
        dim(-3.5),
    );
    assert_eq!(AutoController::support(&no_gain.caps(), &l).exposure, Offer::Stops);
    let mut caps = sim.caps();
    caps[0].min = 0;
    caps[0].max = 10_000;
    assert_eq!(
        AutoController::support(&caps, &l).exposure,
        Offer::No(NoOffer::UnknownUnits)
    );
    let mut caps = sim.caps();
    caps[0].can_manual = false;
    assert_eq!(AutoController::support(&caps, &l).exposure, Offer::No(NoOffer::NoManual));
    let blocked = Learned {
        blocked: vec![ControlKey::Exposure],
        ..Learned::default()
    };
    assert_eq!(
        AutoController::support(&sim.caps(), &blocked).exposure,
        Offer::No(NoOffer::Blocked)
    );
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
