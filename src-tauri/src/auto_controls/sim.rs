//! The simulated camera (docs/AUTO-CONTROLS.md 6.1): a discrete-event model driven in 1 ms steps that produces
//! `Stats` for the controller and executes its writes with realistic delays. Tests only.
//!
//! Scene: a 16x16 reflectance map (grey wall, face-like mid patch, two bright cells, dark corner), a scripted light
//! `L(t)` (a linear factor) and optional mains flicker. Light needed is stated in stops: with `L = 2^(-need)` the
//! picture is on target when `E + gs(G) = need` (the scale is calibrated numerically at construction for target 0.45).
//!
//! Not built yet (phase A2, with the faults it needs): white balance (`T(t)`, `K_set`), `ignore_writes`, `no_effect`,
//! `fail`, `meddle`, the `slow` switch for every write.

use std::collections::VecDeque;

use super::meter::{measure_exposure, srgb_decode, srgb_encode, GRID};
use super::*;
use crate::capture::controls::AccessPath;

pub type Light = Box<dyn Fn(f64) -> f64>;

/// A scene `need` stops dark (`E + gs(G) = need` puts it on target), constant.
pub fn dim(need: f64) -> Light {
    let l = (-need).exp2();
    Box::new(move |_| l)
}

/// Gaussian noise from xorshift64*.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn gauss(&mut self) -> f64 {
        let (a, b) = (self.uniform().max(1e-12), self.uniform());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }
}

#[derive(Clone)]
pub struct SimCfg {
    /// `F`.
    pub fps: f64,
    /// The camera has a gain control.
    pub gain: bool,
    /// Stops across the whole gain range.
    pub s_true: f64,
    /// DB-linear to the middle of the range, twice as steep after (the total stays `s_true`).
    pub bent: bool,
    /// The camera lowers its rate when `tau > 0.8 / F`.
    pub rate_drop: bool,
    /// The camera never exceeds this rate, whatever the exposure.
    pub fps_cap: Option<f64>,
    /// The first write to exposure after "open" takes 0.5 s.
    pub slow_first_write: bool,
    pub flicker: Option<(f64, f64)>,
    /// Standard deviation of the per-frame light jitter in stops.
    pub jitter: f64,
    /// `stale_auto`: reads report this exposure and the Auto flag on.
    pub stale_exposure: Option<i32>,
    pub start_e: i32,
    pub start_g: i32,
    pub seed: u64,
}

impl Default for SimCfg {
    fn default() -> Self {
        SimCfg {
            fps: 30.0,
            gain: true,
            s_true: 3.0,
            bent: false,
            rate_drop: false,
            fps_cap: None,
            slow_first_write: true,
            flicker: None,
            jitter: 0.0,
            stale_exposure: None,
            start_e: -6,
            start_g: 0,
            seed: 7,
        }
    }
}

const GAIN_MAX: i32 = 100;
const WRITE_S: f64 = 0.005;
const FIRST_WRITE_S: f64 = 0.5;
/// A write lands at the first frame starting this long after it returned.
const LAND_S: f64 = 0.1;
/// A measurement is submitted at most this often (the engine's rate).
const SUBMIT_EVERY: f64 = 0.066;
const SUB: usize = 16;

/// The scene: reflectances, in-cell texture and the calibrated scale.
struct Scene {
    refl: Vec<f64>,
    tex: [f64; SUB],
    scale: f64,
    s_true: f64,
    bent: bool,
}

impl Scene {
    fn new(s_true: f64, bent: bool) -> Scene {
        let mut refl = vec![1.0; GRID * GRID];
        for y in 0..GRID {
            for x in 0..GRID {
                let i = y * GRID + x;
                if (5..=10).contains(&x) && (3..=9).contains(&y) {
                    refl[i] = 1.6; // the face
                }
                if x >= 12 && y >= 12 {
                    refl[i] = 0.05; // the dark corner
                }
            }
        }
        refl[2 * GRID + 3] = 5.0; // two bright cells
        refl[3 * GRID + 12] = 5.0;
        let mut tex = [0.0; SUB];
        for (s, t) in tex.iter_mut().enumerate() {
            *t = 1.0 + 0.2 * (((s * 7) % 5) as f64 - 2.0) / 2.0;
        }
        let mean = tex.iter().sum::<f64>() / SUB as f64;
        for t in &mut tex {
            *t /= mean;
        }
        let mut scene = Scene {
            refl,
            tex,
            scale: 1.0,
            s_true,
            bent,
        };
        // Calibrate: `E + gs(G) = 0` and `L = 1` is on target (0.45).
        let (mut lo, mut hi) = (-16.0f64, 16.0f64);
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            scene.scale = mid.exp2();
            let e = measure_exposure(&scene.render(0, 0, 1.0, None), 0.45).error;
            if e > 0.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        scene.scale = (0.5 * (lo + hi)).exp2();
        scene
    }

    /// The true stops of gain at setting `g`.
    fn gs(&self, g: i32) -> f64 {
        let u = f64::from(g) / f64::from(GAIN_MAX);
        if self.bent {
            let a = self.s_true / 1.5;
            if u <= 0.5 {
                a * u
            } else {
                a * 0.5 + 2.0 * a * (u - 0.5)
            }
        } else {
            self.s_true * u
        }
    }

    /// The statistics image of a frame exposed with `2^e` s, gain setting `g`, at light factor `l`.
    fn render(&self, e: i32, g: i32, l: f64, mut rng: Option<&mut Rng>) -> Stats {
        let gain = self.gs(g).exp2();
        let base = self.scale * l * f64::from(e).exp2() * gain;
        let mut cells = Vec::with_capacity(GRID * GRID);
        for r in &self.refl {
            let mut sum = 0.0;
            for t in &self.tex {
                let lin = base * r * t;
                let v = match rng.as_deref_mut() {
                    // A covered lens reads black: the camera's black level clamp leaves no noise floor.
                    Some(_) if lin <= 0.0 => 0.0,
                    Some(rng) => {
                        let sigma = 0.01 * lin.sqrt() + 0.002 * gain;
                        let noisy = (lin + sigma * rng.gauss()).clamp(0.0, 1.0);
                        // 8-bit coded, decoded again by the GPU pass.
                        srgb_decode((srgb_encode(noisy) * 255.0).round() / 255.0)
                    }
                    None => lin.clamp(0.0, 1.0),
                };
                sum += v;
            }
            let m = (sum / SUB as f64) as f32;
            cells.push([m, m, m, 1.0]);
        }
        Stats { cells }
    }
}

#[derive(Clone, Copy, Debug)]
struct Capture {
    t: f64,
    e: i32,
    g: i32,
    light: f64,
}

#[derive(Clone, Copy, Debug)]
struct Landing {
    at: f64,
    key: ControlKey,
    value: i32,
}

#[derive(Clone, Copy, Debug)]
struct InFlight {
    key: ControlKey,
    value: i32,
    started: f64,
    done: f64,
}

/// One write as the camera saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WriteRec {
    pub started: f64,
    pub done: f64,
    pub key: ControlKey,
    pub value: i32,
}

/// The oracle's view at one tick.
#[derive(Clone, Copy, Debug)]
pub struct Snap {
    pub t: f64,
    /// True error in stops of the landed state (noiseless), positive = needs more light.
    pub err: f64,
    pub clip: f64,
    pub e: i32,
    pub g: i32,
}

pub struct Sim {
    pub cfg: SimCfg,
    scene: Scene,
    light: Light,
    rng: Rng,
    ms: u64,
    pub t: f64,
    dev_e: i32,
    dev_g: i32,
    landings: Vec<Landing>,
    queue: VecDeque<(ControlKey, i32)>,
    in_flight: Option<InFlight>,
    first_exposure_write: bool,
    next_frame: f64,
    last_submit: f64,
    /// Delivery times of frames.
    frames: Vec<f64>,
    deliveries: VecDeque<(f64, Capture)>,
    pub paused: bool,
    pub resumed_at: f64,
    pub writes: Vec<WriteRec>,
    /// Every `Write` action the controller returned, with the tick time.
    pub issued: Vec<(f64, ControlKey, i32)>,
    pub history: Vec<Snap>,
    /// The highest exposure that was ever applied to a frame.
    pub max_e_applied: i32,
    pub emits: usize,
}

impl Sim {
    pub fn new(cfg: SimCfg, light: Light) -> Sim {
        let scene = Scene::new(cfg.s_true, cfg.bent);
        Sim {
            rng: Rng::new(cfg.seed),
            dev_e: cfg.start_e,
            dev_g: cfg.start_g,
            max_e_applied: cfg.start_e,
            scene,
            light,
            ms: 0,
            t: 0.0,
            landings: Vec::new(),
            queue: VecDeque::new(),
            in_flight: None,
            first_exposure_write: true,
            next_frame: 0.0,
            last_submit: f64::NEG_INFINITY,
            frames: Vec::new(),
            deliveries: VecDeque::new(),
            paused: false,
            resumed_at: f64::NEG_INFINITY,
            writes: Vec::new(),
            issued: Vec::new(),
            history: Vec::new(),
            emits: 0,
            cfg,
        }
    }

    /// The camera as `read_all` reports it (W4GS ranges). With `stale_exposure` the Auto flag is on and the value
    /// is the stale one.
    pub fn caps(&self) -> Vec<ControlInfo> {
        let mut v = vec![ControlInfo {
            key: ControlKey::Exposure,
            min: -13,
            max: -1,
            step: 1,
            default: -6,
            value: self.cfg.stale_exposure.unwrap_or(self.dev_e),
            can_auto: true,
            can_manual: true,
            auto: self.cfg.stale_exposure.is_some(),
            path: AccessPath::Ks,
        }];
        if self.cfg.gain {
            v.push(ControlInfo {
                key: ControlKey::Gain,
                min: 0,
                max: GAIN_MAX,
                step: 1,
                default: 0,
                value: self.dev_g,
                can_auto: false,
                can_manual: true,
                auto: false,
                path: AccessPath::Ks,
            });
        }
        v
    }

    pub fn device(&self) -> (i32, i32) {
        (self.dev_e, self.dev_g)
    }

    fn oracle(&self, target: f64) -> meter::ExposureReading {
        let stats = self
            .scene
            .render(self.dev_e, self.dev_g, (self.light)(self.t), None);
        measure_exposure(&stats, target)
    }

    /// The true stops of gain at a setting.
    pub fn true_gs(&self, g: i32) -> f64 {
        self.scene.gs(g)
    }

    /// The rate of frames delivered in `[a, b]` (intervals, so a window edge does not cost a frame).
    pub fn fps_between(&self, a: f64, b: f64) -> f64 {
        let lo = self.frames.partition_point(|&t| t < a);
        let hi = self.frames.partition_point(|&t| t <= b);
        if hi < lo + 2 {
            return 0.0;
        }
        (hi - lo - 1) as f64 / (self.frames[hi - 1] - self.frames[lo])
    }

    /// The 2 s capture rate the engine reports.
    fn capture_fps(&self) -> f64 {
        let window = self.t.min(2.0);
        if window <= 0.0 {
            return 0.0;
        }
        let hi = self.frames.partition_point(|&t| t <= self.t);
        let lo = self.frames.partition_point(|&t| t <= self.t - window);
        (hi - lo) as f64 / window
    }

    pub fn facts(&self) -> SessionFacts {
        SessionFacts {
            token: Some(1),
            target_fps: self.cfg.fps,
            capture_fps: self.capture_fps(),
            paused: self.paused,
            resumed_at: self.resumed_at,
        }
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
        self.resumed_at = self.t;
    }

    /// Writes started in `[a, b)`.
    pub fn writes_between(&self, a: f64, b: f64) -> Vec<WriteRec> {
        self.writes
            .iter()
            .filter(|w| w.started >= a && w.started < b)
            .copied()
            .collect()
    }

    /// Advance to `until` seconds, running the controller as the runner would: `tick` every 100 ms, writes one at a
    /// time with their delays, a sample every 66 ms two frames after capture.
    pub fn run(&mut self, ctrl: &mut AutoController, until: f64) {
        let end = (until * 1000.0).round() as u64;
        while self.ms < end {
            self.ms += 1;
            self.t = self.ms as f64 / 1000.0;
            self.step_writes(ctrl);
            self.step_frames();
            self.step_deliveries(ctrl);
            if self.ms.is_multiple_of(100) {
                self.step_tick(ctrl);
            }
        }
    }

    fn step_writes(&mut self, ctrl: &mut AutoController) {
        if let Some(w) = self.in_flight {
            if self.t >= w.done {
                self.in_flight = None;
                self.writes.push(WriteRec {
                    started: w.started,
                    done: w.done,
                    key: w.key,
                    value: w.value,
                });
                self.landings.push(Landing {
                    at: w.done + LAND_S,
                    key: w.key,
                    value: w.value,
                });
                ctrl.on_write_done(WriteDone {
                    key: w.key,
                    requested: w.value,
                    readback: Some(w.value),
                    ok: true,
                    started: w.started,
                    done: w.done,
                });
            }
        }
        if self.in_flight.is_none() {
            if let Some((key, value)) = self.queue.pop_front() {
                let slow = key == ControlKey::Exposure
                    && self.first_exposure_write
                    && self.cfg.slow_first_write;
                if key == ControlKey::Exposure {
                    self.first_exposure_write = false;
                }
                let d = if slow { FIRST_WRITE_S } else { WRITE_S };
                self.in_flight = Some(InFlight {
                    key,
                    value,
                    started: self.t,
                    done: self.t + d,
                });
            }
        }
    }

    fn step_frames(&mut self) {
        if self.t < self.next_frame {
            return;
        }
        let start = self.next_frame;
        let mut i = 0;
        while i < self.landings.len() {
            if self.landings[i].at <= start {
                let l = self.landings.remove(i);
                match l.key {
                    ControlKey::Exposure => self.dev_e = l.value,
                    _ => self.dev_g = l.value,
                }
            } else {
                i += 1;
            }
        }
        self.max_e_applied = self.max_e_applied.max(self.dev_e);
        let tau = f64::from(self.dev_e).exp2();
        let mut interval = (1.0 / self.cfg.fps).max(if self.cfg.rate_drop {
            tau / 0.8
        } else {
            tau + 0.001
        });
        if let Some(cap) = self.cfg.fps_cap {
            interval = interval.max(1.0 / cap);
        }
        self.frames.push(start + interval);
        self.next_frame = start + interval;
        if !self.paused && start - self.last_submit >= SUBMIT_EVERY {
            self.last_submit = start;
            let mut light = (self.light)(start);
            if let Some((a, hz)) = self.cfg.flicker {
                let w = std::f64::consts::TAU * hz;
                light *= 1.0 + a * ((w * (start + interval - tau)).cos() - (w * (start + interval)).cos()) / (w * tau);
            }
            if self.cfg.jitter > 0.0 {
                light *= (self.cfg.jitter * self.rng.gauss()).exp2();
            }
            self.deliveries.push_back((
                start + 2.0 * interval,
                Capture {
                    t: start,
                    e: self.dev_e,
                    g: self.dev_g,
                    light,
                },
            ));
        }
    }

    fn step_deliveries(&mut self, ctrl: &mut AutoController) {
        while self.deliveries.front().is_some_and(|(at, _)| *at <= self.t) {
            let (_, c) = self.deliveries.pop_front().expect("checked");
            if self.paused {
                continue;
            }
            let stats = self.scene.render(c.e, c.g, c.light, Some(&mut self.rng));
            ctrl.on_sample(
                &MeterSample {
                    token: 1,
                    capture_qpc: 0,
                    stats,
                },
                c.t,
            );
        }
    }

    fn step_tick(&mut self, ctrl: &mut AutoController) {
        let facts = self.facts();
        let actions = ctrl.tick(self.t, &facts);
        for a in actions {
            match a {
                Action::Write { key, value } => {
                    self.issued.push((self.t, key, value));
                    self.queue.push_back((key, value));
                }
                Action::Emit => self.emits += 1,
                Action::ReadBack { .. } | Action::HandBack { .. } => {}
            }
        }
        let o = self.oracle(ctrl_target(ctrl));
        self.history.push(Snap {
            t: self.t,
            err: o.error,
            clip: o.clip,
            e: self.dev_e,
            g: self.dev_g,
        });
    }
}

/// The target the oracle measures against (the controller's own setting).
fn ctrl_target(ctrl: &AutoController) -> f64 {
    ctrl.cfg.exposure.target
}
